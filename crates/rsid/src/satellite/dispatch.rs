//! Hub-owned delivery of queued manager messages (#1017 slice 3).
//!
//! Runs after the registry tick, never as its own socket loop. A queued row is
//! sent only while its peer is enabled, paired, read- and dispatch-enabled and
//! the target is still in the operator-declared scope. Before every send the
//! hub re-verifies the remote identity and reads the target's status fresh; a
//! session that is not idle keeps the row queued until it expires. The
//! satellite is the final arbiter: it re-checks its own policy and idleness and
//! never interrupts a running session.

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
    SATELLITE_TARGET_NOT_AUTHORIZED, SatelliteDeliverOutcomeV1, SatelliteDeliverRequestV1,
    satellite_launch_failure_class,
};
use tokio::io::BufReader;
use tokio::sync::Mutex;
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
    /// Keep the row queued: the target is busy, or the peer is unreachable.
    NotYet,
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
    if message.contains(SATELLITE_BUSY) {
        DeliveryOutcome::NotYet
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

/// One custody-checked delivery. Errors and busy targets are `NotYet`.
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
        // answers `busy`, `target_not_authorized` or delivers. Only a seat
        // the snapshot shows active or interrupted keeps the hub-side wait.
        let snapshot_blocks = snapshot
            .sessions
            .iter()
            .find(|session| session.session_id.0 == target)
            .is_some_and(|session| {
                matches!(
                    session.status,
                    SessionStatus::Starting
                        | SessionStatus::Running
                        | SessionStatus::WaitingApproval
                        | SessionStatus::Interrupted
                )
            });
        // Interrupted is not idle: hub mail waits (and may expire) rather
        // than resuming a session the operator or a restart stopped.
        if snapshot_blocks {
            return DeliveryOutcome::NotYet;
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
            return DeliveryOutcome::NotYet;
        }
        return match call_deliver_rpc(&mut rpc, request, RPC_DEADLINE).await {
            Ok(DeliverReply::Accepted(result)) => match result.outcome {
                SatelliteDeliverOutcomeV1::Uncertain => DeliveryOutcome::Uncertain,
                SatelliteDeliverOutcomeV1::Delivered
                | SatelliteDeliverOutcomeV1::AlreadyDelivered => DeliveryOutcome::Delivered,
            },
            Ok(DeliverReply::Refused(message)) => classify_refusal(&message),
            Err(_) => DeliveryOutcome::NotYet,
        };
    }
    DeliveryOutcome::NotYet
}

/// Settle expired rows, then attempt each still-queued row once (paced).
pub(crate) async fn dispatch_round(
    store: &Arc<Mutex<Store>>,
    root: &Path,
    state: &mut DispatchState,
) {
    let queued = {
        let guard = store.lock().await;
        if let Err(error) = guard.expire_satellite_messages(Utc::now()) {
            tracing::warn!("satellite dispatch: expiry settlement failed: {error}");
            return;
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

async fn attempt(store: &Arc<Mutex<Store>>, root: &Path, row: &QueuedSatelliteMessage) {
    let prepared = {
        let guard = store.lock().await;
        // Re-read policy on every attempt so an operator switching dispatch
        // off, or narrowing scope, stops delivery immediately.
        let Ok(Some(_)) = guard.dispatch_peer_facts(row.peer_id, row.remote_session_id) else {
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
    let request = SatelliteDeliverRequestV1 {
        wire_version: SATELLITE_WIRE_VERSION_V1,
        message_id: row.id,
        hub_installation_id: SatelliteUuidV1(hub_installation),
        sender_label: label,
        sender_session_id: row.owner_session_id,
        remote_session_id: SatelliteUuidV1(row.remote_session_id),
        message: row.payload.clone(),
    };
    let outcome = tokio::time::timeout(DELIVERY_DEADLINE, deliver_one(root, &peer, &request))
        .await
        .unwrap_or(DeliveryOutcome::NotYet);
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
        DeliveryOutcome::NotYet => return,
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
    use serde_json::json;
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
        refuse: Arc<StdMutex<Option<&'static str>>>,
        uncertain: Arc<StdMutex<bool>>,
        /// Whether the snapshot lists the target at all (a rotated seat is not).
        listed: Arc<StdMutex<bool>>,
    }

    struct Rig {
        _temp: tempfile::TempDir,
        root: std::path::PathBuf,
        store: Arc<Mutex<Store>>,
        owner: Uuid,
        peer: Uuid,
        remote: Uuid,
        fake: Fake,
    }

    async fn rig(dispatch_enabled: bool, status: SessionStatus) -> Rig {
        let temp = tempfile::Builder::new()
            .prefix("sat-disp-")
            .tempdir_in("/tmp")
            .unwrap();
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
            refuse: Arc::new(StdMutex::new(None)),
            uncertain: Arc::new(StdMutex::new(false)),
            listed: Arc::new(StdMutex::new(true)),
        };
        let (st, calls, delivered, refuse, uncertain, listed) = (
            Arc::clone(&fake.status),
            Arc::clone(&fake.calls),
            Arc::clone(&fake.delivered),
            Arc::clone(&fake.refuse),
            Arc::clone(&fake.uncertain),
            Arc::clone(&fake.listed),
        );
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (st, calls, delivered, refuse, uncertain, listed) = (
                    Arc::clone(&st),
                    Arc::clone(&calls),
                    Arc::clone(&delivered),
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
                                    RpcResponse::success(
                                        Some(request.id.clone().into()),
                                        serde_json::to_value(SatelliteDeliverResultV1 {
                                            message_id: params.message_id,
                                            outcome: if *uncertain.lock().unwrap() {
                                                SatelliteDeliverOutcomeV1::Uncertain
                                            } else {
                                                SatelliteDeliverOutcomeV1::Delivered
                                            },
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
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
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
                    message: "please rebase".into(),
                    idempotency_key: key.into(),
                    expires_at: None,
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
                    message: "please rebase".into(),
                    idempotency_key: "k1".into(),
                    expires_at: None,
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
    async fn running_target_waits_and_receives_after_it_goes_idle() {
        let rig = rig(true, SessionStatus::Running).await;
        let id = queue(&rig, "k1").await;
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "queued");
        assert!(rig.fake.delivered.lock().unwrap().is_empty());
        *rig.fake.status.lock().unwrap() = SessionStatus::Completed;
        tokio::time::sleep(Duration::from_millis(40)).await;
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "sent");
        assert_eq!(*rig.fake.delivered.lock().unwrap(), vec![id]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn interrupted_target_is_not_idle_and_the_message_stays_queued() {
        let rig = rig(true, SessionStatus::Interrupted).await;
        let id = queue(&rig, "k1").await;
        let mut dispatch = DispatchState::default();
        dispatch_round(&rig.store, &rig.root, &mut dispatch).await;
        assert_eq!(state(&rig, id).await, "queued");
        assert!(rig.fake.delivered.lock().unwrap().is_empty());
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
                    message: "please rebase".into(),
                    idempotency_key: "k1".into(),
                    expires_at: None,
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
                    message: "please rebase".into(),
                    idempotency_key: "k1".into(),
                    expires_at: None,
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
