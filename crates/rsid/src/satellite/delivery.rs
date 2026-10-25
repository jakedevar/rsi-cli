//! Satellite side of #1017 slice 3: accept one message from an allowlisted hub
//! for a session inside the operator-declared scope.
//!
//! The satellite trusts nothing the hub says beyond its own operator-set
//! policy: the hub installation must be allowlisted and the target must be a
//! scope root or a descendant of one. An idle target is continued with the
//! message. A live (running) target is not made to wait for full idle (#2566):
//! the message is queued durably on the target's operator queue, which the
//! per-session `PostToolUse` hook drains at the next tool boundary (#1049) and
//! the turn-end dispatcher drains otherwise. A target that is neither idle nor
//! live (interrupted, failed, archived) is refused with no side effect, and
//! every refusal that could reveal whether a session exists is identical.

use crate::error::{DaemonError, Result};
use crate::session::SessionManager;
use crate::store::satellite_inbound_attempts::InboundState;
use chrono::Utc;
use rsi_common::SessionStatus;
use rsi_common::satellite_dispatch::{
    SATELLITE_BUSY, SATELLITE_LAUNCH_FAILED_CONTINUATION,
    SATELLITE_LAUNCH_FAILED_PROVIDER_BINARY_MISSING, SATELLITE_LAUNCH_FAILED_PROVIDER_SPAWN,
    SATELLITE_LAUNCH_FAILED_SANDBOX_CUSTODY, SATELLITE_MESSAGE_KEY_CONFLICT,
    SATELLITE_TARGET_NOT_AUTHORIZED, SatelliteDeliverOutcomeV1, SatelliteDeliverRedirectV1,
    SatelliteDeliverRequestV1, SatelliteDeliverResultV1, SatelliteDeployRequestV1,
    remote_terminal_refusal_with_terminal,
};
use uuid::Uuid;

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
        // #2956: the typed custody refusal crosses as its own static class.
        DaemonError::StructuredRpc { ref data, .. }
            if data.get("kind").and_then(serde_json::Value::as_str) == Some("sandbox_custody") =>
        {
            SATELLITE_LAUNCH_FAILED_SANDBOX_CUSTODY
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
/// #272: a message redirected from a replaced seat says so in its header.
fn render(
    request: &SatelliteDeliverRequestV1,
    redirected: Option<SatelliteDeliverRedirectV1>,
) -> String {
    let redirect = redirected.map_or_else(String::new, |redirect| {
        format!(
            "\n(Addressed to session {}, which resolved to {}, a manager seat another session has replaced; delivered to you, the project's current appointed seat, #272.)",
            redirect.addressed_session_id, redirect.displaced_session_id,
        )
    });
    let body = format!(
        "From hub manager \"{}\" (session {}, hub installation {}):{redirect}\n\n{}",
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

/// How an accepted message reaches its target (#2566).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// The target is idle: continue it with the message.
    Continue,
    /// The target is mid-turn: queue the message for its next boundary.
    QueueForBoundary,
}

/// The effect for a live target: one durable operator-queue row keyed by the
/// hub message id, so a replay can never queue a second copy. The row is the
/// whole effect; the hook (or, failing that, the turn-end dispatcher) delivers
/// it. A store refusal happened before any row existed, so it is a definite
/// non-effect: `busy`, which the hub retries.
async fn queue_for_boundary(
    manager: &SessionManager,
    target: Uuid,
    request: &SatelliteDeliverRequestV1,
    text: String,
) -> Result<()> {
    let queued = manager.store().lock().await.queue_operator_message(
        target,
        &text,
        &request.message_id.to_string(),
    );
    match queued {
        Ok(_) => Ok(()),
        Err(error) => {
            tracing::warn!(message_id = %request.message_id, "satellite boundary queue failed: {error}");
            Err(DaemonError::PolicyDenied(SATELLITE_BUSY.into()))
        }
    }
}

/// # Errors
/// `target_not_authorized` for every policy or scope refusal, `busy` when the
/// target is neither idle nor live, or a persistence/continuation error.
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
    let redirected;
    let route;
    {
        let store = manager.store().lock().await;
        // #1928: one resolver for delivery, policy push, deploy and reports.
        let scope = store.inbound_scope()?;
        if !scope.allows_hub(request.hub_installation_id.0) {
            return Err(not_authorized());
        }
        let scope_project = request.scope_project_id.map(|id| id.0);
        let Some(hit) = store.inbound_scope_hit(&scope, target, scope_project)? else {
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
                        redirected: None,
                    });
                }
                // Recorded before the effect and never settled: the effect
                // may or may not have happened. Report it; never replay.
                InboundState::Pending => {
                    return Ok(SatelliteDeliverResultV1 {
                        message_id: request.message_id,
                        outcome: SatelliteDeliverOutcomeV1::Uncertain,
                        redirected: None,
                    });
                }
                // A definite non-effect (the target was busy): retry below.
                InboundState::NotDelivered => {}
            }
        }
        // #1112: a declared seat that rotated is reached at its current
        // rotation tip; #1928: a declared project is reached at its current
        // appointed manager. An ambiguous, cross-project, unappointed or
        // revoked scope fails closed.
        // #272: an address that resolves to a replaced seat reaches the
        // declared project's current seat, and the receipt says so.
        let Some(delivery) = store.inbound_scope_delivery(&scope, hit, target)? else {
            return Err(not_authorized());
        };
        delivery_target = delivery.seat;
        redirected = delivery
            .displaced
            .map(|displaced| SatelliteDeliverRedirectV1 {
                addressed_session_id: target,
                displaced_session_id: displaced,
                delivered_session_id: delivery.seat,
            });
        if let Some(redirect) = redirected {
            tracing::info!(message_id = %request.message_id, addressed = %target,
                displaced = %redirect.displaced_session_id, seat = %delivery.seat,
                "hub message to a replaced manager seat redirected to the current seat");
        }
        let session = store.get_session(delivery_target)?;
        let status = session.as_ref().map(|s| s.status);
        route = match status {
            Some(SessionStatus::Completed) => Route::Continue,
            // #2566: a live turn gets the message at its next boundary; it is
            // not held until the session happens to go idle.
            Some(
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval,
            ) => Route::QueueForBoundary,
            // #2744: a failed, archived or deleted target can never take a
            // turn. Say so with its real status and stop reason, so the
            // sender repairs the target instead of reading `busy` as "working".
            Some(
                terminal @ (SessionStatus::Failed
                | SessionStatus::Archived
                | SessionStatus::Deleted),
            ) => {
                let name = match terminal {
                    SessionStatus::Failed => "failed",
                    SessionStatus::Archived => "archived",
                    _ => "deleted",
                };
                let stop_reason = session.as_ref().and_then(|s| s.stop_reason.as_deref());
                // hub #3859: also the persisted terminal_reason (for example
                // `api_error` for a provider weekly limit).
                let terminal_reason = session.as_ref().and_then(|s| s.terminal_reason.as_deref());
                return Err(DaemonError::PolicyDenied(
                    remote_terminal_refusal_with_terminal(name, stop_reason, terminal_reason),
                ));
            }
            // Interrupted (by the operator or a restart) is not idle: the
            // operator stopped it, so hub mail must not resume it.
            _ => return Err(DaemonError::PolicyDenied(SATELLITE_BUSY.into())),
        };
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
            redirected: None,
        });
    }
    // The continuation itself refuses (`busy`) rather than interrupts if the
    // session became active after the read above.
    let effect_result = match route {
        Route::Continue => effect(delivery_target, render(&request, redirected)).await,
        Route::QueueForBoundary => {
            let text = render(&request, redirected);
            queue_for_boundary(manager, delivery_target, &request, text).await
        }
    };
    if let Err(error) = effect_result {
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
        redirected,
    })
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
        // #1131/#1928: the owner is the live seat of the first declared scope
        // entry (a root's rotation tip or a project's appointed manager). The
        // replay identity stays the declared id, stable across rotation and
        // re-appointment.
        let scope = store.inbound_scope()?;
        if !scope.allows_hub(request.hub_installation_id.0) {
            return Err(not_authorized());
        }
        let (root, owner) = scope.first_live().ok_or_else(|| {
            DaemonError::PolicyDenied(
                rsi_common::agent_deploy::SATELLITE_DEPLOY_OWNER_REQUIRED.into(),
            )
        })?;
        (owner, root, scope.declared_addresses())
    };
    let local_request = request.as_local_request();
    if local_request.is_cancel() {
        let receipt = manager
            .agent_control()
            .cancel_owned_deploy(owner, &local_request, now, true, Some(&roots))
            .await?;
        return Ok(receipt);
    }
    crate::session::deploy_verb::stage_owned_deploy(
        manager.store(),
        owner,
        Some(crate::session::deploy_verb::DeployReplayScope {
            lookup: &roots,
            record: record_root,
        }),
        local_request,
        service,
        now,
        None,
        Some((&manager.deploy_drain(), &manager.runtime_config)),
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
        let store = Store::open_test_fixture(&dir.path().join("rsi.db")).unwrap();
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
                ..Default::default()
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
            scope_project_id: None,
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

    /// Drive one queued operator row through the tool-boundary claim the hook
    /// makes (`claim_operator_queue`): claim, cross the effect boundary, settle
    /// `delivered`. Returns the text the model would be shown.
    async fn boundary_claim(rig: &Rig, target: Uuid) -> Option<String> {
        let mut store = rig.manager.store().lock().await;
        let message = store.claim_operator_message(target).unwrap()?;
        store
            .mark_operator_message_effect_possible(message.id)
            .unwrap();
        store
            .deliver_operator_message_with_transcript(message.id, None)
            .unwrap();
        Some(message.content)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn running_target_in_scope_gets_the_message_at_its_next_boundary() {
        let rig = rig(true);
        let id = Uuid::new_v4();
        let delivered =
            deliver_hub_message(&rig.manager, request(&rig, rig.child, id, "assignment"))
                .await
                .unwrap();
        assert_eq!(delivered.outcome, SatelliteDeliverOutcomeV1::Delivered);
        assert_eq!(attempt_state(&rig, id).await, Some(InboundState::Delivered));
        {
            let store = rig.manager.store().lock().await;
            let queued = store.list_operator_messages(rig.child).unwrap();
            assert_eq!(queued.len(), 1);
            assert_eq!(queued[0].state, "queued");
        }
        let shown = boundary_claim(&rig, rig.child).await.expect("claimable");
        assert!(shown.contains("assignment"), "{shown}");
        assert!(shown.contains(&id.to_string()), "{shown}");
        // The session is still running; nothing was continued or interrupted.
        let store = rig.manager.store().lock().await;
        assert_eq!(
            store.get_session(rig.child).unwrap().unwrap().status,
            SessionStatus::Running
        );
        assert_eq!(
            store.list_operator_messages(rig.child).unwrap()[0].state,
            "delivered"
        );
    }

    /// #2566: a target that never goes idle still receives every message, in
    /// order, and a replay of a delivered id queues nothing new.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_target_that_never_goes_idle_receives_every_message_in_order() {
        let rig = rig(true);
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        for (n, id) in ids.iter().enumerate() {
            let outcome = deliver_hub_message(
                &rig.manager,
                request(&rig, rig.child, *id, &format!("work-{n}")),
            )
            .await
            .unwrap();
            assert_eq!(outcome.outcome, SatelliteDeliverOutcomeV1::Delivered);
        }
        let replay = deliver_hub_message(&rig.manager, request(&rig, rig.child, ids[0], "work-0"))
            .await
            .unwrap();
        assert_eq!(replay.outcome, SatelliteDeliverOutcomeV1::AlreadyDelivered);
        for n in 0..3 {
            let shown = boundary_claim(&rig, rig.child).await.expect("queued");
            assert!(shown.contains(&format!("work-{n}")), "{shown}");
        }
        assert!(boundary_claim(&rig, rig.child).await.is_none());
        let store = rig.manager.store().lock().await;
        assert_eq!(
            store.get_session(rig.child).unwrap().unwrap().status,
            SessionStatus::Running
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_failed_boundary_queue_is_a_definite_non_effect_the_hub_retries() {
        let rig = rig(true);
        let id = Uuid::new_v4();
        rig.manager
            .store()
            .lock()
            .await
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_queue BEFORE INSERT ON operator_messages BEGIN SELECT RAISE(ABORT,'injected'); END;",
            )
            .unwrap();
        let text = refusal_text(
            deliver_hub_message(&rig.manager, request(&rig, rig.child, id, "hi")).await,
        );
        assert!(text.contains(SATELLITE_BUSY), "{text}");
        assert_eq!(
            attempt_state(&rig, id).await,
            Some(InboundState::NotDelivered)
        );
        rig.manager
            .store()
            .lock()
            .await
            .conn
            .execute_batch("DROP TRIGGER fail_queue;")
            .unwrap();
        let retry = deliver_hub_message(&rig.manager, request(&rig, rig.child, id, "hi"))
            .await
            .unwrap();
        assert_eq!(retry.outcome, SatelliteDeliverOutcomeV1::Delivered);
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

    /// #2744: a failed, archived or deleted target is answered with its real
    /// status and stop reason (never `busy`), records nothing, and a target
    /// mid-turn is still accepted for its next boundary.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn terminal_targets_are_named_and_a_mid_turn_target_is_not_refused() {
        let rig = rig(true);
        for (status, name) in [
            (SessionStatus::Failed, "failed"),
            (SessionStatus::Archived, "archived"),
            (SessionStatus::Deleted, "deleted"),
        ] {
            let target = {
                let store = rig.manager.store().lock().await;
                let mut session = rsid_store::test_support::make_test_session();
                session.parent_id = Some(rig.root);
                session.status = status;
                session.stop_reason = Some("provider_error: codex 401 /home/x".into());
                session.terminal_reason = Some("api_error".into());
                store.insert_session(&session).unwrap();
                session.id
            };
            let id = Uuid::new_v4();
            let text = refusal_text(
                deliver_hub_message(&rig.manager, request(&rig, target, id, "hi")).await,
            );
            assert!(
                text.contains(&format!(
                    "remote_terminal|{name}|provider_error:_codex_401__home_x;terminal_reason=api_error"
                )),
                "{text}"
            );
            assert!(!text.contains(SATELLITE_BUSY), "{text}");
            let store = rig.manager.store().lock().await;
            assert!(store.satellite_inbound_delivery(id).unwrap().is_none());
        }
        let delivered = deliver_hub_message(
            &rig.manager,
            request(&rig, rig.child, Uuid::new_v4(), "work"),
        )
        .await
        .unwrap();
        assert_eq!(delivered.outcome, SatelliteDeliverOutcomeV1::Delivered);
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
    async fn a_running_rotation_tip_is_queued_to_even_when_the_declared_seat_is_idle() {
        let rig = rig(true);
        let tip = {
            let store = rig.manager.store().lock().await;
            rotate(&store, rig.root, SessionStatus::Running)
        };
        let id = Uuid::new_v4();
        let delivered = deliver_hub_message(&rig.manager, request(&rig, rig.root, id, "hi"))
            .await
            .unwrap();
        assert_eq!(delivered.outcome, SatelliteDeliverOutcomeV1::Delivered);
        let store = rig.manager.store().lock().await;
        assert_eq!(store.list_operator_messages(tip).unwrap().len(), 1);
        assert!(store.list_operator_messages(rig.root).unwrap().is_empty());
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

    /// #2956: a target whose custody refuses the continuation crosses the link
    /// as its own static class, so the hub's notice names the cause.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_custody_refusal_crosses_as_the_sandbox_custody_class() {
        use rsi_common::types::{
            SandboxCustodyErrorCodeV1, SandboxCustodyErrorV1, SandboxCustodyRecoveryV1,
            SandboxCustodyTransitionV1,
        };
        let refused =
            launch_failure_refusal(crate::error::sandbox_custody_error(SandboxCustodyErrorV1 {
                version: 1,
                code: SandboxCustodyErrorCodeV1::OwnershipMissing,
                session_id: Some(Uuid::new_v4()),
                transition: SandboxCustodyTransitionV1::Continue,
                retryable: false,
                recovery: SandboxCustodyRecoveryV1::CreateNewSession,
            }));
        assert_eq!(
            refused.to_string(),
            DaemonError::PolicyDenied(SATELLITE_LAUNCH_FAILED_SANDBOX_CUSTODY.into()).to_string()
        );
        assert_eq!(
            rsi_common::satellite_dispatch::satellite_launch_failure_class(&refused.to_string()),
            Some(SATELLITE_LAUNCH_FAILED_SANDBOX_CUSTODY)
        );
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
            interrupt_workers: None,
            cancel: None,
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

    /// #2587: the hub's `interrupt_workers` reaches the satellite's own deploy
    /// row, so its drain hold ends and a mid-turn worker no longer blocks it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn hub_deploy_records_the_interrupt_workers_option_on_the_satellite_row() {
        let rig = rig(true);
        let deploy = deploy_rig(true);
        let mut wire = deploy_request(&rig, &deploy, "k-interrupt");
        wire.interrupt_workers = Some(true);
        let receipt = request_hub_deploy(&rig.manager, &deploy.service, wire, Utc::now())
            .await
            .unwrap();
        assert!(receipt.interrupt_workers);
        let interrupt = rig
            .manager
            .store()
            .lock()
            .await
            .agent_deploy_interrupt(receipt.deploy_id)
            .unwrap();
        assert!(interrupt.requested);
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

    /// #268: the scope root is a manager seat the operator replaced; it is
    /// still declared first, beside the project. A hub deploy is owned by the
    /// seat that holds the published appointment now, so its settlement wake
    /// ("Check AgentGetDaemonInfo") reaches the current manager.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn hub_deploy_is_owned_by_the_current_appointed_seat_not_a_replaced_root() {
        let rig = rig(true);
        let deploy = deploy_rig(true);
        let current = reseat_declared_root(&rig).await;
        let receipt = request_hub_deploy(
            &rig.manager,
            &deploy.service,
            deploy_request(&rig, &deploy, "k-reseated"),
            Utc::now(),
        )
        .await
        .unwrap();
        let row = rig
            .manager
            .store()
            .lock()
            .await
            .get_agent_deploy(receipt.deploy_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.owner_session_id, Some(current));
    }

    /// #268/#272: the scope root was the project's manager and another
    /// session was appointed in its place; the root stays declared first,
    /// beside the project. Returns the current seat.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    async fn reseat_declared_root(rig: &Rig) -> Uuid {
        let project = crate::store::d04_test_project_id();
        let store = rig.manager.store().lock().await;
        // The file fixture does not seed the sessions' project row.
        store
            .insert_project(&rsi_common::types::Project {
                id: project,
                name: "Satellite project".into(),
                path: None,
                description: None,
                color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .unwrap();
        appoint(&store, project, rig.root, Some(None));
        let current = insert(&store, None, SessionStatus::Completed);
        appoint(&store, project, current, Some(None));
        store
            .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                allowed_hub_installations: vec![SatelliteUuidV1(rig.hub)],
                scope_roots: vec![SatelliteUuidV1(rig.root)],
                scope_projects: vec![SatelliteUuidV1(project)],
                ..Default::default()
            })
            .unwrap();
        current
    }

    /// #272: the hub PM addresses the old seat's session id (a declared scope
    /// root) after the operator re-appointed the project. The message reaches
    /// the seat that holds the appointment now, says whom the hub addressed,
    /// and the receipt names the original and actual recipient. Once the old
    /// session is appointed again it receives its own mail directly.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_hub_message_to_a_replaced_seat_reaches_the_current_appointed_seat() {
        let rig = rig(true);
        let current = reseat_declared_root(&rig).await;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = |seen: &Arc<std::sync::Mutex<Vec<(Uuid, String)>>>| {
            let seen = Arc::clone(seen);
            move |target: Uuid, text: String| {
                seen.lock().unwrap().push((target, text));
                std::future::ready(Ok(()))
            }
        };
        let result = deliver_with_effect(
            &rig.manager,
            request(&rig, rig.root, Uuid::new_v4(), "work order"),
            recorder(&seen),
        )
        .await
        .unwrap();
        assert_eq!(result.outcome, SatelliteDeliverOutcomeV1::Delivered);
        assert_eq!(
            result.redirected,
            Some(SatelliteDeliverRedirectV1 {
                addressed_session_id: rig.root,
                displaced_session_id: rig.root,
                delivered_session_id: current,
            })
        );
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].0, current);
            assert!(seen[0].1.contains("work order"), "{}", seen[0].1);
            assert!(
                seen[0]
                    .1
                    .contains(&format!("Addressed to session {}", rig.root)),
                "{}",
                seen[0].1
            );
        }

        appoint(
            &*rig.manager.store().lock().await,
            crate::store::d04_test_project_id(),
            rig.root,
            Some(None),
        );
        let result = deliver_with_effect(
            &rig.manager,
            request(&rig, rig.root, Uuid::new_v4(), "second order"),
            recorder(&seen),
        )
        .await
        .unwrap();
        assert_eq!(result.outcome, SatelliteDeliverOutcomeV1::Delivered);
        assert_eq!(result.redirected, None);
        assert_eq!(seen.lock().unwrap()[1].0, rig.root);
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn hub_deploy_cancel_withdraws_only_a_waiting_row() {
        let rig = rig(true);
        let deploy = deploy_rig(true);
        let mut request = deploy_request(&rig, &deploy, "cancel-waiting");
        let staged = request_hub_deploy(&rig.manager, &deploy.service, request.clone(), Utc::now())
            .await
            .unwrap();
        request.cancel = Some(true);
        let cancelled =
            request_hub_deploy(&rig.manager, &deploy.service, request.clone(), Utc::now())
                .await
                .unwrap();
        assert_eq!(cancelled.deploy_id, staged.deploy_id);
        assert_eq!(
            cancelled.reason.as_deref(),
            Some(rsi_common::agent_deploy::DEPLOY_CANCELLED)
        );
        let repeated = request_hub_deploy(&rig.manager, &deploy.service, request, Utc::now())
            .await
            .expect_err("a settled satellite deploy cannot be cancelled")
            .to_string();
        assert!(repeated.contains("deploy_cancel_too_late"), "{repeated}");

        let mut started = deploy_request(&rig, &deploy, "cancel-started");
        let staged = request_hub_deploy(&rig.manager, &deploy.service, started.clone(), Utc::now())
            .await
            .unwrap();
        rig.manager
            .store()
            .lock()
            .await
            .mark_agent_deploy_restarting(staged.deploy_id, Utc::now())
            .unwrap();
        started.cancel = Some(true);
        let error = request_hub_deploy(&rig.manager, &deploy.service, started, Utc::now())
            .await
            .expect_err("a restarting deploy cannot be cancelled")
            .to_string();
        assert!(error.contains("deploy_cancel_too_late"), "{error}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn hub_deploy_cancel_finds_the_row_after_the_seat_rotates() {
        let rig = rig(true);
        let deploy = deploy_rig(true);
        let mut request = deploy_request(&rig, &deploy, "cancel-rotated");
        let staged = request_hub_deploy(&rig.manager, &deploy.service, request.clone(), Utc::now())
            .await
            .unwrap();
        // The seat that staged the deploy rotates while it waits.
        let tip = {
            let store = rig.manager.store().lock().await;
            let tip = rotate(&store, rig.root, SessionStatus::Completed);
            store
                .update_session_status(rig.root, SessionStatus::Archived)
                .unwrap();
            tip
        };
        assert_ne!(tip, rig.root);
        request.cancel = Some(true);
        let cancelled = request_hub_deploy(&rig.manager, &deploy.service, request, Utc::now())
            .await
            .unwrap();
        assert_eq!(cancelled.deploy_id, staged.deploy_id);
        assert_eq!(
            cancelled.reason.as_deref(),
            Some(rsi_common::agent_deploy::DEPLOY_CANCELLED)
        );
        let row = rig
            .manager
            .store()
            .lock()
            .await
            .get_agent_deploy(staged.deploy_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.state, rsi_common::agent_deploy::DeployState::Failed);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn hub_deploy_cancel_of_an_unknown_deploy_is_not_found() {
        let rig = rig(true);
        let deploy = deploy_rig(true);
        let mut request = deploy_request(&rig, &deploy, "cancel-unknown");
        request.cancel = Some(true);
        let error = request_hub_deploy(&rig.manager, &deploy.service, request, Utc::now())
            .await
            .expect_err("nothing was staged")
            .to_string();
        assert!(error.contains("deploy_not_found"), "{error}");
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
                ..Default::default()
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
                ..Default::default()
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

    /// #1928: declare the rig's project (not a pinned session id) as the whole
    /// inbound scope and appoint `seat` as its manager.
    async fn declare_project_scope(rig: &Rig, seat: Uuid) -> Uuid {
        let store = rig.manager.store().lock().await;
        let project = store
            .get_session(seat)
            .unwrap()
            .unwrap()
            .project_id
            .unwrap();
        // The rig's sessions use the shared test project id; make it a real
        // project row so a manager can be appointed in it.
        if store.get_project(project).unwrap().is_none() {
            store
                .insert_project(&rsi_common::types::Project {
                    id: project,
                    name: "satellite scope".into(),
                    path: None,
                    description: None,
                    color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
                    context_files: None,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                })
                .unwrap();
        }
        appoint(&store, project, seat, Some(None));
        store
            .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                allowed_hub_installations: vec![SatelliteUuidV1(rig.hub)],
                scope_projects: vec![SatelliteUuidV1(project)],
                ..Default::default()
            })
            .unwrap();
        project
    }

    /// The operator seats `seat` as the project's manager; `scope` is `Some(None)`
    /// for the whole project and `Some(Some(vec![]))` revokes the appointment.
    fn appoint(store: &Store, project: Uuid, seat: Uuid, scope: Option<Option<Vec<Uuid>>>) {
        let version = store
            .get_harness_manager(project)
            .unwrap()
            .map_or(0, |config| config.row_version);
        store
            .configure_harness_manager(
                &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                    project_id: project,
                    session_id: seat,
                    epic_ids: scope.unwrap_or(None),
                    group_ids: Vec::new(),
                    expected_row_version: version,
                },
            )
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn project_scope_mail_reaches_the_current_seat_across_rotation_and_reappointment() {
        let rig = rig(true);
        let project = declare_project_scope(&rig, rig.root).await;
        let first_id = Uuid::new_v4();
        assert_eq!(
            delivered_to(&rig, project, first_id).await.unwrap(),
            rig.root
        );

        // The operator seats a NEW manager (not a rotation of the declared
        // session): the same project declaration reaches it.
        let reseated = {
            let store = rig.manager.store().lock().await;
            let reseated = insert(&store, None, SessionStatus::Completed);
            appoint(&store, project, reseated, Some(None));
            reseated
        };
        assert_eq!(
            delivered_to(&rig, project, Uuid::new_v4()).await.unwrap(),
            reseated
        );

        // A replay of the first message is answered from its receipt, even
        // though the seat moved since.
        let replay = deliver_hub_message(&rig.manager, request(&rig, project, first_id, "hi"))
            .await
            .unwrap();
        assert_eq!(replay.outcome, SatelliteDeliverOutcomeV1::AlreadyDelivered);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn unappointed_or_revoked_project_scope_refuses_like_any_out_of_scope_target() {
        let rig = rig(true);
        // Declared but never appointed.
        {
            let store = rig.manager.store().lock().await;
            let project = store
                .get_session(rig.root)
                .unwrap()
                .unwrap()
                .project_id
                .unwrap();
            store
                .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                    allowed_hub_installations: vec![SatelliteUuidV1(rig.hub)],
                    scope_projects: vec![SatelliteUuidV1(project)],
                    ..Default::default()
                })
                .unwrap();
        }
        let project = {
            let store = rig.manager.store().lock().await;
            store
                .get_session(rig.root)
                .unwrap()
                .unwrap()
                .project_id
                .unwrap()
        };
        let unappointed = refusal_text(
            deliver_hub_message(&rig.manager, request(&rig, project, Uuid::new_v4(), "hi")).await,
        );
        assert_eq!(unappointed, not_authorized().to_string());

        // Appointed, then revoked by an explicit empty scope.
        declare_project_scope(&rig, rig.root).await;
        assert_eq!(
            delivered_to(&rig, project, Uuid::new_v4()).await.unwrap(),
            rig.root
        );
        {
            let store = rig.manager.store().lock().await;
            appoint(&store, project, rig.root, Some(Some(Vec::new())));
        }
        let revoked = refusal_text(
            deliver_hub_message(&rig.manager, request(&rig, project, Uuid::new_v4(), "hi")).await,
        );
        assert_eq!(revoked, not_authorized().to_string());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn descendants_of_the_project_seat_are_reachable_by_session_id() {
        let rig = rig(true);
        let _ = declare_project_scope(&rig, rig.root).await;
        let child = {
            let store = rig.manager.store().lock().await;
            insert(&store, Some(rig.root), SessionStatus::Completed)
        };
        assert_eq!(
            delivered_to(&rig, child, Uuid::new_v4()).await.unwrap(),
            child
        );
        // An unrelated idle session is still out of scope.
        let outside = refusal_text(
            deliver_hub_message(
                &rig.manager,
                request(&rig, rig.outsider, Uuid::new_v4(), "hi"),
            )
            .await,
        );
        assert_eq!(outside, not_authorized().to_string());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn hub_deploy_owner_follows_the_project_seat() {
        let rig = rig(true);
        let project = declare_project_scope(&rig, rig.root).await;
        let first = deploy_after_rotation(&rig, "k1").await.unwrap();
        async fn owner_of(rig: &Rig, id: Uuid) -> Option<Uuid> {
            rig.manager
                .store()
                .lock()
                .await
                .get_agent_deploy(id)
                .unwrap()
                .unwrap()
                .owner_session_id
        }
        assert_eq!(owner_of(&rig, first.deploy_id).await, Some(rig.root));
        // The first deploy settles; the operator re-seats the manager.
        let reseated = {
            let store = rig.manager.store().lock().await;
            store
                .settle_agent_deploy(
                    first.deploy_id,
                    rsi_common::agent_deploy::DeployState::Succeeded,
                    None,
                    Utc::now(),
                )
                .unwrap();
            let reseated = insert(&store, None, SessionStatus::Completed);
            appoint(&store, project, reseated, Some(None));
            reseated
        };
        let second = deploy_after_rotation(&rig, "k2").await.unwrap();
        assert_eq!(owner_of(&rig, second.deploy_id).await, Some(reseated));

        // An unappointed (revoked) project leaves no owner.
        {
            let store = rig.manager.store().lock().await;
            store
                .settle_agent_deploy(
                    second.deploy_id,
                    rsi_common::agent_deploy::DeployState::Succeeded,
                    None,
                    Utc::now(),
                )
                .unwrap();
            appoint(&store, project, reseated, Some(Some(Vec::new())));
        }
        let error = deploy_after_rotation(&rig, "k3").await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("satellite_deploy_owner_required"),
            "{error}"
        );
    }

    /// The hub host for the two-host tests: one dispatch-enabled peer whose
    /// whole declared scope is `project`, with no pinned session id.
    struct Hub {
        store: Store,
        owner: Uuid,
        peer: Uuid,
    }

    fn hub_declaring_project(project: Uuid) -> Hub {
        use rsi_common::satellite::{SatellitePeerConfigV1, SatellitePutPeerRequestV1};
        let store = Store::open_in_memory().unwrap();
        let owner = rsid_store::test_support::make_test_session();
        store.insert_session(&owner).unwrap();
        let peer = Uuid::new_v4();
        store
            .put_satellite_peer(
                &SatellitePutPeerRequestV1 {
                    expected_registry_revision: 1,
                    peer: SatellitePeerConfigV1 {
                        peer_id: SatelliteUuidV1(peer),
                        label: "laptop".into(),
                        expected_installation_id: Some(SatelliteUuidV1(Uuid::new_v4())),
                        enabled: true,
                        read_enabled: true,
                        dispatch_enabled: true,
                    },
                    repair_quarantine: false,
                },
                std::path::Path::new("/tmp/satellites"),
            )
            .unwrap();
        store
            .put_satellite_peer_scope_entries(2, peer, &[], &[SatelliteUuidV1(project)])
            .unwrap();
        Hub {
            store,
            owner: owner.id,
            peer,
        }
    }

    fn hub_request(
        hub: &Hub,
        target: Uuid,
        project: Option<Uuid>,
        key: &str,
    ) -> rsi_common::satellite_dispatch::AgentSendSatelliteMessageRequestV1 {
        rsi_common::satellite_dispatch::AgentSendSatelliteMessageRequestV1 {
            peer_id: SatelliteUuidV1(hub.peer),
            remote_session_id: SatelliteUuidV1(target),
            scope_project_id: project.map(SatelliteUuidV1),
            message: "please rebase".into(),
            idempotency_key: key.into(),
            expires_at: None,
            issue_refs: Vec::new(),
        }
    }

    /// One hub message end to end: the hub's admission, its per-attempt scope
    /// re-check, the wire request exactly as the dispatcher builds it (through
    /// a JSON round trip), then the satellite's own decision. Returns the wire
    /// request and the satellite session that received it.
    async fn hub_message(
        rig: &Rig,
        hub: &Hub,
        target: Uuid,
        project: Option<Uuid>,
        key: &str,
    ) -> Result<(SatelliteDeliverRequestV1, Uuid)> {
        let receipt = hub.store.queue_satellite_message(
            hub.owner,
            &hub_request(hub, target, project, key),
            Utc::now(),
        )?;
        let row = hub
            .store
            .queued_satellite_messages(64)?
            .into_iter()
            .find(|row| row.id == receipt.message_id)
            .expect("the accepted message is queued");
        hub.store
            .dispatch_peer_facts(row.peer_id, row.remote_session_id, row.scope_project_id)?
            .ok_or_else(not_authorized)?;
        let wire = crate::satellite::dispatch::deliver_request(&row, rig.hub, "hub manager".into());
        let wire: SatelliteDeliverRequestV1 =
            serde_json::from_value(serde_json::to_value(&wire).unwrap()).unwrap();
        let seen = Arc::new(std::sync::Mutex::new(None));
        let sink = Arc::clone(&seen);
        deliver_with_effect(&rig.manager, wire.clone(), move |tip, _| {
            *sink.lock().unwrap() = Some(tip);
            std::future::ready(Ok(()))
        })
        .await?;
        let tip = seen.lock().unwrap().expect("effect ran");
        Ok((wire, tip))
    }

    /// Self-succession of the project's manager: `seat` rotates to a recorded
    /// successor, which is the appointed manager from now on.
    fn rotate_seat(store: &Store, seat: Uuid) -> Uuid {
        store
            .update_session_status(seat, SessionStatus::Archived)
            .unwrap();
        let mut successor = rsid_store::test_support::make_test_session();
        successor.status = SessionStatus::Completed;
        successor.continued_from = Some(seat);
        successor.rotation_depth += 1;
        store.insert_session(&successor).unwrap();
        assert!(
            store
                .record_harness_manager_rotation(seat, successor.id)
                .unwrap()
        );
        successor.id
    }

    /// Review #1943: a hub that declared only the project reaches a session in
    /// the seat's tree. The hub carries the project beside the session target;
    /// the satellite validates live ancestry on every delivery.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_project_only_hub_scope_reaches_the_seat_tree_across_rotation_and_reappointment() {
        let rig = rig(true);
        let project = declare_project_scope(&rig, rig.root).await;
        let hub = hub_declaring_project(project);
        let child = {
            let store = rig.manager.store().lock().await;
            insert(&store, Some(rig.root), SessionStatus::Completed)
        };

        // The hub cannot see the satellite's sessions, so a bare descendant id
        // is not a declared address: only the bound form is admitted.
        let bare = hub
            .store
            .queue_satellite_message(hub.owner, &hub_request(&hub, child, None, "k0"), Utc::now())
            .unwrap_err()
            .to_string();
        assert_eq!(bare, not_authorized().to_string());

        // Bound to the project: a descendant, the seat and the address.
        let (first, tip) = hub_message(&rig, &hub, child, Some(project), "k1")
            .await
            .unwrap();
        assert_eq!(tip, child);
        assert_eq!(first.scope_project_id, Some(SatelliteUuidV1(project)));
        assert_eq!(
            hub_message(&rig, &hub, rig.root, Some(project), "k2")
                .await
                .unwrap()
                .1,
            rig.root
        );
        assert_eq!(
            hub_message(&rig, &hub, project, None, "k3")
                .await
                .unwrap()
                .1,
            rig.root
        );

        // Self-succession: the seat's old tree stays reachable, the old seat
        // id is reached at its successor, and the successor's new children
        // count, with no scope edit on either host.
        let (successor, fresh) = {
            let store = rig.manager.store().lock().await;
            let successor = rotate_seat(&store, rig.root);
            (
                successor,
                insert(&store, Some(successor), SessionStatus::Completed),
            )
        };
        assert_eq!(
            hub_message(&rig, &hub, child, Some(project), "k4")
                .await
                .unwrap()
                .1,
            child
        );
        assert_eq!(
            hub_message(&rig, &hub, rig.root, Some(project), "k5")
                .await
                .unwrap()
                .1,
            successor
        );
        assert_eq!(
            hub_message(&rig, &hub, fresh, Some(project), "k6")
                .await
                .unwrap()
                .1,
            fresh
        );
        // A session in the project that is not under the seat is out.
        let refused = hub_message(&rig, &hub, rig.outsider, Some(project), "k7")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(refused, not_authorized().to_string());

        // Operator re-appointment: a different manager now owns the project.
        // The previous tree is out of scope on the next message; the new
        // tree is in. The hub's declaration never changed.
        let (reseated, reseated_child) = {
            let store = rig.manager.store().lock().await;
            let reseated = insert(&store, None, SessionStatus::Completed);
            appoint(&store, project, reseated, Some(None));
            (
                reseated,
                insert(&store, Some(reseated), SessionStatus::Completed),
            )
        };
        for stale in [child, fresh, successor] {
            let refused = hub_message(&rig, &hub, stale, Some(project), &format!("s-{stale}"))
                .await
                .unwrap_err()
                .to_string();
            assert_eq!(refused, not_authorized().to_string());
        }
        assert_eq!(
            hub_message(&rig, &hub, reseated_child, Some(project), "k8")
                .await
                .unwrap()
                .1,
            reseated_child
        );
        assert_eq!(
            hub_message(&rig, &hub, project, None, "k9")
                .await
                .unwrap()
                .1,
            reseated
        );

        // A replay of a message delivered before the seat moved is answered
        // from its receipt, not refused.
        let replay = deliver_hub_message(&rig.manager, first).await.unwrap();
        assert_eq!(replay.outcome, SatelliteDeliverOutcomeV1::AlreadyDelivered);
    }

    /// The hub cannot widen what the satellite's own operator declared: a
    /// project the satellite does not declare, or a revoked appointment,
    /// refuses the bound message however the hub scope reads.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_hub_bound_project_must_be_declared_and_appointed_on_the_satellite() {
        let rig = rig(true);
        let project = declare_project_scope(&rig, rig.root).await;
        let child = {
            let store = rig.manager.store().lock().await;
            insert(&store, Some(rig.root), SessionStatus::Completed)
        };
        // The hub declares a project the satellite never declared.
        let stray = Uuid::new_v4();
        let stray_hub = hub_declaring_project(stray);
        let refused = hub_message(&rig, &stray_hub, child, Some(stray), "k1")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(refused, not_authorized().to_string());

        // A matching declaration works, until the appointment is revoked.
        let hub = hub_declaring_project(project);
        assert_eq!(
            hub_message(&rig, &hub, child, Some(project), "k2")
                .await
                .unwrap()
                .1,
            child
        );
        {
            let store = rig.manager.store().lock().await;
            appoint(&store, project, rig.root, Some(Some(Vec::new())));
        }
        let revoked = hub_message(&rig, &hub, child, Some(project), "k3")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(revoked, not_authorized().to_string());
    }
}
