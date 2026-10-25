//! Hub-owned delivery of queued manager messages (#1017 slice 3).
//!
//! Runs after the registry tick, never as its own socket loop. A queued row is
//! sent only while its peer is enabled, paired, read- and dispatch-enabled and
//! the target is still in the operator-declared scope. Before every send the
//! hub re-verifies the remote identity and reads the target's status fresh. A
//! running target is not waited on (#2566): the satellite queues the message
//! for the target's next tool or turn boundary. An interrupted target keeps
//! the row queued until it expires. The satellite is the final arbiter: it
//! re-checks its own policy and never interrupts a running session.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use rsi_common::SessionStatus;
use rsi_common::satellite::SATELLITE_WIRE_VERSION_V1;
use rsi_common::satellite::SatelliteUuidV1;
use rsi_common::satellite_dispatch::{
    SATELLITE_BUSY, SATELLITE_MESSAGE_KEY_CONFLICT, SATELLITE_SENDER_LABEL_MAX_BYTES,
    SATELLITE_TARGET_NOT_AUTHORIZED, SATELLITE_WAITING_PEER_UNREACHABLE,
    SATELLITE_WAITING_REMOTE_BUSY, SatelliteDeliverOutcomeV1, SatelliteDeliverRequestV1,
    find_remote_terminal, satellite_launch_failure_class,
};
use tokio::io::BufReader;
use uuid::Uuid;

use super::hub::registry_peer;
use super::link::connect_owned_socket;
use super::poll::{DeliverReply, RPC_DEADLINE, call_deliver_rpc, call_read_rpc, probe_link};
use super::registry::{ContinuityResult, PeerContinuity, RegistryPeer};
use crate::store::Store;
use crate::store::satellite_dispatch::QueuedSatelliteMessage;

const ROUND_LIMIT: usize = 16;
#[cfg(not(test))]
const RETRY_AFTER: Duration = Duration::from_secs(15);
#[cfg(test)]
const RETRY_AFTER: Duration = Duration::from_millis(20);
const DELIVERY_DEADLINE: Duration = Duration::from_secs(30);

/// Process-local retry pacing. Rows, receipts and expiry live in the store.
#[derive(Debug, Default)]
pub(crate) struct DispatchState {
    next_try: HashMap<Uuid, Instant>,
    notice_cursor: Option<Uuid>,
    stall_cursor: Option<Uuid>,
}

/// Why a still-queued row was not delivered by the last attempt (#2566).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WaitReason {
    /// The target cannot take the message now (interrupted, or a satellite
    /// that answered `busy`).
    RemoteBusy,
    /// The target is failed, archived or deleted (#2744) and can take no turn
    /// until it is repaired. Holds the satellite's typed
    /// `remote_terminal|<status>|<stop reason>` token, stored whole.
    RemoteTerminal(String),
    /// The peer could not be reached or verified.
    PeerUnreachable,
}

impl WaitReason {
    /// The value stored as the row's transient wait reason.
    pub(crate) fn stored(&self) -> &str {
        match self {
            Self::RemoteBusy => SATELLITE_WAITING_REMOTE_BUSY,
            Self::RemoteTerminal(token) => token,
            Self::PeerUnreachable => SATELLITE_WAITING_PEER_UNREACHABLE,
        }
    }
}

/// What one delivery attempt established.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    /// The satellite acknowledged this exact `message_id`.
    Delivered,
    /// The satellite recorded an earlier attempt for this `message_id` and
    /// cannot say whether it took effect. The row settles `failed` with
    /// `delivery_uncertain` and is never replayed (at most once, #1059).
    Uncertain,
    /// Keep the row queued, for the stated reason.
    NotYet(WaitReason),
    /// A permanent refusal; the row settles `failed` with this class.
    Refused(&'static str),
}

pub(crate) fn sanitize_label(raw: &str) -> String {
    let mut label: String = raw.chars().filter(|c| !c.is_control()).collect();
    if label.trim().is_empty() {
        label = "manager".into();
    }
    while label.len() > SATELLITE_SENDER_LABEL_MAX_BYTES {
        label.pop();
    }
    label
}

fn classify_refusal(message: &str) -> DeliveryOutcome {
    // #2744: first, so a stop reason that happens to contain `busy` is never
    // read as one.
    if let Some(token) = find_remote_terminal(message) {
        DeliveryOutcome::NotYet(WaitReason::RemoteTerminal(token.to_string()))
    } else if message.contains(SATELLITE_BUSY) {
        DeliveryOutcome::NotYet(WaitReason::RemoteBusy)
    } else if message.contains(SATELLITE_TARGET_NOT_AUTHORIZED) {
        DeliveryOutcome::Refused("target_not_authorized")
    } else if message.contains(SATELLITE_MESSAGE_KEY_CONFLICT) {
        DeliveryOutcome::Refused("message_conflict")
    } else if let Some(class) = satellite_launch_failure_class(message) {
        DeliveryOutcome::Refused(class)
    } else {
        DeliveryOutcome::Refused("satellite_refused")
    }
}

/// One custody-checked delivery. Errors and refused-busy targets are `NotYet`.
pub(crate) async fn deliver_one(
    root: &Path,
    peer: &RegistryPeer,
    request: &SatelliteDeliverRequestV1,
) -> DeliveryOutcome {
    let mut links: Vec<_> = peer.links.iter().filter(|link| link.enabled).collect();
    links.sort_by_key(|link| (link.priority, link.id));
    for link in links {
        let mut continuity = PeerContinuity::new(peer.expected_installation_id);
        continuity.begin_round();
        // A fresh read: identity continuity plus the target's status now.
        let Ok(snapshot) = probe_link(root, peer, link, &mut continuity).await else {
            continue;
        };
        let target = request.remote_session_id.0;
        // #1112: a declared seat that rotated is terminal or absent in the
        // snapshot, which carries no lineage. Absent and terminal targets are
        // therefore not judged here: the satellite resolves the declared
        // seat's current rotation tip (project-bound, unique, bounded) and
        // answers `busy`, `target_not_authorized`, queues or delivers. Only a
        // seat the snapshot shows interrupted keeps the hub-side wait.
        let snapshot_blocks = snapshot
            .sessions
            .iter()
            .find(|session| session.session_id.0 == target)
            .is_some_and(|session| matches!(session.status, SessionStatus::Interrupted));
        // Interrupted is not idle: hub mail waits (and may expire) rather
        // than resuming a session the operator or a restart stopped. A running
        // target is NOT waited on (#2566): the satellite queues the message
        // for its next tool or turn boundary, so a manager that is busy for an
        // hour still gets the instruction.
        if snapshot_blocks {
            return DeliveryOutcome::NotYet(WaitReason::RemoteBusy);
        }
        let Ok(socket) = connect_owned_socket(root, &link.socket_path, RPC_DEADLINE).await else {
            continue;
        };
        let mut rpc = BufReader::new(socket);
        let Ok(value) = call_read_rpc(
            &mut rpc,
            "GetSatelliteIdentity",
            serde_json::Value::Null,
            super::poll::MAX_RPC_ENVELOPE_BYTES,
            RPC_DEADLINE,
        )
        .await
        else {
            continue;
        };
        let Ok(identity) =
            serde_json::from_value::<rsi_common::satellite::SatelliteIdentityV1>(value)
        else {
            continue;
        };
        if identity.installation_id.0 != snapshot.installation_id
            || identity.daemon_incarnation_id.0 != snapshot.incarnation_id
            || continuity.observe(
                link.id,
                identity.installation_id.0,
                identity.daemon_incarnation_id.0,
            ) != ContinuityResult::Matched
        {
            return DeliveryOutcome::NotYet(WaitReason::PeerUnreachable);
        }
        return match call_deliver_rpc(&mut rpc, request, RPC_DEADLINE).await {
            Ok(DeliverReply::Accepted(result)) => match result.outcome {
                SatelliteDeliverOutcomeV1::Uncertain => DeliveryOutcome::Uncertain,
                SatelliteDeliverOutcomeV1::Delivered
                | SatelliteDeliverOutcomeV1::AlreadyDelivered => DeliveryOutcome::Delivered,
            },
            Ok(DeliverReply::Refused(message)) => classify_refusal(&message),
            Err(_) => DeliveryOutcome::NotYet(WaitReason::PeerUnreachable),
        };
    }
    DeliveryOutcome::NotYet(WaitReason::PeerUnreachable)
}

/// Settle expired rows, then attempt each still-queued row once (paced).
pub(crate) async fn dispatch_round(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    root: &Path,
    state: &mut DispatchState,
) {
    let queued = {
        let guard = store.lock().await;
        if let Err(error) = guard.expire_satellite_messages(Utc::now()) {
            tracing::warn!("satellite dispatch: expiry settlement failed: {error}");
            return;
        }
        match guard.reconcile_satellite_message_notices(state.notice_cursor) {
            Ok(cursor) => state.notice_cursor = cursor,
            Err(error) => {
                tracing::warn!("satellite dispatch: notice reconciliation failed: {error}")
            }
        }
        match guard.notify_stalled_satellite_messages(Utc::now(), state.stall_cursor) {
            Ok(cursor) => state.stall_cursor = cursor,
            Err(error) => {
                tracing::warn!("satellite dispatch: stalled-message notice failed: {error}")
            }
        }
        match guard.queued_satellite_messages(ROUND_LIMIT) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!("satellite dispatch: queue read failed: {error}");
                return;
            }
        }
    };
    let live: std::collections::HashSet<Uuid> = queued.iter().map(|row| row.id).collect();
    state.next_try.retain(|id, _| live.contains(id));
    for row in queued {
        let now = Instant::now();
        if state.next_try.get(&row.id).is_some_and(|due| *due > now) {
            continue;
        }
        state.next_try.insert(row.id, now + RETRY_AFTER);
        attempt(store, root, &row).await;
    }
}

/// The wire request for one queued row: the target travels with the project
/// that authorized it (#1928), so the satellite checks the target against
/// that project's live seat.
pub(crate) fn deliver_request(
    row: &QueuedSatelliteMessage,
    hub_installation: Uuid,
    sender_label: String,
) -> SatelliteDeliverRequestV1 {
    SatelliteDeliverRequestV1 {
        wire_version: SATELLITE_WIRE_VERSION_V1,
        message_id: row.id,
        hub_installation_id: SatelliteUuidV1(hub_installation),
        sender_label,
        sender_session_id: row.owner_session_id,
        remote_session_id: SatelliteUuidV1(row.remote_session_id),
        scope_project_id: row.scope_project_id.map(SatelliteUuidV1),
        message: row.payload.clone(),
    }
}

async fn attempt(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    root: &Path,
    row: &QueuedSatelliteMessage,
) {
    let prepared = {
        let guard = store.lock().await;
        // Re-read policy on every attempt so an operator switching dispatch
        // off, or narrowing scope, stops delivery immediately.
        let Ok(Some(_)) =
            guard.dispatch_peer_facts(row.peer_id, row.remote_session_id, row.scope_project_id)
        else {
            return;
        };
        let Ok(view) = guard.satellite_registry_view() else {
            return;
        };
        let Some(value) = view
            .peers
            .iter()
            .find(|value| value.config.peer_id.0 == row.peer_id)
        else {
            return;
        };
        let Ok(hub_installation) = guard.satellite_installation_id() else {
            return;
        };
        let label = guard
            .get_session(row.owner_session_id)
            .ok()
            .flatten()
            .map(|session| {
                session
                    .agent_role
                    .clone()
                    .or(session.title.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let _ = guard.note_satellite_message_attempt(row.id, Utc::now());
        (
            registry_peer(value),
            hub_installation,
            sanitize_label(&label),
        )
    };
    let (peer, hub_installation, label) = prepared;
    let request = deliver_request(row, hub_installation, label);
    let outcome = tokio::time::timeout(DELIVERY_DEADLINE, deliver_one(root, &peer, &request))
        .await
        .unwrap_or(DeliveryOutcome::NotYet(WaitReason::PeerUnreachable));
    let guard = store.lock().await;
    let now = Utc::now();
    let settled = match outcome {
        DeliveryOutcome::Delivered => guard.settle_satellite_message(row.id, "sent", None, now),
        DeliveryOutcome::Uncertain => {
            guard.settle_satellite_message(row.id, "failed", Some("delivery_uncertain"), now)
        }
        DeliveryOutcome::Refused(class) => {
            guard.settle_satellite_message(row.id, "failed", Some(class), now)
        }
        DeliveryOutcome::NotYet(reason) => {
            // #2566: the sender can read why the message is still waiting.
            if let Err(error) = guard.note_satellite_message_wait(row.id, reason.stored(), now) {
                tracing::warn!(message_id = %row.id, "satellite dispatch: wait reason not recorded: {error}");
            }
            return;
        }
    };
    if let Err(error) = settled {
        tracing::warn!(message_id = %row.id, "satellite dispatch: settlement failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rsi_common::rpc::{RpcRequest, RpcResponse};
    use rsi_common::satellite::{
        SatelliteLinkConfigV1, SatelliteLinkDirectionV1, SatellitePeerConfigV1,
        SatellitePutLinkRequestV1, SatellitePutPeerRequestV1, SatelliteSessionPageV1,
        SatelliteSessionSummaryV1,
    };
    use rsi_common::satellite_dispatch::{
        AgentSendSatelliteMessageRequestV1, SatelliteDeliverOutcomeV1, SatelliteDeliverResultV1,
    };
    use rsi_common::{SessionProvider, SessionStatus};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// A fake satellite: answers the three methods the hub may call and
    /// records every delivery and every call it receives.
    struct Fake {
        status: Arc<StdMutex<SessionStatus>>,
        calls: Arc<AtomicUsize>,
        delivered: Arc<StdMutex<Vec<Uuid>>>,
        /// `(remote_session_id, scope_project_id)` of every delivery on the wire.
        wire: Arc<StdMutex<Vec<(Uuid, Option<Uuid>)>>>,
        refuse: Arc<StdMutex<Option<&'static str>>>,
        uncertain: Arc<StdMutex<bool>>,
        /// Whether the snapshot lists the target at all (a rotated seat is not).
        listed: Arc<StdMutex<bool>>,
    }

    struct Rig {
        _temp: tempfile::TempDir,
        root: std::path::PathBuf,
        store: Arc<rsid_core::store_lock::Mutex<Store>>,
        owner: Uuid,
        peer: Uuid,
        remote: Uuid,
        fake: Fake,
    }

    async fn rig(dispatch_enabled: bool, status: SessionStatus) -> Rig {
        let temp = crate::test_support::socket_tempdir("sat-disp-");
        let root = temp.path().join("satellites");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("peer.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        let installation = Uuid::new_v4();
        let incarnation = Uuid::new_v4();
        let remote = Uuid::new_v4();
        let fake = Fake {
            status: Arc::new(StdMutex::new(status)),
            calls: Arc::new(AtomicUsize::new(0)),
            delivered: Arc::new(StdMutex::new(Vec::new())),
            wire: Arc::new(StdMutex::new(Vec::new())),
            refuse: Arc::new(StdMutex::new(None)),
            uncertain: Arc::new(StdMutex::new(false)),
            listed: Arc::new(StdMutex::new(true)),
        };
        let (st, calls, delivered, wire, refuse, uncertain, listed) = (
            Arc::clone(&fake.status),
            Arc::clone(&fake.calls),
            Arc::clone(&fake.delivered),
            Arc::clone(&fake.wire),
            Arc::clone(&fake.refuse),
            Arc::clone(&fake.uncertain),
            Arc::clone(&fake.listed),
        );
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (st, calls, delivered, wire, refuse, uncertain, listed) = (
                    Arc::clone(&st),
                    Arc::clone(&calls),
                    Arc::clone(&delivered),
                    Arc::clone(&wire),
                    Arc::clone(&refuse),
                    Arc::clone(&uncertain),
                    Arc::clone(&listed),
                );
                tokio::spawn(async move {
                    let mut stream = tokio::io::BufReader::new(stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        calls.fetch_add(1, Ordering::SeqCst);
                        let request: RpcRequest = serde_json::from_str(&line).unwrap();
                        let response = match request.method.as_str() {
                            "GetSatelliteIdentity" => RpcResponse::success(
                                Some(request.id.clone().into()),
                                serde_json::to_value(crate::satellite::identity(
                                    installation,
                                    incarnation,
                                ))
                                .unwrap(),
                            ),
                            "ListSatelliteSessions" => {
                                let status = *st.lock().unwrap();
                                let is_listed = *listed.lock().unwrap();
                                let now = Utc::now();
                                let page = SatelliteSessionPageV1 {
                                    wire_version: SATELLITE_WIRE_VERSION_V1,
                                    installation_id: SatelliteUuidV1(installation),
                                    daemon_incarnation_id: SatelliteUuidV1(incarnation),
                                    snapshot_id: SatelliteUuidV1(Uuid::new_v4()),
                                    snapshot_total_sessions: u32::from(is_listed),
                                    snapshot_offset: 0,
                                    observed_at: now,
                                    sessions: if is_listed {
                                        vec![SatelliteSessionSummaryV1 {
                                            session_id: SatelliteUuidV1(remote),
                                            title: None,
                                            provider: SessionProvider::Claude,
                                            status,
                                            working_dir: None,
                                            created_at: now,
                                            updated_at: now,
                                        }]
                                    } else {
                                        Vec::new()
                                    },
                                    next_cursor: None,
                                };
                                RpcResponse::success(
                                    Some(request.id.clone().into()),
                                    serde_json::to_value(page).unwrap(),
                                )
                            }
                            "DeliverHubMessage" => {
                                if let Some(code) = *refuse.lock().unwrap() {
                                    RpcResponse::error(
                                        Some(request.id.clone().into()),
                                        rsi_common::RpcError {
                                            code: -32000,
                                            message: format!("Policy denied: {code}"),
                                            data: None,
                                        },
                                    )
                                } else {
                                    let params: SatelliteDeliverRequestV1 =
                                        serde_json::from_value(request.params.clone()).unwrap();
                                    delivered.lock().unwrap().push(params.message_id);
                                    wire.lock().unwrap().push((
                                        params.remote_session_id.0,
                                        params.scope_project_id.map(|id| id.0),
                                    ));
                                    RpcResponse::success(
                                        Some(request.id.clone().into()),
                                        serde_json::to_value(SatelliteDeliverResultV1 {
                                            message_id: params.message_id,
                                            outcome: if *uncertain.lock().unwrap() {
                                                SatelliteDeliverOutcomeV1::Uncertain
                                            } else {
                                                SatelliteDeliverOutcomeV1::Delivered
                                            },
                                            redirected: None,
                                        })
                                        .unwrap(),
                                    )
                                }
                            }
                            other => panic!("hub called a method outside the allowlist: {other}"),
                        };
                        let mut bytes = serde_json::to_vec(&response).unwrap();
                        bytes.push(b'\n');
                        stream.get_mut().write_all(&bytes).await.unwrap();
                    }
                });
            }
        });
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let session = rsid_store::test_support::make_test_session();
        let peer = Uuid::new_v4();
        {
            let guard = store.lock().await;
            guard.insert_session(&session).unwrap();
            let revision = guard.satellite_registry_revision().unwrap();
            guard
                .put_satellite_peer(
                    &SatellitePutPeerRequestV1 {
                        expected_registry_revision: revision,
                        peer: SatellitePeerConfigV1 {
                            peer_id: SatelliteUuidV1(peer),
                            label: "peer".into(),
                            expected_installation_id: Some(SatelliteUuidV1(installation)),
                            enabled: true,
                            read_enabled: true,
                            dispatch_enabled,
                        },
                        repair_quarantine: false,
                    },
                    &root,
                )
                .unwrap();
            let revision = guard.satellite_registry_revision().unwrap();
            guard
                .put_satellite_link(
                    &SatellitePutLinkRequestV1 {
                        expected_registry_revision: revision,
                        peer_id: SatelliteUuidV1(peer),
                        link: SatelliteLinkConfigV1 {
                            link_id: SatelliteUuidV1(Uuid::new_v4()),
                            direction: SatelliteLinkDirectionV1::DialHomeReverse,
                            socket_path: socket.to_string_lossy().into_owned(),
                            ssh_target: None,
                            trust_reference: "ssh-config:peer".into(),
                            enabled: true,
                            priority: 0,
                        },
                    },
                    &root,
                )
                .unwrap();
            let revision = guard.satellite_registry_revision().unwrap();
            guard
                .put_satellite_peer_scope(revision, peer, &[SatelliteUuidV1(remote)])
                .unwrap();
        }
        Rig {
            _temp: temp,
            root,
            store,
            owner: session.id,
            peer,
            remote,
            fake,
        }
    }

    async fn queue(rig: &Rig, key: &str) -> Uuid {
        rig.store
            .lock()
            .await
            .queue_satellite_message(
                rig.owner,
                &AgentSendSatelliteMessageRequestV1 {
                    peer_id: SatelliteUuidV1(rig.peer),
                    remote_session_id: SatelliteUuidV1(rig.remote),
                    scope_project_id: None,
                    message: "please rebase".into(),
                    idempotency_key: key.into(),
                    expires_at: None,
                    issue_refs: Vec::new(),
                },
                Utc::now(),
            )
            .unwrap()
            .message_id
    }

    async fn state(rig: &Rig, id: Uuid) -> String {
        rig.store
            .lock()
            .await
            .satellite_message_state(id)
            .unwrap()
            .unwrap()
            .0
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn idle_target_receives_the_message_exactly_once_across_rounds() {
        let rig = rig(true, SessionStatus::Completed).await;
        let id = queue(&rig, "k1").await;
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "sent");
        tokio::time::sleep(Duration::from_millis(40)).await;
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(*rig.fake.delivered.lock().unwrap(), vec![id]);
    }

    /// Review #1943: the project that authorized a session target travels with
    /// it over the link, so the satellite can validate live ancestry.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_project_bound_message_carries_its_project_to_the_satellite() {
        let rig = rig(true, SessionStatus::Completed).await;
        let (project, descendant) = (Uuid::new_v4(), Uuid::new_v4());
        let id = {
            let guard = rig.store.lock().await;
            let revision = guard.satellite_registry_revision().unwrap();
            guard
                .put_satellite_peer_scope_entries(
                    revision,
                    rig.peer,
                    &[],
                    &[SatelliteUuidV1(project)],
                )
                .unwrap();
            guard
                .queue_satellite_message(
                    rig.owner,
                    &AgentSendSatelliteMessageRequestV1 {
                        peer_id: SatelliteUuidV1(rig.peer),
                        remote_session_id: SatelliteUuidV1(descendant),
                        scope_project_id: Some(SatelliteUuidV1(project)),
                        message: "please rebase".into(),
                        idempotency_key: "k1".into(),
                        expires_at: None,
                        issue_refs: Vec::new(),
                    },
                    Utc::now(),
                )
                .unwrap()
                .message_id
        };
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "sent");
        assert_eq!(
            *rig.fake.wire.lock().unwrap(),
            vec![(descendant, Some(project))]
        );

        // Narrowing the hub scope stops a queued bound message before it is sent.
        let second = {
            let guard = rig.store.lock().await;
            let second = guard
                .queue_satellite_message(
                    rig.owner,
                    &AgentSendSatelliteMessageRequestV1 {
                        peer_id: SatelliteUuidV1(rig.peer),
                        remote_session_id: SatelliteUuidV1(descendant),
                        scope_project_id: Some(SatelliteUuidV1(project)),
                        message: "second".into(),
                        idempotency_key: "k2".into(),
                        expires_at: None,
                        issue_refs: Vec::new(),
                    },
                    Utc::now(),
                )
                .unwrap()
                .message_id;
            let revision = guard.satellite_registry_revision().unwrap();
            guard
                .put_satellite_peer_scope_entries(revision, rig.peer, &[], &[])
                .unwrap();
            second
        };
        tokio::time::sleep(Duration::from_millis(40)).await;
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, second).await, "queued");
        assert_eq!(rig.fake.wire.lock().unwrap().len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_uncertain_reply_settles_failed_delivery_uncertain_and_is_never_replayed() {
        let rig = rig(true, SessionStatus::Completed).await;
        *rig.fake.uncertain.lock().unwrap() = true;
        let id = queue(&rig, "k1").await;
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "failed");
        let class: Option<String> = rig
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT safe_error_class FROM satellite_messages WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(class.as_deref(), Some("delivery_uncertain"));
        // A manager re-reading its message by idempotency key sees the class.
        let receipt = rig
            .store
            .lock()
            .await
            .queue_satellite_message(
                rig.owner,
                &AgentSendSatelliteMessageRequestV1 {
                    peer_id: SatelliteUuidV1(rig.peer),
                    remote_session_id: SatelliteUuidV1(rig.remote),
                    scope_project_id: None,
                    message: "please rebase".into(),
                    idempotency_key: "k1".into(),
                    expires_at: None,
                    issue_refs: Vec::new(),
                },
                Utc::now(),
            )
            .unwrap();
        assert!(receipt.replayed);
        assert_eq!(receipt.state, "failed");
        assert_eq!(
            receipt.settled_error_class.as_deref(),
            Some("delivery_uncertain")
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(rig.fake.delivered.lock().unwrap().len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn running_target_is_sent_to_the_satellite_without_waiting_for_idle() {
        // #2566: the hub does not hold a message for a busy target; the
        // satellite queues it for the target's next boundary and acknowledges.
        let rig = rig(true, SessionStatus::Running).await;
        let id = queue(&rig, "k1").await;
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "sent");
        assert_eq!(*rig.fake.delivered.lock().unwrap(), vec![id]);
    }

    async fn replay_receipt(
        rig: &Rig,
        key: &str,
    ) -> rsi_common::satellite_dispatch::AgentSendSatelliteMessageReceiptV1 {
        rig.store
            .lock()
            .await
            .queue_satellite_message(
                rig.owner,
                &AgentSendSatelliteMessageRequestV1 {
                    peer_id: SatelliteUuidV1(rig.peer),
                    remote_session_id: SatelliteUuidV1(rig.remote),
                    scope_project_id: None,
                    message: "please rebase".into(),
                    idempotency_key: key.into(),
                    expires_at: None,
                    issue_refs: Vec::new(),
                },
                Utc::now(),
            )
            .unwrap()
    }

    /// #2744: a satellite that names a terminal target keeps the row queued
    /// with the target's real status and stop reason, never `remote_busy`; a
    /// stop reason that contains `busy` is not read as one.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_terminal_remote_target_is_reported_as_remote_terminal_not_busy() {
        let rig = rig(true, SessionStatus::Completed).await;
        let id = queue(&rig, "k1").await;
        *rig.fake.refuse.lock().unwrap() =
            Some("remote_terminal|failed|provider_error:codex_busy_401");
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "queued");
        let waiting = replay_receipt(&rig, "k1").await;
        assert_eq!(waiting.attempt_count, 1);
        assert_eq!(waiting.waiting_for.as_deref(), Some("remote_terminal"));
        assert_eq!(waiting.remote_status.as_deref(), Some("failed"));
        assert_eq!(
            waiting.remote_stop_reason.as_deref(),
            Some("provider_error:codex_busy_401")
        );
        // A repaired target takes the message and the reason is gone.
        *rig.fake.refuse.lock().unwrap() = None;
        tokio::time::sleep(Duration::from_millis(40)).await;
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        let sent = replay_receipt(&rig, "k1").await;
        assert_eq!(sent.state, "sent");
        assert_eq!((sent.waiting_for, sent.remote_status), (None, None));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_waiting_message_reports_why_and_how_many_attempts() {
        let rig = rig(true, SessionStatus::Interrupted).await;
        let id = queue(&rig, "k1").await;
        let fresh = replay_receipt(&rig, "k1").await;
        assert_eq!((fresh.attempt_count, fresh.waiting_for.clone()), (0, None));
        let mut dispatch = DispatchState::default();
        for _ in 0..3 {
            dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        assert_eq!(state(&rig, id).await, "queued");
        let waiting = replay_receipt(&rig, "k1").await;
        assert_eq!(waiting.state, "queued");
        assert_eq!(waiting.attempt_count, 3);
        assert_eq!(waiting.waiting_for.as_deref(), Some("remote_busy"));
        assert_eq!(waiting.settled_error_class, None);
        // Once it is delivered the reason is gone.
        *rig.fake.status.lock().unwrap() = SessionStatus::Completed;
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        let sent = replay_receipt(&rig, "k1").await;
        assert_eq!(sent.state, "sent");
        assert_eq!(sent.waiting_for, None);
        assert_eq!(sent.settled_error_class, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_unreachable_peer_is_reported_as_such_not_as_busy() {
        let rig = rig(true, SessionStatus::Completed).await;
        let id = queue(&rig, "k1").await;
        // No listener answers: remove the peer's socket.
        std::fs::remove_file(rig.root.join("peer.sock")).unwrap();
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "queued");
        let receipt = replay_receipt(&rig, "k1").await;
        assert_eq!(receipt.waiting_for.as_deref(), Some("peer_unreachable"));
        assert_eq!(receipt.attempt_count, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_rotated_declared_seat_absent_from_the_snapshot_is_delivered_once() {
        let rig = rig(true, SessionStatus::Completed).await;
        *rig.fake.listed.lock().unwrap() = false;
        let id = queue(&rig, "k1").await;
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "sent");
        tokio::time::sleep(Duration::from_millis(40)).await;
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(*rig.fake.delivered.lock().unwrap(), vec![id]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_terminal_declared_seat_in_the_snapshot_is_left_to_the_satellite() {
        for status in [
            SessionStatus::Failed,
            SessionStatus::Archived,
            SessionStatus::Deleted,
        ] {
            let rig = rig(true, status).await;
            let id = queue(&rig, "k1").await;
            let mut dispatch = DispatchState::default();
            dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
            assert_eq!(state(&rig, id).await, "sent", "{status:?}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_absent_target_with_a_branched_or_unknown_lineage_settles_failed_with_a_static_class()
     {
        let rig = rig(true, SessionStatus::Completed).await;
        *rig.fake.listed.lock().unwrap() = false;
        *rig.fake.refuse.lock().unwrap() = Some("target_not_authorized");
        let id = queue(&rig, "k1").await;
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "failed");
        let receipt = rig
            .store
            .lock()
            .await
            .queue_satellite_message(
                rig.owner,
                &AgentSendSatelliteMessageRequestV1 {
                    peer_id: SatelliteUuidV1(rig.peer),
                    remote_session_id: SatelliteUuidV1(rig.remote),
                    scope_project_id: None,
                    message: "please rebase".into(),
                    idempotency_key: "k1".into(),
                    expires_at: None,
                    issue_refs: Vec::new(),
                },
                Utc::now(),
            )
            .unwrap();
        assert!(receipt.replayed);
        assert_eq!(
            receipt.settled_error_class.as_deref(),
            Some("target_not_authorized")
        );
        assert!(rig.fake.delivered.lock().unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_absent_target_whose_tip_is_busy_waits_and_then_delivers() {
        let rig = rig(true, SessionStatus::Completed).await;
        *rig.fake.listed.lock().unwrap() = false;
        *rig.fake.refuse.lock().unwrap() = Some("busy");
        let id = queue(&rig, "k1").await;
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "queued");
        *rig.fake.refuse.lock().unwrap() = None;
        tokio::time::sleep(Duration::from_millis(40)).await;
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "sent");
        assert_eq!(*rig.fake.delivered.lock().unwrap(), vec![id]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn dispatch_switched_off_after_queueing_sends_nothing() {
        let rig = rig(true, SessionStatus::Completed).await;
        let id = queue(&rig, "k1").await;
        {
            let guard = rig.store.lock().await;
            let view = guard.satellite_registry_view().unwrap();
            let mut config = view.peers[0].config.clone();
            config.dispatch_enabled = false;
            guard
                .put_satellite_peer(
                    &SatellitePutPeerRequestV1 {
                        expected_registry_revision: view.revision,
                        peer: config,
                        repair_quarantine: false,
                    },
                    &rig.root,
                )
                .unwrap();
        }
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "queued");
        assert_eq!(rig.fake.calls.load(Ordering::SeqCst), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn satellite_refusal_settles_failed_and_busy_keeps_the_row_queued() {
        let rig = rig(true, SessionStatus::Completed).await;
        let denied = queue(&rig, "k1").await;
        *rig.fake.refuse.lock().unwrap() = Some("busy");
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, denied).await, "queued");
        *rig.fake.refuse.lock().unwrap() = Some("target_not_authorized");
        tokio::time::sleep(Duration::from_millis(40)).await;
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, denied).await, "failed");
        assert!(rig.fake.delivered.lock().unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_missing_provider_binary_settles_with_its_static_class_a_manager_can_read() {
        let rig = rig(true, SessionStatus::Completed).await;
        let id = queue(&rig, "k1").await;
        *rig.fake.refuse.lock().unwrap() =
            Some(rsi_common::satellite_dispatch::SATELLITE_LAUNCH_FAILED_PROVIDER_BINARY_MISSING);
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "failed");
        // The typed read: replaying the same idempotent request returns the
        // settled state and class without a second message.
        let receipt = rig
            .store
            .lock()
            .await
            .queue_satellite_message(
                rig.owner,
                &AgentSendSatelliteMessageRequestV1 {
                    peer_id: SatelliteUuidV1(rig.peer),
                    remote_session_id: SatelliteUuidV1(rig.remote),
                    scope_project_id: None,
                    message: "please rebase".into(),
                    idempotency_key: "k1".into(),
                    expires_at: None,
                    issue_refs: Vec::new(),
                },
                Utc::now(),
            )
            .unwrap();
        assert!(receipt.replayed);
        assert_eq!(receipt.state, "failed");
        assert_eq!(
            receipt.settled_error_class.as_deref(),
            Some("satellite_target_launch_failed:provider_binary_missing")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn launch_failure_text_maps_to_its_static_class_and_other_text_stays_generic() {
        assert!(matches!(
            classify_refusal("Policy denied: satellite_target_launch_failed:provider_spawn_failed"),
            DeliveryOutcome::Refused("satellite_target_launch_failed:provider_spawn_failed")
        ));
        assert!(matches!(
            classify_refusal("Claude binary not found in PATH"),
            DeliveryOutcome::Refused("satellite_refused")
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn failed_remote_delivery_arms_one_current_sender_notice_without_replaying() {
        use rsi_common::harness_manager::{
            AgentManagerInboxRequestV1, ConfigureHarnessManagerRequestV1,
        };
        use rsi_common::harness_manager_v2::{
            ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2,
        };
        for class in ["target_not_authorized", "delivery_uncertain", "expired"] {
            let rig = rig(true, SessionStatus::Completed).await;
            let sender = {
                let store = rig.store.lock().await;
                let mut sender = store.get_session(rig.owner).unwrap().unwrap();
                sender.project_id = Some(rsid_store::store::d04_test_project_id());
                sender.status = SessionStatus::Completed;
                store
                    .update_session_project(sender.id, sender.project_id)
                    .unwrap();
                store
                    .update_session_status(sender.id, sender.status)
                    .unwrap();
                let config = store
                    .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                        project_id: sender.project_id.unwrap(),
                        session_id: sender.id,
                        epic_ids: None,
                        group_ids: vec![],
                        expected_row_version: 0,
                    })
                    .unwrap();
                store
                    .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                        project_id: config.project_id,
                        expected_scope_version: config.row_version,
                        expected_policy_version: 0,
                        idempotency_key: "failure-policy".into(),
                        policy: ManagerPolicyV2::default(),
                    })
                    .unwrap();
                sender
            };
            let id = queue(&rig, "failure").await;
            if class == "delivery_uncertain" {
                *rig.fake.uncertain.lock().unwrap() = true;
            } else if class == "target_not_authorized" {
                *rig.fake.refuse.lock().unwrap() = Some(SATELLITE_TARGET_NOT_AUTHORIZED.into());
            } else {
                rig.store
                    .lock()
                    .await
                    .expire_satellite_messages(Utc::now() + chrono::Duration::hours(1))
                    .unwrap();
            }
            dispatch_round(&rig.store, &rig.root, &mut DispatchState::default()).await;
            let next = {
                let store = rig.store.lock().await;
                let mut next = rsid_store::test_support::make_test_session();
                next.project_id = sender.project_id;
                next.parent_id = sender.parent_id;
                next.continued_from = Some(sender.id);
                next.rotation_depth = sender.rotation_depth + 1;
                next.status = SessionStatus::Completed;
                store.insert_session(&next).unwrap();
                store
                    .update_session_status(sender.id, SessionStatus::Archived)
                    .unwrap();
                assert!(
                    store
                        .record_harness_manager_rotation(sender.id, next.id)
                        .unwrap()
                );
                let job = store
                    .manager_notice_job_for_subject(
                        "ledger_change",
                        "satellite_message_failure",
                        &id.to_string(),
                    )
                    .unwrap()
                    .unwrap();
                assert!(store.get_scheduled_job(&job).unwrap().unwrap().enabled);
                assert_eq!(
                    store
                        .harness_manager_watch_route(job)
                        .unwrap()
                        .map(|(_, target)| target),
                    Some(next.id)
                );
                let inbox = store
                    .manager_inbox(next.id, &AgentManagerInboxRequestV1::default())
                    .unwrap();
                let notices: Vec<_> = inbox
                    .notices
                    .iter()
                    .filter(|n| n.subject_id == "satellite_message_failure")
                    .collect();
                assert_eq!(notices.len(), 1);
                assert_eq!(notices[0].recipient_session_id, next.id);
                assert_eq!(notices[0].state["message_id"], id.to_string());
                assert_eq!(notices[0].state["peer_id"], rig.peer.to_string());
                assert_eq!(
                    notices[0].state["terminal_state"],
                    if class == "expired" {
                        "expired"
                    } else {
                        "failed"
                    }
                );
                assert_eq!(
                    notices[0].state["safe_error_class"],
                    if class == "expired" {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!(class)
                    }
                );
                next
            };
            // A fresh dispatcher models process-local state loss at restart.
            let calls = rig.fake.calls.load(Ordering::SeqCst);
            dispatch_round(&rig.store, &rig.root, &mut DispatchState::default()).await;
            assert_eq!(rig.fake.calls.load(Ordering::SeqCst), calls);
            let store = rig.store.lock().await;
            assert!(store.queued_satellite_messages(10).unwrap().is_empty());
            assert!(
                store
                    .manager_inbox(next.id, &AgentManagerInboxRequestV1::default())
                    .unwrap()
                    .notices
                    .is_empty()
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn unreachable_peer_keeps_the_row_queued_until_it_expires() {
        let rig = rig(true, SessionStatus::Completed).await;
        let id = queue(&rig, "k1").await;
        fs::remove_file(rig.root.join("peer.sock")).unwrap();
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "queued");
        let far = Utc::now() + chrono::Duration::hours(1);
        rig.store
            .lock()
            .await
            .expire_satellite_messages(far)
            .unwrap();
        assert_eq!(state(&rig, id).await, "expired");
    }
}
