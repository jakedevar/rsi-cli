//! Satellite side of #1017 slice 3: accept one message from an allowlisted hub
//! for an idle session inside the operator-declared scope.
//!
//! The satellite trusts nothing the hub says beyond its own operator-set
//! policy: the hub installation must be allowlisted, the target must be a
//! scope root or a descendant of one, and the target must be idle at the
//! moment of the call. Anything else is refused with no side effect, and every
//! refusal that could reveal whether a session exists is identical.

use crate::error::{DaemonError, Result};
use crate::session::SessionManager;
use crate::store::satellite_inbound_attempts::InboundState;
use chrono::Utc;
use rsi_common::SessionStatus;
use rsi_common::satellite_dispatch::{
    SATELLITE_BUSY, SATELLITE_LAUNCH_FAILED_CONTINUATION,
    SATELLITE_LAUNCH_FAILED_PROVIDER_BINARY_MISSING, SATELLITE_LAUNCH_FAILED_PROVIDER_SPAWN,
    SATELLITE_MESSAGE_KEY_CONFLICT, SATELLITE_TARGET_NOT_AUTHORIZED, SatelliteDeliverOutcomeV1,
    SatelliteDeliverRequestV1, SatelliteDeliverResultV1, SatelliteDeployRequestV1,
};
use uuid::Uuid;

/// Bound on the parent walk that decides whether a target is in scope.
const MAX_SCOPE_DEPTH: usize = 32;

fn not_authorized() -> DaemonError {
    DaemonError::PolicyDenied(SATELLITE_TARGET_NOT_AUTHORIZED.into())
}

/// What crosses the link when the continuation fails (#1087): a static class,
/// never the provider error text, stderr or a path. `busy` and the other
/// policy refusals keep their own codes.
fn launch_failure_refusal(error: DaemonError) -> DaemonError {
    let class = match error {
        // The target vanished between the checks and the effect.
        DaemonError::SessionNotFound(_) => return not_authorized(),
        // Only the satellite's own static policy codes cross the link; any
        // other text could carry a session id or a path.
        DaemonError::PolicyDenied(ref code)
            if code == SATELLITE_BUSY || code == SATELLITE_TARGET_NOT_AUTHORIZED =>
        {
            return error;
        }
        DaemonError::ClaudeBinaryNotFound
        | DaemonError::CodexBinaryNotFound
        | DaemonError::AgyBinaryNotFound => SATELLITE_LAUNCH_FAILED_PROVIDER_BINARY_MISSING,
        DaemonError::Process(_)
        | DaemonError::OpenAiApiError(_)
        | DaemonError::ExecutionScratchUnavailable(_)
        | DaemonError::StreamFallbackRequired(_) => SATELLITE_LAUNCH_FAILED_PROVIDER_SPAWN,
        _ => SATELLITE_LAUNCH_FAILED_CONTINUATION,
    };
    DaemonError::PolicyDenied(class.into())
}

/// The provider-ready text: the shared attributed envelope naming the hub
/// manager session, with the hub identity in the body for the audit trail.
fn render(request: &SatelliteDeliverRequestV1) -> String {
    let body = format!(
        "From hub manager \"{}\" (session {}, hub installation {}):\n\n{}",
        request.sender_label,
        request.sender_session_id,
        request.hub_installation_id.0,
        request.message,
    );
    rsi_common::daemon_message::wrap_agent_message(
        request.message_id,
        request.sender_session_id,
        &body,
    )
}

/// # Errors
/// `target_not_authorized` for every policy or scope refusal, `busy` when the
/// target is not idle, or a persistence/continuation error.
pub(crate) async fn deliver_hub_message(
    manager: &SessionManager,
    request: SatelliteDeliverRequestV1,
) -> Result<SatelliteDeliverResultV1> {
    deliver_with_effect(manager, request, |target, text| {
        manager.continue_session_hub_delivery(target, text)
    })
    .await
}

/// The delivery protocol with the effect injected, so the write-ahead order is
/// testable without a provider process.
async fn deliver_with_effect<Fut>(
    manager: &SessionManager,
    request: SatelliteDeliverRequestV1,
    effect: impl FnOnce(Uuid, String) -> Fut,
) -> Result<SatelliteDeliverResultV1>
where
    Fut: std::future::Future<Output = Result<()>>,
{
    request
        .validate()
        .map_err(|code| DaemonError::InvalidParam(code.into()))?;
    let target = request.remote_session_id.0;
    let delivery_target;
    {
        let store = manager.store().lock().await;
        let policy = store.satellite_inbound_policy()?;
        if !policy
            .allowed_hub_installations
            .iter()
            .any(|id| id.0 == request.hub_installation_id.0)
        {
            return Err(not_authorized());
        }
        let roots: std::collections::HashSet<Uuid> =
            policy.scope_roots.iter().map(|id| id.0).collect();
        let mut cursor = Some(target);
        let mut scope_root = None;
        for _ in 0..MAX_SCOPE_DEPTH {
            let Some(id) = cursor else { break };
            let Some(session) = store.get_session(id)? else {
                break;
            };
            if roots.contains(&id) {
                scope_root = Some(id);
                break;
            }
            cursor = session.parent_id;
        }
        let Some(scope_root) = scope_root else {
            return Err(not_authorized());
        };
        // A settled or pending receipt is reported before the lineage is
        // resolved, so a lineage that branched afterwards cannot turn a
        // recorded delivery or uncertainty into an authorization failure.
        if let Some(receipt) = store.satellite_inbound_receipt(request.message_id)? {
            if receipt.target_session_id != target
                || receipt.payload_digest
                    != crate::store::satellite_dispatch::inbound_payload_digest(&request.message)
            {
                return Err(DaemonError::InvalidParam(
                    SATELLITE_MESSAGE_KEY_CONFLICT.into(),
                ));
            }
            match receipt.state {
                InboundState::Delivered => {
                    return Ok(SatelliteDeliverResultV1 {
                        message_id: request.message_id,
                        outcome: SatelliteDeliverOutcomeV1::AlreadyDelivered,
                    });
                }
                // Recorded before the effect and never settled: the effect
                // may or may not have happened. Report it; never replay.
                InboundState::Pending => {
                    return Ok(SatelliteDeliverResultV1 {
                        message_id: request.message_id,
                        outcome: SatelliteDeliverOutcomeV1::Uncertain,
                    });
                }
                // A definite non-effect (the target was busy): retry below.
                InboundState::NotDelivered => {}
            }
        }
        // #1112: a declared seat that rotated is reached at its current
        // rotation tip. An ambiguous, cross-project or unresolvable lineage
        // fails closed.
        let Some(tip) = store.satellite_delivery_tip(target, scope_root)? else {
            return Err(not_authorized());
        };
        delivery_target = tip;
        let status = store.get_session(delivery_target)?.map(|s| s.status);
        // Interrupted (by the operator or a restart) is not idle: the operator
        // stopped it, so hub mail must not resume it.
        if !matches!(status, Some(SessionStatus::Completed)) {
            return Err(DaemonError::PolicyDenied(SATELLITE_BUSY.into()));
        }
    }
    // Write-ahead: the attempt is durable BEFORE the effect, so a crash or a
    // failed settle can only leave it `pending` (uncertain), never a second
    // delivery. If the record cannot be written, nothing was delivered.
    let began = manager
        .store()
        .lock()
        .await
        .begin_satellite_inbound_attempt(
            request.message_id,
            request.hub_installation_id.0,
            request.sender_session_id,
            target,
            &request.message,
            Utc::now(),
        )?;
    if !began {
        // A concurrent request for the same id got there first.
        return Ok(SatelliteDeliverResultV1 {
            message_id: request.message_id,
            outcome: SatelliteDeliverOutcomeV1::Uncertain,
        });
    }
    // The continuation itself refuses (`busy`) rather than interrupts if the
    // session became active after the read above.
    if let Err(error) = effect(delivery_target, render(&request)).await {
        // Only `busy` is a definite non-effect; any other failure may have
        // happened after the provider was handed the message, so the attempt
        // stays `pending` (uncertain) rather than risk a replay.
        if matches!(&error, DaemonError::PolicyDenied(code) if code == SATELLITE_BUSY) {
            let _ = manager
                .store()
                .lock()
                .await
                .settle_satellite_inbound_attempt(request.message_id, false, Utc::now());
        }
        return Err(launch_failure_refusal(error));
    }
    // The effect happened. If this settle fails the row stays `pending` and a
    // retry reports `uncertain`; the message is not delivered twice.
    if let Err(error) = manager
        .store()
        .lock()
        .await
        .settle_satellite_inbound_attempt(request.message_id, true, Utc::now())
    {
        tracing::warn!(message_id = %request.message_id, "satellite inbound settle failed: {error}");
    }
    Ok(SatelliteDeliverResultV1 {
        message_id: request.message_id,
        outcome: SatelliteDeliverOutcomeV1::Delivered,
    })
}

/// #1131: the first declared scope root whose current rotation tip is live,
/// as `(declared root, tip)`. The scope stays checked on the declared id and
/// keys the deploy replay (stable across rotation); the deploy is owned by the
/// seat's tip (the same lineage rule as hub mail, #1112), so a rotated
/// sub-manager still owns its settlement wake. A branched, cross-project,
/// over-long or unknown lineage skips that root (fail closed).
fn live_deploy_owner(
    store: &crate::store::Store,
    scope_roots: &[rsi_common::satellite::SatelliteUuidV1],
) -> Result<Option<(Uuid, Uuid)>> {
    for root in scope_roots {
        let Some(tip) = store.satellite_delivery_tip(root.0, root.0)? else {
            continue;
        };
        let Some(session) = store.get_session(tip)? else {
            continue;
        };
        if matches!(
            session.status,
            SessionStatus::Archived | SessionStatus::Deleted
        ) {
            continue;
        }
        return Ok(Some((root.0, session.id)));
    }
    Ok(None)
}

/// #1017 slice 2: a hub-initiated deploy on this satellite. Accepted only from
/// an allowlisted hub installation (the same operator-set policy as message
/// delivery; every refusal is the uniform `target_not_authorized`). It then
/// runs this daemon's own #1045 deploy flow locally: staging under the allowed
/// roots, hash and build-info checks, the quiet point, exit 75 and startup
/// verification. The deploy row needs a local session to own its settlement
/// wake, so the owner is the current rotation tip of the first live scope
/// root (the satellite's manager seat); with none declared the request is refused.
///
/// # Errors
/// `target_not_authorized`, `satellite_deploy_owner_required`, or a stable
/// `deploy_*` refusal from the local flow.
pub(crate) async fn request_hub_deploy(
    manager: &SessionManager,
    service: &crate::deploy::DeployService,
    request: SatelliteDeployRequestV1,
    now: chrono::DateTime<Utc>,
) -> Result<rsi_common::agent_deploy::AgentRequestDeployReceiptV1> {
    request
        .validate()
        .map_err(|code| DaemonError::InvalidParam(code.into()))?;
    let (owner, record_root, roots) = {
        let store = manager.store().lock().await;
        let policy = store.satellite_inbound_policy()?;
        if !policy
            .allowed_hub_installations
            .iter()
            .any(|id| id.0 == request.hub_installation_id.0)
        {
            return Err(not_authorized());
        }
        let (root, owner) = live_deploy_owner(&store, &policy.scope_roots)?.ok_or_else(|| {
            DaemonError::PolicyDenied(
                rsi_common::agent_deploy::SATELLITE_DEPLOY_OWNER_REQUIRED.into(),
            )
        })?;
        let roots: Vec<Uuid> = policy.scope_roots.iter().map(|id| id.0).collect();
        (owner, root, roots)
    };
    crate::session::deploy_verb::stage_owned_deploy(
        manager.store(),
        owner,
        Some(crate::session::deploy_verb::DeployReplayScope {
            lookup: &roots,
            record: record_root,
        }),
        request.as_local_request(),
        service,
        now,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::store::Store;
    use rsi_common::satellite::SatelliteUuidV1;
    use rsi_common::satellite_dispatch::SatelliteInboundPolicyV1;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct Rig {
        manager: SessionManager,
        _dir: TempDir,
        hub: Uuid,
        root: Uuid,
        child: Uuid,
        outsider: Uuid,
    }

    fn insert(store: &Store, parent: Option<Uuid>, status: SessionStatus) -> Uuid {
        let mut session = rsid_store::test_support::make_test_session();
        session.parent_id = parent;
        session.status = status;
        store.insert_session(&session).unwrap();
        session.id
    }

    /// A satellite with a scope root (idle), a running child of it, and an
    /// unrelated idle session that is outside the scope.
    fn rig(allow_hub: bool) -> Rig {
        let dir = TempDir::new().unwrap();
        let sandbox = TempDir::new().unwrap();
        let store = Store::open(&dir.path().join("rsi.db")).unwrap();
        let root = insert(&store, None, SessionStatus::Completed);
        let child = insert(&store, Some(root), SessionStatus::Running);
        let outsider = insert(&store, None, SessionStatus::Completed);
        let hub = Uuid::new_v4();
        store
            .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                allowed_hub_installations: if allow_hub {
                    vec![SatelliteUuidV1(hub)]
                } else {
                    Vec::new()
                },
                scope_roots: vec![SatelliteUuidV1(root)],
            })
            .unwrap();
        let manager = SessionManager::new(
            Arc::new(EventBus::new(64)),
            store,
            false,
            dir.path().join("daemon.sock"),
            None,
            Vec::new(),
            RuntimeConfig::from_config(&Config::from_env()),
            sandbox.path().to_path_buf(),
        )
        .unwrap();
        std::mem::forget(sandbox);
        Rig {
            manager,
            _dir: dir,
            hub,
            root,
            child,
            outsider,
        }
    }

    fn request(rig: &Rig, target: Uuid, message_id: Uuid, text: &str) -> SatelliteDeliverRequestV1 {
        SatelliteDeliverRequestV1 {
            wire_version: rsi_common::satellite::SATELLITE_WIRE_VERSION_V1,
            message_id,
            hub_installation_id: SatelliteUuidV1(rig.hub),
            sender_label: "hub manager".into(),
            sender_session_id: Uuid::new_v4(),
            remote_session_id: SatelliteUuidV1(target),
            message: text.into(),
        }
    }

    fn refusal_text(result: Result<SatelliteDeliverResultV1>) -> String {
        result.expect_err("must be refused").to_string()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn satellite_refuses_a_hub_that_is_not_allowlisted() {
        let rig = rig(false);
        let text = refusal_text(
            deliver_hub_message(&rig.manager, request(&rig, rig.root, Uuid::new_v4(), "hi")).await,
        );
        assert!(text.contains(SATELLITE_TARGET_NOT_AUTHORIZED), "{text}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn out_of_scope_and_unknown_targets_are_refused_identically() {
        let rig = rig(true);
        let outside = refusal_text(
            deliver_hub_message(
                &rig.manager,
                request(&rig, rig.outsider, Uuid::new_v4(), "hi"),
            )
            .await,
        );
        let unknown = refusal_text(
            deliver_hub_message(
                &rig.manager,
                request(&rig, Uuid::new_v4(), Uuid::new_v4(), "hi"),
            )
            .await,
        );
        assert_eq!(outside, unknown);
        assert!(
            outside.contains(SATELLITE_TARGET_NOT_AUTHORIZED),
            "{outside}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn running_target_in_scope_is_busy_and_records_nothing() {
        let rig = rig(true);
        let id = Uuid::new_v4();
        let text = refusal_text(
            deliver_hub_message(&rig.manager, request(&rig, rig.child, id, "hi")).await,
        );
        assert!(text.contains(SATELLITE_BUSY), "{text}");
        let store = rig.manager.store().lock().await;
        assert!(store.satellite_inbound_delivery(id).unwrap().is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_recorded_message_id_replays_without_a_second_delivery() {
        let rig = rig(true);
        let id = Uuid::new_v4();
        rig.manager
            .store()
            .lock()
            .await
            .record_satellite_inbound_delivery(
                id,
                rig.hub,
                Uuid::new_v4(),
                rig.root,
                "hi",
                Utc::now(),
            )
            .unwrap();
        let replay = deliver_hub_message(&rig.manager, request(&rig, rig.root, id, "hi"))
            .await
            .unwrap();
        assert_eq!(replay.outcome, SatelliteDeliverOutcomeV1::AlreadyDelivered);
        let conflict = refusal_text(
            deliver_hub_message(&rig.manager, request(&rig, rig.root, id, "different")).await,
        );
        assert!(
            conflict.contains(SATELLITE_MESSAGE_KEY_CONFLICT),
            "{conflict}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn interrupted_target_is_not_idle_and_records_nothing() {
        let rig = rig(true);
        let interrupted = {
            let store = rig.manager.store().lock().await;
            insert(&store, Some(rig.root), SessionStatus::Interrupted)
        };
        let id = Uuid::new_v4();
        let text = refusal_text(
            deliver_hub_message(&rig.manager, request(&rig, interrupted, id, "hi")).await,
        );
        assert!(text.contains(SATELLITE_BUSY), "{text}");
        let store = rig.manager.store().lock().await;
        assert!(store.satellite_inbound_delivery(id).unwrap().is_none());
    }

    fn counting_effect(
        count: &Arc<std::sync::atomic::AtomicUsize>,
    ) -> impl FnOnce(Uuid, String) -> std::future::Ready<Result<()>> {
        let count = Arc::clone(count);
        move |_, _| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Ok(()))
        }
    }

    async fn attempt_state(rig: &Rig, id: Uuid) -> Option<InboundState> {
        rig.manager
            .store()
            .lock()
            .await
            .satellite_inbound_receipt(id)
            .unwrap()
            .map(|receipt| receipt.state)
    }

    fn rotate(store: &Store, predecessor: Uuid, status: SessionStatus) -> Uuid {
        let mut session = rsid_store::test_support::make_test_session();
        session.continued_from = Some(predecessor);
        session.status = status;
        store.insert_session(&session).unwrap();
        session.id
    }

    async fn delivered_to(rig: &Rig, target: Uuid, id: Uuid) -> Result<Uuid> {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let sink = Arc::clone(&seen);
        deliver_with_effect(
            &rig.manager,
            request(rig, target, id, "hi"),
            move |tip, _| {
                *sink.lock().unwrap() = Some(tip);
                std::future::ready(Ok(()))
            },
        )
        .await?;
        let tip = seen.lock().unwrap().expect("effect ran");
        Ok(tip)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn mail_to_a_rotated_declared_seat_is_delivered_to_its_current_tip() {
        let rig = rig(true);
        let (second, tip) = {
            let store = rig.manager.store().lock().await;
            let second = rotate(&store, rig.root, SessionStatus::Completed);
            (second, rotate(&store, second, SessionStatus::Completed))
        };
        let delivered = delivered_to(&rig, rig.root, Uuid::new_v4()).await.unwrap();
        assert_eq!(delivered, tip);
        assert_ne!(delivered, second);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_busy_rotation_tip_refuses_even_when_the_declared_seat_is_idle() {
        let rig = rig(true);
        {
            let store = rig.manager.store().lock().await;
            rotate(&store, rig.root, SessionStatus::Running);
        }
        let id = Uuid::new_v4();
        let text = refusal_text(
            deliver_hub_message(&rig.manager, request(&rig, rig.root, id, "hi")).await,
        );
        assert!(text.contains(SATELLITE_BUSY), "{text}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_branched_lineage_is_refused_not_guessed() {
        let rig = rig(true);
        {
            let store = rig.manager.store().lock().await;
            rotate(&store, rig.root, SessionStatus::Completed);
            rotate(&store, rig.root, SessionStatus::Completed);
        }
        let text = refusal_text(
            deliver_hub_message(&rig.manager, request(&rig, rig.root, Uuid::new_v4(), "hi")).await,
        );
        assert!(text.contains(SATELLITE_TARGET_NOT_AUTHORIZED), "{text}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_rotation_tip_is_not_reached_from_an_out_of_scope_seat() {
        let rig = rig(true);
        {
            let store = rig.manager.store().lock().await;
            rotate(&store, rig.outsider, SessionStatus::Completed);
        }
        let text = refusal_text(
            deliver_hub_message(
                &rig.manager,
                request(&rig, rig.outsider, Uuid::new_v4(), "hi"),
            )
            .await,
        );
        assert!(text.contains(SATELLITE_TARGET_NOT_AUTHORIZED), "{text}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_successor_in_another_project_is_not_authorized() {
        let rig = rig(true);
        {
            let store = rig.manager.store().lock().await;
            let mut session = rsid_store::test_support::make_test_session();
            session.continued_from = Some(rig.root);
            session.project_id = None;
            store.insert_session(&session).unwrap();
        }
        let text = refusal_text(
            deliver_hub_message(&rig.manager, request(&rig, rig.root, Uuid::new_v4(), "hi")).await,
        );
        assert_eq!(text, not_authorized().to_string());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn effect_errors_never_put_a_session_id_on_the_wire() {
        let rig = rig(true);
        let tip = {
            let store = rig.manager.store().lock().await;
            rotate(&store, rig.root, SessionStatus::Completed)
        };
        let vanished = deliver_with_effect(
            &rig.manager,
            request(&rig, rig.root, Uuid::new_v4(), "hi"),
            move |id, _| std::future::ready(Err(DaemonError::SessionNotFound(id))),
        )
        .await;
        let wire = format!("{:?} {}", vanished, vanished.as_ref().unwrap_err());
        assert!(!wire.contains(&tip.to_string()), "{wire}");
        assert_eq!(
            vanished.unwrap_err().to_string(),
            not_authorized().to_string()
        );
        let invalid = deliver_with_effect(
            &rig.manager,
            request(&rig, rig.root, Uuid::new_v4(), "hi"),
            move |id, _| std::future::ready(Err(DaemonError::InvalidParam(format!("bad {id}")))),
        )
        .await;
        let wire = format!("{:?} {}", invalid, invalid.as_ref().unwrap_err());
        assert!(!wire.contains(&tip.to_string()), "{wire}");
        assert!(
            wire.contains(SATELLITE_LAUNCH_FAILED_CONTINUATION),
            "{wire}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_recorded_receipt_is_returned_even_after_the_lineage_branches() {
        let rig = rig(true);
        let pending = Uuid::new_v4();
        let delivered = Uuid::new_v4();
        {
            let store = rig.manager.store().lock().await;
            assert!(
                store
                    .begin_satellite_inbound_attempt(
                        pending,
                        rig.hub,
                        Uuid::new_v4(),
                        rig.root,
                        "hi",
                        Utc::now(),
                    )
                    .unwrap()
            );
            store
                .record_satellite_inbound_delivery(
                    delivered,
                    rig.hub,
                    Uuid::new_v4(),
                    rig.root,
                    "hi",
                    Utc::now(),
                )
                .unwrap();
            rotate(&store, rig.root, SessionStatus::Completed);
            rotate(&store, rig.root, SessionStatus::Completed);
        }
        let uncertain = deliver_hub_message(&rig.manager, request(&rig, rig.root, pending, "hi"))
            .await
            .unwrap();
        assert_eq!(uncertain.outcome, SatelliteDeliverOutcomeV1::Uncertain);
        let replay = deliver_hub_message(&rig.manager, request(&rig, rig.root, delivered, "hi"))
            .await
            .unwrap();
        assert_eq!(replay.outcome, SatelliteDeliverOutcomeV1::AlreadyDelivered);
        // A new message on the branched lineage is still refused.
        let text = refusal_text(
            deliver_hub_message(&rig.manager, request(&rig, rig.root, Uuid::new_v4(), "hi")).await,
        );
        assert_eq!(text, not_authorized().to_string());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn normal_delivery_is_exactly_once_and_a_replay_is_already_delivered() {
        let rig = rig(true);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let id = Uuid::new_v4();
        let first = deliver_with_effect(
            &rig.manager,
            request(&rig, rig.root, id, "hi"),
            counting_effect(&count),
        )
        .await
        .unwrap();
        assert_eq!(first.outcome, SatelliteDeliverOutcomeV1::Delivered);
        assert_eq!(attempt_state(&rig, id).await, Some(InboundState::Delivered));
        let replay = deliver_with_effect(
            &rig.manager,
            request(&rig, rig.root, id, "hi"),
            counting_effect(&count),
        )
        .await
        .unwrap();
        assert_eq!(replay.outcome, SatelliteDeliverOutcomeV1::AlreadyDelivered);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_failure_between_effect_and_settle_is_uncertain_not_a_duplicate() {
        let rig = rig(true);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let id = Uuid::new_v4();
        // The effect runs, then the settle write fails (a store fault or a
        // crash at that point leaves the same durable row).
        let manager_store = rig.manager.store();
        let effect = {
            let count = Arc::clone(&count);
            move |_: Uuid, _: String| {
                let count = Arc::clone(&count);
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    manager_store
                        .lock()
                        .await
                        .conn
                        .execute_batch(
                            "CREATE TEMP TRIGGER inject_settle_failure BEFORE UPDATE ON \
                             satellite_inbound_attempts BEGIN SELECT RAISE(ABORT,'injected'); END;",
                        )
                        .unwrap();
                    Ok(())
                }
            }
        };
        let first = deliver_with_effect(&rig.manager, request(&rig, rig.root, id, "hi"), effect)
            .await
            .unwrap();
        assert_eq!(first.outcome, SatelliteDeliverOutcomeV1::Delivered);
        assert_eq!(attempt_state(&rig, id).await, Some(InboundState::Pending));
        // The hub's paced retry: uncertain, and the effect does not run again.
        let retry = deliver_with_effect(
            &rig.manager,
            request(&rig, rig.root, id, "hi"),
            counting_effect(&count),
        )
        .await
        .unwrap();
        assert_eq!(retry.outcome, SatelliteDeliverOutcomeV1::Uncertain);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_crash_after_the_attempt_record_is_uncertain_and_never_delivers() {
        let rig = rig(true);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let id = Uuid::new_v4();
        // A previous process recorded the attempt and died before settling.
        assert!(
            rig.manager
                .store()
                .lock()
                .await
                .begin_satellite_inbound_attempt(
                    id,
                    rig.hub,
                    Uuid::new_v4(),
                    rig.root,
                    "hi",
                    Utc::now(),
                )
                .unwrap()
        );
        let retry = deliver_with_effect(
            &rig.manager,
            request(&rig, rig.root, id, "hi"),
            counting_effect(&count),
        )
        .await
        .unwrap();
        assert_eq!(retry.outcome, SatelliteDeliverOutcomeV1::Uncertain);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        let conflict = refusal_text(
            deliver_with_effect(
                &rig.manager,
                request(&rig, rig.root, id, "different"),
                counting_effect(&count),
            )
            .await,
        );
        assert!(
            conflict.contains(SATELLITE_MESSAGE_KEY_CONFLICT),
            "{conflict}"
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_busy_refusal_is_a_definite_non_effect_that_a_retry_may_deliver() {
        let rig = rig(true);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let id = Uuid::new_v4();
        let busy = deliver_with_effect(&rig.manager, request(&rig, rig.root, id, "hi"), |_, _| {
            std::future::ready(Err(DaemonError::PolicyDenied(SATELLITE_BUSY.into())))
        })
        .await;
        assert!(refusal_text(busy).contains(SATELLITE_BUSY));
        assert_eq!(
            attempt_state(&rig, id).await,
            Some(InboundState::NotDelivered)
        );
        let retry = deliver_with_effect(
            &rig.manager,
            request(&rig, rig.root, id, "hi"),
            counting_effect(&count),
        )
        .await
        .unwrap();
        assert_eq!(retry.outcome, SatelliteDeliverOutcomeV1::Delivered);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(attempt_state(&rig, id).await, Some(InboundState::Delivered));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_missing_provider_binary_is_refused_with_a_static_class_and_stays_uncertain() {
        let rig = rig(true);
        let id = Uuid::new_v4();
        let failed =
            deliver_with_effect(&rig.manager, request(&rig, rig.root, id, "hi"), |_, _| {
                std::future::ready(Err(DaemonError::ClaudeBinaryNotFound))
            })
            .await;
        let text = refusal_text(failed);
        assert!(
            text.contains("satellite_target_launch_failed:provider_binary_missing"),
            "{text}"
        );
        assert_eq!(attempt_state(&rig, id).await, Some(InboundState::Pending));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_unclassified_effect_error_carries_only_a_static_class_over_the_link() {
        let rig = rig(true);
        let id = Uuid::new_v4();
        let failed =
            deliver_with_effect(&rig.manager, request(&rig, rig.root, id, "hi"), |_, _| {
                std::future::ready(Err(DaemonError::Process(
                    "stderr: /home/someone/secret path".into(),
                )))
            })
            .await;
        assert_eq!(
            refusal_text(failed),
            "Policy denied: satellite_target_launch_failed:provider_spawn_failed"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_unclassified_effect_error_leaves_the_attempt_uncertain() {
        let rig = rig(true);
        let id = Uuid::new_v4();
        let failed =
            deliver_with_effect(&rig.manager, request(&rig, rig.root, id, "hi"), |_, _| {
                std::future::ready(Err(DaemonError::Store("provider spawn failed".into())))
            })
            .await;
        assert!(failed.is_err());
        assert_eq!(attempt_state(&rig, id).await, Some(InboundState::Pending));
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retry = deliver_with_effect(
            &rig.manager,
            request(&rig, rig.root, id, "hi"),
            counting_effect(&count),
        )
        .await
        .unwrap();
        assert_eq!(retry.outcome, SatelliteDeliverOutcomeV1::Uncertain);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    const DEPLOY_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    struct DeployRig {
        _dir: TempDir,
        source: std::path::PathBuf,
        service: crate::deploy::DeployService,
    }

    fn deploy_rig(supervised: bool) -> DeployRig {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("build");
        let install = dir.path().join("install");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(source.join("rsid"), b"new-rsid").unwrap();
        std::fs::write(install.join("rsid"), b"old-rsid").unwrap();
        let service = crate::deploy::DeployService::new(
            install,
            vec![dir.path().to_path_buf()],
            Box::new(move || supervised),
            Arc::new(|_: &std::path::Path| Ok((DEPLOY_SHA.to_string(), 999))),
        );
        DeployRig {
            _dir: dir,
            source,
            service,
        }
    }

    fn deploy_request(rig: &Rig, deploy: &DeployRig, key: &str) -> SatelliteDeployRequestV1 {
        SatelliteDeployRequestV1 {
            wire_version: rsi_common::satellite::SATELLITE_WIRE_VERSION_V1,
            hub_installation_id: SatelliteUuidV1(rig.hub),
            sender_label: "hub manager".into(),
            sender_session_id: Uuid::new_v4(),
            sha: DEPLOY_SHA.into(),
            binaries_dir: deploy.source.to_string_lossy().into_owned(),
            idempotency_key: key.into(),
            max_wait_secs: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn hub_deploy_refuses_a_hub_that_is_not_allowlisted() {
        let rig = rig(false);
        let deploy = deploy_rig(true);
        let error = request_hub_deploy(
            &rig.manager,
            &deploy.service,
            deploy_request(&rig, &deploy, "k"),
            Utc::now(),
        )
        .await
        .expect_err("must be refused")
        .to_string();
        assert!(error.contains(SATELLITE_TARGET_NOT_AUTHORIZED), "{error}");
        assert!(
            rig.manager
                .store()
                .lock()
                .await
                .latest_agent_deploy()
                .unwrap()
                .is_none()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn hub_deploy_runs_the_local_flow_owned_by_the_scope_root_and_replays() {
        let rig = rig(true);
        let deploy = deploy_rig(true);
        let first = request_hub_deploy(
            &rig.manager,
            &deploy.service,
            deploy_request(&rig, &deploy, "k1"),
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(first.state, rsi_common::agent_deploy::DeployState::Staged);
        assert_eq!(first.sha, DEPLOY_SHA);
        assert!(!first.replayed);
        let row = rig
            .manager
            .store()
            .lock()
            .await
            .get_agent_deploy(first.deploy_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.owner_session_id, Some(rig.root));
        // The same hub request (same sender and key) replays, not re-stages.
        let mut again = deploy_request(&rig, &deploy, "k1");
        let live = rig
            .manager
            .store()
            .lock()
            .await
            .live_agent_deploy()
            .unwrap()
            .unwrap();
        assert_eq!(live.id, first.deploy_id);
        again.sender_session_id = Uuid::new_v4();
        // A different hub session is a different owner key: the one-live rule
        // refuses it instead of staging a second deploy.
        let error = request_hub_deploy(&rig.manager, &deploy.service, again, Utc::now())
            .await
            .expect_err("one live deploy")
            .to_string();
        assert!(error.contains("deploy_already_in_progress"), "{error}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn hub_deploy_replays_the_same_request_and_reports_local_refusals() {
        let rig = rig(true);
        let deploy = deploy_rig(true);
        let request = deploy_request(&rig, &deploy, "k2");
        let first = request_hub_deploy(&rig.manager, &deploy.service, request.clone(), Utc::now())
            .await
            .unwrap();
        let replay = request_hub_deploy(&rig.manager, &deploy.service, request, Utc::now())
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.deploy_id, first.deploy_id);

        // Not run by the supervisor: the local flow's own refusal comes back.
        let rig = self::rig(true);
        let unsupervised = deploy_rig(false);
        let error = request_hub_deploy(
            &rig.manager,
            &unsupervised.service,
            deploy_request(&rig, &unsupervised, "k3"),
            Utc::now(),
        )
        .await
        .expect_err("needs supervisor")
        .to_string();
        assert!(error.contains("deploy_needs_supervisor"), "{error}");
    }

    async fn deploy_after_rotation(
        rig: &Rig,
        key: &str,
    ) -> Result<rsi_common::agent_deploy::AgentRequestDeployReceiptV1> {
        let deploy = deploy_rig(true);
        request_hub_deploy(
            &rig.manager,
            &deploy.service,
            deploy_request(rig, &deploy, key),
            Utc::now(),
        )
        .await
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_rotated_scope_root_deploys_owned_by_its_current_tip() {
        let rig = rig(true);
        let tip = {
            let store = rig.manager.store().lock().await;
            let second = rotate(&store, rig.root, SessionStatus::Completed);
            let tip = rotate(&store, second, SessionStatus::Completed);
            // The incident: the declared seat is terminal after rotating.
            store
                .update_session_status(rig.root, SessionStatus::Archived)
                .unwrap();
            tip
        };
        let receipt = deploy_after_rotation(&rig, "k").await.unwrap();
        let row = rig
            .manager
            .store()
            .lock()
            .await
            .get_agent_deploy(receipt.deploy_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.owner_session_id, Some(tip));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_identical_retry_after_rotation_replays_the_original_deploy() {
        let rig = rig(true);
        let deploy = deploy_rig(true);
        let request = deploy_request(&rig, &deploy, "k");
        let first = request_hub_deploy(&rig.manager, &deploy.service, request.clone(), Utc::now())
            .await
            .unwrap();
        // The declared seat rotates and the first deploy settles meanwhile.
        let tip = {
            let store = rig.manager.store().lock().await;
            let tip = rotate(&store, rig.root, SessionStatus::Completed);
            store
                .update_session_status(rig.root, SessionStatus::Archived)
                .unwrap();
            store
                .settle_agent_deploy(
                    first.deploy_id,
                    rsi_common::agent_deploy::DeployState::Succeeded,
                    None,
                    Utc::now(),
                )
                .unwrap()
                .expect("settled");
            tip
        };
        let retry = request_hub_deploy(&rig.manager, &deploy.service, request, Utc::now())
            .await
            .unwrap();
        assert!(retry.replayed);
        assert_eq!(retry.deploy_id, first.deploy_id);
        let store = rig.manager.store().lock().await;
        assert!(store.live_agent_deploy().unwrap().is_none());
        assert_eq!(
            store.latest_agent_deploy().unwrap().unwrap().id,
            first.deploy_id
        );
        // A new key after the rotation stages a fresh deploy owned by the tip.
        drop(store);
        let fresh = request_hub_deploy(
            &rig.manager,
            &deploy.service,
            deploy_request(&rig, &deploy, "k-new"),
            Utc::now(),
        )
        .await
        .unwrap();
        assert!(!fresh.replayed);
        let row = rig
            .manager
            .store()
            .lock()
            .await
            .get_agent_deploy(fresh.deploy_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.owner_session_id, Some(tip));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_nonempty_unknown_declared_root_leaves_the_deploy_owner_required() {
        let rig = rig(true);
        rig.manager
            .store()
            .lock()
            .await
            .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                allowed_hub_installations: vec![SatelliteUuidV1(rig.hub)],
                scope_roots: vec![SatelliteUuidV1(Uuid::new_v4())],
            })
            .unwrap();
        let error = deploy_after_rotation(&rig, "k").await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("satellite_deploy_owner_required"),
            "{error}"
        );
        assert!(
            rig.manager
                .store()
                .lock()
                .await
                .latest_agent_deploy()
                .unwrap()
                .is_none()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_dead_rotation_tip_leaves_the_deploy_owner_required() {
        let rig = rig(true);
        {
            let store = rig.manager.store().lock().await;
            rotate(&store, rig.root, SessionStatus::Archived);
        }
        let error = deploy_after_rotation(&rig, "k").await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("satellite_deploy_owner_required")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_branched_scope_root_lineage_refuses_the_deploy_without_ids() {
        let rig = rig(true);
        let (first, second) = {
            let store = rig.manager.store().lock().await;
            (
                rotate(&store, rig.root, SessionStatus::Completed),
                rotate(&store, rig.root, SessionStatus::Completed),
            )
        };
        let error = deploy_after_rotation(&rig, "k").await.unwrap_err();
        let wire = format!("{error:?} {error}");
        assert!(wire.contains("satellite_deploy_owner_required"), "{wire}");
        for id in [rig.root, first, second] {
            assert!(!wire.contains(&id.to_string()), "{wire}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_cross_project_successor_refuses_the_deploy() {
        let rig = rig(true);
        {
            let store = rig.manager.store().lock().await;
            let mut session = rsid_store::test_support::make_test_session();
            session.continued_from = Some(rig.root);
            session.project_id = None;
            store.insert_session(&session).unwrap();
        }
        let error = deploy_after_rotation(&rig, "k").await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("satellite_deploy_owner_required")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn hub_deploy_without_a_live_scope_root_is_refused_with_owner_required() {
        let rig = rig(true);
        rig.manager
            .store()
            .lock()
            .await
            .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                allowed_hub_installations: vec![SatelliteUuidV1(rig.hub)],
                scope_roots: Vec::new(),
            })
            .unwrap();
        let deploy = deploy_rig(true);
        let error = request_hub_deploy(
            &rig.manager,
            &deploy.service,
            deploy_request(&rig, &deploy, "k"),
            Utc::now(),
        )
        .await
        .expect_err("owner required")
        .to_string();
        assert!(error.contains("satellite_deploy_owner_required"), "{error}");
    }
}
