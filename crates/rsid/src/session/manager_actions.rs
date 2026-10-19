//! Scoped manager action coordinator. Admission is synchronous and durable;
//! only this bounded reconciler owns physical effects. It never invokes a lead
//! Agent verb with a substituted identity.
use super::agent_verbs::AgentControlHandle;
use super::types::ManagerActionLaunchContext;
use super::{CompletedSession, SessionManager};
use crate::bus::DaemonEvent;
use crate::claude::LaunchConfig;
use crate::error::{DaemonError, Result};
use crate::store::harness_manager_v2::refused;
use crate::store::manager_actions::{
    MANAGER_LAUNCH_DRAIN_PENDING, ManagerActionClaimV2, ManagerActionOriginV2, action_epic,
    action_fence,
};
use rsi_common::harness_manager_v2::*;
use rsi_common::types::{Session, SessionProvider, SessionStatus};
use std::sync::atomic::Ordering;
use uuid::Uuid;

mod recovery;

const BATCH: usize = 4;
/// #1118: actions in flight at once within one reconcile pass. The store's
/// claim query decides which actions may overlap (independent create_session
/// launches); everything else stays one active command per project.
const MAX_PARALLEL_ACTIONS: usize =
    crate::store::manager_actions::MANAGER_CREATE_PARALLELISM as usize;
/// While a launch is slow, re-attempt claims this often so a later independent
/// launch does not wait for the slow one's pass to end.
const CLAIM_REFILL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
/// #1118: a launch step that makes no progress for this long settles the
/// action as `blocked` with the named step (or `uncertain` once a provider
/// effect started), never as an indefinite `running`.
pub(crate) const MANAGER_LAUNCH_STEP_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(300);

/// #1166: how long the lead-assignment commit keeps retrying a busy candidate
/// custody stripe (releasing the Store and the active map between attempts)
/// before surrendering with the typed retryable `root_busy`.
const MANAGER_ASSIGNMENT_STRIPE_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);
const MANAGER_ASSIGNMENT_STRIPE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(25);

/// #1143: runtime guard matching the transactional same-parent fence in
/// `claim_manager_action_holding`. A distinct key space, so it conflicts only
/// with other `create_session` launches under the same parent and never with
/// spawn guards taken on the parent session itself.
fn manager_create_parent_guard_key(parent_id: Uuid) -> Uuid {
    const NAMESPACE: Uuid = Uuid::from_u128(0x6d61_6e61_6765_725f_6372_6561_7465_0001);
    Uuid::new_v5(&NAMESPACE, parent_id.as_bytes())
}

tokio::task_local! {
    /// The launch step the current manager action is in (empty = untracked).
    static LAUNCH_STEP: std::sync::Arc<std::sync::Mutex<&'static str>>;
}

/// Record the launch step the current action entered. No-op outside a
/// bounded action.
fn note_launch_step(step: &'static str) {
    let _ = LAUNCH_STEP.try_with(|s| *s.lock().unwrap() = step);
}

/// Drive `run` with a per-step deadline (#1118). Only code that calls
/// [`note_launch_step`] is bounded; the deadline restarts whenever the step
/// changes. On expiry `run` is dropped and the typed
/// `manager_v2_launch_timeout_<step>` refusal is returned.
/// `expire_now` is an extra future that, once ready,
/// counts as the current step's deadline having elapsed (#2285). Production
/// passes a pending future; tests pass the manager's deadline trigger so the
/// deadline path runs without a wall-clock wait.
pub(super) async fn bound_launch_steps<T>(
    deadline: std::time::Duration,
    operation_id: Uuid,
    expire_now: impl std::future::Future<Output = ()>,
    run: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let step = std::sync::Arc::new(std::sync::Mutex::new(""));
    let mut run = Box::pin(LAUNCH_STEP.scope(std::sync::Arc::clone(&step), run));
    let mut expire_now = std::pin::pin!(expire_now);
    let mut seen = "";
    loop {
        let wait = if seen.is_empty() {
            std::time::Duration::from_millis(250)
        } else {
            deadline
        };
        let forced = tokio::select! {
            result = tokio::time::timeout(wait, &mut run) => match result {
                Ok(result) => return result,
                Err(_) => false,
            },
            () = &mut expire_now => true,
        };
        if forced {
            // The trigger stands for the step's deadline elapsing: wait until
            // the action has entered a step (the test fires after the launch
            // parked, so this is immediate), then expire it.
            while step.lock().unwrap().is_empty() {
                if let Ok(result) =
                    tokio::time::timeout(std::time::Duration::from_millis(10), &mut run).await
                {
                    return result;
                }
            }
            seen = *step.lock().unwrap();
        } else {
            let now = *step.lock().unwrap();
            if now.is_empty() || now != seen {
                seen = now;
                continue;
            }
        }
        tracing::warn!(%operation_id, step = seen, "manager launch step deadline expired");
        return Err(refused(launch_timeout_code(seen)));
    }
}

#[cfg(test)]
pub(super) fn note_launch_step_for_tests(step: &'static str) {
    note_launch_step(step);
}

/// Poll until a spawned task has exited; `false` when `wait` elapses first.
async fn wait_task_exit(task: &tokio::task::AbortHandle, wait: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    while !task.is_finished() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    true
}

fn is_launch_timeout_error(error: &DaemonError) -> bool {
    safe_action_error(error).starts_with("manager_v2_launch_timeout")
}

fn launch_timeout_code(step: &str) -> &'static str {
    match step {
        "predecessor_guard" => "manager_v2_launch_timeout_predecessor_guard",
        "runtime_check" => "manager_v2_launch_timeout_runtime_check",
        "settle_predecessor" => "manager_v2_launch_timeout_settle_predecessor",
        "prepare_fork" => "manager_v2_launch_timeout_prepare_fork",
        "launch_candidate" => "manager_v2_launch_timeout_launch_candidate",
        "confirm_candidate" => "manager_v2_launch_timeout_confirm_candidate",
        _ => "manager_v2_launch_timeout",
    }
}

impl AgentControlHandle {
    pub async fn agent_manager_prepare_control(
        &self,
        caller: Uuid,
        request: AgentManagerPrepareControlRequestV2,
    ) -> Result<ManagerPreparedActionReceiptV2> {
        crate::store_lock::lock(
            &self.store,
            "manager_actions::agent_manager_prepare_control",
        )
        .await
        .prepare_manager_action(caller, request)
    }

    pub async fn agent_manager_commit_prepared_control(
        &self,
        caller: Uuid,
        request: AgentManagerCommitPreparedControlRequestV2,
    ) -> Result<ManagerPreparedActionCommitResultV2> {
        crate::store_lock::lock(
            &self.store,
            "manager_actions::agent_manager_commit_prepared_control",
        )
        .await
        .commit_prepared_manager_action(caller, request)
    }

    pub async fn agent_manager_get_action(
        &self,
        caller: Uuid,
        request: AgentManagerGetActionRequestV2,
    ) -> Result<ManagerActionReceiptV2> {
        crate::store_lock::lock(&self.store, "manager_actions::agent_manager_get_action")
            .await
            .manager_action_receipt_for_caller(caller, request)
    }

    /// `AgentManagerGetAction` (#1311, #1417): the receipt plus, while the
    /// action is queued behind a hold, the reason. A draining deploy holds
    /// every worker start and reports when the hold releases; the host-load
    /// admission holds `create_session` while the host is busy and reports the
    /// load and the threshold. The claim loop holds exactly these actions
    /// (`claim_manager_action_with_create_admission`).
    pub async fn agent_manager_get_action_view(
        &self,
        caller: Uuid,
        request: AgentManagerGetActionRequestV2,
    ) -> Result<ManagerActionViewV2> {
        let receipt = self.agent_manager_get_action(caller, request).await?;
        let mut held = None;
        let deploy_hold = self
            .deploy_drain
            .as_ref()
            .and_then(|drain| drain.release_by());
        let load_hold = self
            .host_load
            .as_ref()
            .and_then(|gate| gate.hold_for_create(receipt.operation_id));
        if receipt.state == ManagerActionStateV2::Queued
            && (deploy_hold.is_some() || load_hold.is_some())
            && let Some((action, new_worker)) = crate::store_lock::lock(
                &self.store,
                "manager_actions::agent_manager_get_action_view",
            )
            .await
            .manager_action_operation(receipt.operation_id)?
            .map(|operation| {
                let new_worker = operation.is_new_worker_create();
                (operation.context.request.operation, new_worker)
            })
        {
            let starts_worker = matches!(
                action,
                ManagerActionV2::CreateSession { .. }
                    | ManagerActionV2::ReplaceLead { .. }
                    | ManagerActionV2::RetryLead { .. }
                    | ManagerActionV2::ResumeLead { .. }
            );
            if let Some((deploy_id, release_by)) = deploy_hold
                && starts_worker
            {
                held = Some(ManagerActionHeldV1 {
                    reason: crate::deploy_drain::DEPLOY_DRAINING.to_string(),
                    deploy_id: Some(deploy_id),
                    release_by: Some(
                        release_by.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                    ),
                    ..ManagerActionHeldV1::default()
                });
            } else if let Some(hold) = load_hold
                && new_worker
            {
                held = Some(admission_held_view(hold));
            }
        }
        Ok(ManagerActionViewV2 { receipt, held })
    }

    pub async fn agent_manager_control(
        &self,
        caller: Uuid,
        request: AgentManagerControlRequestV2,
    ) -> Result<ManagerActionReceiptV2> {
        if matches!(request.operation, ManagerActionV2::SucceedManager { .. }) {
            return self.enqueue_root_manager_succession(caller, request).await;
        }
        if matches!(request.operation, ManagerActionV2::Integrate { .. }) {
            return crate::store_lock::lock(&self.store, "manager_actions::agent_manager_control")
                .await
                .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller }, request);
        }
        crate::store_lock::lock(&self.store, "manager_actions::agent_manager_control")
            .await
            .enqueue_manager_action(ManagerActionOriginV2::Agent { caller }, request)
    }
}

/// #1417, #2562: the `held` view of a create queued behind the host-admission
/// gate: `host_load`, `thermal` or `disk_pressure`, with the reading that holds it.
fn admission_held_view(hold: crate::host_load::AdmissionHold) -> ManagerActionHeldV1 {
    use crate::host_load::AdmissionHold;
    use crate::host_pressure::PressureHold;
    let mut held = ManagerActionHeldV1 {
        reason: hold.reason().to_string(),
        ..ManagerActionHeldV1::default()
    };
    match hold {
        AdmissionHold::Load(load) => {
            held.load = Some(load.load);
            held.threshold = Some(load.threshold);
            held.recent_admissions = Some(load.recent_admissions);
        }
        AdmissionHold::Pressure(PressureHold::Thermal(hot)) => {
            held.temperature_c = Some(hot.temperature_c);
            held.ceiling_c = Some(hot.ceiling_c);
        }
        AdmissionHold::Pressure(PressureHold::DiskPressure { free_gb, min_gb }) => {
            held.free_disk_gb = Some(free_gb);
            held.min_free_disk_gb = Some(min_gb);
        }
        // #2920: the sandbox floor rides the same two fields.
        AdmissionHold::Pressure(PressureHold::SandboxFreeSpace { free_gb, min_gb }) => {
            held.free_disk_gb = Some(free_gb);
            held.min_free_disk_gb = Some(min_gb);
        }
    }
    held
}

impl SessionManager {
    /// Admission and its immediate disk-pressure response share one path, so
    /// the store's memoized held-claim retry also observes the debounce.
    pub(super) fn admit_manager_create(
        &self,
        key: Uuid,
        since: chrono::DateTime<chrono::Utc>,
        target: Option<Uuid>,
    ) -> bool {
        let admitted = self.host_load.admit_manager_create(key, since, target);
        if !admitted && let Some(hold) = self.host_load.hold_for_create(key) {
            self.request_launch_hold_reclaim(hold);
        }
        admitted
    }

    /// Must be called explicitly after runtime restoration and before normal
    /// reconciliation. A lost running claim is surfaced as uncertain, even if
    /// its durable provider row is missing. Missing evidence is not permission.
    pub async fn reconcile_manager_actions_startup(&self) -> Result<usize> {
        self.reconcile_manager_actions_once().await
    }

    pub async fn reconcile_manager_actions_once(&self) -> Result<usize> {
        let Ok(_flight) = self.manager_action_reconcile.try_lock() else {
            return Ok(0);
        };
        // #1166: the coordinator breadcrumb lives exactly as long as this
        // single-flight pass; every exit (success, error, cancellation) clears it.
        let _phase = crate::launch_breadcrumbs::PhaseGuard::new(Uuid::nil(), "manager_actions");
        let mut count = crate::store_lock::lock(
            &self.store,
            "manager_actions::reconcile_manager_actions_once",
        )
        .await
        .recover_abandoned_manager_action_claims()?;
        // RME-S2A-003: Reconcile uncertain integrate claims against actual
        // refs so a crash after publish can be settled and the singleton
        // unblocked without duplicate publication.
        count += self.reconcile_uncertain_integrate_claims().await?;
        // K2 (#380): settle uncertain pause/assign actions whose exact
        // predecessor is terminal and process-absent before claiming, so an
        // exact queued resume/repair is no longer fenced by a visible effect.
        count += self.reconcile_uncertain_lead_actions().await?;
        // Reserved DB reviews journal their CreateSession before the claim
        // loop. After restart, they then compete for the same last slot as
        // already-queued children, ahead of automatic lead recovery.
        let (reviews_changed, review_notice_jobs) = crate::store_lock::lock(
            &self.store,
            "manager_actions::reconcile_manager_actions_once",
        )
        .await
        .reconcile_reserved_manager_reviews_once()?;
        count += reviews_changed;
        for job_id in review_notice_jobs {
            self.event_bus
                .publish(DaemonEvent::ManagerNoticeQueued { job_id });
        }
        // #1930: relaunch capacity-failed Issue workers so the claim loop
        // below picks the admitted `create_session` up in this pass.
        count += self.reconcile_issue_worker_capacity_retries().await;
        let mut inflight = futures::stream::FuturesUnordered::new();
        let mut completed = 0usize;
        let mut drained = false;
        let mut first_error = None;
        loop {
            // #1118: independent launches proceed while another is slow or
            // stuck. The store's claim query enforces which actions may be
            // `running` together (create_session pairs, bounded); this loop
            // just keeps up to MAX_PARALLEL_ACTIONS in flight.
            while !drained && completed < BATCH && inflight.len() < MAX_PARALLEL_ACTIONS {
                // #1166: the in-flight launches are polled only by the `select!`
                // below, never while this loop awaits. Awaiting the Store here
                // queues behind (or behind a grant to) an in-flight launch that
                // is itself waiting for or holding the Store; that launch cannot
                // run until this await returns, so the pass and the Store wedge
                // together. With launches in flight, take the Store only if it
                // is free and otherwise retry after the refill interval.
                crate::launch_breadcrumbs::note_phase(Uuid::nil(), "manager_actions:claim");
                // Admit the oldest eligible create through the shared queue.
                // The synchronous callback never accesses the Store; a held
                // create leaves recovery and continuations claimable.
                let admit = |key, since, target| self.admit_manager_create(key, since, target);
                let claim = if inflight.is_empty() {
                    crate::store_lock::lock(
                        &self.store,
                        "manager_actions::reconcile_manager_actions_once",
                    )
                    .await
                    .claim_manager_action_with_create_admission(
                        self.program_run_boot_id,
                        self.deploy_drain.is_draining(),
                        &admit,
                    )?
                } else {
                    let Some(store) = crate::store_lock::try_lock(
                        &self.store,
                        "manager_actions::reconcile_manager_actions_once",
                    ) else {
                        drained = true;
                        break;
                    };
                    store.claim_manager_action_with_create_admission(
                        self.program_run_boot_id,
                        self.deploy_drain.is_draining(),
                        &admit,
                    )?
                };
                match claim {
                    None => drained = true,
                    Some(claim) => {
                        inflight.push(Box::pin(self.run_claimed_manager_action(claim)));
                    }
                }
            }
            if inflight.is_empty() {
                break;
            }
            crate::launch_breadcrumbs::note_phase(Uuid::nil(), "manager_actions:select");
            tokio::select! {
                done = futures::StreamExt::next(&mut inflight) => {
                    match done {
                        Some(Ok(true)) => {
                            count += 1;
                            completed += 1;
                            drained = false;
                        }
                        Some(Ok(false)) => drained = false,
                        // Never cancel the other in-flight launches: stop
                        // claiming, drain them, then surface the error.
                        Some(Err(error)) => {
                            first_error.get_or_insert(error);
                            completed = BATCH;
                        }
                        None => {}
                    }
                }
                () = tokio::time::sleep(CLAIM_REFILL_INTERVAL), if drained => {
                    drained = false;
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        let (reviews_changed, review_notice_jobs) = crate::store_lock::lock(
            &self.store,
            "manager_actions::reconcile_manager_actions_once",
        )
        .await
        .reconcile_manager_review_assignments_once()?;
        count += reviews_changed;
        for job_id in review_notice_jobs {
            self.event_bus
                .publish(DaemonEvent::ManagerNoticeQueued { job_id });
        }
        // #985: the halt awaits stop_manager_candidate. Build it in a
        // non-async helper so neither its state nor a poll-frame temporary
        // grows this reconcile future (debug test threads have 2 MiB stacks).
        count += self.boxed_halt_orphaned_manager_reviewers().await?;
        count += self.reconcile_manager_launch_drain_debt().await;
        count += self.reconcile_failed_manager_launch_cleanup().await;
        count += self.reconcile_issue_worker_watches().await;
        count += self.reconcile_manager_succession_cleanup().await?;
        Ok(count)
    }

    /// Settle one claimed action: execute it under its launch deadline, map a
    /// failure to the receipt state the at-most-once rules allow, and queue
    /// the notice. `Ok(false)` means the action was handed back or vanished
    /// and does not count as completed work.
    pub(super) async fn run_claimed_manager_action(
        &self,
        claim: ManagerActionClaimV2,
    ) -> Result<bool> {
        if let Err(error) = self.execute_manager_action_bounded(&claim).await {
            // #1143: the deadline dropped the launch future, so its in-band
            // candidate cleanup never ran. Cancel and drain here, before the
            // action settles.
            let drain_pending = is_launch_timeout_error(&error)
                && !self.boxed_drain_timed_out_manager_launch(&claim).await;
            // #1073: the deploy drain engaged after the claim and before
            // any provider effect. Hand the action back to the queue so it
            // runs after the deploy settles (or is re-admitted by a
            // restart); it is never blocked or made uncertain.
            if crate::deploy_drain::is_draining_error(&error)
                && crate::store_lock::lock(
                    &self.store,
                    "manager_actions::run_claimed_manager_action",
                )
                .await
                .requeue_held_manager_action(&claim)?
            {
                self.deploy_drain.note_manager_action_held();
                return Ok(false);
            }
            // #2920: the sandbox allocator refused for free space, a condition that
            // clears on its own. The refusal latched a `sandbox_free_space` hold
            // (`HostLoadAdmission::note_sandbox_free_space`), so the action goes
            // back to the queue and starts once space returns, as a host-load or
            // disk-pressure hold does; it is never blocked.
            if is_sandbox_free_space_error(&error)
                && crate::store_lock::lock(
                    &self.store,
                    "manager_actions::run_claimed_manager_action",
                )
                .await
                .requeue_held_manager_action(&claim)?
            {
                if let Some(hold) = self.host_load.hold_for_create(claim.id()) {
                    self.request_launch_hold_reclaim(hold);
                }
                return Ok(false);
            }
            let code = if drain_pending {
                MANAGER_LAUNCH_DRAIN_PENDING
            } else {
                safe_action_error(&error)
            };
            let is_integrate = matches!(claim.action(), ManagerActionV2::Integrate { .. });
            // RME-S2A-003: For integrate actions with effect_started,
            // reconcile against actual refs after dropping the store
            // lock (git_ref is async). For all other actions, finish
            // immediately inside the store lock.
            let (needs_integrate_reconcile, ended_launch) = {
                let store = crate::store_lock::lock(
                    &self.store,
                    "manager_actions::run_claimed_manager_action",
                )
                .await;
                let op = store.manager_action_operation(claim.id())?;
                let Some(op) = op else {
                    tracing::warn!(operation_id=%claim.id(),error=%error,
                        "manager action vanished during error handling");
                    return Ok(false);
                };
                let effect_started = op.effect_started;
                let mut ended_launch = false;
                let needs_reconcile = is_integrate
                    && effect_started
                    && op.receipt.state == ManagerActionStateV2::Running;
                if !needs_reconcile {
                    if op.receipt.state == ManagerActionStateV2::Running {
                        let state = if effect_started || drain_pending {
                            ManagerActionStateV2::Uncertain
                        } else if matches!(
                            code,
                            "manager_v2_scope_changed"
                                // #1335: the queuing seat retired with no
                                // successor to inherit the action.
                                | "manager_v2_actor_seat_retired"
                                // #1716: another session was appointed over
                                // the queuing seat; nothing launched.
                                | "manager_v2_actor_seat_replaced"
                                | "manager_review_assignment_terminal"
                                | "manager_v2_policy_changed"
                                | "manager_v2_capability_denied"
                                | "manager_v2_epic_out_of_scope"
                                | "manager_v2_container_out_of_scope"
                        ) {
                            ManagerActionStateV2::Revoked
                        } else {
                            ManagerActionStateV2::Blocked
                        };
                        store.finish_manager_action(&claim, state, code)?;
                        ended_launch = true;
                    }
                }
                (needs_reconcile, ended_launch)
            };
            if ended_launch {
                self.settle_ended_issue_worker_launch(&claim.operation, code)
                    .await;
            }
            if needs_integrate_reconcile {
                // Same-boot path: the claim is still Running with the
                // original boot_id, so finish_manager_action works.
                // Use settle_integrate_claim only for lost-boot
                // recovery where the row is already terminal Uncertain.
                let target_ref: String = match claim.action() {
                    ManagerActionV2::Integrate { target_ref, .. } => target_ref.clone(),
                    _ => unreachable!("checked above"),
                };
                let repo = self
                    .resolve_project_working_dir(claim.operation.project_id)
                    .await
                    .ok();
                let resolved = if let Some(r) = repo {
                    git_ref(&r, &target_ref).await
                } else {
                    None
                };
                let store = crate::store_lock::lock(
                    &self.store,
                    "manager_actions::run_claimed_manager_action",
                )
                .await;
                let state = if let Some(ref_oid) = resolved {
                    match store.reconcile_integrate_claim(&claim, &ref_oid)? {
                        ManagerActionStateV2::Succeeded => ManagerActionStateV2::Succeeded,
                        ManagerActionStateV2::Failed => ManagerActionStateV2::Failed,
                        _ => ManagerActionStateV2::Uncertain,
                    }
                } else {
                    ManagerActionStateV2::Uncertain
                };
                let outcome = match state {
                    ManagerActionStateV2::Succeeded => "integrated_crash_reconciled",
                    ManagerActionStateV2::Failed => "not_integrated_crash_reconciled",
                    _ => "execution_owner_lost_unconfirmed",
                };
                store.finish_manager_action(&claim, state, outcome)?;
            }
            tracing::warn!(operation_id=%claim.id(),error=%error,"manager lifecycle action did not establish a confirmed result");
        }
        match crate::store_lock::lock(&self.store, "manager_actions::run_claimed_manager_action")
            .await
            .reconcile_manager_action_notice(claim.id())
        {
            Ok(Some(job_id)) => self
                .event_bus
                .publish(DaemonEvent::ManagerNoticeQueued { job_id }),
            Ok(None) => {}
            Err(error) => tracing::warn!(operation_id=%claim.id(),%error,
                "manager action notice reconciliation deferred"),
        }
        Ok(true)
    }

    /// Run an action with a per-step launch deadline (#1118). Only actions
    /// that record a launch step are bounded; the deadline restarts whenever
    /// the step changes. On expiry the in-flight future is dropped and the
    /// caller settles the action from its durable `effect_started` flag, so a
    /// pre-effect stall becomes `blocked` with the named step and an uncertain
    /// effect stays `uncertain` for reconciliation.
    pub(crate) fn manager_launch_step_deadline(&self) -> std::time::Duration {
        *self.manager_launch_step_deadline.lock().unwrap()
    }

    /// Test-only: expire the current launch step's deadline of every bounded
    /// action running on this manager now (#2285), instead of waiting it out.
    /// It reaches only runs already waiting; a later action is unaffected.
    #[cfg(test)]
    pub(crate) fn expire_manager_launch_step_deadline(&self) {
        self.manager_launch_deadline_trigger.notify_waiters();
    }

    #[cfg(test)]
    async fn manager_launch_deadline_forced(&self) {
        let notified = self.manager_launch_deadline_trigger.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        notified.await;
    }

    #[cfg(not(test))]
    async fn manager_launch_deadline_forced(&self) {
        std::future::pending().await
    }

    async fn execute_manager_action_bounded(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        bound_launch_steps(
            self.manager_launch_step_deadline(),
            claim.id(),
            self.manager_launch_deadline_forced(),
            self.execute_manager_action(claim),
        )
        .await
    }

    /// The `create_session` / `replace_lead` / `retry_lead` arm of
    /// [`Self::execute_manager_action`], built by a non-async `#[inline(never)]`
    /// helper so its large temporaries are not part of the caller's poll frame
    /// (the manager-intent race scenario runs on a small stack, #985, #2908).
    #[inline(never)]
    fn boxed_execute_manager_launch_action<'a>(
        &'a self,
        claim: &'a ManagerActionClaimV2,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            use ManagerActionV2::*;
            // #1073: a create waits behind a draining deploy before the
            // predecessor guard, fork or any other effect.
            self.deploy_drain
                .refuse_if_draining(claim.operation.context.target_session_id, true)?;
            note_launch_step("predecessor_guard");
            // #1143: same-parent creates serialize at runtime too (the
            // claim predicate already fences them transactionally).
            let _parent_guard = match claim.action() {
                CreateSession { parent_id, .. } => Some(
                    super::spawn_single_flight::acquire_spawn_guard(
                        manager_create_parent_guard_key(*parent_id),
                    )
                    .await,
                ),
                _ => None,
            };
            let predecessor = action_fence(claim.action()).and_then(|f| f.lead_session_id);
            // Held until CAS: an operator/worker continuation cannot restart
            // the old process between quiescence and assignment.
            let _predecessor_guard = if let Some(id) = predecessor {
                Some(super::spawn_single_flight::acquire_spawn_guard(id).await)
            } else {
                None
            };
            note_launch_step("runtime_check");
            self.check_manager_action_runtime(claim, false).await?;
            note_launch_step("settle_predecessor");
            if let Some(id) = predecessor {
                self.settle_manager_predecessor(claim, id).await?;
            }
            let frozen = claim
                .operation
                .context
                .source
                .as_ref()
                .ok_or_else(|| refused("manager_v2_source_unavailable"))?;
            let source = crate::store_lock::lock(
                &self.store,
                "manager_actions::boxed_execute_manager_launch_action",
            )
            .await
            .get_session(frozen.session_id)?
            .ok_or_else(|| refused("manager_v2_source_unavailable"))?;
            note_launch_step("prepare_fork");
            let runtime = self.custody_execution_runtime();
            // #1195: an explicit or default create source names its
            // commit; only legacy pinned sources follow the source HEAD.
            let fork = if frozen.historical_commit {
                runtime
                    .prepare_manager_review_fork_at(&source, &frozen.commit)
                    .await?
            } else if frozen.forks_at_frozen_commit() {
                runtime
                    .prepare_manager_action_fork_at(&source, &frozen.commit)
                    .await?
            } else {
                runtime
                    .prepare_manager_action_fork_pinned(
                        &source,
                        &frozen.commit,
                        frozen.branch.as_deref(),
                    )
                    .await?
            };
            if source.working_dir != frozen.working_dir
                || source.sandbox_root != frozen.sandbox_root
                || fork.fork_commit() != frozen.commit
            {
                return Err(refused("manager_v2_source_changed"));
            }
            note_launch_step("launch_candidate");
            // #1254: a `continue_from` Issue launch continues its
            // predecessor's lineage and display identity.
            let continued = match claim
                .operation
                .context
                .issue_binding
                .as_ref()
                .and_then(|binding| binding.continue_from)
            {
                Some(id) => Some(
                    crate::store_lock::lock(
                        &self.store,
                        "manager_actions::boxed_execute_manager_launch_action",
                    )
                    .await
                    .get_session(id)?
                    .ok_or_else(|| refused("manager_v2_source_unavailable"))?,
                ),
                None => None,
            };
            // #1640: a lead handover whose predecessor has no forkable
            // sandbox forks from the Epic's published rolling instead, so
            // the launch identity (kind, role, ordinal, tags) still comes
            // from the predecessor, not from the fork source.
            // The lineage row is scoped to this block so the large
            // `Session` is not kept in the launch future's state across
            // the awaits below (#985, #2774).
            let config = {
                let lineage = match predecessor {
                    Some(id) if id != source.id => crate::store_lock::lock(
                        &self.store,
                        "manager_actions::boxed_execute_manager_launch_action",
                    )
                    .await
                    .get_session(id)?
                    .ok_or_else(|| refused("manager_v2_source_unavailable"))?,
                    _ => source.clone(),
                };
                manager_launch_config(claim, &lineage, continued.as_ref())?
            };
            if let Some(model) = config.model.as_deref() {
                self.preflight_manager_action_model(
                    config.provider.unwrap_or(SessionProvider::Claude),
                    model,
                )
                .await?;
            }
            let target = claim
                .operation
                .context
                .target_session_id
                .expect("reserved candidate");
            let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            self.boxed_launch_manager_action_candidate(
                config,
                ManagerActionLaunchContext {
                    claim: claim.clone(),
                    fork,
                    generation: generation.clone(),
                },
            )
            .await?;
            let generation = generation.load(Ordering::Acquire);
            note_launch_step("confirm_candidate");
            let result = async {
                #[cfg(test)]
                super::launch::pause_controller_candidate_test(
                    target,
                    super::launch::ControllerCandidateTestPhase::ManagerActionConfirming,
                )
                .await;
                self.confirm_manager_candidate(claim, target, generation)
                    .await?;
                if predecessor.is_some() {
                    self.commit_manager_assignment(claim, Some(generation))
                        .await
                } else {
                    // #1115: the caller's terminal watch commits with the
                    // launch success, never after it.
                    // #2114: resolve the watch owner under the commit's own
                    // lock hold so succession cannot publish in between.
                    let store = crate::store_lock::lock(
                        &self.store,
                        "manager_actions::boxed_execute_manager_launch_action",
                    )
                    .await;
                    let watch = Self::issue_worker_watch_candidate_locked(&store, claim);
                    store.commit_manager_created_session_with_watch(claim, watch.as_ref())
                }
            }
            .await;
            if result.is_err() {
                // Keep the allocated source and durable uncertain result;
                // stop a losing candidate instead of leaving an unowned writer.
                self.boxed_stop_manager_candidate(target, generation)
                    .await?;
            }
            result?;
            Ok(())
        })
    }

    pub(super) async fn execute_manager_action(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        use ManagerActionV2::*;
        if matches!(claim.action(), SucceedManager { .. }) {
            return self.execute_manager_succession(claim).await;
        }
        if matches!(claim.action(), Integrate { .. } | OperatorCall { .. }) {
            // One shared await of a heap future built by a non-async helper:
            // K14's delegated executor adds no await site, state or poll-frame
            // temporaries here (debug test threads run on a 2 MiB stack).
            return self.boxed_dispatched_manager_action(claim).await;
        }
        self.check_manager_action_runtime(claim, false).await?;
        match claim.action() {
            SucceedManager { .. } => unreachable!("root succession dispatched above"),
            Integrate { .. } => unreachable!("integrate dispatched above"),
            OperatorCall { .. } => unreachable!("operator call dispatched above"),
            SettleUncertainAction { operation_id, .. } => {
                self.execute_manager_settle(claim, *operation_id).await?;
            }
            RetireLeadContinuations { .. } => {
                self.execute_manager_retire(claim).await?;
            }
            CreateContainer {
                parent_id,
                kind,
                name,
                tags,
            } => {
                let working_dir = if let Some(parent) = parent_id {
                    crate::store_lock::lock(&self.store, "manager_actions::execute_manager_action")
                        .await
                        .get_session(*parent)?
                        .ok_or_else(|| refused("manager_v2_target_unavailable"))?
                        .working_dir
                } else {
                    self.resolve_project_working_dir(claim.operation.project_id)
                        .await?
                };
                let mut normalized = tags
                    .iter()
                    .map(|t| {
                        rsi_common::normalize_tag(t).map_err(|_| refused("manager_v2_invalid_tags"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                normalized.sort();
                normalized.dedup();
                let params = rsi_common::rpc::CreateContainerParams {
                    kind: *kind,
                    name: name.clone(),
                    parent_id: *parent_id,
                    project_id: Some(claim.operation.project_id),
                    tags: normalized.clone(),
                    topology_id: None,
                };
                let session = super::hierarchy_ops::build_container_session(
                    claim
                        .operation
                        .context
                        .target_session_id
                        .expect("reserved container"),
                    params,
                    working_dir,
                    normalized,
                );
                let (session, _) =
                    crate::store_lock::lock(&self.store, "manager_actions::execute_manager_action")
                        .await
                        .apply_manager_container_action(claim, Some(&session))?;
                self.project_manager_container(session, claim.action())
                    .await;
            }
            ArchiveContainer { .. } | RestoreContainer { .. } => {
                self.execute_manager_container_cascade(claim).await?;
            }
            ArchiveSession { .. } | RestoreSession { .. } | UpdateSession { .. } => {
                self.execute_manager_session_action(claim).await?;
            }
            UpdateContainer { container_id, .. } | DeleteContainer { container_id, .. } => {
                // K2 authority gate: Delete clears the container's lead.
                let _lead_guards = self.container_lead_guards(*container_id).await?;
                let (session, _) =
                    crate::store_lock::lock(&self.store, "manager_actions::execute_manager_action")
                        .await
                        .apply_manager_container_action(claim, None)?;
                self.project_manager_container(session, claim.action())
                    .await;
            }
            ResumeLead { message, .. } => {
                self.continue_manager_action(claim.clone(), message.clone())
                    .await?;
                // The continuation holds the guard through installation. Its
                // success is a confirmed start, not proof of task completion.
                crate::store_lock::lock(&self.store, "manager_actions::execute_manager_action")
                    .await
                    .finish_manager_action(
                        claim,
                        ManagerActionStateV2::Succeeded,
                        "lead_resumed",
                    )?;
            }
            PauseLead { .. } => {
                let predecessor = action_fence(claim.action())
                    .and_then(|f| f.lead_session_id)
                    .ok_or_else(|| refused("manager_v2_lead_unavailable"))?;
                let _guard = super::spawn_single_flight::acquire_spawn_guard(predecessor).await;
                self.check_manager_action_runtime(claim, false).await?;
                self.settle_manager_predecessor(claim, predecessor).await?;
                self.check_manager_action_runtime(claim, false).await?;
                crate::store_lock::lock(&self.store, "manager_actions::execute_manager_action")
                    .await
                    .finish_manager_action(claim, ManagerActionStateV2::Succeeded, "lead_paused")?;
            }
            CreateSession { .. } | ReplaceLead { .. } | RetryLead { .. } => {
                // Boxed (#2908): the launch arm's temporaries stay out of this
                // fn's poll frame (debug test threads run on a 2 MiB stack).
                self.boxed_execute_manager_launch_action(claim).await?;
            }
            AssignLead { session_id, .. } => {
                let predecessor = action_fence(claim.action()).and_then(|f| f.lead_session_id);
                let mut ids = predecessor
                    .into_iter()
                    .chain(*session_id)
                    .collect::<Vec<_>>();
                ids.sort();
                ids.dedup();
                let mut guards = Vec::new();
                for id in ids {
                    guards.push(super::spawn_single_flight::acquire_spawn_guard(id).await);
                }
                self.check_manager_action_runtime(claim, false).await?;
                if predecessor != *session_id {
                    if let Some(old) = predecessor {
                        self.settle_manager_predecessor(claim, old).await?;
                    }
                }
                self.commit_manager_assignment(claim, None).await?;
            }
        }
        Ok(())
    }

    /// #1084: retry the fenced launch cleanup of a terminal manager action
    /// whose target row is still Starting (or Failed with an open invocation).
    /// The durable pending state is exactly that pair; a live launch keeps its
    /// spawn guard, so it is never raced.
    async fn reconcile_failed_manager_launch_cleanup(&self) -> usize {
        let targets = match crate::store_lock::lock(
            &self.store,
            "manager_actions::reconcile_failed_manager_launch_cleanup",
        )
        .await
        .manager_launch_cleanup_candidates(32)
        {
            Ok(targets) => targets,
            Err(error) => {
                tracing::warn!(%error, "manager launch cleanup scan failed");
                return 0;
            }
        };
        let mut settled = 0;
        for target in targets {
            let Some(_guard) = super::spawn_single_flight::try_acquire_spawn_guard(target) else {
                continue;
            };
            if self.active.read().await.contains_key(&target) {
                continue;
            }
            match self.settle_failed_manager_launch_target(target).await {
                Ok(()) => settled += 1,
                Err(error) => {
                    tracing::warn!(session_id = %target, %error, "manager launch cleanup remains owed");
                }
            }
        }
        settled
    }

    /// Guard of the container's current lead (K2 authority gate).
    async fn container_lead_guards(
        &self,
        container_id: Uuid,
    ) -> Result<Vec<super::spawn_single_flight::SpawnGuard>> {
        let lead = crate::store_lock::lock(&self.store, "manager_actions::container_lead_guards")
            .await
            .get_session(container_id)?
            .and_then(|container| container.lead_session_id);
        super::hierarchy_ops::lead_mutation_guards(
            &lead.into_iter().collect::<Vec<_>>(),
            super::hierarchy_ops::LeadGuardMode::Acquire,
        )
        .await
    }

    async fn execute_manager_container_cascade(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        match claim.action() {
            ManagerActionV2::ArchiveContainer { container_id, .. } => {
                let mut tree = crate::store_lock::lock(
                    &self.store,
                    "manager_actions::execute_manager_container_cascade",
                )
                .await
                .manager_action_container_tree_ids(*container_id)?;
                tree.sort();
                let mut spawn_guards = Vec::with_capacity(tree.len());
                for id in &tree {
                    spawn_guards.push(super::spawn_single_flight::acquire_spawn_guard(*id).await);
                }
                {
                    let active = self.active.read().await;
                    if tree.iter().any(|id| active.contains_key(id)) {
                        return Err(refused("container_not_terminal"));
                    }
                }
                {
                    let completed = self.completed.read().await;
                    if tree.iter().any(|id| {
                        completed
                            .get(id)
                            .is_some_and(|cs| cs.superseded_by_retry.is_some())
                    }) {
                        return Err(refused("manager_v2_human_or_recovery_owner"));
                    }
                }
                let (_, ids) = crate::store_lock::lock(
                    &self.store,
                    "manager_actions::execute_manager_container_cascade",
                )
                .await
                .apply_manager_container_action(claim, None)?;
                self.project_manager_cascade(ids, false).await?;
                drop(spawn_guards);
            }
            ManagerActionV2::RestoreContainer { container_id, .. } => {
                let recorded = crate::store_lock::lock(
                    &self.store,
                    "manager_actions::execute_manager_container_cascade",
                )
                .await
                .manager_action_cascade_archive_ids(*container_id)?;
                let Some(record) = recorded else {
                    // K2 authority gate: a restore clears the container lead.
                    let _lead_guards = self.container_lead_guards(*container_id).await?;
                    let (session, _) = crate::store_lock::lock(
                        &self.store,
                        "manager_actions::execute_manager_container_cascade",
                    )
                    .await
                    .apply_manager_container_action(claim, None)?;
                    self.project_manager_container(session, claim.action())
                        .await;
                    return Ok(());
                };
                let mut ids = record.ids;
                if !ids.contains(container_id) {
                    ids.push(*container_id);
                }
                ids.sort();
                ids.dedup();
                let mut spawn_guards = Vec::with_capacity(ids.len());
                for id in &ids {
                    spawn_guards.push(super::spawn_single_flight::acquire_spawn_guard(*id).await);
                }
                {
                    let active = self.active.read().await;
                    if ids.iter().any(|id| active.contains_key(id)) {
                        return Err(refused("manager_v2_target_active"));
                    }
                }
                let (_, transitioned) = crate::store_lock::lock(
                    &self.store,
                    "manager_actions::execute_manager_container_cascade",
                )
                .await
                .apply_manager_container_action(claim, None)?;
                self.project_manager_cascade(transitioned, true).await?;
                drop(spawn_guards);
            }
            _ => return Err(refused("manager_v2_not_container_action")),
        }
        Ok(())
    }

    /// Single-leaf housekeeping. The spawn guard keeps an operator/worker
    /// continuation from starting the target between these checks and the
    /// store transaction; in-memory effects mirror the operator handlers.
    async fn execute_manager_session_action(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        let id = claim
            .action()
            .housekeeping_session()
            .ok_or_else(|| refused("manager_v2_not_session_action"))?;
        let _spawn_guard = super::spawn_single_flight::acquire_spawn_guard(id).await;
        // Archive/restore require a quiescent row; metadata edits may target a
        // running leaf exactly as the operator handlers do.
        if !matches!(claim.action(), ManagerActionV2::UpdateSession { .. })
            && self.active.read().await.contains_key(&id)
        {
            return Err(refused("manager_v2_session_active"));
        }
        match claim.action() {
            ManagerActionV2::ArchiveSession { .. } => {
                if self
                    .completed
                    .read()
                    .await
                    .get(&id)
                    .is_some_and(|cs| cs.superseded_by_retry.is_some())
                {
                    return Err(refused("manager_v2_human_or_recovery_owner"));
                }
                crate::store_lock::lock(
                    &self.store,
                    "manager_actions::execute_manager_session_action",
                )
                .await
                .apply_manager_session_action(claim)?;
                self.project_manager_cascade(vec![id], false).await?;
            }
            ManagerActionV2::RestoreSession { .. } => {
                crate::store_lock::lock(
                    &self.store,
                    "manager_actions::execute_manager_session_action",
                )
                .await
                .apply_manager_session_action(claim)?;
                self.project_manager_cascade(vec![id], true).await?;
            }
            ManagerActionV2::UpdateSession { patch, .. } => {
                let session = crate::store_lock::lock(
                    &self.store,
                    "manager_actions::execute_manager_session_action",
                )
                .await
                .apply_manager_session_action(claim)?;
                let project = |target: &mut Session| {
                    if patch.title.is_some() {
                        target.title.clone_from(&session.title);
                    }
                    if patch.description.is_some() {
                        target.description.clone_from(&session.description);
                    }
                    if patch.rating.is_some() {
                        target.rating = session.rating;
                    }
                    if patch.active_task.is_some() {
                        target.active_task.clone_from(&session.active_task);
                    }
                    if patch.label.is_some() {
                        target.group_id = session.group_id;
                    }
                    if patch.tags.is_some() {
                        target.tags.clone_from(&session.tags);
                        target.tag.clone_from(&session.tag);
                    }
                    target.updated_at = session.updated_at;
                };
                if let Some(tracked) = self.active.write().await.get_mut(&id) {
                    project(&mut tracked.session);
                }
                if let Some(completed) = self.completed.write().await.get_mut(&id) {
                    project(&mut completed.session);
                }
                self.event_bus.publish(DaemonEvent::SessionMetadataChanged {
                    session_id: id,
                    model: None,
                    pinned_at: None,
                    project_id: None,
                    parent_id: None,
                    lead_session_id: None,
                    testing_needed_at: None,
                    rotation_disabled_at: None,
                    resolved_context_budget: None,
                });
            }
            _ => return Err(refused("manager_v2_not_session_action")),
        }
        Ok(())
    }

    pub(super) async fn project_manager_cascade(
        &self,
        ids: Vec<Uuid>,
        restore: bool,
    ) -> Result<()> {
        for id in ids {
            if restore {
                let store = self.store.clone();
                let (mut session, events, turn_metrics) = tokio::task::spawn_blocking(move || {
                    let store = crate::store_lock::blocking_lock(
                        &store,
                        "manager_actions::project_manager_cascade",
                    );
                    let session = store
                        .get_session(id)?
                        .ok_or(DaemonError::SessionNotFound(id))?;
                    let events = store.load_events(id)?;
                    let turn_metrics = store.load_turn_metrics(id)?;
                    drop(store);
                    Ok::<_, DaemonError>((session, events, turn_metrics))
                })
                .await
                .map_err(|error| DaemonError::Store(error.to_string()))??;
                session.context_fill_pct =
                    super::monitor::context_fill_pct_from_persisted(&session);
                super::cohort_settlement::publish_completed_from_store(
                    &self.completed,
                    &self.store,
                    id,
                    CompletedSession {
                        session,
                        events,
                        turn_metrics,
                        retry_cancel: None,
                        retry_fired_at: None,
                        superseded_by_retry: None,
                        events_hydrated: true,
                        store_writes: Default::default(),
                    },
                )
                .await;
                self.event_bus
                    .publish(DaemonEvent::SessionUnarchived { session_id: id });
            } else {
                let mut completed = { self.completed.write().await.remove(&id) };
                if let Some(cancel) = completed.as_mut().and_then(|cs| cs.retry_cancel.take()) {
                    let _ = cancel.send(());
                }
                self.event_bus.publish(DaemonEvent::SessionArchived {
                    session_id: id,
                    projection_id: None,
                });
            }
        }
        Ok(())
    }

    async fn project_manager_container(&self, session: Session, action: &ManagerActionV2) {
        let id = session.id;
        if matches!(
            session.status,
            SessionStatus::Archived | SessionStatus::Deleted
        ) {
            self.completed.write().await.remove(&id);
            self.event_bus
                .publish(if session.status == SessionStatus::Archived {
                    DaemonEvent::SessionArchived {
                        session_id: id,
                        projection_id: None,
                    }
                } else {
                    DaemonEvent::SessionDeleted { session_id: id }
                });
        } else {
            self.completed.write().await.insert(
                id,
                CompletedSession {
                    session: session.clone(),
                    events: Vec::new(),
                    turn_metrics: Vec::new(),
                    retry_cancel: None,
                    retry_fired_at: None,
                    superseded_by_retry: None,
                    events_hydrated: true,
                    store_writes: Default::default(),
                },
            );
            self.event_bus
                .publish(DaemonEvent::SessionCreated { session });
            if matches!(action, ManagerActionV2::RestoreContainer { .. }) {
                self.event_bus
                    .publish(DaemonEvent::SessionUnarchived { session_id: id });
            }
        }
    }

    /// Called while the actual target spawn guard is held; launch.rs calls it
    /// again just before the provider effect, including deferred AppServer.
    pub(super) async fn check_manager_action_runtime(
        &self,
        claim: &ManagerActionClaimV2,
        effect: bool,
    ) -> Result<()> {
        if let Some(lead) = action_fence(claim.action()).and_then(|f| f.lead_session_id) {
            if let Some(cs) = self.completed.read().await.get(&lead) {
                if cs.retry_cancel.is_some()
                    || cs.retry_fired_at.is_some()
                    || cs.superseded_by_retry.is_some()
                    || cs.session.pending_question.is_some()
                    || cs.session.pending_archive
                {
                    return Err(refused("manager_v2_human_or_recovery_owner"));
                }
            }
        }
        if effect {
            manager_action_source_gate(&self.custody_execution_runtime(), &self.store, claim)
                .await?;
        }
        crate::store_lock::lock(&self.store, "manager_actions::check_manager_action_runtime")
            .await
            .manager_action_runtime_gate(claim, effect)
    }

    async fn settle_manager_predecessor(
        &self,
        claim: &ManagerActionClaimV2,
        predecessor: Uuid,
    ) -> Result<()> {
        let generation = self
            .active
            .read()
            .await
            .get(&predecessor)
            .map(|s| s.spawn_generation);
        let deadline = tokio::time::Instant::now() + Self::CONTINUE_INTERRUPT_WAIT;
        if generation.is_some() {
            self.check_manager_action_runtime(claim, true).await?;
            super::lifecycle::interrupt_active_in_maps_from(
                &self.active,
                predecessor,
                crate::terminal_cause::InterruptSource::ManagerAction,
            )
            .await?;
            loop {
                let active = self.active.read().await;
                match active.get(&predecessor) {
                    None => break,
                    Some(s) if Some(s.spawn_generation) != generation => {
                        return Err(refused("manager_v2_predecessor_changed"));
                    }
                    _ => {}
                }
                drop(active);
                if tokio::time::Instant::now() >= deadline {
                    return Err(refused("manager_v2_predecessor_unsettled"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
        // #1033: leaving `active` is not the end of the exit. The terminal
        // finalizer still persists final status and the terminal-sandbox
        // observation event (which advances the lead's event fence) after it
        // removes the entry. Accepting the settled fence before that lands
        // would freeze a fence the finalizer then invalidates
        // (`manager_v2_lead_changed`). Wait for the settlement guard, which is
        // registered under the same lock that removes the entry.
        while crate::reconciliation::terminal_settlement_in_progress(predecessor) {
            if tokio::time::Instant::now() >= deadline {
                return Err(refused("manager_v2_predecessor_unsettled"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // Map removal alone is not an OS-process settlement witness. Reprove
        // the exact daemon-owned cohort to a bounded two-empty-pass fixed point.
        let orphan_store = std::sync::Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let turns = crate::store_lock::blocking_lock(
                &orphan_store,
                "manager_actions::settle_manager_predecessor",
            )
            .list_active_provider_turn_custody()?;
            super::reaper::reap_orphans_for_session(predecessor, turns)
        })
        .await
        .map_err(|_| refused("manager_v2_predecessor_unsettled"))??;
        crate::store_lock::lock(&self.store, "manager_actions::settle_manager_predecessor")
            .await
            .manager_action_accept_settled_fence(claim)
    }

    /// #985: heap future for [`Self::halt_orphaned_manager_reviewers`].
    #[inline(never)]
    fn boxed_halt_orphaned_manager_reviewers(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<usize>> + Send + '_>> {
        Box::pin(self.halt_orphaned_manager_reviewers())
    }

    /// #967: a reviewer whose DB review assignment already failed, was
    /// cancelled or was superseded can never record its receipt. Stop any such
    /// reviewer process still running so it does not keep spending money.
    pub(super) async fn halt_orphaned_manager_reviewers(&self) -> Result<usize> {
        let reviewers = crate::store_lock::lock(
            &self.store,
            "manager_actions::halt_orphaned_manager_reviewers",
        )
        .await
        .manager_review_orphaned_reviewers()?;
        let mut halted = 0;
        for reviewer in reviewers {
            let generation = self
                .active
                .read()
                .await
                .get(&reviewer)
                .map(|s| s.spawn_generation);
            let Some(generation) = generation else {
                continue;
            };
            match self.stop_manager_candidate(reviewer, generation).await {
                Ok(()) => {
                    halted += 1;
                    tracing::info!(%reviewer, "halted reviewer of a terminal review assignment");
                }
                Err(error) => {
                    tracing::warn!(%reviewer, %error, "orphaned reviewer halt deferred");
                }
            }
        }
        Ok(halted)
    }

    /// #1143: heap future for [`Self::drain_timed_out_manager_launch`].
    #[inline(never)]
    fn boxed_drain_timed_out_manager_launch<'a>(
        &'a self,
        claim: &'a ManagerActionClaimV2,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(self.drain_timed_out_manager_launch(claim))
    }

    /// #1143: a launch-step deadline drops the in-flight launch, so the
    /// candidate it may have started is cancelled, drained and settled here.
    /// Returns `true` only when the drain was VERIFIED (monitor task exited,
    /// process cohort reaped, nothing of this incarnation left in `active`) and
    /// the candidate rows settled. `false` keeps the durable cleanup debt: the
    /// row and invocation stay open, the action settles `uncertain` with
    /// [`MANAGER_LAUNCH_DRAIN_PENDING`] and the sweep retries until proved.
    async fn drain_timed_out_manager_launch(&self, claim: &ManagerActionClaimV2) -> bool {
        if !matches!(
            claim.action(),
            ManagerActionV2::CreateSession { .. }
                | ManagerActionV2::ReplaceLead { .. }
                | ManagerActionV2::RetryLead { .. }
        ) {
            return true;
        }
        let Some(target) = claim.operation.context.target_session_id else {
            return true;
        };
        let generation = self
            .active
            .read()
            .await
            .get(&target)
            .map(|s| s.spawn_generation);
        if let Err(error) = self
            .drain_manager_launch_candidate(target, generation)
            .await
        {
            tracing::warn!(session_id = %target, %error,
                "timed-out manager launch is not proved drained; cleanup stays owed");
            return false;
        }
        if let Err(error) = self.settle_failed_manager_launch_target(target).await {
            tracing::warn!(session_id = %target, %error,
                "timed-out manager launch cleanup incomplete; the sweep will retry");
        }
        true
    }

    /// Cooperative-stop budget before the runner cancels the launch task itself.
    #[cfg(not(test))]
    const MANAGER_DRAIN_COOPERATIVE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
    #[cfg(test)]
    const MANAGER_DRAIN_COOPERATIVE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
    const MANAGER_DRAIN_TASK_EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// #1143: cancel and drain one manager-action candidate until its exit is
    /// proved, or return the typed drain-pending refusal.
    ///
    /// `generation` is the exact `active` incarnation owned by the caller
    /// (`None`: none is, so a live entry is never stopped or removed: the
    /// sweep must not stop an operator continuation). Order:
    /// 1. a bounded cooperative stop (the monitor finalizes the session); it
    ///    can wait behind the deferred launch's start gate, so it is only a
    ///    grace period, never the sole route;
    /// 2. the runner-owned abort handle of the launch task cancels it directly,
    ///    independent of that gate, and its exit is awaited;
    /// 3. the (now unmonitored) entry of this incarnation is forced out;
    /// 4. the session's exact process cohort is reaped to a fixed point.
    async fn drain_manager_launch_candidate(
        &self,
        target: Uuid,
        generation: Option<u64>,
    ) -> Result<()> {
        let mut cooperative = generation.is_none();
        if let Some(generation) = generation {
            cooperative = matches!(
                tokio::time::timeout(
                    Self::MANAGER_DRAIN_COOPERATIVE_WAIT,
                    self.stop_manager_candidate(target, generation),
                )
                .await,
                Ok(Ok(()))
            );
        }
        if let Some(task) = super::launch::take_manager_launch_task(target) {
            if cooperative && generation.is_some() {
                // Let the monitor's own finalizer run before cancelling it.
                wait_task_exit(&task, Self::MANAGER_DRAIN_COOPERATIVE_WAIT).await;
            }
            if !task.is_finished() {
                task.abort();
            }
            if !wait_task_exit(&task, Self::MANAGER_DRAIN_TASK_EXIT_WAIT).await {
                super::launch::register_manager_launch_task(target, task);
                return Err(refused(MANAGER_LAUNCH_DRAIN_PENDING));
            }
        }
        if let Some(generation) = generation {
            let removed = {
                let mut active = self.active.write().await;
                if active
                    .get(&target)
                    .is_some_and(|s| s.spawn_generation == generation)
                {
                    active.remove(&target)
                } else {
                    None
                }
            };
            if let Some(process) = removed.as_ref().and_then(|t| t.process.as_ref()) {
                let _ = process.interrupt();
            }
        }
        let orphan_store = std::sync::Arc::clone(&self.store);
        match tokio::task::spawn_blocking(move || {
            let turns = crate::store_lock::blocking_lock(
                &orphan_store,
                "manager_actions::drain_manager_launch_candidate",
            )
            .list_active_provider_turn_custody()?;
            super::reaper::reap_orphans_for_session(target, turns)
        })
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::warn!(session_id = %target, %error, "manager launch process reap failed");
                return Err(refused(MANAGER_LAUNCH_DRAIN_PENDING));
            }
            Err(error) => {
                tracing::warn!(session_id = %target, %error, "manager launch process reap did not run");
                return Err(refused(MANAGER_LAUNCH_DRAIN_PENDING));
            }
        }
        let still_active =
            self.active.read().await.get(&target).is_some_and(|s| {
                generation.is_none_or(|generation| s.spawn_generation == generation)
            });
        if still_active {
            return Err(refused(MANAGER_LAUNCH_DRAIN_PENDING));
        }
        Ok(())
    }

    /// #1143: retry owed drains. Exit is proved again (task joined, cohort
    /// reaped) before the candidate rows settle and the debt clears. A target
    /// back in `active` (an operator continuation) is never touched.
    async fn reconcile_manager_launch_drain_debt(&self) -> usize {
        let targets = match crate::store_lock::lock(
            &self.store,
            "manager_actions::reconcile_manager_launch_drain_debt",
        )
        .await
        .manager_launch_drain_pending_targets(32)
        {
            Ok(targets) => targets,
            Err(error) => {
                tracing::warn!(%error, "manager launch drain-debt scan failed");
                return 0;
            }
        };
        let mut cleared = 0;
        for target in targets {
            if self.active.read().await.contains_key(&target) {
                continue;
            }
            if let Err(error) = self.drain_manager_launch_candidate(target, None).await {
                tracing::warn!(session_id = %target, %error, "manager launch drain remains owed");
                continue;
            }
            if let Err(error) = self.settle_failed_manager_launch_target(target).await {
                tracing::warn!(session_id = %target, %error, "manager launch drain settlement remains owed");
                continue;
            }
            match crate::store_lock::lock(
                &self.store,
                "manager_actions::reconcile_manager_launch_drain_debt",
            )
            .await
            .clear_manager_launch_drain_pending(target)
            {
                Ok(_) => cleared += 1,
                Err(error) => {
                    tracing::warn!(session_id = %target, %error, "manager launch drain debt not cleared");
                }
            }
        }
        cleared
    }

    /// #985: heap future for [`Self::stop_manager_candidate`], which awaits
    /// interrupt, guarded quiescence and spawn-blocking reaping.
    #[inline(never)]
    fn boxed_stop_manager_candidate(
        &self,
        target: Uuid,
        generation: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(self.stop_manager_candidate(target, generation))
    }

    async fn stop_manager_candidate(&self, target: Uuid, generation: u64) -> Result<()> {
        let _guard = super::spawn_single_flight::acquire_spawn_guard(target).await;
        if !self
            .active
            .read()
            .await
            .get(&target)
            .is_some_and(|s| s.spawn_generation == generation)
        {
            return Ok(());
        }
        super::lifecycle::interrupt_active_in_maps_from(
            &self.active,
            target,
            crate::terminal_cause::InterruptSource::ManagerAction,
        )
        .await?;
        let deadline = tokio::time::Instant::now() + Self::CONTINUE_INTERRUPT_WAIT;
        loop {
            if !self
                .active
                .read()
                .await
                .get(&target)
                .is_some_and(|s| s.spawn_generation == generation)
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(refused("manager_v2_candidate_unconfirmed"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let orphan_store = std::sync::Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let turns = crate::store_lock::blocking_lock(
                &orphan_store,
                "manager_actions::stop_manager_candidate",
            )
            .list_active_provider_turn_custody()?;
            super::reaper::reap_orphans_for_session(target, turns)
        })
        .await
        .map_err(|_| refused("manager_v2_candidate_unconfirmed"))??;
        Ok(())
    }

    /// RME-S2A-003: Reconcile uncertain integrate actions by checking
    /// actual refs. If the target matches the stored candidate, the publish
    /// succeeded; if it still matches expected_tip, it did not. This unblocks
    /// the per-target singleton without duplicate publication.
    async fn reconcile_uncertain_integrate_claims(&self) -> Result<usize> {
        // RME-S2A-003: Reconcile uncertain integrate actions by checking
        // actual refs. Uses settle_integrate_claim (CAS) instead of
        // finish_manager_action, which requires a Running claim with the
        // original boot_id — the crashed boot's id is gone.
        let uncertain_ops: Vec<crate::store::manager_actions::ManagerActionOperationV2> = {
            let store = crate::store_lock::lock(
                &self.store,
                "manager_actions::reconcile_uncertain_integrate_claims",
            )
            .await;
            let mut stmt = store.conn.prepare(
                "SELECT id FROM harness_manager_v2_operations
                 WHERE kind='lifecycle_action' AND state='uncertain'
                 AND json_extract(payload_json,'$.request.operation.action')='integrate'
                 ORDER BY id LIMIT 16",
            )?;
            let ids: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            ids.into_iter()
                .filter_map(|id| {
                    let oid = uuid::Uuid::parse_str(&id).ok()?;
                    store.manager_action_operation(oid).ok().flatten()
                })
                .collect()
        };
        let mut count = 0;
        for op in uncertain_ops {
            let (target_ref, project_id, manager_session_id, scope_version, actor, operation_id) = {
                let target_ref = match &op.context.request.operation {
                    ManagerActionV2::Integrate { target_ref, .. } => target_ref.clone(),
                    _ => continue,
                };
                let actor = match &op.context.origin {
                    crate::store::manager_actions::ManagerActionOriginV2::Agent { caller } => {
                        Some(*caller)
                    }
                    _ => None,
                };
                (
                    target_ref,
                    op.project_id,
                    op.manager_session_id,
                    op.scope_version,
                    actor,
                    op.receipt.operation_id,
                )
            };
            let repo = self.resolve_project_working_dir(project_id).await?;
            let resolved = git_ref(&repo, &target_ref).await;
            let store = crate::store_lock::lock(
                &self.store,
                "manager_actions::reconcile_uncertain_integrate_claims",
            )
            .await;
            if let Some(ref_oid) = resolved {
                match store.reconcile_integrate_claim(
                    &crate::store::manager_actions::ManagerActionClaimV2 {
                        operation: op,
                        boot_id: self.program_run_boot_id,
                    },
                    &ref_oid,
                )? {
                    ManagerActionStateV2::Succeeded => {
                        store.settle_integrate_claim(
                            operation_id,
                            project_id,
                            manager_session_id,
                            scope_version,
                            actor,
                            ManagerActionStateV2::Succeeded,
                            "integrated_crash_reconciled",
                        )?;
                    }
                    ManagerActionStateV2::Failed => {
                        store.settle_integrate_claim(
                            operation_id,
                            project_id,
                            manager_session_id,
                            scope_version,
                            actor,
                            ManagerActionStateV2::Failed,
                            "not_integrated_crash_reconciled",
                        )?;
                    }
                    _ => {} // Still genuinely uncertain; leave it
                }
            }
            count += 1;
        }
        Ok(count)
    }

    /// Heap future for the actions dispatched ahead of the lifecycle match:
    /// `Integrate` (unchanged executor) and K14 `OperatorCall` (runtime gate,
    /// then the delegated executor).
    #[inline(never)]
    fn boxed_dispatched_manager_action<'a>(
        &'a self,
        claim: &'a ManagerActionClaimV2,
    ) -> super::delegated_operator::DelegatedOperatorFuture<'a> {
        if matches!(claim.action(), ManagerActionV2::Integrate { .. }) {
            Box::pin(self.execute_integrate_action(claim))
        } else {
            self.boxed_delegated_operator_call(claim)
        }
    }

    async fn execute_integrate_action(&self, claim: &ManagerActionClaimV2) -> Result<()> {
        use crate::integration::{self, IntegrationConfig, Prepared};
        let (target_ref, expected_tip, source_commit) = match claim.action() {
            ManagerActionV2::Integrate {
                work_key: _,
                target_ref,
                expected_tip,
                source_commit,
            } => (
                target_ref.clone(),
                expected_tip.clone(),
                source_commit.clone(),
            ),
            _ => return Err(refused("manager_v2_not_integrate_action")),
        };
        // Pre-effect gate: verify claim, authority, scope, policy.
        crate::store_lock::lock(&self.store, "manager_actions::execute_integrate_action")
            .await
            .manager_v2_integrate_runtime_gate(claim, false)?;
        let repo = self
            .resolve_project_working_dir(claim.operation.project_id)
            .await?;
        let config = IntegrationConfig {
            allowed_targets: vec![target_ref.clone()],
            identity: integration::CommitIdentity {
                name: "rsi integration".into(),
                email: "integration@rsi.local".into(),
            },
            git_timeout: std::time::Duration::from_secs(120),
        };
        let scratch_dir = self
            .sandbox_allocator
            .base_dir()
            .join("integration-scratch");
        tokio::fs::create_dir_all(&scratch_dir)
            .await
            .map_err(|e| DaemonError::Process(format!("scratch dir: {e}")))?;
        let prepared = integration::prepare_candidate(
            &config,
            &repo,
            &target_ref,
            &expected_tip,
            &source_commit,
            &scratch_dir,
        )
        .await?;
        match prepared {
            Prepared::AlreadyIntegrated => {
                crate::store_lock::lock(&self.store, "manager_actions::execute_integrate_action")
                    .await
                    .finish_manager_action(
                        claim,
                        ManagerActionStateV2::Succeeded,
                        "already_integrated",
                    )?;
                Ok(())
            }
            Prepared::Candidate(candidate) => {
                // D20: Recheck acceptance at the effect boundary. A crash after
                // effect_started is Uncertain; a failed acceptance recheck
                // never marks effect_started and must clean up the candidate.
                //
                // The guard from `manager_v2_integrate_runtime_gate` must drop
                // before `discard_candidate` awaits so the MutexGuard does not
                // live across the async cleanup.
                let gate_result = {
                    let store = crate::store_lock::lock(
                        &self.store,
                        "manager_actions::execute_integrate_action",
                    )
                    .await;
                    store.manager_v2_integrate_runtime_gate(claim, true)
                };
                if let Err(error) = gate_result {
                    let _ = integration::discard_candidate(&config, &repo, &candidate.handle).await;
                    return Err(error);
                }
                // RME-S2A-002: Persist the candidate OID so crash recovery can
                // determine whether publish succeeded.
                {
                    let store = crate::store_lock::lock(
                        &self.store,
                        "manager_actions::execute_integrate_action",
                    )
                    .await;
                    store.persist_integrate_candidate(claim, &candidate.oid)?;
                }
                // RME-S2A-002: Verify target ref hasn't moved between prepare
                // and publish. The engine's publish also checks this, but the
                // durable custody record provides evidence for crash recovery.
                let resolved_ref = git_ref(&repo, &target_ref).await;
                if let Some(ref_oid) = resolved_ref {
                    let custody_result = {
                        let store = crate::store_lock::lock(
                            &self.store,
                            "manager_actions::execute_integrate_action",
                        )
                        .await;
                        store.verify_integrate_custody(claim, &ref_oid)
                    };
                    if let Err(error) = custody_result {
                        let _ =
                            integration::discard_candidate(&config, &repo, &candidate.handle).await;
                        return Err(error);
                    }
                }
                let publish_result = integration::publish(
                    &config,
                    &repo,
                    &target_ref,
                    &expected_tip,
                    &candidate.oid,
                )
                .await;
                // Best-effort cleanup of the candidate worktree.
                let _ = integration::discard_candidate(&config, &repo, &candidate.handle).await;
                publish_result?;
                crate::store_lock::lock(&self.store, "manager_actions::execute_integrate_action")
                    .await
                    .finish_manager_action(
                        claim,
                        ManagerActionStateV2::Succeeded,
                        "source_integrated",
                    )?;
                Ok(())
            }
        }
    }

    async fn confirm_manager_candidate(
        &self,
        claim: &ManagerActionClaimV2,
        target: Uuid,
        generation: u64,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if let Some(tracked) = self.active.write().await.get_mut(&target) {
                if tracked.spawn_generation != generation || generation == 0 {
                    return Err(refused("manager_v2_candidate_changed"));
                }
                if tracked.interrupt_requested {
                    return Err(refused("manager_v2_candidate_cancelled"));
                }
                if tracked.process.as_mut().is_some_and(|process| {
                    manager_provider_established(tracked.session.provider, process)
                }) {
                    return Ok(());
                }
            } else {
                return Err(refused("manager_v2_candidate_unconfirmed"));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(refused("manager_v2_candidate_unconfirmed"));
            }
            // Scope revocation stops the candidate; it never wins the lead CAS.
            let scope_gate = {
                crate::store_lock::lock(&self.store, "manager_actions::confirm_manager_candidate")
                    .await
                    .manager_action_runtime_gate(claim, false)
            };
            if let Err(error) = scope_gate {
                return Err(error);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    /// The lead-assignment commit. Its critical section holds the active map and
    /// the Store while it authenticates the candidate's custody, so it must never
    /// wait for the candidate's custody stripe there: a maintenance proof on that
    /// stripe would pin the Store and every RPC (#1166). A busy stripe releases
    /// everything, backs off asynchronously and retries the whole (idempotent,
    /// pre-write) critical section.
    async fn commit_manager_assignment(
        &self,
        claim: &ManagerActionClaimV2,
        generation: Option<u64>,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + MANAGER_ASSIGNMENT_STRIPE_BUDGET;
        loop {
            match self.commit_manager_assignment_once(claim, generation).await {
                Err(error)
                    if crate::store::custody_lock_order::is_lock_order_busy_error(&error)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(MANAGER_ASSIGNMENT_STRIPE_BACKOFF).await;
                }
                result => return result,
            }
        }
    }

    async fn commit_manager_assignment_once(
        &self,
        claim: &ManagerActionClaimV2,
        generation: Option<u64>,
    ) -> Result<()> {
        let require_established = generation.is_some();
        #[cfg(test)]
        if require_established {
            super::launch::pause_controller_candidate_test(
                claim
                    .operation
                    .context
                    .target_session_id
                    .expect("candidate"),
                super::launch::ControllerCandidateTestPhase::BeforeAssignment,
            )
            .await;
        }
        let target = claim.operation.context.target_session_id;
        // Match custody and model invocation while the active incarnation is
        // locked; a stale or merely allocated Starting row cannot win a lead.
        let mut active = self.active.write().await;
        let mut store = crate::store_lock::lock(
            &self.store,
            "manager_actions::commit_manager_assignment_once",
        )
        .await;
        if let Some(target) = target {
            let candidate = store
                .get_session(target)?
                .ok_or_else(|| refused("manager_v2_candidate_unavailable"))?;
            if matches!(claim.action(), ManagerActionV2::AssignLead { .. }) {
                store.manager_restart_human_gate(target, false)?;
            } else {
                store.manager_action_human_gate(target)?;
            }
            if candidate.pending_question.is_some()
                || candidate.pending_archive
                || candidate.status == SessionStatus::WaitingApproval
            {
                return Err(refused("manager_v2_human_or_recovery_owner"));
            }
            if require_established {
                let invocation = store
                    .session_model_invocation_id(target)?
                    .ok_or_else(|| refused("manager_v2_candidate_unconfirmed"))?;
                let bound:bool=store.conn.query_row("SELECT EXISTS(SELECT 1 FROM model_invocations WHERE id=?1 AND session_id=?2 AND project_id=?3 AND dedup_key=?4)",rusqlite::params![invocation.to_string(),target.to_string(),claim.operation.project_id.to_string(),format!("manager.action:{}",claim.id())],|r|r.get(0))?;
                if !bound {
                    return Err(refused("manager_v2_candidate_unconfirmed"));
                }
            }
            if let Some(tracked) = active.get_mut(&target) {
                if generation
                    .is_some_and(|expected| expected == 0 || expected != tracked.spawn_generation)
                {
                    return Err(refused("manager_v2_candidate_changed"));
                }
                if tracked.interrupt_requested
                    || !tracked.process.as_mut().is_some_and(|process| {
                        manager_provider_established(tracked.session.provider, process)
                    })
                {
                    return Err(refused("manager_v2_candidate_unconfirmed"));
                }
                let tokens = self.agent_tokens.read().await;
                if tokens.binding_for_session(target).is_none() {
                    return Err(refused("manager_v2_candidate_unconfirmed"));
                }
            } else if require_established
                || matches!(
                    candidate.status,
                    SessionStatus::Starting
                        | SessionStatus::Running
                        | SessionStatus::WaitingApproval
                )
            {
                return Err(refused("manager_v2_candidate_unconfirmed"));
            }
            match crate::sandbox::custody::CustodyService::classify(&candidate)? {
                crate::sandbox::custody::CustodyClassification::OrdinaryUnsandboxed => {
                    crate::sandbox::custody::CustodyService::authorize_ordinary(&candidate)?;
                }
                crate::sandbox::custody::CustodyClassification::RequiresPersistedAuthentication => {
                    crate::sandbox::custody::CustodyService::authorize_live_try(
                        &candidate,
                        &mut store,
                        self.sandbox_allocator.base_dir(),
                        rsi_common::types::SandboxCustodyTransitionV1::Continue,
                    )?;
                }
            }
        }
        let jobs = store.commit_manager_lead_action(claim)?;
        drop(store);
        drop(active);
        for job_id in jobs {
            self.event_bus
                .publish(DaemonEvent::ManagerNoticeQueued { job_id });
        }
        let epic = action_epic(claim.action()).expect("lead action");
        self.set_lead_in_memory(epic, target).await;
        if let Some(target) = target {
            self.mark_lead_session(target).await;
        }
        self.publish_lead_delta(epic, target);
        if let Some(old) = action_fence(claim.action())
            .and_then(|f| f.lead_session_id)
            .filter(|id| Some(*id) != target)
        {
            self.revoke_agent_token_for_session(old).await;
        }
        Ok(())
    }
}

/// Fresh manager launches reauthenticate the predecessor immediately before
/// the effect. Same-session resume has no fork source and keeps dirty custody.
pub(super) async fn manager_action_source_gate(
    runtime: &crate::sandbox::custody::CustodyExecutionRuntime,
    store: &std::sync::Arc<tokio::sync::Mutex<crate::store::Store>>,
    claim: &ManagerActionClaimV2,
) -> Result<()> {
    let Some(frozen) = claim.operation.context.source.as_ref() else {
        return Ok(());
    };
    let source = crate::store_lock::lock(&store, "manager_actions::manager_action_source_gate")
        .await
        .get_session(frozen.session_id)?
        .ok_or_else(|| refused("manager_v2_source_unavailable"))?;
    let fork = if frozen.historical_commit {
        runtime
            .prepare_manager_review_fork_at(&source, &frozen.commit)
            .await?
    } else if frozen.forks_at_frozen_commit() {
        runtime
            .prepare_manager_action_fork_at(&source, &frozen.commit)
            .await?
    } else {
        runtime
            .prepare_manager_action_fork_pinned(&source, &frozen.commit, frozen.branch.as_deref())
            .await?
    };
    if source.working_dir != frozen.working_dir
        || source.sandbox_root != frozen.sandbox_root
        || fork.fork_commit() != frozen.commit
    {
        return Err(refused("manager_v2_source_changed"));
    }
    if let Some(generation) = frozen.custody_generation {
        let store =
            crate::store_lock::lock(&store, "manager_actions::manager_action_source_gate").await;
        let current = if frozen.historical_commit {
            store
                .manager_review_reclaimed_source(&source)?
                .map(|proof| proof.generation)
        } else {
            None
        };
        let current = match current {
            Some(generation) => generation,
            None => store.live_custody_for_session(source.id)?.generation,
        };
        if current != generation as u64 {
            return Err(refused("manager_v2_source_changed"));
        }
    }
    Ok(())
}

fn manager_provider_established(
    provider: SessionProvider,
    process: &mut super::types::ProviderProcess,
) -> bool {
    if provider == SessionProvider::CodexAppServer {
        // The process is installed only after initialize and thread/start.
        matches!(process, super::types::ProviderProcess::CodexAppServer(_)) && process.is_alive()
    } else {
        super::provider_spawn::installed_provider_confirmation(provider, process).is_some()
    }
}

/// Error-code list extracted so `safe_action_error` stays under the
/// `too_many_lines` threshold. Order matters only for matching priority.
const MANAGER_ACTION_ERROR_CODES: &[&str] = &[
    "manager_succession_action_required",
    "manager_succession_admission_already_exists",
    "manager_succession_admission_required",
    "manager_succession_already_committed",
    "manager_succession_appointment_missing",
    "manager_succession_authority_changed",
    "manager_succession_candidate_changed",
    "manager_succession_candidate_custody_changed",
    "manager_succession_candidate_history",
    "manager_succession_candidate_missing",
    "manager_succession_candidate_unconfirmed",
    "manager_succession_claim_changed",
    "manager_succession_cleanup_changed",
    "manager_succession_cleanup_required",
    "manager_succession_cleanup_unsettled",
    "manager_succession_custody_runtime_required",
    "manager_succession_depth_overflow",
    "manager_succession_distinct_custody_required",
    "manager_succession_effect_unavailable",
    "manager_succession_establishment_changed",
    "manager_succession_establishment_invalid",
    "manager_succession_establishment_uncertain",
    "manager_succession_handoff_blob_changed",
    "manager_succession_handoff_blob_invalid",
    "manager_succession_handoff_head_changed",
    "manager_succession_handoff_size",
    "manager_succession_handoff_unavailable",
    "manager_succession_identity_corrupt",
    "manager_succession_invalid_limit",
    "manager_succession_invalid_spend",
    "manager_succession_invocation_changed",
    "manager_succession_launch_invalid",
    "manager_succession_origin_required",
    "manager_succession_predecessor_changed",
    "manager_succession_predecessor_invocation_unproven",
    "manager_succession_predecessor_ledger_live",
    "manager_succession_predecessor_unsettled",
    "manager_succession_publication_changed",
    "manager_succession_publication_required",
    "manager_succession_resource_origin_missing",
    "manager_succession_resource_identity_changed",
    "manager_succession_resource_project_changed",
    "manager_succession_root_required",
    "manager_succession_rotation_disabled",
    "manager_succession_settlement_changed",
    "manager_succession_source_changed",
    "manager_succession_source_custody_changed",
    "manager_succession_token_changed",
    "manager_succession_transaction_required",
    "manager_succession_unavailable",
    "manager_succession_unresolved",
    "manager_succession_unsettled_predecessor",
    "manager_review_assignment_terminal",
    "manager_review_reclaimed_source_unavailable",
    "manager_v2_actor_seat_retired",
    // #1716: the queuing seat was replaced by another appointment while its
    // create was held; and the raw authority denials, so a stored action that
    // fails them any other way still names its real class.
    "manager_v2_actor_seat_replaced",
    "manager_scope_denied",
    "manager_current_session_required",
    "manager_v2_scope_changed",
    "manager_v2_policy_changed",
    "manager_v2_capability_denied",
    "manager_v2_epic_out_of_scope",
    "manager_v2_container_out_of_scope",
    "manager_v2_lead_changed",
    "manager_v2_launch_timeout_predecessor_guard",
    "manager_v2_launch_timeout_runtime_check",
    "manager_v2_launch_timeout_settle_predecessor",
    "manager_v2_launch_timeout_prepare_fork",
    "manager_v2_launch_timeout_launch_candidate",
    "manager_v2_launch_timeout_confirm_candidate",
    "manager_v2_launch_timeout",
    "manager_v2_launch_drain_pending",
    "container_not_empty",
    "manager_v2_container_changed",
    "manager_v2_container_state_changed",
    "manager_v2_policy_paused",
    "manager_v2_manager_paused",
    "manager_v2_pause_malformed",
    "manager_v2_pending_operator_decision",
    "manager_v2_decision_hold",
    "manager_v2_no_ready_work",
    "manager_v2_human_or_recovery_owner",
    "manager_v2_program_evidence_unknown",
    "manager_v2_retry_disabled",
    "manager_v2_resource_limit",
    "manager_v2_concurrency_capacity",
    "manager_v2_provider_capacity",
    "manager_v2_spend_exhausted",
    "manager_v2_provider_usage_limit",
    "manager_v2_spend_unknown",
    "manager_v2_launch_not_granted",
    "manager_v2_source_changed",
    "manager_v2_source_worktree_dirty",
    "manager_v2_replace_lead_requires_lead_use_create_session",
    "manager_v2_predecessor_unsettled",
    "manager_v2_candidate_unconfirmed",
    "manager_v2_candidate_cancelled",
    "manager_v2_lead_not_resumable",
    "manager_v2_resume_unavailable",
    "manager_v2_retry_lead_resumable",
    "manager_v2_retry_requires_terminal",
    "manager_v2_candidate_already_exists",
    "manager_v2_integrate_in_progress",
    "manager_v2_source_acceptance_required",
    "manager_v2_not_integrate_action",
    // K14 (#672) delegated operator calls.
    "manager_v2_operator_method_not_delegable",
    "manager_v2_operator_params_invalid",
    "manager_v2_operator_fence_required",
    "manager_v2_operator_result_too_large",
    "manager_v2_target_out_of_project",
    "manager_v2_retention_pinned",
    "manager_v2_retention_enabled_wake",
    "manager_v2_retention_live_review",
    "manager_v2_retention_sealed_source",
    "manager_v2_retention_recent_activity",
    "manager_v2_retention_live_worktree",
    "manager_v2_effect_unobservable",
    "manager_v2_action_not_uncertain",
    "manager_v2_action_changed",
    "manager_v2_action_out_of_scope",
    "manager_v2_action_unavailable",
    "manager_v2_lead_active",
];

/// Static receipt classes for every typed pre-provider launch refusal. Only
/// fixed strings are returned: no path, environment value or credential text.
fn launch_refusal_code(error: &DaemonError) -> Option<&'static str> {
    match error {
        DaemonError::ExecutionScratchUnavailable(_) => {
            Some("manager_v2_execution_scratch_unavailable")
        }
        // #2920: the sandbox capacity gate keeps its own typed class. A free-space
        // shortfall is requeued and held before it gets here (only a refusal after
        // the effect began ends the action); a root count at the cap is genuinely
        // unsatisfiable until a sandbox is released.
        DaemonError::StructuredRpc {
            rpc_code: -32029,
            data,
            ..
        } if data["kind"] == "sandbox_capacity" => Some(match data["code"].as_str() {
            Some("source_root_limit") => "manager_v2_sandbox_source_root_limit",
            _ => "manager_v2_sandbox_free_space_limit",
        }),
        DaemonError::ClaudeBinaryNotFound => Some("manager_v2_claude_binary_not_found"),
        DaemonError::CodexBinaryNotFound => Some("manager_v2_codex_binary_not_found"),
        DaemonError::AgyBinaryNotFound => Some("manager_v2_agy_binary_not_found"),
        DaemonError::CodexResumeToolHistory(_) => {
            Some("manager_v2_codex_resume_tool_history_invalid")
        }
        DaemonError::CodexResumeTornTail => Some("manager_v2_codex_resume_rollout_torn_tail"),
        DaemonError::OpenRouterRoutePreflight { cause } => Some(match *cause {
            "missing_credential" => "manager_v2_provider_credential_missing_openrouter",
            "no_tool_support" => "manager_v2_openrouter_route_no_tool_support",
            "unsupported_model" => "manager_v2_openrouter_route_unsupported_model",
            _ => "manager_v2_openrouter_route_preflight_failed",
        }),
        _ => {
            let text = error.to_string();
            if text.contains("OpenRouter credential") {
                Some("manager_v2_provider_credential_missing_openrouter")
            } else if text.contains("Bedrock API key") {
                Some("manager_v2_provider_credential_missing_bedrock")
            } else if text.contains("Pioneer credential") {
                Some("manager_v2_provider_credential_missing_pioneer")
            } else if text.contains("model_unavailable: provider") {
                // #2735: the provider refused the model; the receipt keeps the typed
                // code instead of a generic lifecycle-unconfirmed class.
                Some("model_unavailable")
            } else if text.contains("provider_login_required: the ") {
                // #2753: the provider CLI is logged out on the host; the receipt
                // keeps the typed code (the fix is the operator's login).
                Some("provider_login_required")
            } else if text
                .contains("the installed codex-cli does not trust the sandbox custody hook")
            {
                // #2755: the Codex custody hook is not trusted; the receipt keeps
                // the typed reason (the fix is RSI's hook trust for this codex-cli).
                rsi_common::provider_contract::CODEX_CUSTODY_REASONS
                    .into_iter()
                    .find(|reason| text.contains(&format!("{reason}: ")))
            } else {
                None
            }
        }
    }
}

/// #2920: whether `error` is the sandbox allocator's free-space refusal (not the
/// root-count cap, which stays a refusal).
fn is_sandbox_free_space_error(error: &DaemonError) -> bool {
    matches!(
        error,
        DaemonError::StructuredRpc { rpc_code: -32029, data, .. }
            if data["kind"] == "sandbox_capacity" && data["code"] == "free_space_limit"
    )
}

pub(super) fn safe_action_error(error: &DaemonError) -> &'static str {
    // #981/#1084: a typed launch refusal keeps its own static class in the
    // receipt, so a manager can read why a launch failed without the daemon log.
    if let Some(code) = launch_refusal_code(error) {
        return code;
    }
    let message = error.to_string();
    if message.contains("manager_notice_deferred") {
        return "manager_v2_human_or_recovery_owner";
    }
    if message.contains("source_worktree_dirty") {
        return "manager_v2_source_worktree_dirty";
    }
    for &code in MANAGER_ACTION_ERROR_CODES {
        if message.contains(code) {
            return code;
        }
    }
    "manager_v2_lifecycle_unconfirmed"
}

/// Resolve a Git ref to its commit OID using the hardened engine git helper.
async fn git_ref(repo: &std::path::Path, ref_name: &str) -> Option<String> {
    use crate::integration::{CommitIdentity, IntegrationConfig};
    let config = IntegrationConfig {
        allowed_targets: vec![ref_name.to_string()],
        identity: CommitIdentity {
            name: "rsi integration".into(),
            email: "integration@rsi.local".into(),
        },
        git_timeout: std::time::Duration::from_secs(30),
    };
    crate::integration::resolve_ref(&config, repo, ref_name).await
}

fn manager_launch_config(
    claim: &ManagerActionClaimV2,
    source: &Session,
    continued: Option<&Session>,
) -> Result<LaunchConfig> {
    let choice = claim
        .operation
        .context
        .launch
        .as_ref()
        .ok_or_else(|| refused("manager_v2_launch_unavailable"))?;
    let (query, kind, parent, predecessor) = match claim.action() {
        ManagerActionV2::CreateSession {
            query,
            kind,
            parent_id,
            ..
        } => (query.clone(), *kind, *parent_id, None),
        ManagerActionV2::ReplaceLead { query, epic_id, .. } => (
            query.clone(),
            source.session_kind,
            *epic_id,
            Some(source.id),
        ),
        ManagerActionV2::RetryLead {
            message, epic_id, ..
        } => (
            message.clone(),
            source.session_kind,
            *epic_id,
            Some(source.id),
        ),
        _ => return Err(refused("manager_v2_not_launch_action")),
    };
    let sandbox = Some(rsi_common::types::SandboxSpec {
        kind: Some(rsi_common::types::SandboxKind::GitWorktree),
        branch: None,
    });
    Ok(LaunchConfig {
        completion_gates: None,
        query,
        title: None,
        agent_role: match continued {
            Some(previous) => previous.agent_role.clone(),
            None => predecessor.and_then(|_| source.agent_role.clone()),
        },
        epic_spawn_ordinal: match continued {
            Some(previous) => previous.epic_spawn_ordinal,
            None => predecessor.and(source.epic_spawn_ordinal),
        },
        working_dir: Some(source.working_dir.clone()),
        provider: Some(choice.provider),
        model: Some(choice.model.clone()),
        configured_context_window: None,
        max_turns: None,
        system_prompt: None,
        resume_session_id: None,
        session_kind: Some(kind),
        project_id: Some(claim.operation.project_id),
        rsi_session_id: claim.operation.context.target_session_id,
        rsi_socket: None,
        rsi_session_token: None,
        continued_from: predecessor.or_else(|| continued.map(|previous| previous.id)),
        openai_base_url: None,
        openai_api_key: None,
        conversation_history: None,
        workflow_id: source.workflow_id,
        workflow_id_override: None,
        max_retries: Some(0),
        group_id: None,
        parent_id: Some(parent),
        effort: choice.effort.clone(),
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        model_invocation_owner: None,
        model_invocation_dedup_key: Some(format!("manager.action:{}", claim.id())),
        model_invocation_request_fingerprint: Some(crate::model_control::hash_request_fingerprint(
            &[&claim.id().to_string()],
        )),
        // The reservation freezes the effective provider/model witness.
        // A later project-default edit must not rewrite an exact replay.
        skip_project_model_default: true,
        tool_policy: None,
        model_invocation_purpose:
            rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
        sandbox,
        cargo_target_dir: None,
        execution_scratch: None,
        is_eval: false,
        skip_context_pipeline: false,
        capability_class: None,
        tags: source.tags.clone(),
        topology_node_id: None,
        topology_iteration: 0,
        closure_selector: None,
        structured_handoff: false,
    })
}
