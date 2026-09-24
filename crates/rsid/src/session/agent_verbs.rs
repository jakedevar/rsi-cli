//! `SessionManager` wrapper methods behind the P0 tokened `Agent*` RPC verbs.
//!
//! These are the *only* mutations a session-attributed (tokened) RPC caller
//! may reach — the gate in `rpc.rs::agent_gate` default-denies everything
//! else. Each wrapper enforces its own self/lead scoping so the gate does
//! not need method-specific knowledge.
//!
//! `AgentSpawnChild` is a thin wrapper around the already-`pub`
//! `SpawnCoordinator::handle()` — the same validated path the
//! `<docregblock>/spawn_child …</docregblock>` directive scanner uses, so
//! lead-identity, recursion-depth, and per-Epic rate-limit checks are not
//! duplicated here.
//!
//! `AgentGetStatus` / `AgentHalt` scope to: the caller's own session, a
//! *direct* child of the caller (`target.parent_id == Some(caller_id)`), or
//! a child whose parent durably records the caller as lead (normally an Epic).
//! The third case exists
//! because `AgentSpawnChild` parents new children under the **Epic**, not
//! under the spawning lead (`spawn_coordinator` sets `parent_id: epic_id`)
//! — without it, a lead could spawn a child via `AgentSpawnChild` and then
//! never be authorized to `AgentGetStatus`/`AgentHalt` the very session it
//! just created. This is intentionally still conservative beyond that: it
//! does not walk further up the ancestor chain, and it does not admit
//! arbitrary Epic siblings the caller did not spawn or lead; a caller that
//! needs to manage a session outside these three cases is out of scope for
//! P0.

use super::SessionManager;
use super::spawn_coordinator::{SpawnRejectReason, SpawnState};
use super::types::{CompletedSession, TrackedSession};
use crate::error::{DaemonError, Result};
use crate::store::agent_child_relaunch_intents::{RelaunchIntentRow, RelaunchState};
use crate::store::daemon_settings::{C5AutofilePending, RecoveryDisposition};
use crate::store::harness_manager::ManagerSessionScope;
use crate::store::scheduled_jobs::ScheduledJobUpdate;
use crate::store::{C5SettlementOutcome, MasterNoIdleStoreRecovery};
use rsi_common::agent_contract::ProgramContinuationIntentV1;
use rsi_common::agent_coordination::{
    AGENT_PROGRESS_MAX_COHORT, AgentContinuationCursorV1, AgentContinueChildRequestV1,
    AgentContinueChildResultV1, AgentGetProgressResultV1, AgentMessageStateEventV1,
    AgentReserveSuccessorRequestV1, AgentReserveSuccessorResultV1, AgentSendMessageRequestV1,
    AgentSendMessageResultV1, AgentSpawnChildRequestV1, AgentSpawnChildResultV1,
};
use rsi_common::agent_coordination::{AgentArchiveChildRequestV1, AgentArchiveChildResultV1};

const CHILD_RELAUNCH_NAMESPACE: Uuid = Uuid::from_u128(0x87ca4d36_8dd4_5a96_a705_b7e55e16de12);

fn precheck_continue_child_request(
    caller_session_id: Uuid,
    request: &AgentContinueChildRequestV1,
) -> Result<()> {
    use rsi_common::agent_coordination::AgentContinueErrorCodeV1;

    request.validate().map_err(|class| {
        crate::error::agent_continue_error(
            AgentContinueErrorCodeV1::InvalidRequest,
            Some(class.to_string()),
            None,
        )
    })?;
    if request.target_session_id == caller_session_id {
        return Err(crate::error::agent_continue_error(
            AgentContinueErrorCodeV1::SelfContinuationDenied,
            Some(caller_session_id.to_string()),
            None,
        ));
    }
    Ok(())
}

fn child_relaunch_identity(
    caller: Uuid,
    request: &AgentContinueChildRequestV1,
) -> Result<(String, String, Uuid)> {
    let decision = request.idempotency_key.clone().map_or_else(
        || {
            format!(
                "cursor:{}:{}:{:?}",
                request.expected_tip_session_id,
                request.expected_event_sequence,
                request.expected_custody_generation
            )
        },
        |key| format!("key:{key}"),
    );
    let key_digest = crate::model_control::hash_request_fingerprint(&[
        "agent.child_relaunch.v1",
        &caller.to_string(),
        &request.target_session_id.to_string(),
        &decision,
    ]);
    let canonical = serde_json::to_string(request)?;
    let fingerprint = crate::model_control::hash_request_fingerprint(&[&canonical]);
    let request_id = Uuid::new_v5(&CHILD_RELAUNCH_NAMESPACE, key_digest.as_bytes());
    Ok((key_digest, fingerprint, request_id))
}
use rsi_common::rpc::{
    AgentArchiveIssueRequestV1, AgentCreateIssueParams, AgentCreateIssueResult,
    AgentGetIssueRequestV1, AgentIssueMutationResultV1, AgentListIssuesRequestV1,
    AgentRestoreIssueRequestV1, AgentUpdateIssueRequestV1, AgentUpdateIssueStatusRequestV1,
};
use rsi_common::types::{
    IssueEventPageRequestV1, IssueEventPageV1, IssuePageV1, NewIssue, Recurrence, ScheduledJob,
    Session, SessionKind, WakeMode,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// A8: hard ceiling on enabled `OnTerminal` watch rows per wake target
/// (master). Bounds self-DoS from a runaway arming loop; dedup on the
/// (caller, watched) natural key keeps legitimate re-arms free. Lives here
/// (not `rpc.rs`) because [`AgentControlHandle::arm_terminal_watch`] is the
/// single enforcement point for every arm transport.
pub const MAX_TERMINAL_WATCHES_PER_MASTER: usize = 64;

/// Outcome of [`AgentControlHandle::arm_terminal_watch`].
#[derive(Debug)]
pub(crate) enum ArmWatchOutcome {
    /// The candidate row was inserted — a new watch is armed.
    Armed(ScheduledJob),
    /// An identical enabled watch (same caller + watched natural key) already
    /// exists; the EXISTING row is returned and nothing was inserted.
    Deduplicated(ScheduledJob),
}

/// Result of idempotent daemon-authoritative program registration.
#[derive(Debug)]
pub(crate) enum ProgramGuardRegistration {
    Registered(ScheduledJob),
    Deduplicated(ScheduledJob),
}

/// Result of the terminal master-orchestrate liveness interlock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MasterNoIdleOutcome {
    NotApplicable,
    OrdinaryGuardPresent,
    GenericRecovered {
        wake_job_id: Uuid,
        disposition: MasterNoIdleRecoveryDisposition,
    },
    CapacityRecovered {
        settlement: crate::store::capacity_recovery::CapacityFailureSettlement,
    },
    TerminalAllowed {
        capacity: Option<crate::store::capacity_recovery::CapacityTerminalSettlement>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MasterNoIdleRecoveryDisposition {
    WakeAndAttributedIssue { issue_id: Uuid },
    ProjectlessWakeOnly,
}

enum EitherCapacitySettlement {
    Failure(crate::store::capacity_recovery::CapacityFailureSettlement),
    Terminal(crate::store::capacity_recovery::CapacityTerminalSettlement),
}

/// Outcome of `SessionManager::agent_spawn_child`.
#[derive(Debug, Clone)]
pub enum AgentSpawnChildOutcome {
    Accepted(AgentSpawnChildResultV1),
    Rejected(SpawnRejectReason),
}

/// Lightweight, cheaply-clonable handle over exactly the collaborators the
/// tokened `Agent*` verbs need (`active`/`completed` maps, `store`, and the
/// daemon-global `spawn_coordinator`). Constructed by
/// [`SessionManager::agent_control`] and handed to the in-process native
/// `rsi_control` tools (Harness / CodexAppServer) so those tools invoke the
/// **same** guarded authority logic as the CLI-transport `Agent*` RPC verbs —
/// never a second copy. This is the P2 "native tools call the same guarded
/// wrapper as the RPC verb (no duplicated authority logic)" invariant: the
/// verb bodies live here once, and `SessionManager`'s same-named methods are
/// thin delegators that build a handle and call through.
///
/// The handle deliberately holds only `Arc` clones (not a `&SessionManager`)
/// so a tool constructed inside a detached `tokio::spawn` agent loop — which
/// has no `SessionManager` reference — can still reach the verbs. Session
/// reads and interrupts route through the shared `get_session_snapshot` /
/// `interrupt_in_maps` helpers, the same code the RPC verbs use.
#[derive(Clone)]
pub struct AgentControlHandle {
    pub(super) custody_runtime: Option<crate::sandbox::custody::CustodyExecutionRuntime>,
    active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    pub(super) store: Arc<tokio::sync::Mutex<crate::store::Store>>,
    pub(super) event_bus: Arc<crate::bus::EventBus>,
    spawn_coordinator: Arc<super::spawn_coordinator::SpawnCoordinator>,
}

impl AgentControlHandle {
    /// Construct a handle from the collaborator `Arc`s. Used both by
    /// [`SessionManager::agent_control`] (fresh-launch / RPC path) and by the
    /// rotation path, which holds these `Arc`s directly as function
    /// parameters rather than a `&SessionManager`.
    pub fn new(
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        store: Arc<tokio::sync::Mutex<crate::store::Store>>,
        event_bus: Arc<crate::bus::EventBus>,
        spawn_coordinator: Arc<super::spawn_coordinator::SpawnCoordinator>,
    ) -> Self {
        Self {
            custody_runtime: None,
            active,
            completed,
            store,
            event_bus,
            spawn_coordinator,
        }
    }

    pub(super) fn with_custody_runtime(
        mut self,
        runtime: crate::sandbox::custody::CustodyExecutionRuntime,
    ) -> Self {
        self.custody_runtime = Some(runtime);
        self
    }

    /// `AgentSpawnChild` — session-attributed spawn. Delegates all
    /// lead-identity / depth / rate-limit / topology validation to
    /// `SpawnCoordinator::handle()`; adds only an idempotency layer on top
    /// so an RPC client retry (distinct from the directive-scanner's
    /// block-hash dedup) doesn't double-enqueue.
    pub async fn agent_spawn_child(
        &self,
        caller_session_id: Uuid,
        request: AgentSpawnChildRequestV1,
    ) -> AgentSpawnChildOutcome {
        let state = self
            .spawn_coordinator
            .handle_agent(
                caller_session_id,
                request,
                &self.active,
                &self.completed,
                &self.store,
            )
            .await;

        match state {
            SpawnState::Spawning {
                epic_id,
                directive,
                spawn_request_id,
                child_session_id,
                spawn_state,
                agent_role,
                epic_spawn_ordinal,
                deduplicated,
                safe_error_class,
                ..
            } => AgentSpawnChildOutcome::Accepted(AgentSpawnChildResultV1 {
                spawn_request_id,
                child_session_id,
                epic_id,
                kind: directive.kind,
                agent_role,
                epic_spawn_ordinal,
                state: spawn_state,
                deduplicated,
                safe_error_class,
            }),
            SpawnState::Rejected { reason } => AgentSpawnChildOutcome::Rejected(reason),
            // handle() only ever returns Spawning or Rejected — Idle/Detected/
            // Validating/Done are internal-only states not reachable here.
            other => AgentSpawnChildOutcome::Rejected(SpawnRejectReason::StoreError(format!(
                "unexpected spawn coordinator state: {other:?}"
            ))),
        }
    }

    /// Reserve one stable same-Epic successor. Store authority checks and
    /// replay resolution complete before the daemon-owned reconciler receives
    /// a reservation-id-only hint.
    pub async fn agent_reserve_successor(
        &self,
        caller_session_id: Uuid,
        request: AgentReserveSuccessorRequestV1,
    ) -> Result<AgentReserveSuccessorResultV1> {
        use crate::store::successor_reservations::{
            AgentSuccessorReservationIds, ReserveAgentSuccessorOutcome,
        };

        let outcome = self.store.lock().await.reserve_agent_successor(
            caller_session_id,
            &request,
            AgentSuccessorReservationIds {
                reservation_id: Uuid::new_v4(),
                candidate_session_id: Uuid::new_v4(),
                transition_id: Uuid::new_v4(),
            },
        )?;
        let (record, deduplicated) = match outcome {
            ReserveAgentSuccessorOutcome::Reserved(record) => (record, false),
            ReserveAgentSuccessorOutcome::Replayed(record) => (record, true),
        };
        if !record.state.is_terminal() {
            self.spawn_coordinator
                .dispatch_successor(record.reservation_id)
                .await
                .map_err(DaemonError::Store)?;
        }
        Ok(record.receipt(deduplicated))
    }

    /// `AgentGetProgress` — one durable SQLite snapshot for a bounded child
    /// cohort. Caller identity and authorization remain server-bound.
    pub async fn agent_get_progress(
        &self,
        caller_session_id: Uuid,
        requested_ids: &[Uuid],
    ) -> Result<AgentGetProgressResultV1> {
        let started = std::time::Instant::now();
        if requested_ids.len() > AGENT_PROGRESS_MAX_COHORT {
            return Err(crate::error::agent_progress_cohort_too_large(
                requested_ids.len(),
            ));
        }
        let result = self
            .store
            .lock()
            .await
            .agent_get_progress_snapshot(caller_session_id, requested_ids);
        if let Ok(result) = &result {
            let max_staleness_ms = result
                .rows
                .iter()
                .map(|row| row.freshness.staleness_ms)
                .max()
                .unwrap_or(0);
            tracing::info!(
                target: "agent_coordination",
                caller_session_id = %caller_session_id,
                cohort_size = result.cohort_size,
                query_latency_ms = started.elapsed().as_millis() as u64,
                max_cursor_staleness_ms = max_staleness_ms,
                "agent progress snapshot"
            );
        } else if let Err(error) = &result {
            tracing::warn!(
                target: "agent_coordination",
                caller_session_id = %caller_session_id,
                requested_cohort_size = requested_ids.len(),
                query_latency_ms = started.elapsed().as_millis() as u64,
                error = %error,
                "agent progress snapshot rejected"
            );
        }
        result
    }

    /// `AgentGetStatus` — session-attributed status read. Scoped to the
    /// caller's own session, a direct child of the caller, or a child of an
    /// Epic the caller leads (see module docs for why the third case
    /// exists).
    pub async fn agent_get_status(
        &self,
        caller_session_id: Uuid,
        target_session_id: Uuid,
    ) -> Result<Session> {
        let target = self
            .authorize_agent_target(caller_session_id, target_session_id)
            .await?;
        Ok(target)
    }

    /// `AgentHalt` — session-attributed interrupt. Scoped identically to
    /// `agent_get_status`; delegates the actual interrupt to the shared
    /// `interrupt_in_maps` helper (same path the `InterruptSession` /
    /// `SessionManager::interrupt_session` RPC verb uses for unattributed
    /// callers).
    pub async fn agent_halt(&self, caller_session_id: Uuid, target_session_id: Uuid) -> Result<()> {
        let (_, manager_scope) = self
            .authorize_agent_mutation_target(caller_session_id, target_session_id)
            .await?;
        self.audit_manager_control(manager_scope.as_ref(), "AgentHalt")
            .await?;
        if super::lifecycle::interrupt_active_in_maps(&self.active, target_session_id).await? {
            return Ok(());
        }
        let Some(_max_retries) = super::lifecycle::suppress_pending_retry_in_maps(
            &self.completed,
            &self.store,
            target_session_id,
        )
        .await?
        else {
            // A terminal row can outlive both in-memory ownership maps while
            // its provider subprocess survives with the exact RSI_SESSION_ID
            // stamp. Fence this recovery against every resume/retry spawn for
            // the same id before consulting any observation again: a bare
            // active-map recheck followed by /proc scanning would otherwise
            // race a legitimate replacement process.
            let _spawn_guard =
                super::spawn_single_flight::acquire_spawn_guard(target_session_id).await;

            let target = self
                .authorize_agent_mutation_target(caller_session_id, target_session_id)
                .await?;
            let target = target.0;
            if super::lifecycle::interrupt_active_in_maps(&self.active, target_session_id).await? {
                return Ok(());
            }
            if super::lifecycle::suppress_pending_retry_in_maps(
                &self.completed,
                &self.store,
                target_session_id,
            )
            .await?
            .is_some()
            {
                return Ok(());
            }

            // Nonterminal rows remain reconciliation-owned. Reaping one here
            // would bypass the durable Failed/Interrupted transition that an
            // active monitor or the reconciliation loop must author.
            if !target.status.is_terminal() {
                return Err(DaemonError::SessionNotFound(target_session_id));
            }

            #[cfg(all(test, target_os = "linux"))]
            let test_reap =
                super::reaper::prepare_runtime_reap_proc_root_operation(target_session_id)?;
            let reaped = tokio::task::spawn_blocking(move || {
                #[cfg(all(test, target_os = "linux"))]
                if let Some(reap) = test_reap {
                    return reap();
                }
                super::reaper::reap_orphans_for_session(target_session_id)
            })
            .await
            .map_err(|error| {
                DaemonError::Process(format!(
                    "agent_halt_terminal_orphan_reap_join_failed:{target_session_id}:{error}"
                ))
            })??;
            if reaped == 0 {
                // The row is terminal (guarded above) and no stamped orphan
                // survived, so the requested end state -- "not running" --
                // already holds. Reporting `SessionNotFound` here was a false
                // answer: the session exists and is readable through
                // `AgentGetStatus`, and the caller's intent is satisfied. A
                // halt of an already-finished child is the ordinary race, not
                // an error, and the same class of false `SessionNotFound` was
                // already removed from `continue_session` for the identical
                // reason (see `continue_recovers_completed_session_missing_from_memory`).
                // Genuinely absent ids still fail in `authorize_agent_target`.
                return Ok(());
            }

            self.event_bus
                .publish(crate::bus::DaemonEvent::SystemMessage {
                    level: "warn".to_string(),
                    message: format!(
                        "AgentHalt reaped {reaped} terminal provider orphan(s) for session {target_session_id} after active-map custody loss"
                    ),
                });
            tracing::warn!(
                session_id = %target_session_id,
                reaped,
                "AgentHalt reaped terminal provider orphan after active-map custody loss"
            );
            return Ok(());
        };

        Ok(())
    }

    /// Authority, provider, and staleness gate for `AgentContinueChild`.
    ///
    /// Returns the cleared continuation cursor. The actual continuation is
    /// issued by [`SessionManager::agent_continue_child`], because the
    /// continuation engine hangs off `SessionManager` while every authority
    /// plane this must consult hangs off this handle; splitting here keeps
    /// each check on the type that owns the state it reads.
    ///
    /// Refusals are evaluated cheapest-and-safest first so a caller cannot use
    /// the verb to probe state it is not authorized for: bounds, then self,
    /// logical scope, tip scope, provider, and finally staleness.
    pub(crate) async fn authorize_continue_child(
        &self,
        caller_session_id: Uuid,
        request: &AgentContinueChildRequestV1,
    ) -> Result<AgentContinuationCursorV1> {
        use rsi_common::agent_coordination::AgentContinueErrorCodeV1;
        use rsi_common::types::SessionProvider;

        // Self-continuation is refused BEFORE any Store read, so the verb
        // cannot even confirm the caller's own row through this door. A
        // session continuing itself is not delegation: it is an unowned
        // self-injection loop with no settling owner, exactly the hazard that
        // `AgentSendMessage` refuses for self-send and that `agent_fresh` on
        // one's own id has already caused twice (issues #27, #30).
        precheck_continue_child_request(caller_session_id, request)?;

        // Reuse the proven status/halt scope verbatim rather than inventing a
        // third authority plane. Continuation is a lifecycle write on exactly
        // the population `AgentHalt` may already stop, so admitting a narrower
        // or wider set here would leave a master able to halt a child it
        // cannot restart, or restart one it never owned.
        let logical_target = self
            .authorize_agent_mutation_target(caller_session_id, request.target_session_id)
            .await
            .map_err(|error| match error {
                DaemonError::InvalidParam(ref code) if code.starts_with("manager_v2_") => error,
                DaemonError::SessionNotFound(_) => crate::error::agent_continue_error(
                    AgentContinueErrorCodeV1::TargetUnknown,
                    None,
                    None,
                ),
                _ => crate::error::agent_continue_error(
                    AgentContinueErrorCodeV1::TargetNotAuthorized,
                    Some(format!(
                        "{caller_session_id}->{}",
                        request.target_session_id
                    )),
                    None,
                ),
            })?
            .0;

        // Read the effective tip and its cursor in one Store snapshot. Do not
        // disclose that witness until the tip itself has passed authority and
        // provider policy below.
        let observed = {
            let store = self.store.lock().await;
            store.agent_continuation_cursor(request.target_session_id)?
        };

        // Authority and provider policy must apply to the row that will
        // actually be continued. Reserved successor rotation can parent the
        // tip to the owning Epic rather than to the logical predecessor, so a
        // direct parent that is not the Epic lead must not inherit authority
        // over that tip. Reuse the already-authorized row when no rotation
        // occurred.
        let effective_target = if observed.tip_session_id == logical_target.id {
            logical_target
        } else {
            self.authorize_agent_mutation_target(caller_session_id, observed.tip_session_id)
                .await
                .map_err(|error| {
                    if matches!(&error, DaemonError::InvalidParam(code) if code.starts_with("manager_v2_")) {
                        return error;
                    }
                    crate::error::agent_continue_error(
                        AgentContinueErrorCodeV1::TargetNotAuthorized,
                        Some(format!("{caller_session_id}->{}", observed.tip_session_id)),
                        None,
                    )
                })?.0
        };

        // CodexAppServer allocates a FRESH session id on continue, so the row
        // passed to `continue_session` would not remain the durable target.
        // Check the resolved tip rather than the logical root: an AppServer
        // root may already have rotated to an ordinary Codex replacement that
        // is safely continuable under its own id.
        if effective_target.provider == SessionProvider::CodexAppServer {
            return Err(crate::error::agent_continue_error(
                AgentContinueErrorCodeV1::ProviderUnsupported,
                Some(observed.tip_session_id.to_string()),
                None,
            ));
        }

        // A replay owns its original receipt even if the tip has since
        // advanced. Authorization still precedes this lookup.
        let (key_digest, fingerprint, _) = child_relaunch_identity(caller_session_id, request)?;
        let existing_relaunch = self
            .store
            .lock()
            .await
            .child_relaunch_intent_by_key(&key_digest)?;
        if let Some(row) = existing_relaunch {
            if row.request_fingerprint != fingerprint {
                return Err(crate::error::agent_continue_error(
                    AgentContinueErrorCodeV1::IdempotencyConflict,
                    None,
                    None,
                ));
            }
            return Ok(observed);
        }

        // Staleness gate. This is a STALENESS check, not a mutual-exclusion
        // primitive, and the distinction is deliberate: the spawn single-flight
        // guard is a non-reentrant mutex that `continue_session` acquires
        // itself, so holding it across this read would deadlock the very call
        // it protects. What the tuple buys is the property the orchestrator
        // actually needs — a master that decided to continue a child based on
        // an observation must not act on that decision after the child has
        // moved on. Two racing continuations remain the documented
        // `continue_session` behavior (both queries are delivered serially),
        // and are not made worse here.
        //
        // A persisted continuation query advances `event_sequence`, so a later
        // request that still carries the old cursor is stale. Persistence runs
        // asynchronously after dispatch, however: concurrent or rapid replay
        // can clear this check more than once. This is deliberately not an
        // idempotency guarantee.
        if !observed.satisfies(request) {
            return Err(crate::error::agent_continue_error(
                AgentContinueErrorCodeV1::StaleContinuation,
                Some(request.target_session_id.to_string()),
                Some(observed),
            ));
        }

        Ok(observed)
    }

    /// Best-effort terminal-watch re-arm after a committed continuation.
    ///
    /// Re-arms against the LOGICAL child, not the rotation tip: the watch's
    /// natural key is `(caller, watched)` exactly as the owner named it, and
    /// rebinding it to a tip would orphan the watch on the next rotation.
    ///
    /// The child just left its terminal state, so a watch that already fired
    /// has been consumed and the master would otherwise never learn about the
    /// second terminal transition — silently violating the unattended-program
    /// no-idle invariant.
    ///
    /// Failure is swallowed ON PURPOSE. The continuation has already
    /// committed by the time this runs; failing the whole call because the
    /// caller sits at its watch cap would report "not continued" for a child
    /// that very much was. The receipt reports what actually happened.
    pub(crate) async fn rearm_child_watch_after_continue(
        &self,
        caller_session_id: Uuid,
        target_session_id: Uuid,
    ) -> bool {
        match self
            .arm_automatic_child_watch_inner(caller_session_id, target_session_id, true)
            .await
        {
            Ok(ArmWatchOutcome::Armed(_) | ArmWatchOutcome::Deduplicated(_)) => true,
            Err(error) => {
                tracing::warn!(
                    target: "agent_coordination",
                    caller_session_id = %caller_session_id,
                    target_session_id = %target_session_id,
                    %error,
                    "AgentContinueChild continued the child but could not re-arm its terminal watch"
                );
                false
            }
        }
    }

    /// Create a durable attributed issue using the C5 UUIDv5 domain. This
    /// handle is the sole authority owner for both RPC and native transports.
    pub async fn agent_create_issue(
        &self,
        caller_session_id: Uuid,
        params: AgentCreateIssueParams,
    ) -> Result<AgentCreateIssueResult> {
        let caller = self
            .get_session(caller_session_id)
            .await
            .ok_or(DaemonError::SessionNotFound(caller_session_id))?;
        let project_id = caller.project_id.ok_or_else(|| {
            DaemonError::InvalidParam(format!(
                "agent_create_issue_project_unavailable:{caller_session_id}"
            ))
        })?;
        let key = params.idempotency_key.as_bytes();
        if key.is_empty() || key.len() > 128 || key.contains(&0) {
            return Err(DaemonError::InvalidParam(
                "agent_create_issue_idempotency_key_must_be_1_to_128_bytes_without_nul".into(),
            ));
        }
        if let Some(priority) = params.priority
            && !(1..=4).contains(&priority)
        {
            return Err(DaemonError::InvalidParam(
                "agent_create_issue_priority_must_be_1_to_4".into(),
            ));
        }

        let issue_id = deterministic_agent_issue_id(caller_session_id, key);
        let new = NewIssue {
            project_id,
            title: params.title,
            body: params.body,
            priority: params.priority,
            labels: params.labels,
            created_by_session_id: Some(caller_session_id),
            assignee: params.assignee,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        };
        let outcome = self
            .store
            .lock()
            .await
            .create_agent_issue_idempotent_with_key(
                caller_session_id,
                issue_id,
                &new,
                &params.idempotency_key,
            )?;
        if !outcome.create_fields_match {
            return Err(DaemonError::InvalidParam(format!(
                "agent_create_issue_idempotency_conflict:{issue_id}"
            )));
        }
        Ok(AgentCreateIssueResult {
            issue: outcome.issue,
            deduplicated: outcome.deduplicated,
        })
    }

    /// Guarded project Issue control for the current owning-Epic lead or the
    /// `IssueCoordinate` manager. The Store resolves the owning Epic, project,
    /// lead pointer and lead generation (or the manager appointment, grant and
    /// live scope) from persisted rows inside the same read/write transaction
    /// as the requested operation.
    pub async fn agent_list_issues(
        &self,
        caller_session_id: Uuid,
        request: AgentListIssuesRequestV1,
    ) -> Result<IssuePageV1> {
        self.store
            .lock()
            .await
            .agent_list_issues(caller_session_id, &request)
    }

    pub async fn agent_get_issue(
        &self,
        caller_session_id: Uuid,
        request: AgentGetIssueRequestV1,
    ) -> Result<rsi_common::types::AgentGetIssueResultV1> {
        self.store
            .lock()
            .await
            .agent_get_issue(caller_session_id, &request)
    }

    pub async fn agent_update_issue(
        &self,
        caller_session_id: Uuid,
        request: AgentUpdateIssueRequestV1,
    ) -> Result<AgentIssueMutationResultV1> {
        self.store
            .lock()
            .await
            .agent_update_issue(caller_session_id, &request)
    }

    pub async fn agent_update_issue_status(
        &self,
        caller_session_id: Uuid,
        request: AgentUpdateIssueStatusRequestV1,
    ) -> Result<AgentIssueMutationResultV1> {
        self.store
            .lock()
            .await
            .agent_update_issue_status(caller_session_id, &request)
    }

    pub async fn agent_archive_issue(
        &self,
        caller_session_id: Uuid,
        request: AgentArchiveIssueRequestV1,
    ) -> Result<AgentIssueMutationResultV1> {
        self.store
            .lock()
            .await
            .agent_archive_issue(caller_session_id, &request)
    }

    pub async fn agent_restore_issue(
        &self,
        caller_session_id: Uuid,
        request: AgentRestoreIssueRequestV1,
    ) -> Result<AgentIssueMutationResultV1> {
        self.store
            .lock()
            .await
            .agent_restore_issue(caller_session_id, &request)
    }

    pub async fn agent_list_issue_events(
        &self,
        caller_session_id: Uuid,
        request: IssueEventPageRequestV1,
    ) -> Result<IssueEventPageV1> {
        self.store
            .lock()
            .await
            .agent_list_issue_events(caller_session_id, &request)
    }

    /// Enforce the master-orchestrate no-idle invariant at terminal settlement.
    ///
    /// A strict program outcome (or the bounded legacy final-report fallback)
    /// may declare that work remains only when an enabled same-session Resume
    /// wake or child terminal watch exists durably. A missing/mismatched guard
    /// inserts one deterministic one-shot Resume job and creates one
    /// deterministic attributed issue. It never launches synchronously and
    /// can never construct Fresh/AgentFresh.
    pub(crate) async fn enforce_master_no_idle(
        &self,
        caller_session_id: Uuid,
        terminal_sequence: i32,
        assistant_output: &str,
    ) -> Result<MasterNoIdleOutcome> {
        self.enforce_master_no_idle_for_invocation(
            caller_session_id,
            terminal_sequence,
            None,
            assistant_output,
        )
        .await
    }

    pub(crate) async fn enforce_master_no_idle_for_invocation(
        &self,
        caller_session_id: Uuid,
        terminal_sequence: i32,
        model_invocation_id: Option<Uuid>,
        assistant_output: &str,
    ) -> Result<MasterNoIdleOutcome> {
        let caller = self
            .get_session(caller_session_id)
            .await
            .ok_or(DaemonError::SessionNotFound(caller_session_id))?;
        if !matches!(
            caller.status,
            rsi_common::types::SessionStatus::Completed | rsi_common::types::SessionStatus::Failed
        ) {
            return Ok(MasterNoIdleOutcome::NotApplicable);
        }

        // A persisted capacity delivery receipt authenticates the immutable
        // controller/program root before the mutable rotation tip's sentinel
        // is consulted.
        let prior_capacity = if let Some(invocation_id) = model_invocation_id {
            let store = self.store.lock().await;
            store.capacity_attempt_context(invocation_id)?
        } else {
            None
        };
        let exact_capacity_receipt = prior_capacity
            .as_ref()
            .and_then(|_| model_invocation_id.map(|id| (id, terminal_sequence)));
        let controller_session_id = prior_capacity
            .as_ref()
            .map_or(caller_session_id, |context| context.controller_session_id);
        let program_guard_id = prior_capacity.as_ref().map_or_else(
            || {
                crate::session::harness::tools::schedule_wake::deterministic_program_guard_job_id(
                    controller_session_id,
                )
            },
            |context| context.program_guard_job_id,
        );
        let registration_evidence = {
            let store = self.store.lock().await;
            program_registration_evidence(
                &store,
                controller_session_id,
                program_guard_id,
                prior_capacity.as_ref(),
            )?
        };
        let program_registered = matches!(
            registration_evidence,
            ProgramRegistrationEvidence::EnabledSentinel
                | ProgramRegistrationEvidence::ClosedCapacityTerminalReplay
        );
        let parsed = rsi_common::agent_contract::program_continuation_intent_v1_with_registration(
            assistant_output,
            program_registered,
        );
        let mut intent = match registration_evidence {
            ProgramRegistrationEvidence::ClosedSentinel
                if parsed == ProgramContinuationIntentV1::NotProgram =>
            {
                return Ok(MasterNoIdleOutcome::NotApplicable);
            }
            ProgramRegistrationEvidence::ClosedSentinel => {
                ProgramContinuationIntentV1::InvalidProgram(
                    "program guard is closed and must be explicitly re-registered".into(),
                )
            }
            ProgramRegistrationEvidence::Malformed => ProgramContinuationIntentV1::InvalidProgram(
                format!("program guard row {program_guard_id} is malformed"),
            ),
            ProgramRegistrationEvidence::Absent
                if parsed != ProgramContinuationIntentV1::NotProgram =>
            {
                ProgramContinuationIntentV1::InvalidProgram(
                    "program outcome emitted without daemon program registration".into(),
                )
            }
            _ => parsed,
        };
        if caller.status == rsi_common::types::SessionStatus::Failed
            && self
                .completed
                .read()
                .await
                .get(&caller_session_id)
                .is_some_and(|completed| completed.retry_cancel.is_some())
        {
            return Ok(MasterNoIdleOutcome::OrdinaryGuardPresent);
        }

        let exact_capacity_failure = caller.provider == rsi_common::types::SessionProvider::Codex
            && caller.stop_reason.as_deref() == Some("provider_error:codex_usage_limit");
        if exact_capacity_failure {
            let invocation_id = model_invocation_id.ok_or_else(|| {
                DaemonError::Store(
                    "capacity recovery refused: terminal model invocation id missing".into(),
                )
            })?;
            if prior_capacity.is_none()
                && !matches!(
                    registration_evidence,
                    ProgramRegistrationEvidence::EnabledSentinel
                )
            {
                return Err(DaemonError::Store(
                    "capacity recovery refused: valid program guard missing".into(),
                ));
            }
            const MAX_TRANSIENT_SETTLEMENT_ATTEMPTS: usize = 3;
            let mut settled = None;
            for attempt in 1..=MAX_TRANSIENT_SETTLEMENT_ATTEMPTS {
                let result = {
                    let store = self.store.lock().await;
                    if intent == ProgramContinuationIntentV1::TerminalAllowed {
                        store
                            .settle_terminal_capacity(
                                caller_session_id,
                                controller_session_id,
                                program_guard_id,
                                invocation_id,
                                terminal_sequence,
                                chrono::Utc::now(),
                            )
                            .map(EitherCapacitySettlement::Terminal)
                    } else {
                        store
                            .settle_capacity_failure(
                                caller_session_id,
                                controller_session_id,
                                program_guard_id,
                                invocation_id,
                                terminal_sequence,
                                chrono::Utc::now(),
                            )
                            .map(EitherCapacitySettlement::Failure)
                    }
                };
                match result {
                    Ok(result) => {
                        settled = Some(result);
                        break;
                    }
                    Err(error)
                        if attempt < MAX_TRANSIENT_SETTLEMENT_ATTEMPTS
                            && is_transient_master_no_idle_store_error(&error) =>
                    {
                        tracing::warn!(
                            session_id = %caller_session_id,
                            terminal_sequence,
                            attempt,
                            error = %error,
                            "retrying transient provider-capacity settlement transaction"
                        );
                    }
                    Err(error) => return Err(error),
                }
            }
            let settled = settled.ok_or_else(|| {
                DaemonError::Store("capacity_recovery_settlement_retry_exhausted".into())
            })?;
            match settled {
                EitherCapacitySettlement::Terminal(settlement) => {
                    return Ok(MasterNoIdleOutcome::TerminalAllowed {
                        capacity: Some(settlement),
                    });
                }
                EitherCapacitySettlement::Failure(settlement) => {
                    self.event_bus.publish(crate::bus::DaemonEvent::SystemMessage {
                        level: "warn".into(),
                        message: format!(
                            "Codex capacity outage epoch {} retained same-session Resume wake {} at backoff bucket {} (due {}).",
                            settlement.outage_epoch,
                            settlement.wake_job_id,
                            settlement.backoff_bucket,
                            settlement.due_slot.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                        ),
                    });
                    return Ok(MasterNoIdleOutcome::CapacityRecovered { settlement });
                }
            }
        }

        if caller.status == rsi_common::types::SessionStatus::Completed
            && intent != ProgramContinuationIntentV1::TerminalAllowed
        {
            let store = self.store.lock().await;
            store.close_capacity_incident(
                caller_session_id,
                crate::store::capacity_recovery::CapacityCloseKind::NonCapacitySuccess,
                exact_capacity_receipt,
                None,
                chrono::Utc::now(),
            )?;
        }

        if intent == ProgramContinuationIntentV1::NotProgram {
            return Ok(MasterNoIdleOutcome::NotApplicable);
        }

        if intent == ProgramContinuationIntentV1::TerminalAllowed {
            let terminal_disabled = {
                let store = self.store.lock().await;
                if store.close_capacity_incident(
                    caller_session_id,
                    crate::store::capacity_recovery::CapacityCloseKind::ProgramTerminal,
                    exact_capacity_receipt,
                    Some(program_guard_id),
                    chrono::Utc::now(),
                )? != crate::store::capacity_recovery::CapacityCloseOutcome::NoOpenIncident
                {
                    true
                } else {
                    match master_program_guard_state(
                        &store,
                        controller_session_id,
                        program_guard_id,
                    )? {
                        MasterProgramGuardState::Valid => {
                            store.update_scheduled_job(
                                &program_guard_id,
                                &ScheduledJobUpdate {
                                    name: None,
                                    message: None,
                                    schedule: None,
                                    enabled: Some(false),
                                    next_fire_at: None,
                                },
                            )?;
                            true
                        }
                        MasterProgramGuardState::Absent | MasterProgramGuardState::Malformed => {
                            false
                        }
                    }
                }
            };
            if terminal_disabled {
                return Ok(MasterNoIdleOutcome::TerminalAllowed { capacity: None });
            }
            intent = ProgramContinuationIntentV1::InvalidProgram(
                "program guard disappeared or became malformed during terminal settlement".into(),
            );
        }

        if caller.status != rsi_common::types::SessionStatus::Completed
            && let Some(invocation_id) = model_invocation_id
        {
            let store = self.store.lock().await;
            store.finalize_capacity_delivery_attempt(
                invocation_id,
                terminal_sequence,
                false,
                false,
                chrono::Utc::now(),
            )?;
        }

        let recovery_wake_id =
            deterministic_master_no_idle_wake_id(caller_session_id, terminal_sequence);

        let reason = match &intent {
            ProgramContinuationIntentV1::RequireChildWatch { job_id } => {
                format!("declared child_watch job {job_id} is not an enabled terminal-watch row")
            }
            ProgramContinuationIntentV1::RequireResumeWake { job_id } => {
                format!("declared resume_wake job {job_id} is not an enabled one-shot Resume row")
            }
            ProgramContinuationIntentV1::RequireAnyGuard => {
                "legacy program report left an open queue without an enabled continuation row"
                    .to_string()
            }
            ProgramContinuationIntentV1::InvalidProgram(error) => {
                format!("invalid program outcome: {error}")
            }
            ProgramContinuationIntentV1::NotProgram
            | ProgramContinuationIntentV1::TerminalAllowed => unreachable!("returned above"),
        };
        let issue_cause = normalize_master_no_idle_reason(&reason);
        let delivery = rsi_common::daemon_message::wrap(
            "orchestration-no-idle",
            &format!(
                "[rsid-no-idle] Recovered unattended program continuation after {reason}. Reconcile the program ledger and dispatch the exact next authorized slice."
            ),
        );
        let mut wake = crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(
            crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
                message: delivery,
                in_seconds: Some(1),
                at: None,
                name: Some(format!(
                    "master-no-idle-{caller_session_id}-{terminal_sequence}"
                )),
                every_seconds: None,
                mode: Some("resume".to_string()),
                working_dir: caller
                    .sandbox_root
                    .clone()
                    .unwrap_or_else(|| caller.working_dir.clone()),
                provider: Some(caller.provider),
                model: caller.model.clone(),
                project_id: caller.project_id,
                origin_session_id: Some(caller_session_id),
                watch_session_id: None,
            },
        )
        .map_err(DaemonError::InvalidParam)?;
        wake.id = recovery_wake_id;
        debug_assert_eq!(wake.wake_mode, WakeMode::Resume);

        let issue = caller.project_id.map(|project_id| {
            let issue_key = format!("master-no-idle-issue:{project_id}:{issue_cause}");
            let issue_id = Uuid::new_v5(&c5_issue_namespace(), issue_key.as_bytes());
            let new = NewIssue {
                project_id,
                title: "Unattended orchestration violated the no-idle invariant".into(),
                body: format!(
                    "Canonical auto-filed issue for a repeated no-idle recovery cause. Individual affected sessions retain their own deterministic Resume wake rows.\n\nCause fingerprint: {issue_cause}"
                ),
                priority: Some(1),
                labels: vec![
                    "orchestration".into(),
                    "no-idle-invariant".into(),
                    "auto-filed".into(),
                ],
                created_by_session_id: Some(caller_session_id),
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            };
            (issue_id, new)
        });

        const MAX_TRANSIENT_SETTLEMENT_ATTEMPTS: usize = 3;
        let settled = {
            let mut result = None;
            for attempt in 1..=MAX_TRANSIENT_SETTLEMENT_ATTEMPTS {
                let attempt_result = {
                    let store = self.store.lock().await;
                    if exact_master_continuation_guard_present(
                        &store,
                        caller_session_id,
                        program_guard_id,
                        &intent,
                    )? {
                        return Ok(MasterNoIdleOutcome::OrdinaryGuardPresent);
                    }
                    store.settle_master_no_idle_recovery(
                        caller_session_id,
                        &wake,
                        issue.as_ref().map(|(issue_id, new)| (*issue_id, new)),
                    )
                };
                match attempt_result {
                    Ok(outcome) => {
                        result = Some(outcome);
                        break;
                    }
                    Err(error)
                        if attempt < MAX_TRANSIENT_SETTLEMENT_ATTEMPTS
                            && is_transient_master_no_idle_store_error(&error) =>
                    {
                        tracing::warn!(
                            session_id = %caller_session_id,
                            terminal_sequence,
                            attempt,
                            error = %error,
                            "retrying transient no-idle settlement transaction"
                        );
                    }
                    Err(error) => return Err(error),
                }
            }
            result.ok_or_else(|| {
                DaemonError::Store("master_no_idle_settlement_retry_exhausted".into())
            })?
        };

        let (already_settled, disposition, disposition_text) = match settled {
            MasterNoIdleStoreRecovery::WakeAndIssue {
                issue_id,
                wake_deduplicated,
                issue_deduplicated,
            } => (
                wake_deduplicated && issue_deduplicated,
                MasterNoIdleRecoveryDisposition::WakeAndAttributedIssue { issue_id },
                format!("wake+attributed-issue:{issue_id}"),
            ),
            MasterNoIdleStoreRecovery::ProjectlessWakeOnly { wake_deduplicated } => (
                wake_deduplicated,
                MasterNoIdleRecoveryDisposition::ProjectlessWakeOnly,
                "projectless-wake-only:no-attributed-issue".to_string(),
            ),
        };
        if already_settled {
            return Ok(MasterNoIdleOutcome::OrdinaryGuardPresent);
        }

        self.event_bus
            .publish(crate::bus::DaemonEvent::SystemMessage {
                level: "warn".into(),
                message: format!(
                    "Recovered unattended orchestration session {caller_session_id}: inserted same-session Resume wake {recovery_wake_id}; disposition: {disposition_text}; reason: {reason}"
                ),
            });
        tracing::warn!(
            session_id = %caller_session_id,
            terminal_sequence,
            wake_job_id = %recovery_wake_id,
            disposition = %disposition_text,
            reason = %reason,
            "master-orchestrate no-idle invariant recovered"
        );
        Ok(MasterNoIdleOutcome::GenericRecovered {
            wake_job_id: recovery_wake_id,
            disposition,
        })
    }

    /// C5's single post-disposition automatic-file policy choke. It reads the
    /// durable marker, snapshots map state without retaining a map lock across
    /// store I/O, then either resolves an excluded marker or creates one
    /// deterministic lineage issue. Automatic errors are deliberately
    /// non-invasive: the caller's terminal/retry state is never changed here.
    pub(crate) async fn maybe_autofile_terminal_failure(
        &self,
        source_session_id: Uuid,
        disposition: RecoveryDisposition,
    ) {
        let pending_key = crate::store::daemon_settings::c5_autofile_pending_key(source_session_id);
        let pending = {
            let store = self.store.lock().await;
            store.get_c5_autofile_pending(&pending_key)
        };
        let pending = match pending {
            Ok(Some(pending)) => pending,
            Ok(None) => return,
            Err(e) => {
                self.report_c5_autofile_error(source_session_id, None, None, &e);
                return;
            }
        };
        if pending.source_session_id != source_session_id {
            self.report_c5_autofile_error(
                source_session_id,
                None,
                None,
                &DaemonError::Store("c5_autofile_pending_source_mismatch".into()),
            );
            return;
        }
        let ownership = {
            let store = self.store.lock().await;
            store.resolve_c5_if_capacity_owned(source_session_id, &pending_key)
        };
        match ownership {
            Ok(crate::store::capacity_recovery::CapacityC5Ownership::Owned(_)) => return,
            Ok(crate::store::capacity_recovery::CapacityC5Ownership::NotOwned) => {}
            Err(error) => {
                self.report_c5_autofile_error(source_session_id, None, None, &error);
                return;
            }
        }
        let source = match self.store.lock().await.get_session(source_session_id) {
            Ok(Some(source)) => source,
            Ok(None) => {
                self.report_c5_autofile_error(
                    source_session_id,
                    None,
                    None,
                    &DaemonError::Store("c5_autofile_source_missing".into()),
                );
                return;
            }
            Err(e) => {
                self.report_c5_autofile_error(source_session_id, None, None, &e);
                return;
            }
        };
        let active_source = self.active.read().await.contains_key(&source_session_id);
        // A durable explicit retry budget is authoritative.  Looking only at
        // the current kind default suppresses C5 replay for historical rows
        // after a policy-default change, leaving their terminal failures
        // permanently unfiled.
        let permanent_exclusion = source.status != rsi_common::types::SessionStatus::Failed
            || source.max_retries.unwrap_or_else(|| {
                super::retry_policy::kind_default_max_retries(source.session_kind, 1)
            }) == 0
            || source.is_eval
            || source.issue_tracker_id.is_some()
            || source.issue_identifier.is_some();
        if permanent_exclusion {
            if let Err(e) = self
                .store
                .lock()
                .await
                .resolve_c5_autofile_pending(&pending_key)
            {
                self.report_c5_autofile_error(source_session_id, None, None, &e);
            }
            return;
        }
        let has_live_retry = {
            let completed = self.completed.read().await;
            completed
                .get(&source_session_id)
                .is_some_and(|cs| cs.retry_cancel.is_some() || cs.retry_fired_at.is_some())
        };
        if active_source || has_live_retry {
            return;
        }

        let result = {
            let store = self.store.lock().await;
            (|| -> Result<(Uuid, Uuid, NewIssue)> {
                let mut current = source.id;
                let mut visited = std::collections::HashSet::new();
                for depth in 0..=64 {
                    if !visited.insert(current) {
                        return Err(DaemonError::Store("c5_autofile_lineage_cycle".into()));
                    }
                    let row = store
                        .get_session(current)?
                        .ok_or_else(|| DaemonError::Store("c5_autofile_lineage_missing".into()))?;
                    match row.continued_from {
                        Some(parent) if depth < 64 => current = parent,
                        Some(_) => {
                            return Err(DaemonError::Store(
                                "c5_autofile_lineage_depth_exceeded".into(),
                            ));
                        }
                        None => break,
                    }
                }
                let root_id = current;
                let mut name = b"pipeline-failure\0".to_vec();
                name.extend_from_slice(root_id.to_string().as_bytes());
                let issue_id = Uuid::new_v5(&c5_issue_namespace(), &name);
                let kind = format!("{:?}", source.session_kind);
                let kebab = match source.session_kind {
                    SessionKind::TaskRabbit => "task-rabbit",
                    SessionKind::Task => "task",
                    SessionKind::Bug => "bug",
                    SessionKind::Feature => "feature",
                    SessionKind::Refactor => "refactor",
                    SessionKind::Research => "research",
                    _ => return Err(DaemonError::Store("c5_autofile_ineligible_kind".into())),
                };
                let provider = serde_json::to_string(&source.provider)
                    .map_err(DaemonError::Json)?
                    .trim_matches('"')
                    .to_string();
                let body = format!(
                    "Automatic issue for a settled RSI pipeline worker failure.\n\nsource_session_id: {}\nlineage_root_session_id: {}\nsession_kind: {}\nprovider: {}\nmodel: {}\nfailure_cause: {}\nrecovery_disposition: {}\nretry_attempt: {}\nmax_retries: {}\n",
                    source.id,
                    root_id,
                    kind,
                    provider,
                    source.model.as_deref().unwrap_or("<unset>"),
                    pending.cause.as_str(),
                    disposition.as_str(),
                    source.retry_attempt.unwrap_or(0),
                    source.max_retries.unwrap_or(0)
                );
                Ok((
                    root_id,
                    issue_id,
                    NewIssue {
                        project_id: source.project_id.ok_or_else(|| {
                            DaemonError::Store(format!(
                                "c5_autofile_project_unavailable:{}",
                                source.id
                            ))
                        })?,
                        title: format!(
                            "Pipeline worker failed: {kind} {}",
                            &root_id.to_string()[..8]
                        ),
                        body,
                        priority: None,
                        labels: vec![
                            "pipeline-failure".into(),
                            "auto-filed".into(),
                            format!("worker-kind:{kebab}"),
                        ],
                        created_by_session_id: Some(source.id),
                        assignee: None,
                        idea_id: None,
                        source_event_id: None,
                        source_finding_ref: None,
                    },
                ))
            })()
        };
        let context = result
            .as_ref()
            .ok()
            .map(|(root_id, issue_id, _)| (*root_id, *issue_id));
        let write_result: crate::store::daemon_settings::C5TransitionResult<C5SettlementOutcome> =
            match result {
                Ok((_, issue_id, new)) => {
                    let store = self.store.lock().await;
                    store.settle_c5_autofile_pending(
                        source_session_id,
                        issue_id,
                        &new,
                        &pending_key,
                    )
                }
                Err(source) => Err(
                    crate::store::daemon_settings::C5TransitionError::Retryable {
                        operation: "automatic_issue_payload",
                        source,
                    },
                ),
            };
        match write_result {
            Ok(
                C5SettlementOutcome::Committed { .. }
                | C5SettlementOutcome::Suppressed
                | C5SettlementOutcome::StaleMarker,
            ) => {}
            Err(e) => {
                let error: DaemonError = e.into();
                self.report_c5_autofile_error(
                    source_session_id,
                    context.map(|(root_id, _)| root_id),
                    context.map(|(_, issue_id)| issue_id),
                    &error,
                );
            }
        }
    }

    fn report_c5_autofile_error(
        &self,
        source_session_id: Uuid,
        lineage_root_session_id: Option<Uuid>,
        issue_id: Option<Uuid>,
        error: &DaemonError,
    ) {
        tracing::error!(error = %error, source_session_id = %source_session_id, lineage_root_session_id = ?lineage_root_session_id, issue_id = ?issue_id, "C5 automatic issue filing failed");
        self.event_bus.publish(crate::bus::DaemonEvent::SystemMessage {
            level: "error".to_string(),
            message: format!("Automatic issue filing failed for session {source_session_id}; lineage_root_session_id={lineage_root_session_id:?}; issue_id={issue_id:?}: {error}"),
        });
    }

    /// One finite post-restore replay. It enumerates only indexed C5 journal
    /// rows in fixed batches; it never scans historical Failed sessions.
    pub async fn replay_c5_autofile_pending(&self) {
        let mut after = None;
        loop {
            let batch = {
                let store = self.store.lock().await;
                store.list_c5_autofile_pending(
                    after.as_deref(),
                    crate::store::daemon_settings::C5_AUTOFILE_PENDING_BATCH_SIZE,
                )
            };
            let batch = match batch {
                Ok(batch) => batch,
                Err(error) => {
                    self.report_c5_autofile_error(Uuid::nil(), None, None, &error);
                    return;
                }
            };
            if batch.is_empty() {
                return;
            }
            for (key, raw) in &batch {
                after = Some(key.clone());
                let pending = match C5AutofilePending::parse(raw) {
                    Ok(pending) => pending,
                    Err(e) => {
                        let source_session_id =
                            crate::store::daemon_settings::source_session_id_from_c5_pending_key(
                                key,
                            )
                            .unwrap_or(Uuid::nil());
                        let error: DaemonError = e.into();
                        self.report_c5_autofile_error(source_session_id, None, None, &error);
                        continue;
                    }
                };
                self.maybe_autofile_terminal_failure(
                    pending.source_session_id,
                    RecoveryDisposition::NoRecoverySource,
                )
                .await;
            }
            if batch.len() < crate::store::daemon_settings::C5_AUTOFILE_PENDING_BATCH_SIZE {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    /// Resolve a session through the same snapshot path the RPC read verbs
    /// use, so the handle and `SessionManager` never diverge on read
    /// semantics.
    async fn get_session(&self, session_id: Uuid) -> Option<Session> {
        super::queries::get_session_snapshot(&self.active, &self.completed, &self.store, session_id)
            .await
    }

    /// A8 terminal watch: authorize `caller` to arm a watch on `watched`.
    /// Same scope as `AgentGetStatus`/`AgentHalt` (`authorize_agent_target`),
    /// with self-watch rejected explicitly — a session watching itself for
    /// terminality is a deadlock by construction (it must be non-terminal to
    /// receive the wake). The wake TARGET is never part of this check; it is
    /// bound to the resolved caller at the RPC layer.
    pub(crate) async fn authorize_watch_target(
        &self,
        caller_session_id: Uuid,
        watched_session_id: Uuid,
    ) -> Result<()> {
        if watched_session_id == caller_session_id {
            return Err(DaemonError::InvalidParam(format!(
                "watch_self_rejected: session {caller_session_id} cannot arm a terminal watch on itself"
            )));
        }
        self.authorize_agent_target(caller_session_id, watched_session_id)
            .await?;
        Ok(())
    }

    /// Register the program sentinel using only the server-bound caller. This
    /// is the common entry point for native transports that cannot carry the
    /// tokened `AgentScheduleWake` verb (notably CodexAppServer). No tool input
    /// can steer identity, timing, provider, project, or working-directory
    /// custody.
    pub(crate) async fn register_bound_program_guard(
        &self,
        caller_session_id: Uuid,
    ) -> Result<ProgramGuardRegistration> {
        let caller = self
            .get_session(caller_session_id)
            .await
            .ok_or(DaemonError::SessionNotFound(caller_session_id))?;
        let candidate = crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(
            crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
                message: "master-orchestrate program guard".into(),
                in_seconds: None,
                at: None,
                name: None,
                every_seconds: None,
                mode: Some("program_guard".into()),
                working_dir: caller
                    .sandbox_root
                    .clone()
                    .unwrap_or_else(|| caller.working_dir.clone()),
                provider: Some(caller.provider),
                model: caller.model.clone(),
                project_id: caller.project_id,
                origin_session_id: Some(caller_session_id),
                watch_session_id: None,
            },
        )
        .map_err(DaemonError::InvalidParam)?;
        self.register_program_guard(caller_session_id, candidate)
            .await
    }

    /// Persist or re-arm one deterministic, far-future, same-session Resume
    /// sentinel. Both AgentScheduleWake and the native schedule_wake tool use
    /// this exact-ID service, so program identity is daemon-authoritative and
    /// survives restarts without a new verb or schema field.
    pub(crate) async fn register_program_guard(
        &self,
        caller_session_id: Uuid,
        candidate: ScheduledJob,
    ) -> Result<ProgramGuardRegistration> {
        use crate::session::harness::tools::schedule_wake::is_program_guard_sentinel;

        if !is_program_guard_sentinel(&candidate, caller_session_id) || !candidate.enabled {
            return Err(DaemonError::InvalidParam(
                "register_program_guard requires the daemon-derived sentinel envelope".into(),
            ));
        }
        let store = self.store.lock().await;
        if !store.scheduled_job_exists(&candidate.id)? {
            store.insert_scheduled_job(&candidate)?;
            return Ok(ProgramGuardRegistration::Registered(candidate));
        }

        // The authenticated caller plus the deterministic id prove ownership;
        // mutable row fields do not. Explicit registration is therefore also
        // the sole repair path for a malformed legacy deterministic row.
        store.restore_program_guard_scheduled_job(&candidate)?;
        Ok(ProgramGuardRegistration::Deduplicated(candidate))
    }

    /// A8.1 F-2: the SINGLE arm path for terminal watches. Natural-key dedup,
    /// the per-master cap, and the row insert all happen under ONE store-lock
    /// acquisition, so two concurrent identical arms cannot both pass the
    /// check and double-insert (or overshoot the cap) — the pre-A8.1 RPC
    /// handler checked and inserted under separate lock scopes (TOCTOU). Both
    /// the `AgentScheduleWake` RPC verb and the in-process `schedule_wake`
    /// harness tool route through here. The persisted watch row is the dedup
    /// source of truth (stronger than the in-memory `SPAWN_IDEMPOTENCY` map:
    /// it survives restarts). The critical section is await-free apart from
    /// the lock acquisition itself — all rusqlite calls are sync.
    ///
    /// The caller must already be authorized for the watched subject
    /// ([`Self::authorize_watch_target`]); this service enforces the
    /// arm-time row invariants, including (review F2, defense-in-depth)
    /// that the candidate's wake target is bound to the arming caller and
    /// that the caller is not watching itself.
    pub(crate) async fn arm_terminal_watch(
        &self,
        caller_session_id: Uuid,
        candidate: ScheduledJob,
    ) -> Result<ArmWatchOutcome> {
        self.arm_terminal_watch_inner(caller_session_id, candidate, false)
            .await
    }

    async fn arm_terminal_watch_inner(
        &self,
        caller_session_id: Uuid,
        candidate: ScheduledJob,
        reset_existing: bool,
    ) -> Result<ArmWatchOutcome> {
        let WakeMode::OnTerminal(watched) = candidate.wake_mode else {
            return Err(DaemonError::InvalidParam(
                "arm_terminal_watch requires an on_terminal candidate job".into(),
            ));
        };
        // Review F2: the dedup/cap accounting below keys on
        // `caller_session_id`, while the inserted row carries the candidate's
        // `wake_session_id` — a candidate bound to anyone else would corrupt
        // per-master accounting, so reject the mismatch rather than trusting
        // every call site to have bound it correctly.
        if candidate.wake_session_id != Some(caller_session_id) {
            return Err(DaemonError::InvalidParam(
                "arm_terminal_watch: candidate wake target must be the caller".into(),
            ));
        }
        // Self-watch is a deadlock by construction (see
        // `authorize_watch_target`); re-checked here so the service holds the
        // invariant even if a future call site skips authorization.
        if watched == caller_session_id {
            return Err(DaemonError::InvalidParam(format!(
                "watch_self_rejected: session {caller_session_id} cannot arm a terminal watch on itself"
            )));
        }
        let store = self.store.lock().await;
        let jobs = store
            .list_enabled_terminal_watches()
            .map_err(|e| DaemonError::Rpc(format!("failed to list scheduled jobs: {e}")))?;
        let mut mine: Vec<&ScheduledJob> = Vec::new();
        for job in &jobs {
            if job.enabled
                && job.wake_session_id == Some(caller_session_id)
                && matches!(job.wake_mode, WakeMode::OnTerminal(_))
                && !store.is_harness_manager_watch(job.id)?
            {
                mine.push(job);
            }
        }
        if let Some(existing) = mine
            .iter()
            .find(|j| j.wake_mode == WakeMode::OnTerminal(watched))
        {
            if reset_existing {
                // Keep the lookup and epoch reset under the same store lock.
                // Confirmed retirement cannot disable this row between them.
                let reset = store.reset_child_watch_after_continue(existing.id)?;
                if !reset {
                    return Err(DaemonError::Store(
                        "continued child watch reset lost its enabled row".into(),
                    ));
                }
            }
            // Idempotent re-arm: same (caller, watched) natural key.
            return Ok(ArmWatchOutcome::Deduplicated((**existing).clone()));
        }
        if mine.len() >= MAX_TERMINAL_WATCHES_PER_MASTER {
            tracing::warn!(
                target: "agent_coordination",
                owner_session_id = %caller_session_id,
                watched_session_id = %watched,
                enabled_watch_count = mine.len(),
                max_enabled_watches = MAX_TERMINAL_WATCHES_PER_MASTER,
                "automatic/manual terminal watch overflow"
            );
            return Err(DaemonError::InvalidParam(format!(
                "terminal_watch_cap_reached: session {caller_session_id} already has {} enabled watches (max {MAX_TERMINAL_WATCHES_PER_MASTER})",
                mine.len()
            )));
        }
        store
            .insert_scheduled_job(&candidate)
            .map_err(|e| DaemonError::Rpc(format!("failed to create scheduled job: {e}")))?;
        Ok(ArmWatchOutcome::Armed(candidate))
    }

    /// Arm the daemon-owned terminal watch for a durably launched child.
    /// The spawn request itself is the authority witness, so this internal
    /// path does not re-run caller-facing target authorization.
    pub(crate) async fn arm_automatic_child_watch(
        &self,
        owner_session_id: Uuid,
        child_session_id: Uuid,
    ) -> Result<ArmWatchOutcome> {
        self.arm_automatic_child_watch_inner(owner_session_id, child_session_id, false)
            .await
    }

    async fn arm_automatic_child_watch_inner(
        &self,
        owner_session_id: Uuid,
        child_session_id: Uuid,
        reset_existing: bool,
    ) -> Result<ArmWatchOutcome> {
        use crate::session::harness::tools::schedule_wake::{
            ScheduleWakeRequest, build_agent_scheduled_job,
        };
        let owner = self
            .get_session(owner_session_id)
            .await
            .ok_or(DaemonError::SessionNotFound(owner_session_id))?;
        let candidate = build_agent_scheduled_job(ScheduleWakeRequest {
            message: format!("automatic child terminal watch {child_session_id}"),
            in_seconds: None,
            at: None,
            name: Some(format!("agent-child-{child_session_id}")),
            every_seconds: None,
            mode: Some("on_terminal".into()),
            working_dir: owner.working_dir,
            provider: Some(owner.provider),
            model: owner.model,
            project_id: owner.project_id,
            origin_session_id: Some(owner_session_id),
            watch_session_id: Some(child_session_id),
        })
        .map_err(DaemonError::InvalidParam)?;
        let outcome = self
            .arm_terminal_watch_inner(owner_session_id, candidate, reset_existing)
            .await?;
        let disposition = match &outcome {
            ArmWatchOutcome::Armed(_) => "armed",
            ArmWatchOutcome::Deduplicated(_) => "deduplicated",
        };
        tracing::info!(
            target: "agent_coordination",
            owner_session_id = %owner_session_id,
            child_session_id = %child_session_id,
            disposition,
            "automatic agent child terminal watch"
        );
        Ok(outcome)
    }

    /// Finite startup reconciliation over indexed launched spawn requests.
    pub async fn reconcile_automatic_child_watches(&self) -> Result<()> {
        let requests = self
            .store
            .lock()
            .await
            .list_agent_spawn_requests_for_watch_repair()?;
        for request in requests {
            match self
                .arm_automatic_child_watch(request.owner_session_id, request.child_session_id)
                .await
            {
                Ok(ArmWatchOutcome::Armed(_)) => tracing::info!(
                    target: "agent_coordination",
                    spawn_request_id = %request.spawn_request_id,
                    "automatic terminal watch repaired"
                ),
                Ok(ArmWatchOutcome::Deduplicated(_)) => {}
                Err(error) => tracing::warn!(
                    target: "agent_coordination",
                    spawn_request_id = %request.spawn_request_id,
                    error = %error,
                    "automatic terminal watch reconciliation deferred"
                ),
            }
        }
        Ok(())
    }

    pub async fn reconcile_incomplete_agent_spawns(&self) -> Result<usize> {
        self.spawn_coordinator
            .reconcile_incomplete(&self.active, &self.completed, &self.store)
            .await
    }

    /// Shared scoping check for `AgentGetStatus` / `AgentHalt`. Admits: the
    /// caller's own session, a direct child of the caller, or a child of an
    /// Epic the caller leads. Returns the resolved target `Session` on
    /// success.
    async fn authorize_agent_target(
        &self,
        caller_session_id: Uuid,
        target_session_id: Uuid,
    ) -> Result<Session> {
        Ok(self
            .authorize_agent_target_impl(caller_session_id, target_session_id, false)
            .await?
            .0)
    }

    async fn authorize_agent_mutation_target(
        &self,
        caller_session_id: Uuid,
        target_session_id: Uuid,
    ) -> Result<(Session, Option<ManagerSessionScope>)> {
        self.authorize_agent_target_impl(caller_session_id, target_session_id, true)
            .await
    }

    async fn authorize_agent_target_impl(
        &self,
        caller_session_id: Uuid,
        target_session_id: Uuid,
        mutation: bool,
    ) -> Result<(Session, Option<ManagerSessionScope>)> {
        let target = self
            .get_session(target_session_id)
            .await
            .ok_or(DaemonError::SessionNotFound(target_session_id))?;

        if target_session_id == caller_session_id {
            return Ok((target, None));
        }
        if target.parent_id == Some(caller_session_id) {
            return Ok((target, None));
        }
        // `AgentSpawnChild` parents new children under the Epic, not under
        // the spawning lead (spawn_coordinator sets `parent_id: epic_id`),
        // so the lead and its freshly-spawned child are Epic siblings, not
        // parent/child. Without this, a lead could never AgentGetStatus/
        // AgentHalt anything it just spawned via AgentSpawnChild. Admit the
        // target only when SQLite names the caller as its parent's lead. This
        // preserves the historical status/halt scope (which does not require
        // the parent row to be exactly Epic); message send remains narrower.
        // A committed successor baton updates that authority before runtime
        // projection publication, so map-first authorization would let the
        // stale predecessor retain a second lead plane.
        if self
            .store
            .lock()
            .await
            .parent_lead_authorizes_child(caller_session_id, target_session_id)?
        {
            return Ok((target, None));
        }

        let manager_scope = {
            let store = self.store.lock().await;
            store.manager_session_control_scope(caller_session_id, target_session_id, mutation)?
        };
        if let Some(scope) = manager_scope {
            return Ok((target, Some(scope)));
        }

        Err(DaemonError::InvalidParam(format!(
            "agent_verb_scope_denied: session {caller_session_id} may only target itself, a direct child, or a child of an Epic it leads; {target_session_id} is none of these"
        )))
    }

    async fn audit_manager_control(
        &self,
        scope: Option<&ManagerSessionScope>,
        verb: &str,
    ) -> Result<()> {
        if let Some(scope) = scope {
            self.store
                .lock()
                .await
                .audit_manager_session_control(scope, verb, "requested")?;
        }
        Ok(())
    }

    /// P2-03 authority for `AgentSendMessage`. Deliberately **narrower** than
    /// [`Self::authorize_agent_target`], and deliberately a separate function
    /// rather than a flag on it, because the two differ on three axes that a
    /// shared body would blur:
    ///
    /// 1. **Self is denied.** `AgentGetStatus`/`AgentHalt` admit the caller's
    ///    own session; a session mailing itself is not delegation, it is a
    ///    self-injection loop with no owner to settle it.
    /// 2. **The Epic parent row must be exactly [`SessionKind::Epic`].**
    ///    `authorize_agent_target` admits any parent row carrying a matching
    ///    `lead_session_id`; a non-Epic container or a leaf that happens to
    ///    carry one must never grant a send.
    /// 3. **A target with no `sessions` row is admissible** when the caller's
    ///    own live spawn reservation owns it, so mail may be queued into the
    ///    pre-launch window. A reservation owned by another Session grants
    ///    nothing.
    ///
    /// Authority is resolved against the immutable logical root exactly as the
    /// caller named it. No rotation tip is resolved or written here: the
    /// delivery tip belongs to one attempt (P2-04), never to acceptance.
    async fn authorize_agent_message_target(
        &self,
        caller_session_id: Uuid,
        target_session_id: Uuid,
    ) -> Result<AgentMessageTargetAuthority> {
        use rsi_common::agent_coordination::AgentMessageErrorCodeV1;

        let denied = || {
            crate::error::agent_message_error(
                AgentMessageErrorCodeV1::TargetNotAuthorized,
                Some(format!("{caller_session_id}->{target_session_id}")),
                None,
                None,
                None,
            )
        };

        // Self-send is refused before any Store read, so a caller cannot even
        // probe its own row through this verb.
        if target_session_id == caller_session_id {
            return Err(denied());
        }

        if let Some(target) = self.get_session(target_session_id).await {
            // A direct child of the caller.
            if target.parent_id == Some(caller_session_id) {
                return Ok(AgentMessageTargetAuthority::LiveSession);
            }
            // A child of an Epic the caller leads. `AgentSpawnChild` parents
            // new children under the Epic, so a lead and the child it just
            // spawned are Epic siblings rather than parent/child.
            if self
                .store
                .lock()
                .await
                .epic_lead_authorizes_child(caller_session_id, target_session_id)?
            {
                return Ok(AgentMessageTargetAuthority::LiveSession);
            }
            let manager_scope = {
                let store = self.store.lock().await;
                store.manager_session_control_scope(caller_session_id, target_session_id, true)?
            };
            if let Some(scope) = manager_scope {
                return Ok(AgentMessageTargetAuthority::Manager(scope.epic_id));
            }
            return Err(denied());
        }

        // No Session row: the only remaining authority is the caller's own
        // live reservation for exactly this child id.
        let reservation = self
            .store
            .lock()
            .await
            .find_agent_spawn_request_by_child(target_session_id)?;
        let Some(reservation) = reservation else {
            return Err(crate::error::agent_message_error(
                AgentMessageErrorCodeV1::TargetUnknown,
                Some(target_session_id.to_string()),
                None,
                None,
                None,
            ));
        };
        if reservation.owner_session_id != caller_session_id {
            return Err(denied());
        }
        // A permanently failed reservation will never become a Session, and
        // the V81 `agent_messages_v81_target_custody` trigger refuses it as a
        // target. Refuse it here with a typed class rather than letting the
        // insert abort with a raw SQL message.
        if reservation.state == rsi_common::agent_coordination::AgentSpawnStateV1::Failed {
            return Err(crate::error::agent_message_error(
                AgentMessageErrorCodeV1::TargetUnknown,
                Some(format!("{target_session_id}:reservation_failed")),
                None,
                None,
                None,
            ));
        }
        Ok(AgentMessageTargetAuthority::Reserved(
            reservation.spawn_request_id,
        ))
    }

    /// `AgentSendMessage` — accept one durable owner→child message (P2-03).
    ///
    /// Sender identity is the token-resolved caller and is never a request
    /// field. Authority is proved BEFORE the payload reaches SQLite, so an
    /// unauthorized send persists nothing at all. Acceptance itself is one
    /// `BEGIN IMMEDIATE` Store transaction; no provider dispatch happens here
    /// or anywhere inside it.
    pub async fn agent_send_message(
        &self,
        caller_session_id: Uuid,
        request: AgentSendMessageRequestV1,
    ) -> Result<AgentSendMessageResultV1> {
        request
            .validate()
            .map_err(|class| DaemonError::InvalidParam(class.to_string()))?;
        let authority = self
            .authorize_agent_message_target(caller_session_id, request.target_session_id)
            .await?;
        let outcome = self.store.lock().await.accept_authorized_agent_message(
            caller_session_id,
            authority.spawn_request_id(),
            authority.manager_epic_id(),
            &request,
        )?;
        let receipt = outcome.receipt().clone();
        tracing::info!(
            target: "agent_coordination",
            caller_session_id = %caller_session_id,
            target_session_id = %receipt.target_session_id,
            message_id = %receipt.message_id,
            deduplicated = receipt.deduplicated,
            state_version = receipt.state_version,
            "agent message accepted"
        );
        // Cursor-bearing state events are published ONLY after the Store
        // transaction commits (P2-03). A replay republishes the same durable
        // fact rather than inventing a second one, so a client that lost the
        // first event still converges.
        self.publish_agent_message_state(caller_session_id, &receipt);
        Ok(receipt)
    }

    /// Project-manager methods keep their own action scope. In particular they
    /// grant no access to the existing halt, continue, spawn, or Issue controls.
    pub async fn agent_manager_progress(
        &self,
        caller: Uuid,
        request: rsi_common::harness_manager::AgentManagerProgressRequestV1,
    ) -> Result<rsi_common::harness_manager::AgentManagerProgressResultV1> {
        self.store
            .lock()
            .await
            .manager_progress_page(caller, &request)
    }

    pub async fn agent_manager_inbox(
        &self,
        caller: Uuid,
        request: rsi_common::harness_manager::AgentManagerInboxRequestV1,
    ) -> Result<rsi_common::harness_manager::AgentManagerInboxResultV1> {
        self.store.lock().await.manager_inbox(caller, &request)
    }

    /// Issue #548: read-only work/ownership projection for a session the
    /// current manager created. No bus event, wake, notice or continuation.
    ///
    /// # Errors
    ///
    /// Typed store refusals such as `manager_work_view_not_managed`.
    pub async fn agent_manager_work_view(
        &self,
        caller: Uuid,
        request: rsi_common::harness_manager::AgentManagerWorkViewRequestV1,
    ) -> Result<rsi_common::harness_manager::AgentManagerWorkViewResultV1> {
        self.store.lock().await.manager_work_view(caller, &request)
    }

    pub async fn agent_manager_send(
        &self,
        caller: Uuid,
        request: rsi_common::harness_manager::AgentManagerSendRequestV1,
    ) -> Result<rsi_common::harness_manager::HarnessManagerMessageReceiptV1> {
        let (receipt, job_id) = {
            let store = self.store.lock().await;
            let receipt = store.manager_send(caller, &request)?;
            let job_id = store.manager_notice_job_for_subject(
                "message",
                &receipt.message_id.to_string(),
                &receipt.sequence.to_string(),
            )?;
            (receipt, job_id)
        };
        if let Some(job_id) = job_id {
            self.event_bus
                .publish(crate::bus::DaemonEvent::ManagerNoticeQueued { job_id });
        }
        Ok(receipt)
    }

    pub async fn agent_manager_reply(
        &self,
        caller: Uuid,
        request: rsi_common::harness_manager::AgentManagerReplyRequestV1,
    ) -> Result<rsi_common::harness_manager::HarnessManagerMessageReceiptV1> {
        let (receipt, job_id) = {
            let store = self.store.lock().await;
            let receipt = store.manager_reply(caller, &request)?;
            let job_id = store.manager_notice_job_for_subject(
                "message",
                &receipt.message_id.to_string(),
                &receipt.sequence.to_string(),
            )?;
            (receipt, job_id)
        };
        if let Some(job_id) = job_id {
            self.event_bus
                .publish(crate::bus::DaemonEvent::ManagerNoticeQueued { job_id });
        }
        Ok(receipt)
    }

    /// Queue a lead notice and publish its durable manager wake job.
    ///
    /// # Errors
    /// Returns a stable manager refusal or persistence error.
    pub async fn agent_manager_notify(
        &self,
        caller: Uuid,
        request: rsi_common::harness_manager::AgentManagerNotifyRequestV1,
    ) -> Result<rsi_common::harness_manager::HarnessManagerMessageReceiptV1> {
        let (receipt, job_id) = {
            let store = self.store.lock().await;
            let receipt =
                store.manager_lead_notice(caller, &request.message, &request.idempotency_key)?;
            let job_id = store.manager_notice_job_for_subject(
                "message",
                &receipt.message_id.to_string(),
                &receipt.sequence.to_string(),
            )?;
            drop(store);
            (receipt, job_id)
        };
        if let Some(job_id) = job_id {
            self.event_bus
                .publish(crate::bus::DaemonEvent::ManagerNoticeQueued { job_id });
        }
        Ok(receipt)
    }

    /// Publish the durable post-commit `AgentMessageStateEventV1`. Payloads
    /// are never exposed through this envelope.
    fn publish_agent_message_state(
        &self,
        owner_session_id: Uuid,
        receipt: &AgentSendMessageResultV1,
    ) {
        let event = AgentMessageStateEventV1 {
            message_id: receipt.message_id,
            owner_session_id,
            target_session_id: receipt.target_session_id,
            state: receipt.state,
            state_version: receipt.state_version,
            attempt_count: 0,
            current_attempt_number: None,
            safe_error_class: None,
            observed_at: chrono::Utc::now(),
        };
        self.event_bus
            .publish(crate::bus::DaemonEvent::AgentMessageState { event });
    }
}

/// The resolved P2-03 send authority for one target, and the only thing that
/// decides whether acceptance binds a spawn reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentMessageTargetAuthority {
    /// The target has a durable `sessions` row the caller may message. No
    /// reservation is bound.
    LiveSession,
    /// The target has no `sessions` row yet; the caller's own live spawn
    /// reservation is bound instead.
    Reserved(Uuid),
    Manager(Uuid),
}

impl AgentMessageTargetAuthority {
    /// The reservation to record on the accepted row, if any.
    #[must_use]
    pub(crate) const fn spawn_request_id(self) -> Option<Uuid> {
        match self {
            Self::LiveSession | Self::Manager(_) => None,
            Self::Reserved(spawn_request_id) => Some(spawn_request_id),
        }
    }

    pub(crate) const fn manager_epic_id(self) -> Option<Uuid> {
        match self {
            Self::Manager(epic_id) => Some(epic_id),
            _ => None,
        }
    }
}

/// Test seam for `AgentArchiveChild`: runs a one-shot hook between the
/// refusal-ordering pre-check and the archive transaction, so a test can
/// commit a lead handoff exactly in that window.
#[cfg(test)]
pub(crate) mod archive_child_test_seam {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use uuid::Uuid;

    type Hook = Box<dyn FnOnce(&crate::store::Store) + Send>;

    fn hooks() -> &'static Mutex<HashMap<Uuid, Hook>> {
        static HOOKS: OnceLock<Mutex<HashMap<Uuid, Hook>>> = OnceLock::new();
        HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Install a hook for one archive of `target`.
    pub(crate) fn install(target: Uuid, hook: impl FnOnce(&crate::store::Store) + Send + 'static) {
        hooks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(target, Box::new(hook));
    }

    pub(super) async fn run(target: Uuid, manager: &super::SessionManager) {
        let hook = hooks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&target);
        if let Some(hook) = hook {
            hook(&*manager.store.lock().await);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::too_many_lines)]
mod manager_session_control_tests {
    use super::tests::{control_handle_with_store, test_session};
    use super::*;
    use crate::config::{Config, RuntimeConfig};
    use crate::store::Store;
    use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
    use rsi_common::harness_manager_v2::{
        ConfigureHarnessManagerPolicyRequestV2, ManagerCapabilityV2, ManagerOperatingModeV2,
        ManagerPolicyV2,
    };
    use rsi_common::types::{Project, SessionStatus};
    use tempfile::TempDir;

    #[tokio::test]
    async fn manager_continue_child_keeps_manager_watch_after_full_continuation() {
        let directory = TempDir::new().unwrap();
        let sandbox = TempDir::new().unwrap();
        let session_manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            Store::open(&directory.path().join("rsi.db")).unwrap(),
            false,
            directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            RuntimeConfig::from_config(&Config::from_env()),
            sandbox.path().to_path_buf(),
        )
        .unwrap();
        let project = Uuid::new_v4();
        let manager = Uuid::new_v4();
        let group = Uuid::new_v4();
        let epic = Uuid::new_v4();
        let lead = Uuid::new_v4();
        {
            let store = session_manager.store.lock().await;
            let now = chrono::Utc::now();
            store
                .insert_project(&Project {
                    id: project,
                    name: "Manager continuation".into(),
                    path: None,
                    description: None,
                    color: Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
            let mut row = test_session(manager, directory.path().to_path_buf());
            row.project_id = Some(project);
            row.session_kind = SessionKind::Standard;
            row.status = SessionStatus::Completed;
            store.insert_session(&row).unwrap();
            row.id = group;
            row.session_kind = SessionKind::Group;
            store.insert_session(&row).unwrap();
            row.id = epic;
            row.session_kind = SessionKind::Epic;
            row.parent_id = Some(group);
            row.lead_session_id = Some(lead);
            store.insert_session(&row).unwrap();
            row.id = lead;
            row.session_kind = SessionKind::Task;
            row.parent_id = Some(epic);
            row.lead_session_id = None;
            store.insert_session(&row).unwrap();
            store
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    group_ids: vec![],
                    project_id: project,
                    session_id: manager,
                    epic_ids: Some(vec![epic]),
                    expected_row_version: 0,
                })
                .unwrap();
            store
                .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                    project_id: project,
                    expected_scope_version: 1,
                    expected_policy_version: 0,
                    idempotency_key: "continue-control".into(),
                    policy: ManagerPolicyV2 {
                        mode: ManagerOperatingModeV2::Execute,
                        capabilities: vec![ManagerCapabilityV2::SessionControl],
                        ..Default::default()
                    },
                })
                .unwrap();
        }
        let completed = session_manager
            .store
            .lock()
            .await
            .get_session(lead)
            .unwrap()
            .unwrap();
        session_manager
            .completed
            .write()
            .await
            .insert(lead, CompletedSession::for_test(completed));
        session_manager
            .completed
            .write()
            .await
            .get_mut(&lead)
            .unwrap()
            .session
            .claude_session_id = None;
        session_manager
            .completed
            .write()
            .await
            .get_mut(&lead)
            .unwrap()
            .session
            .query = "/create_handoff".into();
        let failed = Box::pin(session_manager.agent_continue_child(
            manager,
            AgentContinueChildRequestV1 {
                target_session_id: lead,
                query: "continue managed lead".into(),
                expected_tip_session_id: lead,
                expected_event_sequence: 0,
                expected_custody_generation: None,
                idempotency_key: None,
            },
        ))
        .await;
        assert!(failed.is_err(), "missing provider ID must fail the effect");
        let requested_before_effect: i64 = session_manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_v2_events WHERE kind='session_control' AND actor_session_id=?1 AND record_key=?2 AND json_extract(payload_json,'$.phase')='requested'",
                rusqlite::params![manager.to_string(), lead.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(requested_before_effect, 1);
        session_manager
            .completed
            .write()
            .await
            .get_mut(&lead)
            .unwrap()
            .session
            .claude_session_id = Some("provider-session".into());
        session_manager
            .completed
            .write()
            .await
            .get_mut(&lead)
            .unwrap()
            .session
            .query = "pending retry".into();
        crate::session::launch::install_controller_candidate_test_process(lead);

        let receipt = Box::pin(session_manager.agent_continue_child(
            manager,
            AgentContinueChildRequestV1 {
                target_session_id: lead,
                query: "continue managed lead".into(),
                expected_tip_session_id: lead,
                expected_event_sequence: 0,
                expected_custody_generation: None,
                idempotency_key: None,
            },
        ))
        .await
        .unwrap();
        assert_eq!(receipt.target_session_id, lead);
        assert_eq!(receipt.continued_session_id, lead);
        assert!(session_manager.active.read().await.contains_key(&lead));
        {
            let store = session_manager.store.lock().await;
            assert_eq!(
                store
                    .list_scheduled_jobs()
                    .unwrap()
                    .iter()
                    .filter(|job| {
                        job.enabled
                            && job.wake_session_id == Some(manager)
                            && job.wake_mode == WakeMode::OnTerminal(lead)
                    })
                    .count(),
                1
            );
            let events: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM harness_manager_v2_events WHERE kind='session_control' AND actor_session_id=?1 AND record_key=?2",
                    rusqlite::params![manager.to_string(), lead.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(events, 2);
            drop(store);
        }
        drop(session_manager);
        crate::session::launch::drop_controller_candidate_test_stream(lead);
    }

    #[tokio::test]
    async fn manager_fresh_relaunch_replay_keeps_one_audit_and_watch() {
        let directory = TempDir::new().unwrap();
        let sandbox = TempDir::new().unwrap();
        let session_manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            Store::open(&directory.path().join("rsi.db")).unwrap(),
            false,
            directory.path().join("daemon.sock"),
            None,
            Vec::new(),
            RuntimeConfig::from_config(&Config::from_env()),
            sandbox.path().to_path_buf(),
        )
        .unwrap();
        let project = Uuid::new_v4();
        let manager = Uuid::new_v4();
        let group = Uuid::new_v4();
        let epic = Uuid::new_v4();
        let lead = Uuid::new_v4();
        {
            let store = session_manager.store.lock().await;
            let now = chrono::Utc::now();
            store
                .insert_project(&Project {
                    id: project,
                    name: "Manager replay".into(),
                    path: None,
                    description: None,
                    color: Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
            let mut row = test_session(manager, directory.path().to_path_buf());
            row.project_id = Some(project);
            row.session_kind = SessionKind::Standard;
            row.status = SessionStatus::Completed;
            store.insert_session(&row).unwrap();
            row.id = group;
            row.session_kind = SessionKind::Group;
            store.insert_session(&row).unwrap();
            row.id = epic;
            row.session_kind = SessionKind::Epic;
            row.parent_id = Some(group);
            row.lead_session_id = Some(lead);
            store.insert_session(&row).unwrap();
            row.id = lead;
            row.session_kind = SessionKind::Task;
            row.parent_id = Some(epic);
            row.lead_session_id = None;
            row.claude_session_id = None;
            row.query = "Implement the durable manager task".into();
            store.insert_session(&row).unwrap();
            store
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    group_ids: vec![],
                    project_id: project,
                    session_id: manager,
                    epic_ids: Some(vec![epic]),
                    expected_row_version: 0,
                })
                .unwrap();
            store
                .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                    project_id: project,
                    expected_scope_version: 1,
                    expected_policy_version: 0,
                    idempotency_key: "manager-replay-control".into(),
                    policy: ManagerPolicyV2 {
                        mode: ManagerOperatingModeV2::Execute,
                        capabilities: vec![ManagerCapabilityV2::SessionControl],
                        ..Default::default()
                    },
                })
                .unwrap();
        }
        let completed = session_manager
            .store
            .lock()
            .await
            .get_session(lead)
            .unwrap()
            .unwrap();
        session_manager
            .completed
            .write()
            .await
            .insert(lead, CompletedSession::for_test(completed));
        let process = crate::session::launch::install_controller_candidate_test_process(lead);
        let request = AgentContinueChildRequestV1 {
            target_session_id: lead,
            query: "continue managed lead".into(),
            expected_tip_session_id: lead,
            expected_event_sequence: 0,
            expected_custody_generation: None,
            idempotency_key: Some("same-manager-decision".into()),
        };
        let first = Box::pin(session_manager.agent_continue_child(manager, request.clone()))
            .await
            .unwrap();
        let first_store = session_manager.store.lock().await;
        let first_jobs = first_store.list_scheduled_jobs().unwrap();
        let first_watches: Vec<_> = first_jobs
            .iter()
            .filter(|job| {
                job.enabled
                    && job.wake_session_id == Some(manager)
                    && job.wake_mode == WakeMode::OnTerminal(lead)
            })
            .collect();
        assert_eq!(first_watches.len(), 1);
        assert_eq!(
            first_watches
                .iter()
                .filter(|job| first_store.is_harness_manager_watch(job.id).unwrap())
                .count(),
            1,
            "manager continuation keeps its dedicated watch"
        );
        drop(first_store);
        let replay = Box::pin(session_manager.agent_continue_child(manager, request))
            .await
            .unwrap();
        assert_eq!(
            first.relaunch.as_ref().unwrap().request_id,
            replay.relaunch.as_ref().unwrap().request_id
        );
        assert!(replay.relaunch.as_ref().unwrap().deduplicated);
        assert_eq!(
            process
                .productive_start_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let store = session_manager.store.lock().await;
        let requested_events: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_v2_events WHERE kind='session_control' AND actor_session_id=?1 AND record_key=?2 AND json_extract(payload_json,'$.phase')='requested'",
                rusqlite::params![manager.to_string(), lead.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(requested_events, 1);
        let replay_watches: Vec<_> = store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| {
                job.enabled
                    && job.wake_session_id == Some(manager)
                    && job.wake_mode == WakeMode::OnTerminal(lead)
            })
            .collect();
        assert_eq!(replay_watches.len(), 1, "replay must not add a watch");
        assert_eq!(
            replay_watches
                .iter()
                .filter(|job| store.is_harness_manager_watch(job.id).unwrap())
                .count(),
            1
        );
        drop(store);
        drop(session_manager);
        crate::session::launch::drop_controller_candidate_test_stream(lead);
    }

    #[tokio::test]
    async fn current_manager_controls_epic_lead_worker_and_nested_worker_through_agent_verbs() {
        let (control, shared) = control_handle_with_store();
        let project = Uuid::new_v4();
        let manager = Uuid::new_v4();
        let group = Uuid::new_v4();
        let epic = Uuid::new_v4();
        let targets = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        {
            let store = shared.lock().await;
            let now = chrono::Utc::now();
            store
                .insert_project(&Project {
                    id: project,
                    name: "Manager control".into(),
                    path: None,
                    description: None,
                    color: Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
            let mut row = test_session(manager, "/tmp/manager-control".into());
            row.project_id = Some(project);
            row.session_kind = SessionKind::Standard;
            row.status = SessionStatus::Completed;
            store.insert_session(&row).unwrap();
            row.id = group;
            row.session_kind = SessionKind::Group;
            store.insert_session(&row).unwrap();
            row.id = epic;
            row.session_kind = SessionKind::Epic;
            row.parent_id = Some(group);
            store.insert_session(&row).unwrap();
            for (index, target) in targets.iter().enumerate() {
                row.id = *target;
                row.session_kind = SessionKind::Task;
                row.status = SessionStatus::Running;
                row.parent_id = Some(if index == 2 { targets[1] } else { epic });
                store.insert_session(&row).unwrap();
            }
            store
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    group_ids: vec![],
                    project_id: project,
                    session_id: manager,
                    epic_ids: Some(vec![epic]),
                    expected_row_version: 0,
                })
                .unwrap();
        }

        for target in targets {
            assert_eq!(
                control.agent_get_status(manager, target).await.unwrap().id,
                target
            );
            control
                .authorize_watch_target(manager, target)
                .await
                .unwrap();
        }
        assert_eq!(
            control
                .agent_get_progress(manager, &targets)
                .await
                .unwrap()
                .rows
                .len(),
            3
        );
        for target in targets {
            assert!(
                control
                    .agent_halt(manager, target)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("manager_v2_capability_denied")
            );
            assert!(
                control
                    .authorize_continue_child(
                        manager,
                        &AgentContinueChildRequestV1 {
                            target_session_id: target,
                            query: "continue".into(),
                            expected_tip_session_id: target,
                            expected_event_sequence: 0,
                            expected_custody_generation: None,
                            idempotency_key: None,
                        }
                    )
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("manager_v2_capability_denied")
            );
            assert!(
                control
                    .agent_send_message(
                        manager,
                        AgentSendMessageRequestV1 {
                            target_session_id: target,
                            message: "manager mail".into(),
                            idempotency_key: format!("before-grant-{target}"),
                            expires_at: None,
                        }
                    )
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("manager_v2_capability_denied")
            );
        }
        {
            let store = shared.lock().await;
            store
                .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                    project_id: project,
                    expected_scope_version: 1,
                    expected_policy_version: 0,
                    idempotency_key: "session-control".into(),
                    policy: ManagerPolicyV2 {
                        mode: ManagerOperatingModeV2::Execute,
                        capabilities: vec![ManagerCapabilityV2::SessionControl],
                        ..Default::default()
                    },
                })
                .unwrap();
        }
        for target in targets {
            assert_eq!(
                control
                    .authorize_agent_mutation_target(manager, target)
                    .await
                    .unwrap()
                    .0
                    .id,
                target
            );
            control
                .authorize_continue_child(
                    manager,
                    &AgentContinueChildRequestV1 {
                        target_session_id: target,
                        query: "continue".into(),
                        expected_tip_session_id: target,
                        expected_event_sequence: 0,
                        expected_custody_generation: None,
                        idempotency_key: None,
                    },
                )
                .await
                .unwrap();
            assert!(
                control
                    .rearm_child_watch_after_continue(manager, target)
                    .await
            );
            let receipt = control
                .agent_send_message(
                    manager,
                    AgentSendMessageRequestV1 {
                        target_session_id: target,
                        message: "manager mail".into(),
                        idempotency_key: format!("manager-mail-{target}"),
                        expires_at: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(receipt.target_session_id, target);
        }
        {
            let store = shared.lock().await;
            store
                .update_session_status(targets[0], SessionStatus::Completed)
                .unwrap();
        }
        assert!(
            control.agent_halt(manager, targets[1]).await.is_err(),
            "an untracked running target must fail after the requested audit event"
        );
        let failed_halt_audit: i64 = shared
            .lock()
            .await
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_v2_events WHERE kind='session_control' AND actor_session_id=?1 AND record_key=?2 AND json_extract(payload_json,'$.verb')='AgentHalt' AND json_extract(payload_json,'$.phase')='requested'",
                rusqlite::params![manager.to_string(), targets[1].to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(failed_halt_audit, 1);
        control.agent_halt(manager, targets[0]).await.unwrap();
        let events: i64 = {
            let store = shared.lock().await;
            store.conn.query_row(
                "SELECT count(*) FROM harness_manager_v2_events WHERE kind='session_control' AND actor_session_id=?1",
                [manager.to_string()], |row| row.get(0)).unwrap()
        };
        assert_eq!(events, 5);
    }
}

impl SessionManager {
    /// Build an [`AgentControlHandle`] over this manager's collaborator `Arc`s.
    /// Cheap (four `Arc::clone`s). The single constructor used by the RPC verb
    /// delegators below and by the fresh-launch tool-registry wiring so native
    /// tools and RPC callers share one authority path.
    pub fn agent_control(&self) -> AgentControlHandle {
        AgentControlHandle::new(
            Arc::clone(&self.active),
            Arc::clone(&self.completed),
            Arc::clone(&self.store),
            Arc::clone(&self.event_bus),
            Arc::clone(&self.spawn_coordinator),
        )
        .with_custody_runtime(self.custody_execution_runtime())
    }

    /// `AgentCreateIssue` RPC delegator — creator identity is passed only from
    /// the already token-resolved caller at the RPC boundary.
    pub async fn agent_create_issue(
        &self,
        caller_session_id: Uuid,
        params: AgentCreateIssueParams,
    ) -> Result<AgentCreateIssueResult> {
        self.agent_control()
            .agent_create_issue(caller_session_id, params)
            .await
    }

    pub async fn agent_list_issues(
        &self,
        caller_session_id: Uuid,
        request: AgentListIssuesRequestV1,
    ) -> Result<IssuePageV1> {
        self.agent_control()
            .agent_list_issues(caller_session_id, request)
            .await
    }

    pub async fn agent_get_issue(
        &self,
        caller_session_id: Uuid,
        request: AgentGetIssueRequestV1,
    ) -> Result<rsi_common::types::AgentGetIssueResultV1> {
        self.agent_control()
            .agent_get_issue(caller_session_id, request)
            .await
    }

    pub async fn agent_update_issue(
        &self,
        caller_session_id: Uuid,
        request: AgentUpdateIssueRequestV1,
    ) -> Result<AgentIssueMutationResultV1> {
        self.agent_control()
            .agent_update_issue(caller_session_id, request)
            .await
    }

    pub async fn agent_update_issue_status(
        &self,
        caller_session_id: Uuid,
        request: AgentUpdateIssueStatusRequestV1,
    ) -> Result<AgentIssueMutationResultV1> {
        self.agent_control()
            .agent_update_issue_status(caller_session_id, request)
            .await
    }

    pub async fn agent_archive_issue(
        &self,
        caller_session_id: Uuid,
        request: AgentArchiveIssueRequestV1,
    ) -> Result<AgentIssueMutationResultV1> {
        self.agent_control()
            .agent_archive_issue(caller_session_id, request)
            .await
    }

    pub async fn agent_restore_issue(
        &self,
        caller_session_id: Uuid,
        request: AgentRestoreIssueRequestV1,
    ) -> Result<AgentIssueMutationResultV1> {
        self.agent_control()
            .agent_restore_issue(caller_session_id, request)
            .await
    }

    pub async fn agent_list_issue_events(
        &self,
        caller_session_id: Uuid,
        request: IssueEventPageRequestV1,
    ) -> Result<IssueEventPageV1> {
        self.agent_control()
            .agent_list_issue_events(caller_session_id, request)
            .await
    }

    /// `AgentSpawnChild` RPC delegator — see [`AgentControlHandle::agent_spawn_child`].
    pub async fn agent_spawn_child(
        &self,
        caller_session_id: Uuid,
        request: AgentSpawnChildRequestV1,
    ) -> AgentSpawnChildOutcome {
        self.agent_control()
            .agent_spawn_child(caller_session_id, request)
            .await
    }

    pub async fn agent_reserve_successor(
        &self,
        caller_session_id: Uuid,
        request: AgentReserveSuccessorRequestV1,
    ) -> Result<AgentReserveSuccessorResultV1> {
        self.agent_control()
            .agent_reserve_successor(caller_session_id, request)
            .await
    }

    pub async fn agent_get_progress(
        &self,
        caller_session_id: Uuid,
        requested_ids: &[Uuid],
    ) -> Result<AgentGetProgressResultV1> {
        self.agent_control()
            .agent_get_progress(caller_session_id, requested_ids)
            .await
    }

    /// `AgentSendMessage` RPC delegator — see
    /// [`AgentControlHandle::agent_send_message`].
    pub async fn agent_send_message(
        &self,
        caller_session_id: Uuid,
        request: AgentSendMessageRequestV1,
    ) -> Result<AgentSendMessageResultV1> {
        self.agent_control()
            .agent_send_message(caller_session_id, request)
            .await
    }

    /// `AgentGetStatus` RPC delegator — see [`AgentControlHandle::agent_get_status`].
    pub async fn agent_get_status(
        &self,
        caller_session_id: Uuid,
        target_session_id: Uuid,
    ) -> Result<Session> {
        self.agent_control()
            .agent_get_status(caller_session_id, target_session_id)
            .await
    }

    /// `AgentHalt` RPC delegator — see [`AgentControlHandle::agent_halt`].
    pub async fn agent_halt(&self, caller_session_id: Uuid, target_session_id: Uuid) -> Result<()> {
        self.agent_control()
            .agent_halt(caller_session_id, target_session_id)
            .await
    }

    /// `AgentContinueChild` — agent-authorized exact-child continuation.
    ///
    /// This is an AUTHORITY WRAPPER, not a second continuation engine.
    /// Daemon-side exact-child continuation already exists and already owns
    /// terminal gating, the single-flight spawn guard, interrupt-then-respawn,
    /// capacity admission, and custody re-authorization; the only thing
    /// missing was an agent-reachable door to it. Scope, provider, and
    /// staleness policy live in
    /// [`AgentControlHandle::authorize_continue_child`]; everything after the
    /// `continue_session` call below is unchanged daemon behavior.
    pub async fn agent_continue_child(
        &self,
        caller_session_id: Uuid,
        request: AgentContinueChildRequestV1,
    ) -> Result<AgentContinueChildResultV1> {
        use rsi_common::agent_coordination::AgentContinueErrorCodeV1;

        use crate::store::manager_actions::fence::{
            CONTINUATION_ACTOR_AUTHORITY_CHANGED, CONTINUATION_LEAD_GENERATION_CHANGED,
            ContinuationAuthorityV1, continuation_fence_code,
        };

        let control = self.agent_control();
        let authority = ContinuationAuthorityV1::AgentChild {
            caller: caller_session_id,
        };
        precheck_continue_child_request(caller_session_id, &request)?;
        // Review round 2 (`agent_continue_lead_authority_race`): bind the
        // Epic lead generation BEFORE authorization. The effect is fenced at
        // this generation and the caller's scope is rechecked under the tip's
        // guard, so a lead replaced after authorization cannot continue W.
        let bound = self
            .store
            .lock()
            .await
            .capture_continuation_fence(request.target_session_id, authority)?;
        let observed = control
            .authorize_continue_child(caller_session_id, &request)
            .await?;
        let manager_scope = control
            .authorize_agent_mutation_target(caller_session_id, observed.tip_session_id)
            .await?
            .1;
        let manager_controlled = manager_scope.is_some();
        #[cfg(test)]
        super::lifecycle::pause_continuation_seam_for_test(
            super::lifecycle::ContinuationPauseSeam::AgentContinueAuthorized,
            request.target_session_id,
        )
        .await;

        // A dirty sandbox is NOT a refusal. A wedged child holding uncommitted
        // work is the single most common reason a master needs this verb at
        // all; refusing it would reject the main recovery case and strand the
        // work in a reclaimable worktree (issues #31, #33). Dirty work is work.
        //
        // The continuation targets the resolved lineage tip, which is the row
        // that actually carries the provider, while the receipt continues to
        // report the immutable logical target the caller named.
        // K2 binds the observed tip and lead generation to the guarded effect.
        // The relaunch intent below supplies the separate durable replay path
        // when the child has no captured provider session id.
        let mut fence = self
            .capture_exact_continuation_fence(observed.tip_session_id, authority)
            .await
            .map_err(|error| match continuation_fence_code(&error) {
                Some(code) => crate::error::agent_continue_error(
                    AgentContinueErrorCodeV1::ContinuationFailed,
                    Some(code.to_string()),
                    None,
                ),
                None => crate::error::agent_continue_error(
                    AgentContinueErrorCodeV1::ContinuationFailed,
                    Some(error.to_string()),
                    None,
                ),
            })?;
        match bound {
            Some(bound) if bound.epic == fence.epic => {
                fence.lead_generation = bound.lead_generation;
            }
            _ => {
                return Err(crate::error::agent_continue_error(
                    AgentContinueErrorCodeV1::ContinuationFailed,
                    Some(CONTINUATION_LEAD_GENERATION_CHANGED.into()),
                    None,
                ));
            }
        }
        let (key_digest, request_fingerprint, request_id) =
            child_relaunch_identity(caller_session_id, &request)?;
        let row = RelaunchIntentRow {
            request_id,
            key_digest,
            request_fingerprint,
            caller_session_id,
            target_session_id: request.target_session_id,
            tip_session_id: observed.tip_session_id,
            observed_event_sequence: request.expected_event_sequence,
            observed_custody_generation: request.expected_custody_generation,
            dedup_key: format!("agent.child_relaunch.v1:{request_id}"),
            state: RelaunchState::Intent,
            invocation_id: None,
            receipt_json: None,
            abandon_reason: None,
        };
        let fresh = Box::pin(self.continue_agent_child(
            observed.tip_session_id,
            request.query.clone(),
            row,
            observed,
            fence,
            manager_scope,
        ))
        .await
        .map_err(|error| {
            if matches!(error, DaemonError::StructuredRpc { .. }) {
                return error;
            }
            match continuation_fence_code(&error) {
                Some(CONTINUATION_ACTOR_AUTHORITY_CHANGED) => crate::error::agent_continue_error(
                    AgentContinueErrorCodeV1::TargetNotAuthorized,
                    Some(format!(
                        "{CONTINUATION_ACTOR_AUTHORITY_CHANGED}:{caller_session_id}->{}",
                        observed.tip_session_id
                    )),
                    None,
                ),
                Some(code) => crate::error::agent_continue_error(
                    AgentContinueErrorCodeV1::ContinuationFailed,
                    Some(code.to_string()),
                    None,
                ),
                None => crate::error::agent_continue_error(
                    AgentContinueErrorCodeV1::ContinuationFailed,
                    Some(error.to_string()),
                    None,
                ),
            }
        })?;
        if let Some(receipt) = fresh {
            return Ok(receipt);
        }
        // The appointed manager already owns a dedicated to_manager watch.
        // An automatic child watch here would publish a second terminal wake
        // for a session the manager did not spawn.
        let watch_rearmed = if manager_controlled {
            false
        } else {
            control
                .rearm_child_watch_after_continue(caller_session_id, request.target_session_id)
                .await
        };

        Ok(AgentContinueChildResultV1 {
            target_session_id: request.target_session_id,
            continued_session_id: observed.tip_session_id,
            observed,
            watch_rearmed,
            relaunch: None,
        })
    }

    /// `AgentArchiveChild`: logically archive a terminal child the caller
    /// owns (#670 R2 design (b)).
    ///
    /// Callable from the RPC dispatch; caller identity must already be
    /// token-resolved. Nothing outside the store transaction returns success:
    /// the pre-check below only orders refusals, and the dedup replay is
    /// decided after authority inside the transaction. This never calls
    /// `archive_session` or `try_archive_cleanup`, so the sandbox is retained.
    ///
    /// # Errors
    ///
    /// Returns a typed `AgentArchiveChild` structured refusal (see
    /// `AgentArchiveErrorCodeV1`); a non-refusal store failure is reported as
    /// `archive_failed`.
    pub async fn agent_archive_child(
        &self,
        caller_session_id: Uuid,
        request: AgentArchiveChildRequestV1,
    ) -> Result<AgentArchiveChildResultV1> {
        use rsi_common::agent_coordination::{
            AgentArchiveErrorCodeV1 as Code, AgentArchiveRefusalDetailV1 as Detail,
        };

        request
            .validate()
            .map_err(crate::error::agent_archive_invalid_request)?;
        if request.target_session_id == caller_session_id {
            return Err(crate::error::agent_archive_error(
                Code::SelfArchiveDenied,
                None,
                None,
            ));
        }
        let lineage = self
            .store
            .lock()
            .await
            .agent_archive_child_precheck(caller_session_id, request.target_session_id)?;
        #[cfg(test)]
        archive_child_test_seam::run(request.target_session_id, self).await;

        let tip = *lineage.last().unwrap_or(&request.target_session_id);
        let _spawn_guard = super::spawn_single_flight::acquire_spawn_guard(tip).await;
        {
            let active = self.active.read().await;
            if lineage.iter().any(|id| active.contains_key(id)) {
                return Err(crate::error::agent_archive_error(
                    Code::LiveContinuation,
                    Some(Detail::Active),
                    None,
                ));
            }
        }
        {
            let completed = self.completed.read().await;
            let retry_live = lineage
                .iter()
                .filter_map(|id| completed.get(id))
                .any(|entry| {
                    entry.retry_cancel.is_some()
                        || entry.retry_fired_at.is_some()
                        || entry.superseded_by_retry.is_some()
                });
            if retry_live {
                return Err(crate::error::agent_archive_error(
                    Code::LiveContinuation,
                    Some(Detail::RetryTimer),
                    None,
                ));
            }
        }
        let result = self
            .store
            .lock()
            .await
            .agent_archive_child_tx(caller_session_id, &request)
            .map_err(|error| {
                if matches!(error, DaemonError::StructuredRpc { .. }) {
                    return error;
                }
                tracing::warn!(
                    target: "agent_coordination",
                    caller_session_id = %caller_session_id,
                    target_session_id = %request.target_session_id,
                    %error,
                    "AgentArchiveChild transaction failed"
                );
                crate::error::agent_archive_error(Code::ArchiveFailed, None, None)
            })?;
        self.finish_agent_archive(&result.archived_session_ids)
            .await;
        Ok(result)
    }

    /// A8 watch-target authz delegator — see
    /// [`AgentControlHandle::authorize_watch_target`].
    pub(crate) async fn authorize_watch_target(
        &self,
        caller_session_id: Uuid,
        watched_session_id: Uuid,
    ) -> Result<()> {
        self.agent_control()
            .authorize_watch_target(caller_session_id, watched_session_id)
            .await
    }

    pub async fn reconcile_automatic_child_watches(&self) -> Result<()> {
        self.agent_control()
            .reconcile_automatic_child_watches()
            .await
    }

    pub async fn reconcile_incomplete_agent_spawns(&self) -> Result<usize> {
        self.agent_control()
            .reconcile_incomplete_agent_spawns()
            .await
    }
}

/// Fixed C5 UUIDv5 namespace. It is UUIDv5(URL,
/// "https://github.com/jakedevar/rsi/local-issue-tracker/c5").
fn c5_issue_namespace() -> Uuid {
    Uuid::from_u128(0x5da34881ecc954fba7da3913bc978130)
}

fn deterministic_agent_issue_id(caller_session_id: Uuid, idempotency_key: &[u8]) -> Uuid {
    let mut name = Vec::with_capacity(16 + 36 + idempotency_key.len());
    name.extend_from_slice(b"agent-create\0");
    name.extend_from_slice(caller_session_id.to_string().as_bytes());
    name.push(0);
    name.extend_from_slice(idempotency_key);
    Uuid::new_v5(&c5_issue_namespace(), &name)
}

fn normalize_master_no_idle_reason(reason: &str) -> String {
    reason
        .split_whitespace()
        .map(|word| {
            let candidate = word
                .trim_matches(|character: char| !character.is_ascii_hexdigit() && character != '-');
            if Uuid::parse_str(candidate).is_ok() {
                word.replace(candidate, "<uuid>")
            } else {
                word.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn master_no_idle_wake_namespace() -> Uuid {
    Uuid::from_u128(0x7e82fcb79748527b855ba7a7d8165411)
}

fn deterministic_master_no_idle_wake_id(session_id: Uuid, terminal_sequence: i32) -> Uuid {
    Uuid::new_v5(
        &master_no_idle_wake_namespace(),
        format!("{session_id}\0{terminal_sequence}").as_bytes(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MasterProgramGuardState {
    Absent,
    Valid,
    Malformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProgramRegistrationEvidence {
    EnabledSentinel,
    ClosedCapacityTerminalReplay,
    ClosedSentinel,
    Absent,
    Malformed,
}

fn program_registration_evidence(
    store: &crate::store::Store,
    session_id: Uuid,
    program_guard_id: Uuid,
    capacity: Option<&crate::store::capacity_recovery::CapacityAttemptContext>,
) -> Result<ProgramRegistrationEvidence> {
    let row = store.get_scheduled_job(&program_guard_id)?;
    match row {
        Some(job)
            if crate::session::harness::tools::schedule_wake::is_program_guard_sentinel(
                &job, session_id,
            ) && !job.enabled
                && capacity.is_some_and(|context| {
                    context.program_guard_job_id == program_guard_id
                        && context.controller_session_id == session_id
                        && context.attempt_state == "program_terminal"
                        && context.incident_state == "closed_terminal"
                }) =>
        {
            Ok(ProgramRegistrationEvidence::ClosedCapacityTerminalReplay)
        }
        Some(job)
            if crate::session::harness::tools::schedule_wake::is_program_guard_sentinel(
                &job, session_id,
            ) && job.enabled =>
        {
            Ok(ProgramRegistrationEvidence::EnabledSentinel)
        }
        Some(job)
            if crate::session::harness::tools::schedule_wake::is_program_guard_sentinel(
                &job, session_id,
            ) && !job.enabled =>
        {
            Ok(ProgramRegistrationEvidence::ClosedSentinel)
        }
        Some(_) => Ok(ProgramRegistrationEvidence::Malformed),
        None if store.scheduled_job_exists(&program_guard_id)? => {
            Ok(ProgramRegistrationEvidence::Malformed)
        }
        None => Ok(ProgramRegistrationEvidence::Absent),
    }
}

fn master_program_guard_state(
    store: &crate::store::Store,
    session_id: Uuid,
    program_guard_id: Uuid,
) -> Result<MasterProgramGuardState> {
    let row = store.get_scheduled_job(&program_guard_id)?;
    match row {
        Some(job)
            if crate::session::harness::tools::schedule_wake::is_program_guard_sentinel(
                &job, session_id,
            ) && job.enabled =>
        {
            Ok(MasterProgramGuardState::Valid)
        }
        Some(_) => Ok(MasterProgramGuardState::Malformed),
        None if store.scheduled_job_exists(&program_guard_id)? => {
            Ok(MasterProgramGuardState::Malformed)
        }
        None => Ok(MasterProgramGuardState::Absent),
    }
}

pub(super) fn exact_master_continuation_guard_present(
    store: &crate::store::Store,
    session_id: Uuid,
    program_guard_id: Uuid,
    intent: &ProgramContinuationIntentV1,
) -> Result<bool> {
    let is_one_shot_resume = |job: &ScheduledJob| {
        job.id != program_guard_id
            && job.enabled
            && job.wake_session_id == Some(session_id)
            && job.wake_mode == WakeMode::Resume
            && matches!(&job.schedule.recurrence, Recurrence::Once)
    };
    let present = match intent {
        ProgramContinuationIntentV1::RequireChildWatch { job_id } => {
            let consumed_at = store.last_provider_output_at(session_id)?;
            store.get_scheduled_job(job_id)?.is_some_and(|job| {
                // The declared watch may still be enabled while its delivered
                // owner turn settles. Provider output after that delivery is
                // proof the watch was consumed; a later scheduler tick will
                // retire it, so it cannot own the next program continuation.
                let already_consumed = job
                    .last_fired_at
                    .zip(consumed_at)
                    .is_some_and(|(delivered, produced)| produced > delivered);
                job.enabled
                    && job.wake_session_id == Some(session_id)
                    && matches!(job.wake_mode, WakeMode::OnTerminal(_))
                    && !already_consumed
            })
        }
        ProgramContinuationIntentV1::RequireResumeWake { job_id } => store
            .get_scheduled_job(job_id)?
            .is_some_and(|job| is_one_shot_resume(&job)),
        // Legacy reports do not carry an exact durable row identity. Recover
        // conservatively instead of scanning enabled historical jobs.
        ProgramContinuationIntentV1::RequireAnyGuard => false,
        ProgramContinuationIntentV1::InvalidProgram(_) => false,
        ProgramContinuationIntentV1::NotProgram | ProgramContinuationIntentV1::TerminalAllowed => {
            true
        }
    };
    Ok(present)
}

fn is_transient_master_no_idle_store_error(error: &DaemonError) -> bool {
    match error {
        DaemonError::Store(message) => {
            message.starts_with("master_no_idle_transient:")
                || message.starts_with("capacity_recovery_transient:")
        }
        DaemonError::Database(rusqlite::Error::SqliteFailure(code, _)) => matches!(
            code.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ),
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::Store;
    use rsi_common::types::{ContextUsageConfidence, SessionProvider, SessionStatus};
    use rusqlite::OptionalExtension;
    use tempfile::TempDir;

    /// Minimal persisted-session fixture. `pub(crate)` so sibling test modules
    /// (e.g. the `schedule_wake` harness-tool tests) reuse one Session literal
    /// instead of duplicating this 70-line struct.
    pub(crate) fn test_session(id: Uuid, working_dir: std::path::PathBuf) -> Session {
        let now = chrono::Utc::now();
        Session {
            context_fill_pct: None,
            id,
            status: SessionStatus::Failed,
            session_kind: SessionKind::Task,
            provider: SessionProvider::Codex,
            context_usage_confidence: ContextUsageConfidence::Missing,
            rotation_depth: 0,
            retry_attempt: Some(0),
            max_retries: Some(2),
            created_at: now,
            updated_at: now,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            query: "pending retry".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            working_dir,
            git_branch: None,
            model: None,
            claude_session_id: Some("provider-session".to_string()),
            project_id: Some(crate::store::d04_test_project_id()),
            continued_from: None,
            parent_id: None,
            lead_session_id: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            tag: String::new(),
            tags: Vec::new(),
            scheduled_job_id: None,
            stop_reason: None,
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            pending_question: None,
            pending_archive: false,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: Some(0),
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    #[tokio::test]
    async fn c5_agent_halt_pending_retry_exhausts_budget_and_suppresses_marker() {
        let active = Arc::new(RwLock::new(HashMap::new()));
        let completed = Arc::new(RwLock::new(HashMap::new()));
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let (spawn_tx, _spawn_rx) = tokio::sync::mpsc::channel(4);
        let coordinator = Arc::new(super::super::spawn_coordinator::SpawnCoordinator::new(
            spawn_tx,
        ));
        let control = AgentControlHandle::new(
            Arc::clone(&active),
            Arc::clone(&completed),
            Arc::clone(&store),
            Arc::new(crate::bus::EventBus::new(16)),
            coordinator,
        );
        let dir = TempDir::new().unwrap();
        let session_id = Uuid::new_v4();
        let session = test_session(session_id, dir.path().to_path_buf());
        store
            .lock()
            .await
            .insert_session(&session)
            .expect("insert session");
        store
            .lock()
            .await
            .update_failed_and_stage_c5_autofile(
                session_id,
                crate::store::daemon_settings::AutofileCause::ProcessDied,
            )
            .expect("stage pending failure marker");

        let (cancel_tx, _cancel_rx) = tokio::sync::oneshot::channel();
        let mut completed_session = CompletedSession::for_test(session);
        completed_session.retry_cancel = Some(cancel_tx);
        completed
            .write()
            .await
            .insert(session_id, completed_session);

        control
            .agent_halt(session_id, session_id)
            .await
            .expect("self AgentHalt should cancel pending retry");

        let row = store
            .lock()
            .await
            .get_session(session_id)
            .expect("load session")
            .expect("session row");
        assert_eq!(row.retry_attempt, Some(2));
        assert_eq!(row.max_retries, Some(2));
        assert!(
            store
                .lock()
                .await
                .get_daemon_setting(&crate::store::daemon_settings::c5_autofile_pending_key(
                    session_id
                ))
                .unwrap()
                .is_none(),
            "successful AgentHalt must durably suppress the pending marker"
        );
    }

    #[tokio::test]
    async fn agent_halt_invalid_nonpending_target_retains_c5_marker() {
        let (control, store) = control_handle_with_store();
        let dir = TempDir::new().unwrap();
        let session_id = Uuid::new_v4();
        let session = test_session(session_id, dir.path().to_path_buf());
        insert_failed_pending(&store, &session).await;
        let pending_key = crate::store::daemon_settings::c5_autofile_pending_key(session_id);

        let error = control
            .agent_halt(session_id, session_id)
            .await
            .expect_err("non-pending halt must retain its existing not-found error");
        assert!(matches!(error, DaemonError::SessionNotFound(id) if id == session_id));
        assert!(
            store
                .lock()
                .await
                .get_daemon_setting(&pending_key)
                .unwrap()
                .is_some(),
            "an invalid AgentHalt must not suppress an eligible failure marker"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_halt_reaps_authorized_terminal_orphan_absent_from_maps() {
        let (control, store, event_bus) = control_handle_with_store_and_bus();
        let mut events = event_bus.subscribe();
        let dir = TempDir::new().unwrap();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, dir.path().to_path_buf());
        session.status = SessionStatus::Interrupted;
        store
            .lock()
            .await
            .insert_session(&session)
            .expect("insert interrupted terminal session");

        let fixture = super::super::reaper::StartupReaperFixture::new();
        let mut orphan = fixture.spawn_runtime_session(session_id);
        let sibling_fixture = super::super::reaper::StartupReaperFixture::new();
        let mut unreadable_sibling =
            sibling_fixture.spawn_unreadable_runtime_session(Uuid::new_v4());
        assert!(
            !fixture
                .proc_root()
                .join(unreadable_sibling.pid().to_string())
                .exists(),
            "outside-root unreadable sibling must not enter the permitted inventory"
        );
        assert!(unreadable_sibling.is_alive("outside-root unreadable sibling starts live"));
        let _permit = fixture
            .scoped_runtime_reap_root(session_id)
            .expect("register exact AgentHalt fixture root");

        control
            .agent_halt(session_id, session_id)
            .await
            .expect("authorized terminal orphan halt succeeds");

        let orphan_pid = orphan.pid();
        orphan.wait_signalled("exact-session terminal provider orphan");
        assert_eq!(
            nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(orphan_pid),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ),
            Err(nix::errno::Errno::ECHILD),
            "AgentHalt must leave no target child for the fixture guard"
        );
        assert!(unreadable_sibling.is_alive("outside-root unreadable sibling survives"));
        assert_eq!(
            store
                .lock()
                .await
                .get_session(session_id)
                .expect("load terminal row")
                .expect("terminal row exists")
                .status,
            SessionStatus::Interrupted
        );
        let warnings = system_messages(&mut events)
            .into_iter()
            .filter(|(level, message)| {
                level == "warn"
                    && message.contains("AgentHalt reaped 1 terminal provider orphan(s)")
                    && message.contains(&session_id.to_string())
            })
            .count();
        assert_eq!(warnings, 1, "AgentHalt publishes exactly one reap warning");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_halt_fixture_root_unreadable_registered_entry_fails_closed_before_signal() {
        let (control, store, event_bus) = control_handle_with_store_and_bus();
        let mut events = event_bus.subscribe();
        let dir = TempDir::new().unwrap();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, dir.path().to_path_buf());
        session.status = SessionStatus::Interrupted;
        store
            .lock()
            .await
            .insert_session(&session)
            .expect("insert interrupted terminal session");

        let fixture = super::super::reaper::StartupReaperFixture::new();
        let mut target = fixture.spawn_runtime_session(session_id);
        let unreadable_id = Uuid::new_v4();
        let mut unreadable = fixture.spawn_unreadable_runtime_session(unreadable_id);
        let _permit = fixture
            .scoped_runtime_reap_root(session_id)
            .expect("register exact AgentHalt fixture root");

        let error = control
            .agent_halt(session_id, session_id)
            .await
            .expect_err("unreadable registered inventory must fail closed");
        assert!(error.to_string().contains("environ read failed"), "{error}");
        assert!(
            error.to_string().contains(&unreadable.pid().to_string()),
            "error must name the unreadable registered pid: {error}"
        );
        assert!(target.is_alive("target survives failed inventory proof"));
        assert!(unreadable.is_alive("unreadable sibling survives failed inventory proof"));
        assert!(
            system_messages(&mut events).is_empty(),
            "no reap warning on failed proof"
        );
        assert_eq!(
            store
                .lock()
                .await
                .get_session(session_id)
                .expect("load terminal row")
                .expect("terminal row exists")
                .status,
            SessionStatus::Interrupted
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_halt_does_not_reap_untracked_nonterminal_process() {
        let (control, store) = control_handle_with_store();
        let dir = TempDir::new().unwrap();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, dir.path().to_path_buf());
        session.status = SessionStatus::Running;
        store
            .lock()
            .await
            .insert_session(&session)
            .expect("insert nonterminal session");

        let fixture = super::super::reaper::StartupReaperFixture::new();
        let mut process = fixture.spawn_runtime_session(session_id);
        let error = control
            .agent_halt(session_id, session_id)
            .await
            .expect_err("nonterminal map miss remains reconciliation-owned");

        assert!(matches!(error, DaemonError::SessionNotFound(id) if id == session_id));
        assert!(
            process.is_alive("nonterminal process remains reconciliation-owned"),
            "AgentHalt must not bypass reconciliation for a nonterminal row"
        );
    }

    #[tokio::test]
    async fn agent_halt_terminal_orphan_proof_failure_is_fail_closed() {
        let (control, store) = control_handle_with_store();
        let dir = TempDir::new().unwrap();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, dir.path().to_path_buf());
        session.status = SessionStatus::Interrupted;
        store
            .lock()
            .await
            .insert_session(&session)
            .expect("insert interrupted terminal session");

        super::super::reaper::fail_runtime_orphan_reap_for_test(session_id);
        let error = control
            .agent_halt(session_id, session_id)
            .await
            .expect_err("unproven terminal ownership must refuse AgentHalt success");

        assert!(
            error
                .to_string()
                .contains("injected runtime orphan reap failure"),
            "checked reaper error must reach the caller: {error}"
        );
        assert_eq!(
            store
                .lock()
                .await
                .get_session(session_id)
                .expect("load session")
                .expect("session row")
                .status,
            SessionStatus::Interrupted
        );
    }

    pub(crate) fn control_handle_with_store() -> (AgentControlHandle, Arc<tokio::sync::Mutex<Store>>)
    {
        let (control, store, _) = control_handle_with_store_and_bus();
        (control, store)
    }

    pub(crate) fn control_handle_with_store_and_bus() -> (
        AgentControlHandle,
        Arc<tokio::sync::Mutex<Store>>,
        Arc<crate::bus::EventBus>,
    ) {
        control_handle_for_store(Store::open_in_memory().expect("in-memory store"))
    }

    fn control_handle_for_store(
        store: Store,
    ) -> (
        AgentControlHandle,
        Arc<tokio::sync::Mutex<Store>>,
        Arc<crate::bus::EventBus>,
    ) {
        let active = Arc::new(RwLock::new(HashMap::new()));
        let completed = Arc::new(RwLock::new(HashMap::new()));
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let (spawn_tx, _spawn_rx) = tokio::sync::mpsc::channel(4);
        let coordinator = Arc::new(super::super::spawn_coordinator::SpawnCoordinator::new(
            spawn_tx,
        ));
        let event_bus = Arc::new(crate::bus::EventBus::new(16));
        let control = AgentControlHandle::new(
            active,
            completed,
            Arc::clone(&store),
            Arc::clone(&event_bus),
            coordinator,
        );
        (control, store, event_bus)
    }

    fn system_messages(
        receiver: &mut tokio::sync::broadcast::Receiver<Arc<crate::bus::DaemonEvent>>,
    ) -> Vec<(String, String)> {
        let mut messages = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            if let crate::bus::DaemonEvent::SystemMessage { level, message } = event.as_ref() {
                messages.push((level.clone(), message.clone()));
            }
        }
        messages
    }

    fn program_outcome(
        next_slice_ready: bool,
        continuation_state: &str,
        continuation_job_id: Option<Uuid>,
        blocker_class: Option<&str>,
    ) -> String {
        let job = continuation_job_id
            .map(|id| format!(",\"continuation_job_id\":\"{id}\""))
            .unwrap_or_default();
        let blocker = blocker_class
            .map(|class| format!(",\"blocker_class\":\"{class}\""))
            .unwrap_or_default();
        format!(
            r#"orchestration_outcome_v1: {{"schema_version":1,"mode":"program","next_slice_ready":{next_slice_ready},"continuation_state":"{continuation_state}"{job}{blocker},"evidence":"focused durable evidence"}}
"#
        )
    }

    async fn register_program_guard(control: &AgentControlHandle, session: &Session) -> Uuid {
        let job = crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(
            crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
                message: "master-orchestrate program guard".into(),
                in_seconds: None,
                at: None,
                name: None,
                every_seconds: None,
                mode: Some("program_guard".into()),
                working_dir: session
                    .sandbox_root
                    .clone()
                    .unwrap_or_else(|| session.working_dir.clone()),
                provider: Some(session.provider),
                model: session.model.clone(),
                project_id: session.project_id,
                origin_session_id: Some(session.id),
                watch_session_id: None,
            },
        )
        .expect("program guard candidate");
        let id = job.id;
        assert!(matches!(
            control
                .register_program_guard(session.id, job)
                .await
                .unwrap(),
            ProgramGuardRegistration::Registered(_) | ProgramGuardRegistration::Deduplicated(_)
        ));
        id
    }

    async fn insert_capacity_invocation(
        store: &Arc<tokio::sync::Mutex<Store>>,
        session_id: Uuid,
    ) -> Uuid {
        let invocation = Uuid::new_v4();
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .lock()
            .await
            .conn
            .execute(
                "INSERT INTO model_invocations(
                    id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                    trigger_source,session_id,policy_snapshot_json,usage_confidence,
                    created_at,completed_at
                 ) VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                          'admitted','failed','capacity-no-idle-test',?2,'{}','unavailable',?3,?3)",
                rusqlite::params![invocation.to_string(), session_id.to_string(), now],
            )
            .unwrap();
        invocation
    }

    #[tokio::test]
    async fn no_idle_replay_capacity_uses_invocation_identity_and_never_sequence_fallback() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Failed;
        session.stop_reason = Some("provider_error:codex_usage_limit".into());
        store.lock().await.insert_session(&session).unwrap();
        let guard = register_program_guard(&control, &session).await;

        let missing = control
            .enforce_master_no_idle_for_invocation(session_id, 70, None, "")
            .await
            .expect_err("recognized capacity without invocation identity fails closed");
        assert!(missing.to_string().contains("invocation id missing"));
        assert_eq!(
            store
                .lock()
                .await
                .conn
                .query_row(
                    "SELECT count(*) FROM master_no_idle_capacity_incidents",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );

        let invocation = insert_capacity_invocation(&store, session_id).await;
        let outcome = control
            .enforce_master_no_idle_for_invocation(session_id, 70, Some(invocation), "")
            .await
            .unwrap();
        let MasterNoIdleOutcome::CapacityRecovered { settlement } = outcome else {
            panic!("exact capacity terminal must open the V89 outage");
        };
        let wake_job_id = settlement.wake_job_id;
        assert!(matches!(
            settlement.issue_disposition,
            crate::store::capacity_recovery::CapacityIssueDisposition::Attributed { .. }
        ));
        assert_ne!(
            wake_job_id,
            deterministic_master_no_idle_wake_id(session_id, 70),
            "capacity identity is incident/epoch scoped, never sequence scoped"
        );
        assert!(
            matches!(
                control
                    .enforce_master_no_idle_for_invocation(session_id, 999, Some(invocation), "")
                    .await
                    .unwrap(),
                MasterNoIdleOutcome::CapacityRecovered {
                    settlement: crate::store::capacity_recovery::CapacityFailureSettlement {
                        commit_kind: crate::store::capacity_recovery::CapacityCommitKind::Replay,
                        ..
                    },
                }
            ),
            "terminal replay must not advance even when diagnostic sequence changes"
        );
        let locked = store.lock().await;
        assert_eq!(
            locked
                .conn
                .query_row(
                    "SELECT count(*) FROM master_no_idle_capacity_incidents",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(locked.list_issues(&Default::default()).unwrap().len(), 1);
        assert!(locked.get_scheduled_job(&guard).unwrap().unwrap().enabled);
        assert!(
            locked
                .get_scheduled_job(&wake_job_id)
                .unwrap()
                .unwrap()
                .enabled
        );
    }

    #[tokio::test]
    async fn capacity_terminal_initial_and_exact_replay_close_all_custody_for_both_carriers() {
        for (continuation_state, blocker_class) in [
            ("queue_exhausted", None),
            ("human_gate", Some("production")),
        ] {
            for project_backed in [true, false] {
                for delivery_receipt in [false, true] {
                    let (control, store) = control_handle_with_store();
                    let session_id = Uuid::new_v4();
                    let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
                    session.status = SessionStatus::Failed;
                    session.stop_reason = Some("provider_error:codex_usage_limit".into());
                    if !project_backed {
                        session.project_id = None;
                    }
                    store.lock().await.insert_session(&session).unwrap();
                    let guard = register_program_guard(&control, &session).await;
                    let invocation = if delivery_receipt {
                        let opening_invocation =
                            insert_capacity_invocation(&store, session_id).await;
                        let opening = store
                            .lock()
                            .await
                            .settle_capacity_failure(
                                session_id,
                                session_id,
                                guard,
                                opening_invocation,
                                80,
                                chrono::Utc::now(),
                            )
                            .unwrap();
                        let delivery_invocation =
                            insert_capacity_invocation(&store, session_id).await;
                        let now =
                            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
                        let locked = store.lock().await;
                        locked
                            .conn
                            .execute(
                                "INSERT INTO master_no_idle_capacity_attempts(
                                    incident_id,model_invocation_id,state,resume_target_session_id,
                                    delivery_wake_job_id,delivery_due_slot,delivery_admitted_at,
                                    terminal_sequence,terminal_recorded_at,created_at,updated_at
                                 ) VALUES(?1,?2,'delivery_admitted',?3,?4,?5,?6,NULL,NULL,?6,?6)",
                                rusqlite::params![
                                    opening.incident_id.to_string(),
                                    delivery_invocation.to_string(),
                                    session_id.to_string(),
                                    opening.wake_job_id.to_string(),
                                    opening
                                        .due_slot
                                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true,),
                                    now,
                                ],
                            )
                            .unwrap();
                        locked
                            .confirm_capacity_delivery_launch(
                                delivery_invocation,
                                opening.wake_job_id,
                                opening.due_slot,
                                chrono::Utc::now(),
                            )
                            .unwrap();
                        delivery_invocation
                    } else {
                        insert_capacity_invocation(&store, session_id).await
                    };
                    {
                        let locked = store.lock().await;
                        locked
                            .set_session_model_invocation(session_id, Some(invocation))
                            .unwrap();
                        locked
                            .update_failed_and_stage_c5_autofile(
                                session_id,
                                crate::store::daemon_settings::AutofileCause::ProcessDied,
                            )
                            .unwrap();
                    }
                    let output = program_outcome(false, continuation_state, None, blocker_class);
                    let first = control
                        .enforce_master_no_idle_for_invocation(
                            session_id,
                            81,
                            Some(invocation),
                            &output,
                        )
                        .await
                        .unwrap();
                    let MasterNoIdleOutcome::TerminalAllowed {
                        capacity: Some(first),
                    } = first
                    else {
                        panic!("capacity terminal carrier was not settled");
                    };
                    assert_eq!(
                        first.commit_kind,
                        crate::store::capacity_recovery::CapacityCommitKind::New
                    );
                    assert_eq!(
                        first.c5_resolution,
                        crate::store::capacity_recovery::CapacityC5Resolution::ResolvedExact
                    );
                    assert_eq!(
                        matches!(
                            first.issue_disposition,
                            crate::store::capacity_recovery::CapacityIssueDisposition::Attributed { .. }
                        ),
                        project_backed
                    );

                    let replay = control
                        .enforce_master_no_idle_for_invocation(
                            session_id,
                            999,
                            Some(invocation),
                            &output,
                        )
                        .await
                        .unwrap();
                    let MasterNoIdleOutcome::TerminalAllowed {
                        capacity: Some(replay),
                    } = replay
                    else {
                        panic!("exact closed terminal receipt did not authenticate replay");
                    };
                    assert_eq!(
                        replay.commit_kind,
                        crate::store::capacity_recovery::CapacityCommitKind::Replay
                    );
                    assert_eq!(replay.incident_id, first.incident_id);
                    assert_eq!(replay.wake_job_id, first.wake_job_id);

                    let locked = store.lock().await;
                    assert_eq!(
                        locked
                            .conn
                            .query_row(
                                "SELECT state FROM master_no_idle_capacity_incidents
                                 WHERE incident_id=?1",
                                [first.incident_id.to_string()],
                                |row| row.get::<_, String>(0),
                            )
                            .unwrap(),
                        "closed_terminal"
                    );
                    assert_eq!(
                        locked
                            .conn
                            .query_row(
                                "SELECT state FROM master_no_idle_capacity_attempts
                                 WHERE model_invocation_id=?1",
                                [invocation.to_string()],
                                |row| row.get::<_, String>(0),
                            )
                            .unwrap(),
                        "program_terminal"
                    );
                    assert!(!locked.get_scheduled_job(&guard).unwrap().unwrap().enabled);
                    assert!(
                        !locked
                            .get_scheduled_job(&first.wake_job_id)
                            .unwrap()
                            .unwrap()
                            .enabled
                    );
                    assert!(
                        locked
                            .get_c5_autofile_pending(
                                &crate::store::daemon_settings::c5_autofile_pending_key(session_id)
                            )
                            .unwrap()
                            .is_none()
                    );
                    assert_eq!(
                        locked.list_issues(&Default::default()).unwrap().len(),
                        usize::from(project_backed)
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn ordinary_terminal_invocation_closes_open_capacity_without_claiming_exact_receipt() {
        for program_terminal in [false, true] {
            let (control, store) = control_handle_with_store();
            let session_id = Uuid::new_v4();
            let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
            session.status = SessionStatus::Failed;
            session.stop_reason = Some("provider_error:codex_usage_limit".into());
            store.lock().await.insert_session(&session).unwrap();
            let guard = register_program_guard(&control, &session).await;
            let capacity_invocation = insert_capacity_invocation(&store, session_id).await;
            let opening = control
                .enforce_master_no_idle_for_invocation(
                    session_id,
                    90,
                    Some(capacity_invocation),
                    "",
                )
                .await
                .unwrap();
            let MasterNoIdleOutcome::CapacityRecovered { settlement } = opening else {
                panic!("capacity failure did not open an outage");
            };

            let ordinary_invocation = insert_capacity_invocation(&store, session_id).await;
            {
                let locked = store.lock().await;
                locked
                    .update_session_status(session_id, SessionStatus::Completed)
                    .unwrap();
                locked
                    .set_session_model_invocation(session_id, Some(ordinary_invocation))
                    .unwrap();
                locked
                    .conn
                    .execute(
                        "UPDATE sessions SET stop_reason=NULL WHERE id=?1",
                        [session_id.to_string()],
                    )
                    .unwrap();
            }

            let output = if program_terminal {
                program_outcome(false, "queue_exhausted", None, None)
            } else {
                String::new()
            };
            let outcome = control
                .enforce_master_no_idle_for_invocation(
                    session_id,
                    91,
                    Some(ordinary_invocation),
                    &output,
                )
                .await
                .unwrap();
            if program_terminal {
                assert_eq!(
                    outcome,
                    MasterNoIdleOutcome::TerminalAllowed { capacity: None }
                );
            } else {
                assert!(matches!(
                    outcome,
                    MasterNoIdleOutcome::GenericRecovered { .. }
                ));
            }

            let locked = store.lock().await;
            let incident_state: String = locked
                .conn
                .query_row(
                    "SELECT state FROM master_no_idle_capacity_incidents
                     WHERE incident_id=?1",
                    [settlement.incident_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                incident_state,
                if program_terminal {
                    "closed_terminal"
                } else {
                    "closed_success"
                }
            );
            assert!(
                !locked
                    .get_scheduled_job(&settlement.wake_job_id)
                    .unwrap()
                    .unwrap()
                    .enabled
            );
            assert_eq!(
                locked.get_scheduled_job(&guard).unwrap().unwrap().enabled,
                !program_terminal
            );
            assert_eq!(
                locked
                    .conn
                    .query_row(
                        "SELECT count(*) FROM master_no_idle_capacity_attempts
                         WHERE model_invocation_id=?1",
                        [ordinary_invocation.to_string()],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                0,
                "ordinary invocation must not be rewritten as a capacity receipt"
            );
        }
    }

    #[tokio::test]
    async fn capacity_terminal_reopen_authenticates_both_strict_carriers_without_rearm() {
        for (continuation_state, blocker_class) in [
            ("queue_exhausted", None),
            ("human_gate", Some("production")),
        ] {
            for project_backed in [true, false] {
                let directory = tempfile::tempdir().unwrap();
                let database = directory.path().join("capacity-terminal-reopen.sqlite");
                let (control, store, _) = control_handle_for_store(Store::open(&database).unwrap());
                let session_id = Uuid::new_v4();
                let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
                session.status = SessionStatus::Failed;
                session.stop_reason = Some("provider_error:codex_usage_limit".into());
                if !project_backed {
                    session.project_id = None;
                }
                {
                    let locked = store.lock().await;
                    if project_backed {
                        locked
                            .conn
                            .execute(
                                "INSERT OR IGNORE INTO projects(
                                    id,name,path,description,color,context_files,created_at,updated_at
                                 ) VALUES(?1,'capacity terminal reopen',NULL,NULL,'#89b4fa',NULL,?2,?2)",
                                rusqlite::params![
                                    session.project_id.unwrap().to_string(),
                                    chrono::Utc::now().to_rfc3339_opts(
                                        chrono::SecondsFormat::Nanos,
                                        true,
                                    )
                                ],
                            )
                            .unwrap();
                    }
                    locked.insert_session(&session).unwrap();
                }
                register_program_guard(&control, &session).await;
                let invocation = insert_capacity_invocation(&store, session_id).await;
                store
                    .lock()
                    .await
                    .set_session_model_invocation(session_id, Some(invocation))
                    .unwrap();
                let output = program_outcome(false, continuation_state, None, blocker_class);
                let first = control
                    .enforce_master_no_idle_for_invocation(
                        session_id,
                        82,
                        Some(invocation),
                        &output,
                    )
                    .await
                    .unwrap();
                let MasterNoIdleOutcome::TerminalAllowed {
                    capacity: Some(first),
                } = first
                else {
                    panic!("initial terminal capacity settlement missing");
                };
                drop(control);
                drop(store);

                let (reopened_control, reopened_store, _) =
                    control_handle_for_store(Store::open(&database).unwrap());
                let replay = reopened_control
                    .enforce_master_no_idle_for_invocation(
                        session_id,
                        999,
                        Some(invocation),
                        &output,
                    )
                    .await
                    .unwrap();
                let MasterNoIdleOutcome::TerminalAllowed {
                    capacity: Some(replay),
                } = replay
                else {
                    panic!("reopened terminal capacity receipt missing");
                };
                assert_eq!(
                    replay.commit_kind,
                    crate::store::capacity_recovery::CapacityCommitKind::Replay
                );
                assert_eq!(replay.incident_id, first.incident_id);
                let locked = reopened_store.lock().await;
                assert!(
                    !locked
                        .get_scheduled_job(&first.wake_job_id)
                        .unwrap()
                        .unwrap()
                        .enabled
                );
                assert_eq!(
                    locked.list_issues(&Default::default()).unwrap().len(),
                    usize::from(project_backed)
                );
            }
        }
    }

    #[tokio::test]
    async fn master_no_idle_capacity_retry_owner_precedes_incident_creation() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Failed;
        session.stop_reason = Some("provider_error:codex_usage_limit".into());
        store.lock().await.insert_session(&session).unwrap();
        register_program_guard(&control, &session).await;
        let invocation = insert_capacity_invocation(&store, session_id).await;
        let (cancel, _receiver) = tokio::sync::oneshot::channel();
        let mut completed_session = CompletedSession::for_test(session);
        completed_session.retry_cancel = Some(cancel);
        control
            .completed
            .write()
            .await
            .insert(session_id, completed_session);

        assert_eq!(
            control
                .enforce_master_no_idle_for_invocation(session_id, 71, Some(invocation), "")
                .await
                .unwrap(),
            MasterNoIdleOutcome::OrdinaryGuardPresent
        );
        assert_eq!(
            store
                .lock()
                .await
                .conn
                .query_row(
                    "SELECT count(*) FROM master_no_idle_capacity_incidents",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn master_no_idle_noncapacity_failure_keeps_sequence_keyed_recovery() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Failed;
        session.stop_reason = Some("provider_error:some_other_failure".into());
        store.lock().await.insert_session(&session).unwrap();
        register_program_guard(&control, &session).await;
        let invocation = insert_capacity_invocation(&store, session_id).await;
        let sequence = 72;
        let output = program_outcome(true, "child_watch", Some(Uuid::new_v4()), None);
        assert!(matches!(
            control
                .enforce_master_no_idle_for_invocation(
                    session_id,
                    sequence,
                    Some(invocation),
                    &output,
                )
                .await
                .unwrap(),
            MasterNoIdleOutcome::GenericRecovered { wake_job_id, .. }
                if wake_job_id == deterministic_master_no_idle_wake_id(session_id, sequence)
        ));
        assert_eq!(
            store
                .lock()
                .await
                .conn
                .query_row(
                    "SELECT count(*) FROM master_no_idle_capacity_incidents",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn no_idle_recovery_capacity_retries_one_atomic_store_fault() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Failed;
        session.stop_reason = Some("provider_error:codex_usage_limit".into());
        store.lock().await.insert_session(&session).unwrap();
        register_program_guard(&control, &session).await;
        let invocation = insert_capacity_invocation(&store, session_id).await;
        crate::store::capacity_recovery::test_fail_next_settlement(
            crate::store::capacity_recovery::CapacitySettlementFault::AfterIssue,
        );
        assert!(matches!(
            control
                .enforce_master_no_idle_for_invocation(session_id, 73, Some(invocation), "")
                .await
                .unwrap(),
            MasterNoIdleOutcome::CapacityRecovered { .. }
        ));
        let locked = store.lock().await;
        assert_eq!(
            locked
                .conn
                .query_row(
                    "SELECT count(*) FROM master_no_idle_capacity_incidents",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            locked
                .conn
                .query_row(
                    "SELECT count(*) FROM master_no_idle_capacity_attempts",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(locked.list_issues(&Default::default()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn master_no_idle_capacity_store_error_retains_ordinary_c5_filing_or_projectless_error() {
        for project_backed in [true, false] {
            let (control, store, bus) = control_handle_with_store_and_bus();
            let mut events = bus.subscribe();
            let session_id = Uuid::new_v4();
            let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
            session.status = SessionStatus::Failed;
            session.stop_reason = Some("provider_error:codex_usage_limit".into());
            if !project_backed {
                session.project_id = None;
            }
            store.lock().await.insert_session(&session).unwrap();
            register_program_guard(&control, &session).await;
            let source_invocation = insert_capacity_invocation(&store, session_id).await;
            {
                let locked = store.lock().await;
                locked
                    .set_session_model_invocation(session_id, Some(source_invocation))
                    .unwrap();
                locked
                    .update_failed_and_stage_c5_autofile(
                        session_id,
                        crate::store::daemon_settings::AutofileCause::ProcessDied,
                    )
                    .unwrap();
            }
            let error = control
                .enforce_master_no_idle_for_invocation(session_id, 83, Some(Uuid::new_v4()), "")
                .await
                .expect_err("missing terminal invocation must fail capacity settlement");
            assert!(
                error
                    .to_string()
                    .contains("capacity_terminal_model_invocation_missing_session"),
                "unexpected capacity Store failure: {error}"
            );

            control
                .maybe_autofile_terminal_failure(
                    session_id,
                    crate::store::daemon_settings::RecoveryDisposition::NoRecoverySource,
                )
                .await;
            let locked = store.lock().await;
            assert_eq!(
                locked.list_issues(&Default::default()).unwrap().len(),
                usize::from(project_backed)
            );
            let marker = locked
                .get_c5_autofile_pending(&crate::store::daemon_settings::c5_autofile_pending_key(
                    session_id,
                ))
                .unwrap();
            assert_eq!(marker.is_none(), project_backed);
            drop(locked);
            if !project_backed {
                assert!(system_messages(&mut events).iter().any(|(level, message)| {
                    level == "error" && message.contains("c5_autofile_project_unavailable")
                }));
            }
            bus.unsubscribe();
        }
    }

    #[tokio::test]
    async fn issue_writer_master_no_idle_recovers_with_resume_and_attributed_issue_once() {
        let (control, store, bus) = control_handle_with_store_and_bus();
        let mut receiver = bus.subscribe();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();

        let output = program_outcome(true, "child_watch", Some(Uuid::new_v4()), None);
        let first = control
            .enforce_master_no_idle(session_id, 17, &output)
            .await
            .unwrap();
        let MasterNoIdleOutcome::GenericRecovered {
            wake_job_id,
            disposition,
        } = first
        else {
            panic!("missing durable guard must recover");
        };
        assert!(matches!(
            disposition,
            MasterNoIdleRecoveryDisposition::WakeAndAttributedIssue { .. }
        ));
        let jobs = store.lock().await.list_scheduled_jobs().unwrap();
        let wake = jobs.iter().find(|job| job.id == wake_job_id).unwrap();
        assert!(wake.enabled);
        assert_eq!(wake.wake_session_id, Some(session_id));
        assert_eq!(wake.wake_mode, WakeMode::Resume);
        assert!(
            jobs.iter()
                .all(|job| { !matches!(job.wake_mode, WakeMode::Fresh | WakeMode::AgentFresh) })
        );
        let first_issue = store
            .lock()
            .await
            .list_issues(&Default::default())
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(first_issue.created_by_session_id, Some(session_id));
        let first_history = store
            .lock()
            .await
            .list_issue_events_v1(&rsi_common::types::IssueEventPageRequestV1 {
                issue_id: first_issue.id,
                after_sequence: 0,
                limit: None,
            })
            .unwrap();
        assert_eq!(first_history.events.len(), 1);
        let event = &first_history.events[0];
        assert_eq!(
            event.actor_kind,
            rsi_common::types::IssueActorKindV1::System
        );
        assert_eq!(event.actor_label.as_deref(), Some("rsi:master-no-idle"));
        assert_eq!(event.issue, first_issue);
        assert_eq!(
            event.request.fingerprint().unwrap(),
            event.request_fingerprint
        );
        assert!(matches!(
            &event.request.operation,
            rsi_common::types::IssueSemanticOperationV1::Created { create }
                if create.issue_id == first_issue.id
                    && create.project_id == first_issue.project_id
                    && create.created_by_session_id == Some(session_id)
                    && create.title == first_issue.title
                    && create.body == first_issue.body
                    && create.labels == first_issue.labels
        ));
        assert!(system_messages(&mut receiver).iter().any(|(_, message)| {
            message.contains("Recovered unattended orchestration session")
        }));

        assert_eq!(
            control
                .enforce_master_no_idle(session_id, 17, &output)
                .await
                .unwrap(),
            MasterNoIdleOutcome::OrdinaryGuardPresent
        );
        assert_eq!(store.lock().await.list_scheduled_jobs().unwrap().len(), 1);
        assert_eq!(
            store
                .lock()
                .await
                .list_issues(&Default::default())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .lock()
                .await
                .list_issue_events_v1(&rsi_common::types::IssueEventPageRequestV1 {
                    issue_id: first_issue.id,
                    after_sequence: 0,
                    limit: None,
                })
                .unwrap()
                .events
                .len(),
            1,
            "master-no-idle replay duplicated its Issue event"
        );

        store
            .lock()
            .await
            .update_scheduled_job(
                &wake_job_id,
                &ScheduledJobUpdate {
                    name: None,
                    message: None,
                    schedule: None,
                    enabled: Some(false),
                    next_fire_at: None,
                },
            )
            .unwrap();
        let replay = control
            .enforce_master_no_idle(session_id, 17, &output)
            .await
            .unwrap();
        assert!(matches!(
            replay,
            MasterNoIdleOutcome::GenericRecovered {
                wake_job_id: recovered,
                disposition: MasterNoIdleRecoveryDisposition::WakeAndAttributedIssue { .. },
            } if recovered == wake_job_id
        ));
        let jobs = store.lock().await.list_scheduled_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].enabled);
        assert_eq!(
            store
                .lock()
                .await
                .list_issues(&Default::default())
                .unwrap()
                .len(),
            1
        );

        let second_session_id = Uuid::new_v4();
        let mut second_session = test_session(second_session_id, std::path::PathBuf::from("/tmp"));
        second_session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&second_session).unwrap();
        let second_output = program_outcome(true, "child_watch", Some(Uuid::new_v4()), None);
        let second = control
            .enforce_master_no_idle(second_session_id, 18, &second_output)
            .await
            .unwrap();
        let MasterNoIdleOutcome::GenericRecovered {
            disposition: MasterNoIdleRecoveryDisposition::WakeAndAttributedIssue { issue_id },
            ..
        } = second
        else {
            panic!("repeated cause should retain attributed recovery");
        };
        assert_eq!(issue_id, first_issue.id);
        let locked = store.lock().await;
        assert_eq!(locked.list_issues(&Default::default()).unwrap().len(), 1);
        assert_eq!(locked.list_scheduled_jobs().unwrap().len(), 2);
    }

    #[test]
    fn master_no_idle_reason_normalization_deduplicates_embedded_job_ids() {
        assert_eq!(
            normalize_master_no_idle_reason(
                "declared child_watch job 3a4e2670-7c36-4756-b035-405860449fc6 is not enabled"
            ),
            normalize_master_no_idle_reason(
                "declared child_watch job f2f1e816-a720-4936-ad57-5422b2f88c4e is not enabled"
            )
        );
        assert_ne!(
            normalize_master_no_idle_reason("registered program omitted outcome"),
            normalize_master_no_idle_reason("program guard is closed")
        );
    }

    #[tokio::test]
    async fn issue373_no_idle_quoted_examples_preserve_existing_jobs_and_issues() {
        let (control, store, bus) = control_handle_with_store_and_bus();
        let mut receiver = bus.subscribe();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();

        // Seed real state using a genuine unregistered report, including its
        // deterministic recovery and replay, before testing illustrative text.
        let actual = "Mode: program\nNext slice ready: true\n";
        assert!(matches!(
            control.enforce_master_no_idle(session_id, 373, actual).await.unwrap(),
            MasterNoIdleOutcome::GenericRecovered { wake_job_id, .. }
                if wake_job_id == deterministic_master_no_idle_wake_id(session_id, 373)
        ));
        assert_eq!(
            control
                .enforce_master_no_idle(session_id, 373, actual)
                .await
                .unwrap(),
            MasterNoIdleOutcome::OrdinaryGuardPresent
        );
        let (jobs_before, issues_before) = {
            let store = store.lock().await;
            (
                store.list_scheduled_jobs().unwrap(),
                store.list_issues(&Default::default()).unwrap(),
            )
        };
        assert_eq!(jobs_before.len(), 1);
        assert_eq!(issues_before.len(), 1);
        assert_eq!(system_messages(&mut receiver).len(), 1);

        let strict = program_outcome(true, "child_watch", Some(Uuid::new_v4()), None);
        for (offset, output) in [
            format!("Example for later:\n```text\n{actual}```\nUse when ready."),
            format!("Example for later:\n~~~text\n{strict}~~~\nUse when ready."),
            format!("Unclosed example:\n```text\n{strict}"),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                control
                    .enforce_master_no_idle(session_id, 374 + offset as i32, &output)
                    .await
                    .unwrap(),
                MasterNoIdleOutcome::NotApplicable,
                "{output}"
            );
            let store = store.lock().await;
            assert_eq!(
                serde_json::to_value(store.list_scheduled_jobs().unwrap()).unwrap(),
                serde_json::to_value(&jobs_before).unwrap()
            );
            assert_eq!(
                store.list_issues(&Default::default()).unwrap(),
                issues_before
            );
        }
        assert_eq!(system_messages(&mut receiver), Vec::new());
    }

    #[tokio::test]
    async fn issue373_no_idle_closed_guard_stays_closed_for_examples() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();
        register_program_guard(&control, &session).await;
        let terminal = program_outcome(false, "queue_exhausted", None, None);
        assert_eq!(
            control
                .enforce_master_no_idle(session_id, 373, &terminal)
                .await
                .unwrap(),
            MasterNoIdleOutcome::TerminalAllowed { capacity: None }
        );
        let jobs_before = store.lock().await.list_scheduled_jobs().unwrap();
        assert_eq!(jobs_before.len(), 1);
        assert_eq!(jobs_before[0].enabled, false);
        for output in [
            "Ordinary example:\n```text\nMode: program\nNext slice ready: true\n```\n".to_string(),
            format!("Ordinary example:\n~~~text\n{terminal}~~~\n"),
        ] {
            assert_eq!(
                control
                    .enforce_master_no_idle(session_id, 374, &output)
                    .await
                    .unwrap(),
                MasterNoIdleOutcome::NotApplicable
            );
            let store = store.lock().await;
            assert_eq!(
                serde_json::to_value(store.list_scheduled_jobs().unwrap()).unwrap(),
                serde_json::to_value(&jobs_before).unwrap()
            );
            assert_eq!(store.list_issues(&Default::default()).unwrap(), Vec::new());
        }
        assert!(matches!(
            control.enforce_master_no_idle(session_id, 375, &terminal).await.unwrap(),
            MasterNoIdleOutcome::GenericRecovered { wake_job_id, .. }
                if wake_job_id == deterministic_master_no_idle_wake_id(session_id, 375)
        ));
        assert_eq!(
            control
                .enforce_master_no_idle(session_id, 375, &terminal)
                .await
                .unwrap(),
            MasterNoIdleOutcome::OrdinaryGuardPresent
        );
        let store = store.lock().await;
        assert_eq!(store.list_scheduled_jobs().unwrap().len(), 2);
        assert_eq!(store.list_issues(&Default::default()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn master_no_idle_accepts_real_watch_queue_exhaustion_and_human_gate() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();
        let program_guard_id = register_program_guard(&control, &session).await;
        let watch = watch_candidate(session_id, Uuid::new_v4());
        store.lock().await.insert_scheduled_job(&watch).unwrap();

        assert_eq!(
            control
                .enforce_master_no_idle(
                    session_id,
                    20,
                    &program_outcome(true, "child_watch", Some(watch.id), None),
                )
                .await
                .unwrap(),
            MasterNoIdleOutcome::OrdinaryGuardPresent
        );
        assert_eq!(
            control
                .enforce_master_no_idle(
                    session_id,
                    21,
                    &program_outcome(false, "queue_exhausted", None, None),
                )
                .await
                .unwrap(),
            MasterNoIdleOutcome::TerminalAllowed { capacity: None }
        );
        assert_eq!(
            register_program_guard(&control, &session).await,
            program_guard_id,
            "each independent terminal classification requires live program identity"
        );
        assert_eq!(
            control
                .enforce_master_no_idle(
                    session_id,
                    22,
                    &program_outcome(false, "human_gate", None, Some("production")),
                )
                .await
                .unwrap(),
            MasterNoIdleOutcome::TerminalAllowed { capacity: None }
        );
        assert_eq!(
            store
                .lock()
                .await
                .list_issues(&Default::default())
                .unwrap()
                .len(),
            0
        );
        let guard = store
            .lock()
            .await
            .get_scheduled_job(&program_guard_id)
            .unwrap()
            .expect("program guard row");
        assert!(!guard.enabled, "typed terminal outcome disables sentinel");
    }

    #[tokio::test]
    async fn consumed_child_watch_recovers_program_before_scheduler_retires_row() {
        use rsi_common::types::{ConversationEvent, EventType, Role};

        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();
        let sentinel = register_program_guard(&control, &session).await;
        let watch = watch_candidate(session_id, Uuid::new_v4());
        let delivered_at = chrono::Utc::now() - chrono::Duration::seconds(5);
        {
            let store = store.lock().await;
            store.insert_scheduled_job(&watch).unwrap();
            store
                .update_scheduled_job_fired(&watch.id, &delivered_at, None, true)
                .unwrap();
            store
                .insert_event(&ConversationEvent {
                    id: 0,
                    session_id,
                    sequence: 1,
                    event_type: EventType::Message,
                    role: Some(Role::Assistant),
                    content: "owner accepted prior child completion".into(),
                    tool_name: None,
                    tool_input: None,
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                })
                .unwrap();
        }

        let outcome = control
            .enforce_master_no_idle(
                session_id,
                2,
                &program_outcome(true, "child_watch", Some(watch.id), None),
            )
            .await
            .unwrap();
        let MasterNoIdleOutcome::GenericRecovered { wake_job_id, .. } = outcome else {
            panic!("consumed child watch requires a recovery continuation: {outcome:?}");
        };
        let store = store.lock().await;
        let recovery = store.get_scheduled_job(&wake_job_id).unwrap().unwrap();
        assert!(recovery.enabled);
        assert_eq!(recovery.wake_mode, WakeMode::Resume);
        assert_eq!(recovery.wake_session_id, Some(session_id));
        assert!(store.get_scheduled_job(&sentinel).unwrap().unwrap().enabled);
        assert!(store.get_scheduled_job(&watch.id).unwrap().unwrap().enabled);
    }

    #[tokio::test]
    async fn closed_program_guard_is_silent_rearms_explicitly_and_recovers_program_output_once() {
        for (offset, terminal_state, blocker_class) in [
            (0, "queue_exhausted", None),
            (10, "human_gate", Some("production")),
        ] {
            let (control, store, bus) = control_handle_with_store_and_bus();
            let mut receiver = bus.subscribe();
            let session_id = Uuid::new_v4();
            let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
            session.status = SessionStatus::Completed;
            store.lock().await.insert_session(&session).unwrap();
            let program_guard_id = register_program_guard(&control, &session).await;

            assert_eq!(
                control
                    .enforce_master_no_idle(
                        session_id,
                        100 + offset,
                        &program_outcome(false, terminal_state, None, blocker_class),
                    )
                    .await
                    .unwrap(),
                MasterNoIdleOutcome::TerminalAllowed { capacity: None }
            );
            assert!(
                !store
                    .lock()
                    .await
                    .get_scheduled_job(&program_guard_id)
                    .unwrap()
                    .expect("closed sentinel")
                    .enabled
            );

            assert_eq!(
                control
                    .enforce_master_no_idle(
                        session_id,
                        101 + offset,
                        "ordinary later terminal turn",
                    )
                    .await
                    .unwrap(),
                MasterNoIdleOutcome::NotApplicable
            );
            {
                let store = store.lock().await;
                assert_eq!(store.list_scheduled_jobs().unwrap().len(), 1);
                assert!(store.list_issues(&Default::default()).unwrap().is_empty());
            }
            assert!(
                system_messages(&mut receiver).is_empty(),
                "ordinary closed turn must publish no warning"
            );

            let rearmed = control
                .register_bound_program_guard(session_id)
                .await
                .unwrap();
            assert!(matches!(
                rearmed,
                ProgramGuardRegistration::Deduplicated(ref job)
                    if job.id == program_guard_id && job.enabled
            ));
            assert_eq!(
                control
                    .enforce_master_no_idle(
                        session_id,
                        102 + offset,
                        &program_outcome(false, terminal_state, None, blocker_class),
                    )
                    .await
                    .unwrap(),
                MasterNoIdleOutcome::TerminalAllowed { capacity: None }
            );

            let violation = program_outcome(true, "child_watch", Some(Uuid::new_v4()), None);
            assert!(matches!(
                control
                    .enforce_master_no_idle(session_id, 103 + offset, &violation)
                    .await
                    .unwrap(),
                MasterNoIdleOutcome::GenericRecovered { .. }
            ));
            assert_eq!(
                control
                    .enforce_master_no_idle(session_id, 103 + offset, &violation)
                    .await
                    .unwrap(),
                MasterNoIdleOutcome::OrdinaryGuardPresent
            );
            {
                let store = store.lock().await;
                assert_eq!(store.list_scheduled_jobs().unwrap().len(), 2);
                assert_eq!(store.list_issues(&Default::default()).unwrap().len(), 1);
            }
            let warnings = system_messages(&mut receiver);
            assert_eq!(warnings.len(), 1, "closed program violation warns once");
            assert!(
                warnings[0]
                    .1
                    .contains("program guard is closed and must be explicitly re-registered")
            );
            bus.unsubscribe();
        }
    }

    #[tokio::test]
    async fn closed_program_guard_remains_silent_after_store_reopen() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("closed-program-guard.db");
        let (control, store, _) =
            control_handle_for_store(Store::open(&database).expect("open store"));
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();
        let guard_id = register_program_guard(&control, &session).await;
        assert_eq!(
            control
                .enforce_master_no_idle(
                    session_id,
                    120,
                    &program_outcome(false, "queue_exhausted", None, None),
                )
                .await
                .unwrap(),
            MasterNoIdleOutcome::TerminalAllowed { capacity: None }
        );
        drop(control);
        drop(store);

        let (reopened, store, bus) =
            control_handle_for_store(Store::open(&database).expect("reopen store"));
        let mut receiver = bus.subscribe();
        assert_eq!(
            reopened
                .enforce_master_no_idle(session_id, 121, "ordinary post-restart turn")
                .await
                .unwrap(),
            MasterNoIdleOutcome::NotApplicable
        );
        let store = store.lock().await;
        let guard = store
            .get_scheduled_job(&guard_id)
            .unwrap()
            .expect("closed guard survives restart");
        assert!(!guard.enabled);
        assert_eq!(store.list_scheduled_jobs().unwrap().len(), 1);
        assert!(store.list_issues(&Default::default()).unwrap().is_empty());
        drop(store);
        assert!(system_messages(&mut receiver).is_empty());
    }

    #[tokio::test]
    async fn master_no_idle_requires_the_exact_one_shot_resume_row() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();
        let program_guard_id = register_program_guard(&control, &session).await;

        let mut recurring = watch_candidate(session_id, Uuid::new_v4());
        recurring.id = Uuid::new_v4();
        recurring.wake_mode = WakeMode::Resume;
        recurring.schedule.recurrence = Recurrence::EverySeconds(60);
        store.lock().await.insert_scheduled_job(&recurring).unwrap();

        assert!(matches!(
            control
                .enforce_master_no_idle(
                    session_id,
                    24,
                    &program_outcome(true, "resume_wake", Some(recurring.id), None),
                )
                .await
                .unwrap(),
            MasterNoIdleOutcome::GenericRecovered { .. }
        ));
        let jobs = store.lock().await.list_scheduled_jobs().unwrap();
        assert_eq!(jobs.len(), 3);
        assert!(jobs.iter().any(|job| {
            job.id != recurring.id
                && job.id != program_guard_id
                && job.wake_mode == WakeMode::Resume
                && matches!(&job.schedule.recurrence, Recurrence::Once)
        }));
    }

    #[tokio::test]
    async fn program_sentinel_is_not_a_continuation_but_real_resume_wake_is() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();
        let program_guard_id = register_program_guard(&control, &session).await;

        assert!(matches!(
            control
                .enforce_master_no_idle(
                    session_id,
                    27,
                    &program_outcome(true, "resume_wake", Some(program_guard_id), None),
                )
                .await
                .unwrap(),
            MasterNoIdleOutcome::GenericRecovered { .. }
        ));

        let resume = crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(
            crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
                message: "continue exact program slice".into(),
                in_seconds: Some(60),
                at: None,
                name: Some("exact-program-resume".into()),
                every_seconds: None,
                mode: Some("resume".into()),
                working_dir: session.working_dir.clone(),
                provider: Some(session.provider),
                model: session.model.clone(),
                project_id: session.project_id,
                origin_session_id: Some(session_id),
                watch_session_id: None,
            },
        )
        .unwrap();
        store.lock().await.insert_scheduled_job(&resume).unwrap();
        assert_eq!(
            control
                .enforce_master_no_idle(
                    session_id,
                    28,
                    &program_outcome(true, "resume_wake", Some(resume.id), None),
                )
                .await
                .unwrap(),
            MasterNoIdleOutcome::OrdinaryGuardPresent
        );
    }

    #[tokio::test]
    async fn master_no_idle_failed_session_recovers_only_without_pending_retry() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Failed;
        store.lock().await.insert_session(&session).unwrap();
        assert!(matches!(
            control
                .enforce_master_no_idle(
                    session_id,
                    25,
                    &program_outcome(true, "child_watch", Some(Uuid::new_v4()), None),
                )
                .await
                .unwrap(),
            MasterNoIdleOutcome::GenericRecovered { .. }
        ));

        let (retry_control, retry_store) = control_handle_with_store();
        let retry_session_id = Uuid::new_v4();
        let mut retry_session = test_session(retry_session_id, std::path::PathBuf::from("/tmp"));
        retry_session.status = SessionStatus::Failed;
        retry_store
            .lock()
            .await
            .insert_session(&retry_session)
            .unwrap();
        let (cancel, _receiver) = tokio::sync::oneshot::channel();
        let mut completed = CompletedSession::for_test(retry_session);
        completed.retry_cancel = Some(cancel);
        retry_control
            .completed
            .write()
            .await
            .insert(retry_session_id, completed);
        assert_eq!(
            retry_control
                .enforce_master_no_idle(
                    retry_session_id,
                    26,
                    &program_outcome(true, "child_watch", Some(Uuid::new_v4()), None),
                )
                .await
                .unwrap(),
            MasterNoIdleOutcome::OrdinaryGuardPresent
        );
        assert!(
            retry_store
                .lock()
                .await
                .list_scheduled_jobs()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn master_no_idle_recovers_invalid_program_outcome() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();
        let malformed = program_outcome(true, "queue_exhausted", None, None);

        assert!(matches!(
            control
                .enforce_master_no_idle(session_id, 23, &malformed)
                .await
                .unwrap(),
            MasterNoIdleOutcome::GenericRecovered { .. }
        ));
    }

    #[tokio::test]
    async fn registered_program_fails_closed_for_missing_duplicate_malformed_and_slice_output() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();
        register_program_guard(&control, &session).await;

        let strict_program = program_outcome(true, "child_watch", Some(Uuid::new_v4()), None);
        let duplicate = format!("{strict_program}{strict_program}");
        let malformed = "orchestration_outcome_v1: not-json\n".to_string();
        let slice = "orchestration_outcome_v1: {\"schema_version\":1,\"mode\":\"slice\",\"next_slice_ready\":false,\"continuation_state\":\"queue_exhausted\",\"evidence\":\"slice returned to parent\"}\n".to_string();
        let legacy_example = "```text\nMode: program\nNext slice ready: false\n```\n".to_string();
        for (offset, output) in ["".to_string(), duplicate, malformed, slice, legacy_example]
            .into_iter()
            .enumerate()
        {
            let sequence = 30 + offset as i32;
            assert!(matches!(
                control
                    .enforce_master_no_idle(session_id, sequence, &output)
                    .await
                    .unwrap(),
                MasterNoIdleOutcome::GenericRecovered { .. }
            ));
            assert_eq!(
                control
                    .enforce_master_no_idle(session_id, sequence, &output)
                    .await
                    .unwrap(),
                MasterNoIdleOutcome::OrdinaryGuardPresent,
                "enabled invalid carrier must settle exactly once"
            );
        }
        let store = store.lock().await;
        assert_eq!(store.list_issues(&Default::default()).unwrap().len(), 5);
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM scheduled_jobs", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            6,
            "one sentinel plus five deterministic recoveries"
        );
    }

    #[tokio::test]
    async fn malformed_strict_plus_legacy_program_recovers_but_real_slice_is_not_applicable() {
        let (control, store) = control_handle_with_store();
        let program_id = Uuid::new_v4();
        let mut program = test_session(program_id, std::path::PathBuf::from("/tmp"));
        program.status = SessionStatus::Completed;
        store.lock().await.insert_session(&program).unwrap();
        let mixed = "orchestration_outcome_v1: not-json\nORCHESTRATION COMPLETE\nMode: program\nNext-slice-ready: yes\n";
        assert!(matches!(
            control
                .enforce_master_no_idle(program_id, 40, mixed)
                .await
                .unwrap(),
            MasterNoIdleOutcome::GenericRecovered { .. }
        ));

        let slice_id = Uuid::new_v4();
        let mut slice = test_session(slice_id, std::path::PathBuf::from("/tmp"));
        slice.status = SessionStatus::Completed;
        store.lock().await.insert_session(&slice).unwrap();
        let slice_output = "orchestration_outcome_v1: {\"schema_version\":1,\"mode\":\"slice\",\"next_slice_ready\":false,\"continuation_state\":\"queue_exhausted\",\"evidence\":\"slice returned to parent\"}\n";
        assert_eq!(
            control
                .enforce_master_no_idle(slice_id, 41, slice_output)
                .await
                .unwrap(),
            MasterNoIdleOutcome::NotApplicable
        );
    }

    #[tokio::test]
    async fn no_idle_recovery_retries_atomically_after_wake_fault() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();
        let output = program_outcome(true, "child_watch", Some(Uuid::new_v4()), None);

        crate::store::master_no_idle_test_fail_after_wake(3);
        let error = control
            .enforce_master_no_idle(session_id, 50, &output)
            .await
            .expect_err("three transient failures exhaust the bounded retry");
        assert!(error.to_string().contains("injected_after_wake"));
        {
            let store = store.lock().await;
            assert!(
                store
                    .get_scheduled_job(&deterministic_master_no_idle_wake_id(session_id, 50))
                    .unwrap()
                    .is_none(),
                "fault after wake mutation must roll the wake back"
            );
            assert!(store.list_issues(&Default::default()).unwrap().is_empty());
        }

        crate::store::master_no_idle_test_fail_after_wake(1);
        assert!(matches!(
            control
                .enforce_master_no_idle(session_id, 50, &output)
                .await
                .unwrap(),
            MasterNoIdleOutcome::GenericRecovered {
                disposition: MasterNoIdleRecoveryDisposition::WakeAndAttributedIssue { .. },
                ..
            }
        ));
        let store = store.lock().await;
        assert!(
            store
                .get_scheduled_job(&deterministic_master_no_idle_wake_id(session_id, 50))
                .unwrap()
                .is_some()
        );
        assert_eq!(store.list_issues(&Default::default()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn no_idle_replay_finishes_a_missing_issue_without_duplicating_the_wake() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();
        let output = program_outcome(true, "child_watch", Some(Uuid::new_v4()), None);
        let terminal_sequence = 55;
        let recovery_wake_id = deterministic_master_no_idle_wake_id(session_id, terminal_sequence);
        let mut recovery_wake =
            crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(
                crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
                    message: "interrupted recovery".into(),
                    in_seconds: Some(60),
                    at: None,
                    name: Some("interrupted-recovery".into()),
                    every_seconds: None,
                    mode: Some("resume".into()),
                    working_dir: session.working_dir.clone(),
                    provider: Some(session.provider),
                    model: session.model.clone(),
                    project_id: session.project_id,
                    origin_session_id: Some(session_id),
                    watch_session_id: None,
                },
            )
            .unwrap();
        recovery_wake.id = recovery_wake_id;
        store
            .lock()
            .await
            .insert_scheduled_job(&recovery_wake)
            .unwrap();

        let outcome = control
            .enforce_master_no_idle(session_id, terminal_sequence, &output)
            .await
            .unwrap();
        assert!(
            matches!(
                outcome,
                MasterNoIdleOutcome::GenericRecovered {
                    wake_job_id,
                    disposition: MasterNoIdleRecoveryDisposition::WakeAndAttributedIssue { .. },
                } if wake_job_id == recovery_wake_id
            ),
            "unexpected replay outcome: {outcome:?}"
        );
        let store = store.lock().await;
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM scheduled_jobs", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1,
            "replay repairs the issue side without creating a second wake"
        );
        assert_eq!(store.list_issues(&Default::default()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn projectless_recovery_is_visible_wake_only_and_uses_sandbox_root() {
        let (control, store, bus) = control_handle_with_store_and_bus();
        let mut receiver = bus.subscribe();
        let session_id = Uuid::new_v4();
        let sandbox_root = std::path::PathBuf::from("/tmp/no-idle-sandbox-root");
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp/shared-root"));
        session.status = SessionStatus::Completed;
        session.project_id = None;
        session.sandbox_root = Some(sandbox_root.clone());
        store.lock().await.insert_session(&session).unwrap();

        let outcome = control
            .enforce_master_no_idle(
                session_id,
                60,
                &program_outcome(true, "child_watch", Some(Uuid::new_v4()), None),
            )
            .await
            .unwrap();
        let MasterNoIdleOutcome::GenericRecovered {
            wake_job_id,
            disposition: MasterNoIdleRecoveryDisposition::ProjectlessWakeOnly,
        } = outcome
        else {
            panic!("project-less recovery must return its explicit wake-only disposition");
        };
        let store = store.lock().await;
        let wake = store
            .get_scheduled_job(&wake_job_id)
            .unwrap()
            .expect("recovery wake");
        assert_eq!(wake.working_dir.as_deref(), Some(sandbox_root.as_path()));
        assert!(store.list_issues(&Default::default()).unwrap().is_empty());
        drop(store);
        assert!(
            system_messages(&mut receiver).iter().any(|(_, message)| {
                message.contains("projectless-wake-only:no-attributed-issue")
            })
        );
    }

    #[tokio::test]
    async fn exact_guard_reads_ignore_a_large_disabled_job_corpus() {
        let (control, store) = control_handle_with_store();
        let session_id = Uuid::new_v4();
        let mut session = test_session(session_id, std::path::PathBuf::from("/tmp"));
        session.status = SessionStatus::Completed;
        store.lock().await.insert_session(&session).unwrap();
        register_program_guard(&control, &session).await;

        {
            let store = store.lock().await;
            for index in 0..512 {
                let mut disabled = watch_candidate(session_id, Uuid::new_v4());
                disabled.id = Uuid::new_v5(
                    &Uuid::NAMESPACE_OID,
                    format!("disabled-terminal-job-{index}").as_bytes(),
                );
                disabled.enabled = false;
                store.insert_scheduled_job(&disabled).unwrap();
            }
        }
        let exact = watch_candidate(session_id, Uuid::new_v4());
        store.lock().await.insert_scheduled_job(&exact).unwrap();
        assert_eq!(
            control
                .enforce_master_no_idle(
                    session_id,
                    70,
                    &program_outcome(true, "child_watch", Some(exact.id), None),
                )
                .await
                .unwrap(),
            MasterNoIdleOutcome::OrdinaryGuardPresent
        );
        let store = store.lock().await;
        assert!(store.list_issues(&Default::default()).unwrap().is_empty());
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM scheduled_jobs", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            514
        );

        let source = include_str!("agent_verbs.rs");
        let terminal = source
            .split("pub(crate) async fn enforce_master_no_idle(")
            .nth(1)
            .unwrap()
            .split("/// C5's single post-disposition")
            .next()
            .unwrap();
        assert!(!terminal.contains("list_scheduled_jobs"));
    }

    // -----------------------------------------------------------------------
    // AgentContinueChild — authority, provider, and staleness gate
    // -----------------------------------------------------------------------

    fn continue_request(
        target: Uuid,
        tip: Uuid,
        sequence: i64,
        custody: Option<i64>,
    ) -> AgentContinueChildRequestV1 {
        AgentContinueChildRequestV1 {
            target_session_id: target,
            query: "resume the stage".to_string(),
            expected_tip_session_id: tip,
            expected_event_sequence: sequence,
            expected_custody_generation: custody,
            idempotency_key: None,
        }
    }

    fn continue_error_code(
        error: &DaemonError,
    ) -> rsi_common::agent_coordination::AgentContinueErrorCodeV1 {
        let DaemonError::StructuredRpc { data, .. } = error else {
            panic!("AgentContinueChild refusals must be structured: {error:?}");
        };
        let envelope: rsi_common::agent_coordination::AgentContinueErrorV1 =
            serde_json::from_value(data.clone()).expect("typed continue error envelope");
        envelope.code
    }

    #[test]
    fn relaunch_intent_migration_creates_table_indexes_and_triggers() {
        let store = Store::open_in_memory().expect("migrated store");
        let names: Vec<String> = store
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE name LIKE 'agent_child_relaunch_intents%' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert!(names.contains(&"agent_child_relaunch_intents".to_string()));
        assert!(names.contains(&"agent_child_relaunch_intents_one_open_per_tip".to_string()));
        assert!(names.contains(&"agent_child_relaunch_intents_no_delete".to_string()));
        assert!(names.contains(&"agent_child_relaunch_intents_forward".to_string()));
    }

    async fn fresh_relaunch_fixture() -> (SessionManager, TempDir, Uuid, Uuid) {
        let dir = TempDir::new().unwrap();
        let sandbox = TempDir::new().unwrap();
        let manager = SessionManager::new(
            Arc::new(crate::bus::EventBus::new(16)),
            Store::open(&dir.path().join("rsi.db")).unwrap(),
            false,
            dir.path().join("daemon.sock"),
            None,
            Vec::new(),
            crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env()),
            sandbox.path().to_path_buf(),
        )
        .unwrap();
        let caller = Uuid::new_v4();
        let tip = Uuid::new_v4();
        let mut child = test_session(tip, dir.path().to_path_buf());
        child.parent_id = Some(caller);
        child.claude_session_id = None;
        child.query = "Implement the durable child task".into();
        {
            let store = manager.store.lock().await;
            store
                .insert_session(&test_session(caller, dir.path().to_path_buf()))
                .unwrap();
            store.insert_session(&child).unwrap();
        }
        manager
            .completed
            .write()
            .await
            .insert(tip, CompletedSession::for_test(child));
        (manager, dir, caller, tip)
    }

    #[tokio::test]
    async fn continue_child_entry_prechecks_before_fence_capture() {
        use rsi_common::agent_coordination::AgentContinueErrorCodeV1;

        let (manager, _dir, caller, tip) = fresh_relaunch_fixture().await;
        let Err(self_error) = manager
            .agent_continue_child(caller, continue_request(caller, caller, 0, None))
            .await
        else {
            panic!("self-continuation must be refused before Store access");
        };
        assert_eq!(
            continue_error_code(&self_error),
            AgentContinueErrorCodeV1::SelfContinuationDenied
        );

        let mut invalid = continue_request(tip, tip, 0, None);
        invalid.query.clear();
        let Err(invalid_error) = manager.agent_continue_child(caller, invalid).await else {
            panic!("invalid request must be refused before fence capture");
        };
        assert_eq!(
            continue_error_code(&invalid_error),
            AgentContinueErrorCodeV1::InvalidRequest
        );
    }

    #[tokio::test]
    async fn continue_child_relaunches_fresh_when_no_provider_session_id() {
        let (manager, _dir, caller, tip) = fresh_relaunch_fixture().await;
        let process = crate::session::launch::install_controller_candidate_test_process(tip);
        let mut request = continue_request(tip, tip, 0, None);
        request.idempotency_key = Some("decision-a".into());
        let result = manager
            .agent_continue_child(caller, request.clone())
            .await
            .unwrap();
        let relaunch = result.relaunch.as_ref().expect("fresh relaunch receipt");
        assert_eq!(relaunch.mode, "fresh");
        assert_eq!(result.continued_session_id, tip);
        assert_eq!(relaunch.task_source_session_id, tip);
        assert_eq!(
            process
                .productive_start_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let row = manager
            .store
            .lock()
            .await
            .child_relaunch_intent_by_key(&child_relaunch_identity(caller, &request).unwrap().0)
            .unwrap()
            .unwrap();
        assert_eq!(row.state, RelaunchState::Launched);
        assert_eq!(row.invocation_id, Some(relaunch.invocation_id));
    }

    #[tokio::test]
    async fn fresh_relaunch_replay_after_success_returns_original_receipt() {
        let (manager, _dir, caller, tip) = fresh_relaunch_fixture().await;
        let process = crate::session::launch::install_controller_candidate_test_process(tip);
        let mut request = continue_request(tip, tip, 0, None);
        request.idempotency_key = Some("decision-a".into());
        let first = manager
            .agent_continue_child(caller, request.clone())
            .await
            .unwrap();
        let second = manager.agent_continue_child(caller, request).await.unwrap();
        assert_eq!(
            first.relaunch.as_ref().unwrap().request_id,
            second.relaunch.as_ref().unwrap().request_id
        );
        assert_eq!(
            first.relaunch.as_ref().unwrap().invocation_id,
            second.relaunch.as_ref().unwrap().invocation_id
        );
        assert!(second.relaunch.as_ref().unwrap().deduplicated);
        assert_eq!(
            process
                .productive_start_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn fresh_relaunch_same_key_different_query_is_idempotency_conflict() {
        let (manager, _dir, caller, tip) = fresh_relaunch_fixture().await;
        crate::session::launch::install_controller_candidate_test_process(tip);
        let mut request = continue_request(tip, tip, 0, None);
        request.idempotency_key = Some("decision-a".into());
        manager
            .agent_continue_child(caller, request.clone())
            .await
            .unwrap();
        request.query = "a different instruction".into();
        let error = manager
            .agent_continue_child(caller, request)
            .await
            .unwrap_err();
        assert_eq!(
            continue_error_code(&error),
            rsi_common::agent_coordination::AgentContinueErrorCodeV1::IdempotencyConflict
        );
    }

    #[tokio::test]
    async fn fresh_relaunch_two_keys_each_replay_returns_its_own_receipt() {
        let (manager, _dir, caller, tip) = fresh_relaunch_fixture().await;
        let first_process = crate::session::launch::install_controller_candidate_test_process(tip);
        let mut a = continue_request(tip, tip, 0, None);
        a.idempotency_key = Some("decision-a".into());
        let ra = manager
            .agent_continue_child(caller, a.clone())
            .await
            .unwrap();
        manager.agent_halt(caller, tip).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if manager.completed.read().await.contains_key(&tip) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let observed = manager
            .store
            .lock()
            .await
            .agent_continuation_cursor(tip)
            .unwrap();
        let second_process = crate::session::launch::install_controller_candidate_test_process(tip);
        let mut b = continue_request(
            tip,
            tip,
            observed.event_sequence,
            observed.custody_generation,
        );
        b.idempotency_key = Some("decision-b".into());
        let rb = manager
            .agent_continue_child(caller, b.clone())
            .await
            .unwrap();
        let replay_a = manager.agent_continue_child(caller, a).await.unwrap();
        let replay_b = manager.agent_continue_child(caller, b).await.unwrap();
        assert_eq!(
            replay_a.relaunch.as_ref().unwrap().request_id,
            ra.relaunch.as_ref().unwrap().request_id
        );
        assert_eq!(
            replay_b.relaunch.as_ref().unwrap().request_id,
            rb.relaunch.as_ref().unwrap().request_id
        );
        assert_eq!(
            replay_a.relaunch.as_ref().unwrap().invocation_id,
            ra.relaunch.as_ref().unwrap().invocation_id
        );
        assert_eq!(
            replay_b.relaunch.as_ref().unwrap().invocation_id,
            rb.relaunch.as_ref().unwrap().invocation_id
        );
        assert!(replay_a.relaunch.as_ref().unwrap().deduplicated);
        assert!(replay_b.relaunch.as_ref().unwrap().deduplicated);
        assert_ne!(
            ra.relaunch.as_ref().unwrap().request_id,
            rb.relaunch.as_ref().unwrap().request_id
        );
        assert_eq!(
            first_process
                .productive_start_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            second_process
                .productive_start_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let launched: i64 = manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT COUNT(*) FROM agent_child_relaunch_intents WHERE state='launched'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(launched, 2);
    }

    #[tokio::test]
    async fn fresh_relaunch_concurrent_identical_requests_launch_once() {
        let (manager, _dir, caller, tip) = fresh_relaunch_fixture().await;
        let process = crate::session::launch::install_controller_candidate_test_process(tip);
        let mut request = continue_request(tip, tip, 0, None);
        request.idempotency_key = Some("concurrent-decision".into());
        let (left, right) = tokio::join!(
            manager.agent_continue_child(caller, request.clone()),
            manager.agent_continue_child(caller, request),
        );
        let left = left.unwrap();
        let right = right.unwrap();
        assert_eq!(
            left.relaunch.as_ref().unwrap().request_id,
            right.relaunch.as_ref().unwrap().request_id
        );
        assert_eq!(
            left.relaunch.as_ref().unwrap().invocation_id,
            right.relaunch.as_ref().unwrap().invocation_id
        );
        assert!(
            left.relaunch.as_ref().unwrap().deduplicated
                || right.relaunch.as_ref().unwrap().deduplicated
        );
        assert_eq!(
            process
                .productive_start_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let (intents, admissions): (i64, i64) = {
            let store = manager.store.lock().await;
            (
                store.conn.query_row("SELECT COUNT(*) FROM agent_child_relaunch_intents", [], |row| row.get(0)).unwrap(),
                store.conn.query_row("SELECT COUNT(*) FROM model_invocations WHERE dedup_key LIKE 'agent.child_relaunch.v1:%'", [], |row| row.get(0)).unwrap(),
            )
        };
        assert_eq!((intents, admissions), (1, 1));
    }

    #[tokio::test]
    async fn continue_child_fresh_relaunch_refuses_without_durable_task() {
        let (manager, _dir, caller, tip) = fresh_relaunch_fixture().await;
        manager
            .completed
            .write()
            .await
            .get_mut(&tip)
            .unwrap()
            .session
            .query = "/create_handoff".into();
        let error = manager
            .agent_continue_child(caller, continue_request(tip, tip, 0, None))
            .await
            .unwrap_err();
        assert_eq!(continue_error_code(&error), rsi_common::agent_coordination::AgentContinueErrorCodeV1::ResumeUnavailableTaskUnresolved);
        let count: i64 = manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT COUNT(*) FROM agent_child_relaunch_intents",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn continue_child_fresh_relaunch_uses_same_sandbox_and_row() {
        let (manager, _dir, caller, tip) = fresh_relaunch_fixture().await;
        let before = manager
            .store
            .lock()
            .await
            .get_session(tip)
            .unwrap()
            .unwrap();
        let process = crate::session::launch::install_controller_candidate_test_process(tip);
        let receipt = manager
            .agent_continue_child(caller, continue_request(tip, tip, 0, None))
            .await
            .unwrap();
        let after = manager
            .store
            .lock()
            .await
            .get_session(tip)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.continued_session_id, tip);
        assert_eq!(after.working_dir, before.working_dir);
        assert_eq!(after.sandbox_root, before.sandbox_root);
        assert_eq!(
            process
                .productive_start_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let rows: i64 = manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id=?1",
                [tip.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn continue_child_fresh_relaunch_task_skips_create_handoff_lineage() {
        let (manager, dir, caller, tip) = fresh_relaunch_fixture().await;
        let source = Uuid::new_v4();
        let mut ancestor = test_session(source, dir.path().to_path_buf());
        ancestor.parent_id = Some(caller);
        ancestor.query = "Original durable implementation task".into();
        manager
            .store
            .lock()
            .await
            .insert_session(&ancestor)
            .unwrap();
        {
            let mut completed = manager.completed.write().await;
            let child = &mut completed.get_mut(&tip).unwrap().session;
            child.query = "/create_handoff".into();
            child.continued_from = Some(source);
            drop(completed);
        }
        crate::session::launch::install_controller_candidate_test_process(tip);
        let result = manager
            .agent_continue_child(caller, continue_request(tip, tip, 0, None))
            .await
            .unwrap();
        assert_eq!(result.relaunch.unwrap().task_source_session_id, source);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event: Option<String> = manager.store.lock().await.conn.query_row(
                    "SELECT content FROM conversation_events WHERE session_id=?1 AND content LIKE 'Original durable implementation task%' ORDER BY sequence DESC LIMIT 1",
                    [tip.to_string()], |row| row.get(0),
                ).optional().unwrap();
                if let Some(event) = event {
                    assert!(event.contains("Lead instruction:\nresume the stage"));
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.unwrap();
    }

    #[test]
    fn fresh_relaunch_system_prompt_carries_kind_preamble() {
        let parts = crate::session::launch::fresh_launch_preamble_parts(SessionKind::Task);
        assert!(
            parts
                .iter()
                .any(|part| part.contains("# Orchestration Router"))
        );
        assert!(
            parts
                .iter()
                .any(|part| part.contains("# RPI Worker Preamble"))
        );
    }

    #[tokio::test]
    async fn boot_recovery_settles_open_intent_and_launches_nothing() {
        let (manager, _dir, caller, tip) = fresh_relaunch_fixture().await;
        let request = continue_request(tip, tip, 0, None);
        let (key_digest, request_fingerprint, request_id) =
            child_relaunch_identity(caller, &request).unwrap();
        let row = RelaunchIntentRow {
            request_id,
            key_digest,
            request_fingerprint,
            caller_session_id: caller,
            target_session_id: tip,
            tip_session_id: tip,
            observed_event_sequence: 0,
            observed_custody_generation: None,
            dedup_key: format!("agent.child_relaunch.v1:{request_id}"),
            state: RelaunchState::Intent,
            invocation_id: None,
            receipt_json: None,
            abandon_reason: None,
        };
        manager
            .store
            .lock()
            .await
            .insert_child_relaunch_intent(&row)
            .unwrap();
        let open_rows = manager
            .store
            .lock()
            .await
            .open_child_relaunch_intents()
            .unwrap();
        for open in open_rows {
            let _guard =
                crate::session::spawn_single_flight::acquire_spawn_guard(open.tip_session_id).await;
            manager.recover_agent_child_relaunch(&open).await.unwrap();
        }
        let store = manager.store.lock().await;
        assert_eq!(
            store
                .child_relaunch_intent_by_key(&row.key_digest)
                .unwrap()
                .unwrap()
                .state,
            RelaunchState::Abandoned
        );
        let launches: i64 = store.conn.query_row("SELECT COUNT(*) FROM model_invocations WHERE dedup_key LIKE 'agent.child_relaunch.v1:%'", [], |row| row.get(0)).unwrap();
        assert_eq!(launches, 0);
        drop(store);
    }

    fn continue_error_observed(
        error: &DaemonError,
    ) -> Option<rsi_common::agent_coordination::AgentContinuationCursorV1> {
        let DaemonError::StructuredRpc { data, .. } = error else {
            panic!("AgentContinueChild refusals must be structured: {error:?}");
        };
        let envelope: rsi_common::agent_coordination::AgentContinueErrorV1 =
            serde_json::from_value(data.clone()).expect("typed continue error envelope");
        envelope.observed
    }

    /// Persist `caller` and a direct child of it, plus `events` conversation
    /// rows on the child so the cursor has a non-trivial sequence.
    async fn insert_caller_and_child(
        store: &Arc<tokio::sync::Mutex<Store>>,
        dir: &TempDir,
        events: i32,
    ) -> (Uuid, Uuid) {
        use rsi_common::types::{ConversationEvent, EventType, Role};

        let caller_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let caller = test_session(caller_id, dir.path().to_path_buf());
        let mut child = test_session(child_id, dir.path().to_path_buf());
        child.parent_id = Some(caller_id);

        let store = store.lock().await;
        store.insert_session(&caller).unwrap();
        store.insert_session(&child).unwrap();
        for sequence in 0..events {
            store
                .insert_event(&ConversationEvent {
                    id: 0,
                    session_id: child_id,
                    sequence,
                    event_type: EventType::Message,
                    role: Some(Role::User),
                    created_at: chrono::Utc::now(),
                    content: String::new(),
                    tool_name: None,
                    tool_input: None,
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                })
                .unwrap();
        }
        (caller_id, child_id)
    }

    /// Self-continuation is an unowned self-injection loop. It must be refused
    /// before ANY Store read, so the verb cannot be used to confirm the
    /// caller's own row.
    #[tokio::test]
    async fn continue_child_refuses_self_target() {
        let (control, _store) = control_handle_with_store();
        let caller = Uuid::new_v4();

        let error = control
            .authorize_continue_child(caller, &continue_request(caller, caller, 0, None))
            .await
            .expect_err("self-continuation must be refused");

        assert_eq!(
            continue_error_code(&error),
            rsi_common::agent_coordination::AgentContinueErrorCodeV1::SelfContinuationDenied
        );
    }

    /// A session the caller neither parents nor leads is out of scope, and the
    /// refusal must not leak whether the row exists.
    #[tokio::test]
    async fn continue_child_refuses_unrelated_target() {
        let (control, store) = control_handle_with_store();
        let dir = TempDir::new().unwrap();
        let caller = Uuid::new_v4();
        let stranger_id = Uuid::new_v4();
        {
            let store = store.lock().await;
            store
                .insert_session(&test_session(caller, dir.path().to_path_buf()))
                .unwrap();
            store
                .insert_session(&test_session(stranger_id, dir.path().to_path_buf()))
                .unwrap();
        }

        let error = control
            .authorize_continue_child(caller, &continue_request(stranger_id, stranger_id, 0, None))
            .await
            .expect_err("an unrelated target must be refused");

        assert_eq!(
            continue_error_code(&error),
            rsi_common::agent_coordination::AgentContinueErrorCodeV1::TargetNotAuthorized
        );
        assert!(
            continue_error_observed(&error).is_none(),
            "a scope refusal must not leak the target's cursor"
        );
    }

    /// CodexAppServer allocates a fresh session id on continue, so continuing
    /// it under the named id would hand back an already-wrong receipt.
    #[tokio::test]
    async fn continue_child_refuses_codex_app_server_provider() {
        let (control, store) = control_handle_with_store();
        let dir = TempDir::new().unwrap();
        let caller_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        {
            let store = store.lock().await;
            store
                .insert_session(&test_session(caller_id, dir.path().to_path_buf()))
                .unwrap();
            let mut child = test_session(child_id, dir.path().to_path_buf());
            child.parent_id = Some(caller_id);
            child.provider = SessionProvider::CodexAppServer;
            store.insert_session(&child).unwrap();
        }

        let error = control
            .authorize_continue_child(caller_id, &continue_request(child_id, child_id, 0, None))
            .await
            .expect_err("CodexAppServer must be refused with a typed class");

        assert_eq!(
            continue_error_code(&error),
            rsi_common::agent_coordination::AgentContinueErrorCodeV1::ProviderUnsupported
        );
    }

    /// Provider policy follows the row actually continued. A logical
    /// CodexAppServer root may already resolve to an ordinary Codex replacement
    /// whose stable id is safe to continue.
    #[tokio::test]
    async fn continue_child_checks_provider_on_the_resolved_tip() {
        let (control, store) = control_handle_with_store();
        let dir = TempDir::new().unwrap();
        let caller_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let tip_id = Uuid::new_v4();
        {
            let store = store.lock().await;
            store
                .insert_session(&test_session(caller_id, dir.path().to_path_buf()))
                .unwrap();
            let mut child = test_session(child_id, dir.path().to_path_buf());
            child.parent_id = Some(caller_id);
            child.provider = SessionProvider::CodexAppServer;
            store.insert_session(&child).unwrap();

            let mut tip = test_session(tip_id, dir.path().to_path_buf());
            tip.parent_id = Some(caller_id);
            tip.continued_from = Some(child_id);
            tip.provider = SessionProvider::Codex;
            store.insert_session(&tip).unwrap();
        }

        let observed = control
            .authorize_continue_child(caller_id, &continue_request(child_id, tip_id, 0, None))
            .await
            .expect("the continuable resolved tip must determine provider policy");

        assert_eq!(observed.tip_session_id, tip_id);
    }

    /// Authority is checked again on the resolved lineage tip before a stale
    /// witness is returned. A caller authorized only on the logical root must
    /// neither continue nor learn the cursor of a reparented successor tip.
    #[tokio::test]
    async fn continue_child_reauthorizes_the_resolved_tip() {
        let (control, store) = control_handle_with_store();
        let dir = TempDir::new().unwrap();
        let caller_id = Uuid::new_v4();
        let stranger_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let tip_id = Uuid::new_v4();
        {
            let store = store.lock().await;
            store
                .insert_session(&test_session(caller_id, dir.path().to_path_buf()))
                .unwrap();
            store
                .insert_session(&test_session(stranger_id, dir.path().to_path_buf()))
                .unwrap();
            let mut child = test_session(child_id, dir.path().to_path_buf());
            child.parent_id = Some(caller_id);
            store.insert_session(&child).unwrap();

            let mut tip = test_session(tip_id, dir.path().to_path_buf());
            tip.parent_id = Some(stranger_id);
            tip.continued_from = Some(child_id);
            store.insert_session(&tip).unwrap();
        }

        let stale_error = control
            .authorize_continue_child(caller_id, &continue_request(child_id, child_id, 99, None))
            .await
            .expect_err("tip authority must be checked before returning a stale witness");

        assert_eq!(
            continue_error_code(&stale_error),
            rsi_common::agent_coordination::AgentContinueErrorCodeV1::TargetNotAuthorized
        );
        assert!(
            continue_error_observed(&stale_error).is_none(),
            "an unauthorized tip's cursor must not be disclosed"
        );

        let exact_error = control
            .authorize_continue_child(caller_id, &continue_request(child_id, tip_id, 0, None))
            .await
            .expect_err("a reparented tip must be re-authorized before continuation");

        assert_eq!(
            continue_error_code(&exact_error),
            rsi_common::agent_coordination::AgentContinueErrorCodeV1::TargetNotAuthorized
        );
    }

    /// The happy path clears and returns the exact cursor the fence checked.
    #[tokio::test]
    async fn continue_child_clears_a_matching_cursor() {
        let (control, store) = control_handle_with_store();
        let dir = TempDir::new().unwrap();
        let (caller_id, child_id) = insert_caller_and_child(&store, &dir, 3).await;

        let observed = control
            .authorize_continue_child(caller_id, &continue_request(child_id, child_id, 2, None))
            .await
            .expect("a matching cursor must clear");

        assert_eq!(observed.tip_session_id, child_id);
        assert_eq!(observed.event_sequence, 2, "MAX(sequence) over 0,1,2");
        assert_eq!(observed.custody_generation, None);
    }

    /// A cursor that has moved on is exactly the case this verb exists to
    /// refuse, and the refusal must carry the version witness to retry with.
    #[tokio::test]
    async fn continue_child_refuses_stale_sequence_and_returns_the_witness() {
        let (control, store) = control_handle_with_store();
        let dir = TempDir::new().unwrap();
        let (caller_id, child_id) = insert_caller_and_child(&store, &dir, 3).await;

        let error = control
            .authorize_continue_child(caller_id, &continue_request(child_id, child_id, 1, None))
            .await
            .expect_err("a stale sequence must be refused");

        assert_eq!(
            continue_error_code(&error),
            rsi_common::agent_coordination::AgentContinueErrorCodeV1::StaleContinuation
        );
        let witness = continue_error_observed(&error).expect("stale refusal carries the witness");
        assert_eq!(witness.event_sequence, 2);
        assert_eq!(witness.tip_session_id, child_id);
    }

    /// Once a continuation event is durable, a request carrying the prior
    /// sequence is stale. This tests the cursor fence only; dispatch itself is
    /// asynchronous and is not deduplicated by this tuple.
    #[tokio::test]
    async fn continue_child_refuses_replay_after_event_sequence_advances() {
        use rsi_common::types::{ConversationEvent, EventType, Role};

        let (control, store) = control_handle_with_store();
        let dir = TempDir::new().unwrap();
        let (caller_id, child_id) = insert_caller_and_child(&store, &dir, 1).await;
        let request = continue_request(child_id, child_id, 0, None);

        control
            .authorize_continue_child(caller_id, &request)
            .await
            .expect("first continuation clears");

        // Stand in for the delivered turn the real continuation persists.
        store
            .lock()
            .await
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: child_id,
                sequence: 1,
                event_type: EventType::Message,
                role: Some(Role::User),
                created_at: chrono::Utc::now(),
                content: "resume the stage".to_string(),
                tool_name: None,
                tool_input: None,
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .unwrap();

        let error = control
            .authorize_continue_child(caller_id, &request)
            .await
            .expect_err("a request carrying the prior sequence must be stale");
        assert_eq!(
            continue_error_code(&error),
            rsi_common::agent_coordination::AgentContinueErrorCodeV1::StaleContinuation
        );
    }

    /// Custody generation is fenced on full equality INCLUDING absence: a caller
    /// that expected a sandboxed child must not silently continue one that has
    /// no custody projection.
    #[tokio::test]
    async fn continue_child_custody_absence_is_part_of_the_fence() {
        let (control, store) = control_handle_with_store();
        let dir = TempDir::new().unwrap();
        let (caller_id, child_id) = insert_caller_and_child(&store, &dir, 1).await;

        let error = control
            .authorize_continue_child(caller_id, &continue_request(child_id, child_id, 0, Some(1)))
            .await
            .expect_err("expecting a custody generation the target lacks must refuse");

        assert_eq!(
            continue_error_code(&error),
            rsi_common::agent_coordination::AgentContinueErrorCodeV1::StaleContinuation
        );
        assert_eq!(
            continue_error_observed(&error)
                .expect("witness")
                .custody_generation,
            None
        );
    }

    /// Bounds are checked before scope, so an oversized payload cannot be used
    /// to probe authority.
    #[tokio::test]
    async fn continue_child_rejects_an_empty_query_before_scope() {
        let (control, _store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let child = Uuid::new_v4();
        let mut request = continue_request(child, child, 0, None);
        request.query = String::new();

        let error = control
            .authorize_continue_child(caller, &request)
            .await
            .expect_err("an empty query must be refused");

        assert_eq!(
            continue_error_code(&error),
            rsi_common::agent_coordination::AgentContinueErrorCodeV1::InvalidRequest
        );
    }

    async fn insert_failed_pending(store: &Arc<tokio::sync::Mutex<Store>>, session: &Session) {
        let store = store.lock().await;
        store.insert_session(session).unwrap();
        store
            .update_failed_and_stage_c5_autofile(
                session.id,
                crate::store::daemon_settings::AutofileCause::ProcessDied,
            )
            .unwrap();
    }

    #[test]
    fn c5_automatic_error_observation_has_one_log_and_one_system_message_choke() {
        // This module has no tracing capture harness. Pin the production source
        // instead: every automatic/replay failure must route through the one
        // helper, which owns exactly one error log and one SystemMessage.
        let source = include_str!("agent_verbs.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        let c5_production = production
            .split("pub(crate) async fn maybe_autofile_terminal_failure(")
            .nth(1)
            .unwrap();
        let helper = c5_production
            .split("fn report_c5_autofile_error(")
            .nth(1)
            .unwrap()
            .split("/// One finite post-restore replay")
            .next()
            .unwrap();
        assert_eq!(helper.matches("tracing::error!(").count(), 1);
        assert_eq!(helper.matches("DaemonEvent::SystemMessage").count(), 1);
        assert_eq!(c5_production.matches("tracing::error!(").count(), 1);
        assert_eq!(
            c5_production.matches("DaemonEvent::SystemMessage").count(),
            1
        );
    }

    #[tokio::test]
    async fn c5_replay_retains_malformed_middle_row_and_settles_later_rows_once() {
        let (control, store, bus) = control_handle_with_store_and_bus();
        let mut receiver = bus.subscribe();
        let dir = TempDir::new().unwrap();
        let source_a = test_session(Uuid::from_u128(1), dir.path().to_path_buf());
        let source_b = test_session(Uuid::from_u128(2), dir.path().to_path_buf());
        let source_c = test_session(Uuid::from_u128(3), dir.path().to_path_buf());
        insert_failed_pending(&store, &source_a).await;
        insert_failed_pending(&store, &source_b).await;
        insert_failed_pending(&store, &source_c).await;
        let bad_key = crate::store::daemon_settings::c5_autofile_pending_key(source_b.id);
        store
            .lock()
            .await
            .set_daemon_setting(&bad_key, "{")
            .unwrap();

        control.replay_c5_autofile_pending().await;
        let first_messages = system_messages(&mut receiver);
        assert_eq!(first_messages.len(), 1);
        assert_eq!(first_messages[0].0, "error");
        assert!(first_messages[0].1.contains(&source_b.id.to_string()));
        let guard = store.lock().await;
        assert_eq!(guard.list_issues(&Default::default()).unwrap().len(), 2);
        assert_eq!(
            guard.get_daemon_setting(&bad_key).unwrap().as_deref(),
            Some("{")
        );
        assert!(
            guard
                .get_c5_autofile_pending(&crate::store::daemon_settings::c5_autofile_pending_key(
                    source_a.id
                ))
                .unwrap()
                .is_none()
        );
        assert!(
            guard
                .get_c5_autofile_pending(&crate::store::daemon_settings::c5_autofile_pending_key(
                    source_c.id
                ))
                .unwrap()
                .is_none()
        );
        drop(guard);

        control.replay_c5_autofile_pending().await;
        assert_eq!(
            system_messages(&mut receiver).len(),
            1,
            "the retained bad row is reported once per replay feed"
        );
        assert_eq!(
            store
                .lock()
                .await
                .list_issues(&Default::default())
                .unwrap()
                .len(),
            2
        );
        bus.unsubscribe();
    }

    #[tokio::test]
    async fn c5_replay_reports_batch_read_failure_once_without_mutating_session_or_issue_state() {
        let (control, store, bus) = control_handle_with_store_and_bus();
        let mut receiver = bus.subscribe();
        let dir = TempDir::new().unwrap();
        let source = test_session(Uuid::new_v4(), dir.path().to_path_buf());
        insert_failed_pending(&store, &source).await;
        store
            .lock()
            .await
            .conn
            .execute_batch("DROP TABLE daemon_settings")
            .unwrap();

        control.replay_c5_autofile_pending().await;
        let messages = system_messages(&mut receiver);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "error");
        let guard = store.lock().await;
        assert_eq!(
            guard.get_session(source.id).unwrap().unwrap().status,
            SessionStatus::Failed
        );
        assert!(guard.list_issues(&Default::default()).unwrap().is_empty());
        bus.unsubscribe();
    }

    #[tokio::test]
    async fn c5_settlement_failure_reports_once_then_replay_repairs_without_duplicate() {
        let (control, store, bus) = control_handle_with_store_and_bus();
        let mut receiver = bus.subscribe();
        let dir = TempDir::new().unwrap();
        let source = test_session(Uuid::new_v4(), dir.path().to_path_buf());
        insert_failed_pending(&store, &source).await;
        let key = crate::store::daemon_settings::c5_autofile_pending_key(source.id);
        store
            .lock()
            .await
            .conn
            .execute_batch(
                "CREATE TRIGGER c5_test_replay_marker_delete BEFORE DELETE ON daemon_settings
             WHEN OLD.key LIKE 'c5.autofile.pending.v1/%'
             BEGIN SELECT RAISE(ABORT, 'injected-marker-delete'); END;",
            )
            .unwrap();

        control
            .maybe_autofile_terminal_failure(source.id, RecoveryDisposition::NoRecoverySource)
            .await;
        let messages = system_messages(&mut receiver);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "error");
        assert!(messages[0].1.contains(&source.id.to_string()));
        let expected_issue_id = Uuid::new_v5(
            &c5_issue_namespace(),
            format!("pipeline-failure\0{}", source.id).as_bytes(),
        );
        assert!(messages[0].1.contains(&expected_issue_id.to_string()));
        assert!(
            store
                .lock()
                .await
                .get_issue(expected_issue_id)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .lock()
                .await
                .get_c5_autofile_pending(&key)
                .unwrap()
                .is_some()
        );

        store
            .lock()
            .await
            .conn
            .execute_batch("DROP TRIGGER c5_test_replay_marker_delete")
            .unwrap();
        control.replay_c5_autofile_pending().await;
        assert!(system_messages(&mut receiver).is_empty());
        assert_eq!(
            store
                .lock()
                .await
                .list_issues(&Default::default())
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .lock()
                .await
                .get_c5_autofile_pending(&key)
                .unwrap()
                .is_none()
        );
        control.replay_c5_autofile_pending().await;
        assert!(system_messages(&mut receiver).is_empty());
        assert_eq!(
            store
                .lock()
                .await
                .list_issues(&Default::default())
                .unwrap()
                .len(),
            1
        );
        bus.unsubscribe();
    }

    #[tokio::test]
    async fn agent_create_issue_is_durable_first_write_wins_and_binds_creator() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        store
            .lock()
            .await
            .insert_session(&test_session(caller, std::path::PathBuf::from("/tmp")))
            .unwrap();
        let params = AgentCreateIssueParams {
            title: "follow up".into(),
            body: "body".into(),
            priority: Some(2),
            labels: vec!["one".into(), "two".into()],
            assignee: None,
            idempotency_key: "stable-key".into(),
        };
        let first = control
            .agent_create_issue(caller, params.clone())
            .await
            .unwrap();
        assert!(!first.deduplicated);
        assert_eq!(first.issue.created_by_session_id, Some(caller));
        let mut expected_name = b"agent-create\0".to_vec();
        expected_name.extend_from_slice(caller.to_string().as_bytes());
        expected_name.push(0);
        expected_name.extend_from_slice(params.idempotency_key.as_bytes());
        assert_eq!(
            first.issue.id,
            Uuid::new_v5(&c5_issue_namespace(), &expected_name),
            "AgentCreateIssue must preserve its pinned UUIDv5 domain/name bytes"
        );
        assert!(
            first.issue.idea_id.is_none()
                && first.issue.source_event_id.is_none()
                && first.issue.source_finding_ref.is_none(),
            "attributed creation must remain unlinked even when controller state exists elsewhere"
        );
        let replay = control
            .agent_create_issue(caller, params.clone())
            .await
            .unwrap();
        assert!(replay.deduplicated);
        assert_eq!(replay.issue.id, first.issue.id);
        let conflict = control
            .agent_create_issue(
                caller,
                AgentCreateIssueParams {
                    title: "changed".into(),
                    ..params
                },
            )
            .await
            .unwrap_err();
        assert!(
            conflict
                .to_string()
                .contains("agent_create_issue_idempotency_conflict")
        );
        assert_eq!(
            store
                .lock()
                .await
                .list_issues(&Default::default())
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn agent_issue_guarded_control_handle_routes_all_seven_lead_verbs() {
        let (control, store) = control_handle_with_store();
        let project = rsi_common::types::Project {
            id: Uuid::new_v4(),
            name: "Agent Issue control project".to_string(),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let group_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let caller = Uuid::new_v4();
        let denied = Uuid::new_v4();
        {
            let store = store.lock().await;
            store.insert_project(&project).unwrap();
            let mut group = test_session(group_id, std::path::PathBuf::from("/tmp"));
            group.project_id = Some(project.id);
            group.session_kind = SessionKind::Group;
            group.status = SessionStatus::Running;
            store.insert_session(&group).unwrap();
            let mut epic = test_session(epic_id, std::path::PathBuf::from("/tmp"));
            epic.project_id = Some(project.id);
            epic.session_kind = SessionKind::Epic;
            epic.parent_id = Some(group_id);
            epic.status = SessionStatus::Running;
            store.insert_session(&epic).unwrap();
            for id in [caller, denied] {
                let mut session = test_session(id, std::path::PathBuf::from("/tmp"));
                session.project_id = Some(project.id);
                session.parent_id = Some(epic_id);
                session.status = SessionStatus::Running;
                store.insert_session(&session).unwrap();
            }
            store.set_lead_session(epic_id, Some(caller)).unwrap();
        }

        let created = control
            .agent_create_issue(
                caller,
                AgentCreateIssueParams {
                    title: "guarded route".into(),
                    body: "original".into(),
                    priority: Some(2),
                    labels: vec!["agent-control".into()],
                    assignee: None,
                    idempotency_key: "agent-control-create".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            control
                .agent_list_issues(denied, AgentListIssuesRequestV1::default())
                .await
                .is_err()
        );
        assert_eq!(
            control
                .agent_list_issues(caller, AgentListIssuesRequestV1::default())
                .await
                .unwrap()
                .issues,
            vec![created.issue.clone()]
        );
        let loaded = control
            .agent_get_issue(
                caller,
                AgentGetIssueRequestV1 {
                    issue_id: created.issue.id,
                },
            )
            .await
            .unwrap();
        assert_eq!(loaded.issue, created.issue);
        assert!(loaded.blocked_by.is_empty());
        assert!(loaded.blocks.is_empty());

        let updated = control
            .agent_update_issue(
                caller,
                AgentUpdateIssueRequestV1 {
                    issue_id: created.issue.id,
                    expected_row_version: created.issue.row_version,
                    idempotency_key: "agent-control-edit".into(),
                    title: None,
                    body: Some("updated".into()),
                    labels: None,
                    priority: None,
                    clear_priority: false,
                    assignee: None,
                    clear_assignee: false,
                },
            )
            .await
            .unwrap();
        let closed = control
            .agent_update_issue_status(
                caller,
                AgentUpdateIssueStatusRequestV1 {
                    issue_id: created.issue.id,
                    status: rsi_common::types::IssueStatus::Closed,
                    expected_row_version: updated.issue.row_version,
                    idempotency_key: "agent-control-close".into(),
                },
            )
            .await
            .unwrap();
        let archived = control
            .agent_archive_issue(
                caller,
                AgentArchiveIssueRequestV1 {
                    issue_id: created.issue.id,
                    expected_row_version: closed.issue.row_version,
                    idempotency_key: "agent-control-archive".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            control
                .agent_list_issues(caller, AgentListIssuesRequestV1::default())
                .await
                .unwrap()
                .issues
                .is_empty()
        );
        let before_restore = control
            .agent_list_issue_events(
                caller,
                IssueEventPageRequestV1 {
                    issue_id: created.issue.id,
                    after_sequence: 0,
                    limit: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(before_restore.events.len(), 4);
        assert!(
            before_restore
                .events
                .iter()
                .skip(1)
                .all(|event| event.owning_epic_id == Some(epic_id))
        );
        let restored = control
            .agent_restore_issue(
                caller,
                AgentRestoreIssueRequestV1 {
                    issue_id: created.issue.id,
                    expected_row_version: archived.issue.row_version,
                    idempotency_key: "agent-control-restore".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            restored.issue.status,
            rsi_common::types::IssueStatus::Closed
        );
        assert!(restored.issue.archived_at.is_none());
        assert_eq!(
            control
                .agent_list_issue_events(
                    caller,
                    IssueEventPageRequestV1 {
                        issue_id: created.issue.id,
                        after_sequence: 0,
                        limit: None,
                    },
                )
                .await
                .unwrap()
                .events
                .len(),
            5
        );
    }

    #[test]
    fn c5_namespace_literal_is_the_pinned_url_v5_derivation() {
        assert_eq!(
            c5_issue_namespace(),
            Uuid::new_v5(
                &Uuid::NAMESPACE_URL,
                b"https://github.com/jakedevar/rsi/local-issue-tracker/c5",
            )
        );
        assert_eq!(
            c5_issue_namespace().to_string(),
            "5da34881-ecc9-54fb-a7da-3913bc978130"
        );
    }

    /// Candidate watch row built through the same shared builder every arm
    /// transport uses, so tests exercise realistic rows (recurring 60s,
    /// enabled, wake target = caller).
    fn watch_candidate(caller: Uuid, watched: Uuid) -> ScheduledJob {
        use crate::session::harness::tools::schedule_wake::{
            ScheduleWakeRequest, build_scheduled_job,
        };
        build_scheduled_job(ScheduleWakeRequest {
            message: "watch note".to_string(),
            in_seconds: None,
            at: None,
            name: None,
            every_seconds: None,
            mode: Some("on_terminal".to_string()),
            working_dir: std::path::PathBuf::from("/tmp"),
            provider: None,
            model: None,
            project_id: None,
            origin_session_id: Some(caller),
            watch_session_id: Some(watched),
        })
        .expect("valid watch candidate")
    }

    fn insert_launched_agent_child(
        store: &Store,
        owner_id: Uuid,
        epic_id: Uuid,
        child_id: Uuid,
        status: SessionStatus,
        key: &str,
    ) {
        let request_id = Uuid::new_v4();
        let mut child = test_session(child_id, std::path::PathBuf::from("/tmp"));
        child.parent_id = Some(epic_id);
        child.status = status;
        store.insert_session(&child).expect("insert child");
        let request = AgentSpawnChildRequestV1 {
            kind: SessionKind::Task,
            provider: None,
            model: None,
            effort: None,
            query: "work".into(),
            agent_role: None,
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: key.into(),
        };
        store
            .reserve_agent_spawn_request(
                owner_id,
                &crate::model_control::hash_request_fingerprint(&["idem", key]),
                &crate::model_control::hash_request_fingerprint(&["request", key]),
                &request,
                epic_id,
                request_id,
                child_id,
            )
            .expect("reserve request");
        store
            .mark_agent_spawn_queued(request_id)
            .expect("mark queued");
        store
            .mark_agent_spawn_launching(request_id)
            .expect("mark launching");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "UPDATE agent_spawn_requests SET state='launched',updated_at=?1,launched_at=?1 WHERE spawn_request_id=?2",
                rusqlite::params![now, request_id.to_string()],
            )
            .expect("settle launched fixture");
        store
            .conn
            .execute(
                "INSERT INTO agent_child_watch_witness
                 (owner_session_id,child_session_id,job_id,state,updated_at)
                 VALUES (?1,?2,NULL,'pending',?3)",
                rusqlite::params![owner_id.to_string(), child_id.to_string(), now],
            )
            .expect("record arm intent fixture");
    }

    async fn enabled_watch_count(store: &tokio::sync::Mutex<Store>, caller: Uuid) -> usize {
        store
            .lock()
            .await
            .list_scheduled_jobs()
            .expect("list jobs")
            .into_iter()
            .filter(|j| {
                j.enabled
                    && j.wake_session_id == Some(caller)
                    && matches!(j.wake_mode, WakeMode::OnTerminal(_))
            })
            .count()
    }

    #[tokio::test]
    async fn terminal_watch_restart_repair_is_automatic_and_deduplicated() {
        let (control, store) = control_handle_with_store();
        let owner_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let mut owner = test_session(owner_id, std::path::PathBuf::from("/tmp"));
        owner.status = SessionStatus::Running;
        let mut epic = test_session(epic_id, std::path::PathBuf::from("/tmp"));
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(owner_id);
        let mut child = test_session(child_id, std::path::PathBuf::from("/tmp"));
        child.parent_id = Some(epic_id);
        child.status = SessionStatus::Running;
        let request = AgentSpawnChildRequestV1 {
            kind: SessionKind::Task,
            provider: None,
            model: None,
            effort: None,
            query: "work".into(),
            agent_role: None,
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: "restart-watch".into(),
        };
        {
            let store = store.lock().await;
            store.insert_session(&owner).expect("insert owner");
            store.insert_session(&epic).expect("insert epic");
            store.insert_session(&child).expect("insert child");
            store
                .reserve_agent_spawn_request(
                    owner_id,
                    &crate::model_control::hash_request_fingerprint(&["idem", "restart-watch"]),
                    &crate::model_control::hash_request_fingerprint(&["request", "restart-watch"]),
                    &request,
                    epic_id,
                    request_id,
                    child_id,
                )
                .expect("reserve request");
            store
                .mark_agent_spawn_queued(request_id)
                .expect("mark queued");
            store
                .mark_agent_spawn_launching(request_id)
                .expect("mark launching");
            let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            store
                .conn
                .execute(
                    "UPDATE agent_spawn_requests SET state='launched',updated_at=?1,launched_at=?1 WHERE spawn_request_id=?2",
                    rusqlite::params![now, request_id.to_string()],
                )
                .expect("settle launched fixture");
            store
                .conn
                .execute(
                    "INSERT INTO agent_child_watch_witness
                     (owner_session_id,child_session_id,job_id,state,updated_at)
                     VALUES (?1,?2,NULL,'pending',?3)",
                    rusqlite::params![owner_id.to_string(), child_id.to_string(), now],
                )
                .expect("record arm intent fixture");
        }

        control
            .reconcile_automatic_child_watches()
            .await
            .expect("repair missing watch");
        control
            .reconcile_automatic_child_watches()
            .await
            .expect("deduplicate repaired watch");
        assert_eq!(enabled_watch_count(&store, owner_id).await, 1);
    }

    #[tokio::test]
    async fn terminal_watch_restart_repairs_consumed_live_and_missing_terminal_watches() {
        let (control, store) = control_handle_with_store();
        let owner_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let delivered_child = Uuid::new_v4();
        let disabled_child = Uuid::new_v4();
        let terminal_child = Uuid::new_v4();
        let mut owner = test_session(owner_id, std::path::PathBuf::from("/tmp"));
        owner.status = SessionStatus::Running;
        let mut epic = test_session(epic_id, std::path::PathBuf::from("/tmp"));
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(owner_id);
        {
            let guard = store.lock().await;
            guard.insert_session(&owner).expect("insert owner");
            guard.insert_session(&epic).expect("insert epic");
            insert_launched_agent_child(
                &guard,
                owner_id,
                epic_id,
                delivered_child,
                SessionStatus::Running,
                "delivered-watch",
            );
            insert_launched_agent_child(
                &guard,
                owner_id,
                epic_id,
                disabled_child,
                SessionStatus::Running,
                "disabled-watch",
            );
            insert_launched_agent_child(
                &guard,
                owner_id,
                epic_id,
                terminal_child,
                SessionStatus::Completed,
                "terminal-watch",
            );

            let delivered = watch_candidate(owner_id, delivered_child);
            guard
                .insert_scheduled_job(&delivered)
                .expect("insert delivered watch");
            guard
                .retire_unchanged_child_watch(&delivered, "consumed")
                .expect("mark delivered watch consumed");
            let disabled = watch_candidate(owner_id, disabled_child);
            guard
                .insert_scheduled_job(&disabled)
                .expect("insert operator-suppressed watch");
            guard
                .update_scheduled_job(
                    &disabled.id,
                    &crate::store::scheduled_jobs::ScheduledJobUpdate {
                        name: None,
                        message: None,
                        schedule: None,
                        enabled: Some(false),
                        next_fire_at: None,
                    },
                )
                .expect("disable watch");
        }

        control
            .reconcile_automatic_child_watches()
            .await
            .expect("restart reconciliation");
        let jobs = store
            .lock()
            .await
            .list_scheduled_jobs()
            .expect("list watches");
        assert_eq!(jobs.len(), 4, "two missing watches are armed");
        assert_eq!(
            jobs.iter()
                .filter(|job| job.enabled && job.wake_mode == WakeMode::OnTerminal(delivered_child))
                .count(),
            1,
            "the consumed live-child watch is rearmed"
        );
        assert!(jobs.iter().any(|job| {
            !job.enabled
                && job.wake_mode == WakeMode::OnTerminal(disabled_child)
                && job.last_fired_at.is_none()
        }));
        assert!(
            jobs.iter().all(|job| {
                !job.enabled || job.wake_mode != WakeMode::OnTerminal(disabled_child)
            })
        );
        assert!(jobs.iter().any(|job| {
            !job.enabled
                && job.wake_mode == WakeMode::OnTerminal(delivered_child)
                && job.last_fired_at.is_some()
        }));
        assert!(
            jobs.iter().any(|job| {
                job.enabled && job.wake_mode == WakeMode::OnTerminal(terminal_child)
            })
        );
    }

    #[tokio::test]
    async fn terminal_watch_restart_rearms_terminal_child_missing_its_watch() {
        let (control, store) = control_handle_with_store();
        let owner_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let terminal_child = Uuid::new_v4();
        let mut owner = test_session(owner_id, std::path::PathBuf::from("/tmp"));
        owner.status = SessionStatus::Running;
        let mut epic = test_session(epic_id, std::path::PathBuf::from("/tmp"));
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(owner_id);
        {
            let guard = store.lock().await;
            guard.insert_session(&owner).expect("insert owner");
            guard.insert_session(&epic).expect("insert epic");
            insert_launched_agent_child(
                &guard,
                owner_id,
                epic_id,
                terminal_child,
                SessionStatus::Completed,
                "terminal-missing-watch",
            );
        }

        control
            .reconcile_automatic_child_watches()
            .await
            .expect("repair terminal child's missing watch");
        assert_eq!(enabled_watch_count(&store, owner_id).await, 1);
        let jobs = store
            .lock()
            .await
            .list_scheduled_jobs()
            .expect("list watches");
        assert!(
            jobs.iter().any(|job| {
                job.enabled && job.wake_mode == WakeMode::OnTerminal(terminal_child)
            })
        );
    }

    #[tokio::test]
    async fn manager_watch_insert_does_not_advance_child_arm_witness() {
        let (control, store) = control_handle_with_store();
        let owner_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let mut owner = test_session(owner_id, std::path::PathBuf::from("/tmp"));
        owner.status = SessionStatus::Running;
        let mut epic = test_session(epic_id, std::path::PathBuf::from("/tmp"));
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(owner_id);
        let guard = store.lock().await;
        let now = chrono::Utc::now();
        let project = rsi_common::types::Project {
            id: Uuid::new_v4(),
            name: "manager watch witness isolation".into(),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        guard.insert_project(&project).expect("insert project");
        guard.insert_session(&owner).expect("insert owner");
        guard.insert_session(&epic).expect("insert epic");
        insert_launched_agent_child(
            &guard,
            owner_id,
            epic_id,
            child_id,
            SessionStatus::Running,
            "manager-watch-collision",
        );
        let manager_watch = watch_candidate(owner_id, child_id);
        crate::store::scheduled_jobs::insert_harness_manager_watch_job_conn(
            &guard.conn,
            &manager_watch,
        )
        .expect("insert manager watch with colliding natural key");
        guard.conn.execute(
            "INSERT INTO harness_manager_watches(job_id,project_id,epic_id,scope_version,direction,source_session_id,target_session_id,attention_signature) VALUES (?1,?2,?3,1,'to_lead',?4,?5,'notice')",
            rusqlite::params![manager_watch.id.to_string(), project.id.to_string(), epic_id.to_string(), child_id.to_string(), owner_id.to_string()],
        ).expect("bind manager watch identity");
        guard
            .update_scheduled_job(
                &manager_watch.id,
                &crate::store::scheduled_jobs::ScheduledJobUpdate {
                    name: None,
                    message: None,
                    schedule: None,
                    enabled: Some(false),
                    next_fire_at: None,
                },
            )
            .expect("retire manager watch");
        guard
            .update_scheduled_job(
                &manager_watch.id,
                &crate::store::scheduled_jobs::ScheduledJobUpdate {
                    name: None,
                    message: None,
                    schedule: None,
                    enabled: Some(true),
                    next_fire_at: None,
                },
            )
            .expect("rearm manager watch");
        let state: String = guard.conn.query_row(
            "SELECT state FROM agent_child_watch_witness WHERE owner_session_id=?1 AND child_session_id=?2",
            rusqlite::params![owner_id.to_string(), child_id.to_string()],
            |row| row.get(0),
        ).expect("read child arm intent");
        assert_eq!(
            state, "pending",
            "manager watch leaves the child arm intent available for repair"
        );
        drop(guard);
        control
            .reconcile_automatic_child_watches()
            .await
            .expect("repair child watch despite manager watch sharing its natural key");
        let guard = store.lock().await;
        let jobs = guard.list_scheduled_jobs().expect("load watches");
        assert_eq!(
            jobs.iter()
                .filter(|job| {
                    job.enabled
                        && job.wake_session_id == Some(owner_id)
                        && job.wake_mode == WakeMode::OnTerminal(child_id)
                        && !guard
                            .is_harness_manager_watch(job.id)
                            .expect("watch identity")
                })
                .count(),
            1,
            "child repair arms its own watch"
        );
        assert!(
            jobs.iter()
                .any(|job| job.id == manager_watch.id && job.enabled),
            "the manager watch keeps its own enabled state"
        );
    }

    #[tokio::test]
    async fn terminal_watch_restart_respects_explicit_retirement_and_owner_state() {
        let (control, store) = control_handle_with_store();
        let owner_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let consumed_child = Uuid::new_v4();
        let abandoned_child = Uuid::new_v4();
        let disabled_child = Uuid::new_v4();
        let toggled_child = Uuid::new_v4();
        let deleted_child = Uuid::new_v4();
        let mut owner = test_session(owner_id, std::path::PathBuf::from("/tmp"));
        owner.status = SessionStatus::Running;
        let mut epic = test_session(epic_id, std::path::PathBuf::from("/tmp"));
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(owner_id);
        {
            let guard = store.lock().await;
            guard.insert_session(&owner).expect("insert owner");
            guard.insert_session(&epic).expect("insert epic");
            for (child, key) in [
                (consumed_child, "terminal-consumed"),
                (abandoned_child, "live-abandoned"),
                (disabled_child, "fired-then-disabled"),
                (toggled_child, "fired-then-toggled"),
                (deleted_child, "operator-deleted"),
            ] {
                insert_launched_agent_child(
                    &guard,
                    owner_id,
                    epic_id,
                    child,
                    if child == abandoned_child {
                        SessionStatus::Running
                    } else {
                        SessionStatus::Completed
                    },
                    key,
                );
            }
            let consumed = watch_candidate(owner_id, consumed_child);
            guard.insert_scheduled_job(&consumed).expect("arm consumed");
            let generic_retirement = guard
                .update_scheduled_job_fired(&consumed.id, &chrono::Utc::now(), None, false)
                .expect_err("generic fired timestamp is not delivery proof");
            assert!(
                generic_retirement
                    .to_string()
                    .contains("requires explicit retirement disposition")
            );
            assert!(
                guard
                    .retire_unchanged_child_watch(&consumed, "consumed")
                    .expect("confirm consumed")
            );
            let abandoned = watch_candidate(owner_id, abandoned_child);
            guard
                .insert_scheduled_job(&abandoned)
                .expect("arm abandoned");
            assert!(
                guard
                    .retire_unchanged_child_watch(&abandoned, "abandoned")
                    .expect("abandon watch")
            );

            let disabled = watch_candidate(owner_id, disabled_child);
            guard.insert_scheduled_job(&disabled).expect("arm disabled");
            guard
                .update_scheduled_job_fired(&disabled.id, &chrono::Utc::now(), None, true)
                .expect("record prior fire");
            guard
                .update_scheduled_job(
                    &disabled.id,
                    &crate::store::scheduled_jobs::ScheduledJobUpdate {
                        name: None,
                        message: None,
                        schedule: None,
                        enabled: Some(false),
                        next_fire_at: None,
                    },
                )
                .expect("operator disable after fire");

            let toggled = watch_candidate(owner_id, toggled_child);
            guard.insert_scheduled_job(&toggled).expect("arm toggled");
            guard
                .update_scheduled_job_fired(&toggled.id, &chrono::Utc::now(), None, true)
                .expect("record prior fire before toggle");
            assert!(
                !guard
                    .toggle_scheduled_job(&toggled.id)
                    .expect("operator toggle off")
            );

            let deleted = watch_candidate(owner_id, deleted_child);
            guard.insert_scheduled_job(&deleted).expect("arm deleted");
            guard
                .delete_scheduled_job(&deleted.id)
                .expect("operator delete");
        }

        control
            .reconcile_automatic_child_watches()
            .await
            .expect("restart reconciliation");
        let guard = store.lock().await;
        let jobs = guard.list_scheduled_jobs().expect("list watches");
        assert_eq!(jobs.len(), 4);
        assert!(
            jobs.iter().any(|job| {
                job.wake_mode == WakeMode::OnTerminal(consumed_child) && !job.enabled
            })
        );
        assert!(
            jobs.iter().any(|job| {
                job.wake_mode == WakeMode::OnTerminal(abandoned_child) && !job.enabled
            })
        );
        assert!(
            jobs.iter().any(|job| {
                job.wake_mode == WakeMode::OnTerminal(disabled_child) && !job.enabled
            })
        );
        for child in [
            consumed_child,
            abandoned_child,
            disabled_child,
            toggled_child,
        ] {
            assert_eq!(
                jobs.iter()
                    .filter(|job| job.wake_mode == WakeMode::OnTerminal(child) && !job.enabled)
                    .count(),
                1,
                "each retained watch has exactly one disabled row"
            );
            assert_eq!(
                jobs.iter()
                    .filter(|job| job.wake_mode == WakeMode::OnTerminal(child) && job.enabled)
                    .count(),
                0,
                "retired or operator-disabled watch stays disabled"
            );
        }
        assert!(
            jobs.iter()
                .all(|job| job.wake_mode != WakeMode::OnTerminal(deleted_child)),
            "the deleted watch stays absent"
        );
        assert!(
            jobs.iter()
                .find(|job| job.wake_mode == WakeMode::OnTerminal(consumed_child))
                .and_then(|job| job.last_fired_at)
                .is_some(),
            "confirmed terminal history retains its delivery timestamp"
        );
        for (child, expected) in [
            (consumed_child, "consumed"),
            (abandoned_child, "abandoned"),
            (disabled_child, "disabled"),
            (toggled_child, "disabled"),
            (deleted_child, "deleted"),
        ] {
            let state: String = guard
                .conn
                .query_row(
                    "SELECT state FROM agent_child_watch_witness
                     WHERE owner_session_id=?1 AND child_session_id=?2",
                    rusqlite::params![owner_id.to_string(), child.to_string()],
                    |row| row.get(0),
                )
                .expect("durable retirement witness");
            assert_eq!(state, expected);
        }
    }

    #[tokio::test]
    async fn terminal_watch_restart_repairs_completed_owner_with_v123_arm_intent() {
        let (control, store) = control_handle_with_store();
        let completed_owner_id = Uuid::new_v4();
        let live_owner_id = Uuid::new_v4();
        let completed_epic_id = Uuid::new_v4();
        let live_epic_id = Uuid::new_v4();
        let pending_child = Uuid::new_v4();
        let legacy_child = Uuid::new_v4();
        let mut completed_owner =
            test_session(completed_owner_id, std::path::PathBuf::from("/tmp"));
        completed_owner.status = SessionStatus::Running;
        let mut live_owner = test_session(live_owner_id, std::path::PathBuf::from("/tmp"));
        live_owner.status = SessionStatus::Running;
        let mut completed_epic = test_session(completed_epic_id, std::path::PathBuf::from("/tmp"));
        completed_epic.session_kind = SessionKind::Epic;
        completed_epic.lead_session_id = Some(completed_owner_id);
        let mut live_epic = test_session(live_epic_id, std::path::PathBuf::from("/tmp"));
        live_epic.session_kind = SessionKind::Epic;
        live_epic.lead_session_id = Some(live_owner_id);
        {
            let guard = store.lock().await;
            guard
                .insert_session(&completed_owner)
                .expect("completed owner");
            guard.insert_session(&live_owner).expect("live owner");
            guard
                .insert_session(&completed_epic)
                .expect("completed epic");
            guard.insert_session(&live_epic).expect("live epic");
            insert_launched_agent_child(
                &guard,
                completed_owner_id,
                completed_epic_id,
                pending_child,
                SessionStatus::Completed,
                "completed-owner-pending",
            );
            guard
                .conn
                .execute(
                    "UPDATE sessions SET status='Completed' WHERE id=?1",
                    [completed_owner_id.to_string()],
                )
                .expect("settle completed owner");
            insert_launched_agent_child(
                &guard,
                live_owner_id,
                live_epic_id,
                legacy_child,
                SessionStatus::Completed,
                "legacy-missing-watch",
            );
            guard
                .conn
                .execute(
                    "DELETE FROM agent_child_watch_witness
                     WHERE owner_session_id=?1 AND child_session_id=?2",
                    rusqlite::params![live_owner_id.to_string(), legacy_child.to_string()],
                )
                .expect("model pre-V123 missing history");
        }
        control
            .reconcile_automatic_child_watches()
            .await
            .expect("restart reconciliation");
        assert_eq!(enabled_watch_count(&store, completed_owner_id).await, 1);
        assert_eq!(enabled_watch_count(&store, live_owner_id).await, 0);
        let guard = store.lock().await;
        assert!(
            guard
                .conn
                .query_row(
                    "SELECT state FROM agent_child_watch_witness
                     WHERE owner_session_id=?1 AND child_session_id=?2",
                    rusqlite::params![completed_owner_id.to_string(), pending_child.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .is_ok(),
            "completed owner retains its V123 witness for the repaired watch"
        );
    }

    #[tokio::test]
    async fn terminal_watch_restart_historical_children_do_not_starve_missing_live_watch() {
        let (control, store) = control_handle_with_store();
        let owner_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let live_child = Uuid::new_v4();
        let mut owner = test_session(owner_id, std::path::PathBuf::from("/tmp"));
        owner.status = SessionStatus::Running;
        let mut epic = test_session(epic_id, std::path::PathBuf::from("/tmp"));
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(owner_id);
        {
            let guard = store.lock().await;
            guard.insert_session(&owner).expect("insert owner");
            guard.insert_session(&epic).expect("insert epic");
            for index in 0..MAX_TERMINAL_WATCHES_PER_MASTER {
                let child_id = Uuid::new_v4();
                let key = format!("historical-watch-{index}");
                insert_launched_agent_child(
                    &guard,
                    owner_id,
                    epic_id,
                    child_id,
                    SessionStatus::Completed,
                    &key,
                );
                let watch = watch_candidate(owner_id, child_id);
                guard
                    .insert_scheduled_job(&watch)
                    .expect("insert historical watch");
                guard
                    .retire_unchanged_child_watch(&watch, "consumed")
                    .expect("consume historical watch");
            }
            insert_launched_agent_child(
                &guard,
                owner_id,
                epic_id,
                live_child,
                SessionStatus::Running,
                "live-missing-watch",
            );
        }

        control
            .reconcile_automatic_child_watches()
            .await
            .expect("repair missing live watch");
        assert_eq!(enabled_watch_count(&store, owner_id).await, 1);
        let jobs = store
            .lock()
            .await
            .list_scheduled_jobs()
            .expect("list watches");
        assert_eq!(
            jobs.iter()
                .filter(|job| job.wake_mode == WakeMode::OnTerminal(live_child))
                .count(),
            1
        );
    }

    /// A8.1 F-2 (a): N concurrent IDENTICAL arms — because dedup + insert
    /// share one store-lock critical section, exactly one task wins the
    /// insert and every other caller gets `Deduplicated` with the winner's
    /// row. The pre-A8.1 split (check under one lock scope, insert under a
    /// later one) allowed double-inserts here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_identical_arms_insert_exactly_one_row() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let watched = Uuid::new_v4();

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let control = control.clone();
            tasks.push(tokio::spawn(async move {
                control
                    .arm_terminal_watch(caller, watch_candidate(caller, watched))
                    .await
            }));
        }

        let mut armed_ids = Vec::new();
        let mut dedup_ids = Vec::new();
        for task in tasks {
            match task.await.expect("join") {
                Ok(ArmWatchOutcome::Armed(job)) => armed_ids.push(job.id),
                Ok(ArmWatchOutcome::Deduplicated(job)) => dedup_ids.push(job.id),
                Err(e) => panic!("unexpected arm error: {e}"),
            }
        }
        assert_eq!(armed_ids.len(), 1, "exactly one task must win the insert");
        assert_eq!(dedup_ids.len(), 7, "every other caller must deduplicate");
        assert!(
            dedup_ids.iter().all(|id| *id == armed_ids[0]),
            "dedup must return the winner's existing row"
        );

        let rows: Vec<ScheduledJob> = store
            .lock()
            .await
            .list_scheduled_jobs()
            .expect("list jobs")
            .into_iter()
            .filter(|j| j.wake_mode == WakeMode::OnTerminal(watched))
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "identical concurrent arms must not double-insert"
        );
        assert_eq!(rows[0].id, armed_ids[0]);
    }

    /// A8.1 F-2 (b): concurrent DISTINCT arms racing for the last slots below
    /// `MAX_TERMINAL_WATCHES_PER_MASTER` — the enabled-watch count never
    /// overshoots the cap, and exactly the free slots are won.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_distinct_arms_never_exceed_cap() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();

        // Preload all but 4 slots with enabled watches on distinct subjects.
        {
            let guard = store.lock().await;
            for _ in 0..(MAX_TERMINAL_WATCHES_PER_MASTER - 4) {
                guard
                    .insert_scheduled_job(&watch_candidate(caller, Uuid::new_v4()))
                    .expect("preload watch");
            }
        }

        // 8 concurrent distinct arms compete for the 4 remaining slots.
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let control = control.clone();
            tasks.push(tokio::spawn(async move {
                control
                    .arm_terminal_watch(caller, watch_candidate(caller, Uuid::new_v4()))
                    .await
            }));
        }

        let mut armed = 0usize;
        let mut capped = 0usize;
        for task in tasks {
            match task.await.expect("join") {
                Ok(ArmWatchOutcome::Armed(_)) => armed += 1,
                Ok(ArmWatchOutcome::Deduplicated(_)) => {
                    panic!("distinct watched subjects cannot deduplicate")
                }
                Err(e) => {
                    assert!(
                        e.to_string().contains("terminal_watch_cap_reached"),
                        "cap rejection must keep its error text, got: {e}"
                    );
                    capped += 1;
                }
            }
        }
        assert_eq!(armed, 4, "exactly the free slots may be won");
        assert_eq!(capped, 4, "every arm past the cap must be rejected");
        assert_eq!(
            enabled_watch_count(&store, caller).await,
            MAX_TERMINAL_WATCHES_PER_MASTER,
            "enabled watch count must never exceed the cap"
        );
    }

    /// A8.1 F-2 (c): a sequential re-arm of the same (caller, watched) key
    /// returns the EXISTING job id (not the discarded candidate's) and
    /// inserts nothing.
    #[tokio::test]
    async fn rearm_returns_existing_job_id() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let watched = Uuid::new_v4();

        let first = match control
            .arm_terminal_watch(caller, watch_candidate(caller, watched))
            .await
            .expect("first arm")
        {
            ArmWatchOutcome::Armed(job) => job,
            other => panic!("first arm must insert, got {other:?}"),
        };

        let second_candidate = watch_candidate(caller, watched);
        assert_ne!(second_candidate.id, first.id, "candidates get fresh ids");
        match control
            .arm_terminal_watch(caller, second_candidate)
            .await
            .expect("re-arm")
        {
            ArmWatchOutcome::Deduplicated(existing) => assert_eq!(existing.id, first.id),
            other => panic!("re-arm must deduplicate, got {other:?}"),
        }
        assert_eq!(enabled_watch_count(&store, caller).await, 1);
    }

    #[tokio::test]
    async fn continued_child_reuses_watch_with_new_delivery_epoch() {
        let (control, store) = control_handle_with_store();
        let owner = Uuid::new_v4();
        let child = Uuid::new_v4();
        let mut owner_row = test_session(owner, std::path::PathBuf::from("/tmp"));
        owner_row.status = SessionStatus::Completed;
        store.lock().await.insert_session(&owner_row).unwrap();
        let watch = watch_candidate(owner, child);
        store.lock().await.insert_scheduled_job(&watch).unwrap();
        let old_attempt = chrono::Utc::now() - chrono::Duration::seconds(10);
        store
            .lock()
            .await
            .update_scheduled_job_fired(&watch.id, &old_attempt, None, true)
            .unwrap();

        let observed = store
            .lock()
            .await
            .get_scheduled_job(&watch.id)
            .unwrap()
            .unwrap();
        assert!(matches!(
            control
                .arm_terminal_watch(owner, watch_candidate(owner, child))
                .await
                .unwrap(),
            ArmWatchOutcome::Deduplicated(_)
        ));
        assert_eq!(
            store
                .lock()
                .await
                .get_scheduled_job(&watch.id)
                .unwrap()
                .unwrap()
                .last_fired_at,
            Some(old_attempt),
            "manual dedup preserves the current delivery witness"
        );

        assert!(control.rearm_child_watch_after_continue(owner, child).await);
        assert!(
            !store
                .lock()
                .await
                .retire_unchanged_child_watch(&observed, "consumed")
                .unwrap(),
            "a Confirmed retirement planned before continued-child rearm loses its row fence"
        );
        let jobs = store.lock().await.list_scheduled_jobs().unwrap();
        assert_eq!(jobs.len(), 1, "the natural-key watch remains single");
        assert_eq!(jobs[0].id, watch.id);
        assert!(jobs[0].enabled);
        assert_eq!(jobs[0].last_fired_at, None);
    }

    /// A8.1 review F2 (defense-in-depth): a candidate whose wake target is
    /// bound to anyone but the arming caller — or that watches the caller
    /// itself — is rejected by the service before any row is written.
    #[tokio::test]
    async fn arm_rejects_misbound_candidate_wake_target() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let watched = Uuid::new_v4();

        // Candidate wake-bound to a DIFFERENT session than the arming caller.
        let misbound = watch_candidate(Uuid::new_v4(), watched);
        let err = control
            .arm_terminal_watch(caller, misbound)
            .await
            .expect_err("misbound candidate must be rejected");
        assert!(
            err.to_string()
                .contains("candidate wake target must be the caller"),
            "{err}"
        );

        // Self-watch candidate: same service-level rejection text as
        // `authorize_watch_target`.
        let err = control
            .arm_terminal_watch(caller, watch_candidate(caller, caller))
            .await
            .expect_err("self-watch candidate must be rejected");
        assert!(err.to_string().contains("watch_self_rejected"), "{err}");

        assert!(
            store
                .lock()
                .await
                .list_scheduled_jobs()
                .expect("list jobs")
                .is_empty(),
            "rejected candidates must not leave rows behind"
        );
    }

    /// A8.1 review F6: the non-`OnTerminal` candidate guard arm — a
    /// fresh-mode job handed to the watch service is `InvalidParam`, and
    /// nothing is inserted.
    #[tokio::test]
    async fn arm_rejects_non_on_terminal_candidate() {
        use crate::session::harness::tools::schedule_wake::{
            ScheduleWakeRequest, build_scheduled_job,
        };

        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();

        let fresh = build_scheduled_job(ScheduleWakeRequest {
            message: "not a watch".to_string(),
            in_seconds: Some(60),
            at: None,
            name: None,
            every_seconds: None,
            mode: None,
            working_dir: std::path::PathBuf::from("/tmp"),
            provider: None,
            model: None,
            project_id: None,
            origin_session_id: Some(caller),
            watch_session_id: None,
        })
        .expect("valid fresh job");

        let err = control
            .arm_terminal_watch(caller, fresh)
            .await
            .expect_err("non-on_terminal candidate must be rejected");
        assert!(
            err.to_string()
                .contains("requires an on_terminal candidate"),
            "{err}"
        );
        assert!(
            store
                .lock()
                .await
                .list_scheduled_jobs()
                .expect("list jobs")
                .is_empty(),
            "rejected candidate must not leave rows behind"
        );
    }
}

// ---- P2-03 `AgentSendMessage` authority ------------------------------------
//
// These prove the authority half of P2-03. The Store half (idempotent
// acceptance, caps, the immutable version-0 acceptance edge) is covered in
// `store::agent_coordination::tests`; what is proved here is that authority is
// decided BEFORE the payload reaches SQLite, and that the send scope is
// strictly narrower than the `AgentGetStatus`/`AgentHalt` scope.

#[cfg(test)]
mod send_message_tests {
    use super::tests::{
        control_handle_with_store, control_handle_with_store_and_bus, test_session,
    };
    use super::*;
    use crate::store::Store;
    use rsi_common::agent_coordination::{AgentMessageErrorCodeV1, AgentMessageStateV1};
    use rsi_common::types::SessionStatus;

    fn send(target: Uuid, key: &str) -> AgentSendMessageRequestV1 {
        AgentSendMessageRequestV1 {
            target_session_id: target,
            message: "do the thing".to_string(),
            idempotency_key: key.to_string(),
            expires_at: None,
        }
    }

    /// Insert a live session row with an explicit kind/parent/lead shape.
    async fn insert(
        store: &Arc<tokio::sync::Mutex<Store>>,
        id: Uuid,
        kind: SessionKind,
        parent_id: Option<Uuid>,
        lead_session_id: Option<Uuid>,
    ) {
        let mut row = test_session(id, std::path::PathBuf::from("/tmp"));
        row.session_kind = kind;
        row.status = SessionStatus::Running;
        row.parent_id = parent_id;
        row.lead_session_id = lead_session_id;
        store.lock().await.insert_session(&row).expect("insert row");
    }

    /// Reserve a spawn owned by `owner` whose child id has NO session row yet.
    async fn reserve(
        store: &Arc<tokio::sync::Mutex<Store>>,
        owner: Uuid,
        epic_id: Uuid,
        child_id: Uuid,
        key: &str,
    ) -> Uuid {
        let request = AgentSpawnChildRequestV1 {
            kind: SessionKind::Task,
            provider: None,
            model: None,
            effort: None,
            query: "work".into(),
            agent_role: None,
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: key.into(),
        };
        let spawn_request_id = Uuid::new_v4();
        store
            .lock()
            .await
            .reserve_agent_spawn_request(
                owner,
                &crate::model_control::hash_request_fingerprint(&[
                    "agent-spawn-idempotency-v1",
                    &owner.to_string(),
                    &request.idempotency_key,
                ]),
                &crate::model_control::hash_request_fingerprint(&[
                    "agent-spawn-request-v1",
                    &owner.to_string(),
                    &serde_json::to_string(&request).expect("serialize"),
                ]),
                &request,
                epic_id,
                spawn_request_id,
                child_id,
            )
            .expect("reserve spawn");
        spawn_request_id
    }

    async fn message_row_count(store: &Arc<tokio::sync::Mutex<Store>>) -> i64 {
        store
            .lock()
            .await
            .conn
            .query_row("SELECT COUNT(*) FROM agent_messages", [], |row| row.get(0))
            .expect("count agent messages")
    }

    fn assert_code(error: &DaemonError, expected: AgentMessageErrorCodeV1) {
        let DaemonError::StructuredRpc { data, message, .. } = error else {
            panic!("expected a typed StructuredRpc messaging error, got: {error}");
        };
        assert_eq!(
            data["code"],
            serde_json::to_value(expected).expect("serialize code"),
            "wrong typed code in {data}"
        );
        assert!(
            message.starts_with(expected.as_str()),
            "Display must keep the stable class prefix: {message}"
        );
        assert!(
            data["next_action"].as_str().is_some_and(|s| !s.is_empty()),
            "every typed messaging error must advertise a next action: {data}"
        );
    }

    #[tokio::test]
    async fn agent_send_message_accepts_a_direct_child_and_binds_no_reservation() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let child = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, None, None).await;
        insert(&store, child, SessionKind::Task, Some(caller), None).await;

        let receipt = control
            .agent_send_message(caller, send(child, "k-direct"))
            .await
            .expect("a direct child is an authorized send target");

        assert_eq!(receipt.target_session_id, child);
        assert_eq!(receipt.state, AgentMessageStateV1::Queued);
        assert_eq!(receipt.state_version, 0);
        assert!(!receipt.deduplicated);
        assert!(
            receipt.target_spawn_request_id.is_none(),
            "a live Session target binds no reservation"
        );
        assert_eq!(message_row_count(&store).await, 1);
    }

    #[tokio::test]
    async fn agent_send_message_refuses_a_terminal_current_target_at_acceptance() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let child = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, None, None).await;
        let mut terminal = test_session(child, std::path::PathBuf::from("/tmp"));
        terminal.parent_id = Some(caller);
        terminal.status = SessionStatus::Completed;
        store
            .lock()
            .await
            .insert_session(&terminal)
            .expect("insert row");

        let error = control
            .agent_send_message(caller, send(child, "k-terminal"))
            .await
            .expect_err("a terminal target cannot receive a newly accepted message");
        assert_code(&error, AgentMessageErrorCodeV1::TargetTerminal);
        assert_eq!(message_row_count(&store).await, 0);
    }

    #[tokio::test]
    async fn agent_send_message_accepts_a_terminal_root_with_a_live_rotation_tip() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let root = Uuid::new_v4();
        let tip = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, None, None).await;
        let mut predecessor = test_session(root, std::path::PathBuf::from("/tmp"));
        predecessor.parent_id = Some(caller);
        predecessor.status = SessionStatus::Completed;
        store
            .lock()
            .await
            .insert_session(&predecessor)
            .expect("insert predecessor");
        let mut successor = test_session(tip, std::path::PathBuf::from("/tmp"));
        successor.parent_id = Some(caller);
        successor.continued_from = Some(root);
        successor.status = SessionStatus::Running;
        store
            .lock()
            .await
            .insert_session(&successor)
            .expect("insert successor");

        let receipt = control
            .agent_send_message(caller, send(root, "k-rotated"))
            .await
            .expect("the logical target remains deliverable through its live tip");
        assert_eq!(receipt.target_session_id, root);
        assert_eq!(receipt.state, AgentMessageStateV1::Queued);
        assert_eq!(message_row_count(&store).await, 1);
    }

    #[tokio::test]
    async fn agent_send_message_accepts_a_child_of_an_epic_the_caller_leads() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let epic = Uuid::new_v4();
        let sibling = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, Some(epic), None).await;
        insert(&store, epic, SessionKind::Epic, None, Some(caller)).await;
        insert(&store, sibling, SessionKind::Task, Some(epic), None).await;

        let receipt = control
            .agent_send_message(caller, send(sibling, "k-epic"))
            .await
            .expect("a lead may message a child of the Epic it leads");
        assert_eq!(receipt.target_session_id, sibling);
        assert_eq!(message_row_count(&store).await, 1);
    }

    /// Self-send is denied even though `AgentGetStatus`/`AgentHalt` admit self.
    /// A session mailing itself is a self-injection loop with no owner to
    /// settle it.
    #[tokio::test]
    async fn agent_send_message_denies_self_and_persists_nothing() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, None, None).await;

        let error = control
            .agent_send_message(caller, send(caller, "k-self"))
            .await
            .expect_err("self-send must be refused");
        assert_code(&error, AgentMessageErrorCodeV1::TargetNotAuthorized);
        assert_eq!(
            message_row_count(&store).await,
            0,
            "an unauthorized send must persist no payload at all"
        );

        // The same target IS reachable through the wider status scope, which
        // is exactly the difference this test pins.
        control
            .agent_get_status(caller, caller)
            .await
            .expect("AgentGetStatus still admits self");
    }

    #[tokio::test]
    async fn agent_send_message_denies_a_foreign_session_and_persists_nothing() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, None, None).await;
        insert(&store, stranger, SessionKind::Task, None, None).await;

        let error = control
            .agent_send_message(caller, send(stranger, "k-foreign"))
            .await
            .expect_err("an unrelated session is not a send target");
        assert_code(&error, AgentMessageErrorCodeV1::TargetNotAuthorized);
        assert_eq!(message_row_count(&store).await, 0);
    }

    #[tokio::test]
    async fn agent_send_message_denies_a_child_of_an_epic_led_by_someone_else() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let other_lead = Uuid::new_v4();
        let epic = Uuid::new_v4();
        let child = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, None, None).await;
        insert(&store, other_lead, SessionKind::Task, None, None).await;
        insert(&store, epic, SessionKind::Epic, None, Some(other_lead)).await;
        insert(&store, child, SessionKind::Task, Some(epic), None).await;

        let error = control
            .agent_send_message(caller, send(child, "k-cross-epic"))
            .await
            .expect_err("cross-Epic send must be refused");
        assert_code(&error, AgentMessageErrorCodeV1::TargetNotAuthorized);
        assert_eq!(message_row_count(&store).await, 0);
    }

    /// P2-03: "a non-Epic container or leaf with a matching `lead_session_id`
    /// never grants authority". `lead_session_id` is only meaningful on a
    /// container kind, so a parent carrying one without being an Epic must not
    /// be treated as one.
    #[tokio::test]
    async fn agent_send_message_denies_a_non_epic_parent_carrying_a_matching_lead() {
        for parent_kind in [SessionKind::Group, SessionKind::Task, SessionKind::Standard] {
            let (control, store) = control_handle_with_store();
            let caller = Uuid::new_v4();
            let parent = Uuid::new_v4();
            let child = Uuid::new_v4();
            insert(&store, caller, SessionKind::Task, None, None).await;
            insert(&store, parent, parent_kind, None, Some(caller)).await;
            insert(&store, child, SessionKind::Task, Some(parent), None).await;

            let error = control
                .agent_send_message(caller, send(child, "k-non-epic"))
                .await
                .err()
                .unwrap_or_else(|| panic!("{parent_kind:?} parent must not grant send authority"));
            assert_code(&error, AgentMessageErrorCodeV1::TargetNotAuthorized);
            assert_eq!(
                message_row_count(&store).await,
                0,
                "{parent_kind:?} parent denial must persist nothing"
            );

            // The wider `AgentGetStatus` scope DOES admit this target — that
            // difference is precisely what P2-03 tightens.
            control
                .agent_get_status(caller, child)
                .await
                .expect("the legacy status scope is intentionally wider");
        }
    }

    #[tokio::test]
    async fn agent_send_message_binds_the_callers_own_reservation_before_launch() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let epic = Uuid::new_v4();
        let child = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, Some(epic), None).await;
        insert(&store, epic, SessionKind::Epic, None, Some(caller)).await;
        let spawn_request_id = reserve(&store, caller, epic, child, "res-1").await;

        let receipt = control
            .agent_send_message(caller, send(child, "k-reserved"))
            .await
            .expect("mail may be queued into the pre-launch window");
        assert_eq!(receipt.target_session_id, child);
        assert_eq!(
            receipt.target_spawn_request_id,
            Some(spawn_request_id),
            "a target with no Session row binds exactly the caller's reservation"
        );
    }

    #[tokio::test]
    async fn agent_send_message_denies_a_reservation_owned_by_another_session() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let other_owner = Uuid::new_v4();
        let epic = Uuid::new_v4();
        let child = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, None, None).await;
        insert(&store, other_owner, SessionKind::Task, Some(epic), None).await;
        insert(&store, epic, SessionKind::Epic, None, Some(other_owner)).await;
        reserve(&store, other_owner, epic, child, "res-foreign").await;

        let error = control
            .agent_send_message(caller, send(child, "k-foreign-res"))
            .await
            .expect_err("another Session's reservation grants nothing");
        assert_code(&error, AgentMessageErrorCodeV1::TargetNotAuthorized);
        assert_eq!(message_row_count(&store).await, 0);
    }

    #[tokio::test]
    async fn agent_send_message_reports_an_unknown_target_as_target_unknown() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, None, None).await;

        let error = control
            .agent_send_message(caller, send(Uuid::new_v4(), "k-unknown"))
            .await
            .expect_err("a target with neither a row nor a reservation is unknown");
        assert_code(&error, AgentMessageErrorCodeV1::TargetUnknown);
        assert_eq!(message_row_count(&store).await, 0);
    }

    /// A reservation that permanently failed will never become a Session, and
    /// the V81 `agent_messages_v81_target_custody` trigger refuses it. Prove
    /// the verb refuses it FIRST, with a typed class rather than a raw SQL
    /// abort leaking through.
    #[tokio::test]
    async fn agent_send_message_refuses_a_permanently_failed_reservation() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let epic = Uuid::new_v4();
        let child = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, Some(epic), None).await;
        insert(&store, epic, SessionKind::Epic, None, Some(caller)).await;
        let spawn_request_id = reserve(&store, caller, epic, child, "res-dead").await;
        store
            .lock()
            .await
            .mark_agent_spawn_failed(spawn_request_id, "spawn_channel_closed")
            .expect("settle failed spawn");

        let error = control
            .agent_send_message(caller, send(child, "k-dead-res"))
            .await
            .expect_err("a failed reservation is not a live target");
        assert_code(&error, AgentMessageErrorCodeV1::TargetUnknown);
        assert_eq!(message_row_count(&store).await, 0);
    }

    /// Payload bounds are refused before authority is even consulted, so an
    /// oversized send cannot be used to probe the topology.
    #[tokio::test]
    async fn agent_send_message_rejects_malformed_requests_before_any_store_write() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let child = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, None, None).await;
        insert(&store, child, SessionKind::Task, Some(caller), None).await;

        let mut oversized = send(child, "k-big");
        oversized.message =
            "x".repeat(rsi_common::agent_coordination::AGENT_MESSAGE_MAX_PAYLOAD_BYTES + 1);
        control
            .agent_send_message(caller, oversized)
            .await
            .expect_err("an oversized payload must be refused");

        let mut empty_key = send(child, "");
        empty_key.idempotency_key = String::new();
        control
            .agent_send_message(caller, empty_key)
            .await
            .expect_err("an empty idempotency key must be refused");

        assert_eq!(message_row_count(&store).await, 0);
    }

    #[tokio::test]
    async fn agent_send_message_exact_replay_returns_the_original_receipt() {
        let (control, store) = control_handle_with_store();
        let caller = Uuid::new_v4();
        let child = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, None, None).await;
        insert(&store, child, SessionKind::Task, Some(caller), None).await;

        let first = control
            .agent_send_message(caller, send(child, "k-replay"))
            .await
            .expect("first send");
        let second = control
            .agent_send_message(caller, send(child, "k-replay"))
            .await
            .expect("exact replay");

        assert_eq!(first.message_id, second.message_id);
        assert_eq!(first.created_at, second.created_at);
        assert!(!first.deduplicated);
        assert!(second.deduplicated, "the replay must be reported as such");
        assert_eq!(
            message_row_count(&store).await,
            1,
            "an exact replay must not create a second durable row"
        );
    }

    /// The cursor-bearing state event is published only after the Store
    /// transaction commits, and a denied send publishes nothing.
    #[tokio::test]
    async fn agent_send_message_publishes_a_cursor_bearing_event_only_after_commit() {
        let (control, store, bus) = control_handle_with_store_and_bus();
        let caller = Uuid::new_v4();
        let child = Uuid::new_v4();
        insert(&store, caller, SessionKind::Task, None, None).await;
        insert(&store, child, SessionKind::Task, Some(caller), None).await;
        let mut events = bus.subscribe();

        // A denied send must publish nothing.
        control
            .agent_send_message(caller, send(Uuid::new_v4(), "k-denied"))
            .await
            .expect_err("unknown target");

        let receipt = control
            .agent_send_message(caller, send(child, "k-event"))
            .await
            .expect("accepted send");

        let published = events.try_recv().expect("exactly one event was published");
        let crate::bus::DaemonEvent::AgentMessageState { event } = published.as_ref() else {
            panic!("first published event must be the message-state event");
        };
        assert_eq!(event.message_id, receipt.message_id);
        assert_eq!(event.owner_session_id, caller);
        assert_eq!(
            event.target_session_id, child,
            "the event is rooted on the immutable logical target, never a tip"
        );
        assert_eq!(event.state, AgentMessageStateV1::Queued);
        assert_eq!(event.state_version, 0);
        assert_eq!(event.attempt_count, 0);
        assert!(event.current_attempt_number.is_none());

        // The envelope must not carry the payload.
        let encoded = serde_json::to_string(event).expect("serialize event");
        assert!(
            !encoded.contains("do the thing"),
            "state events never expose message bodies: {encoded}"
        );
        assert!(events.try_recv().is_err(), "no second event was published");
    }
}
