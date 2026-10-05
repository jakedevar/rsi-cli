mod child_gate;
#[cfg(test)]
mod child_gate_tests;
mod wake_when;
#[cfg(test)]
mod wake_when_tests;

pub(crate) use crate::store_support::wake_target::is_live_wake_target;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use rsi_common::schedule::next_fire_time;
#[cfg(test)]
use rsi_common::types::SessionStatus;
use rsi_common::types::{Recurrence, SessionKind, WakeMode};

use crate::bus::{DaemonEvent, EventBus};
use crate::claude::LaunchConfig;
use crate::error::DaemonError;
use crate::issue_tracker::poller::{SessionLauncher, WatchFireOutcome};
use crate::model_control::hash_request_fingerprint;
use crate::store::Store;
use crate::store::manager_actions::fence::{continuation_fence_code, continuation_fence_retryable};
use crate::store::manager_watch_settlement::WatchFireCapture;
use crate::store::scheduled_jobs::RetentionSweepReport;
use crate::watchdog::LoopHeartbeat;

/// Capacity/pause refusals that are transient for a scheduled Resume: they
/// describe current capacity or operator/manager pause state, never the wake
/// target or its message, so the same wake may succeed unchanged once the
/// condition clears. `DaemonError` renders them as text (e.g.
/// `Policy denied: Invalid parameter: manager_v2_concurrency_capacity`), so the
/// match is by stable code substring.
const TRANSIENT_RESUME_REFUSAL_CODES: [&str; 4] = [
    "manager_v2_concurrency_capacity",
    "manager_v2_provider_capacity",
    "background model work is paused",
    "all model work is stopped",
];

/// The transient capacity/pause code carried by `error`, if any.
pub(crate) fn transient_resume_refusal_code(error: &DaemonError) -> Option<&'static str> {
    let text = error.to_string();
    TRANSIENT_RESUME_REFUSAL_CODES
        .iter()
        .find(|code| text.contains(**code))
        .copied()
}

/// True when a scheduled Resume refusal must retain the wake with bounded
/// backoff instead of consuming it: the K2 continuation-fence refusals plus
/// transient capacity/pause refusals (#996).
pub(crate) fn retryable_resume_refusal(error: &DaemonError) -> bool {
    continuation_fence_retryable(error)
        || transient_resume_refusal_code(error).is_some()
        // #1073: the drain engaged between the wake precheck and the
        // continuation; the wake stays due and is retried after the deploy.
        || crate::deploy_drain::is_draining_error(error)
        // #1157: effect admission surrendered a busy custody stripe after its
        // bounded wait; nothing ran, so the wake backs off and the scheduler
        // keeps serving other jobs.
        || crate::store::custody_lock_order::is_lock_order_busy_error(error)
}

/// The retry code recorded and logged for a retained wake refusal.
fn resume_refusal_code(error: &DaemonError) -> &'static str {
    continuation_fence_code(error)
        .or_else(|| transient_resume_refusal_code(error))
        .or_else(|| {
            crate::store::custody_lock_order::is_lock_order_busy_error(error)
                .then_some("custody_root_busy")
        })
        .unwrap_or("continuation_fence")
}

pub enum SchedulerCommand {
    /// Fire a specific job immediately, regardless of schedule.
    TriggerNow(Uuid),
    /// Re-check all due jobs immediately.
    CheckNow,
    /// Graceful shutdown.
    Shutdown,
}

#[derive(Clone)]
pub struct SchedulerHandle {
    tx: mpsc::Sender<SchedulerCommand>,
}

impl SchedulerHandle {
    pub fn new(tx: mpsc::Sender<SchedulerCommand>) -> Self {
        Self { tx }
    }

    pub async fn trigger_now(&self, job_id: Uuid) -> crate::error::Result<()> {
        self.tx
            .send(SchedulerCommand::TriggerNow(job_id))
            .await
            .map_err(|_| DaemonError::ChannelClosed)
    }

    pub async fn check_now(&self) -> crate::error::Result<()> {
        self.tx
            .send(SchedulerCommand::CheckNow)
            .await
            .map_err(|_| DaemonError::ChannelClosed)
    }

    pub async fn shutdown(&self) -> crate::error::Result<()> {
        self.tx
            .send(SchedulerCommand::Shutdown)
            .await
            .map_err(|_| DaemonError::ChannelClosed)
    }
}

pub fn spawn_scheduler(
    store: Arc<Mutex<Store>>,
    bus: Arc<EventBus>,
    session_launcher: Arc<dyn SessionLauncher>,
    poll_interval_secs: u64,
) -> SchedulerHandle {
    spawn_scheduler_with_heartbeat(store, bus, session_launcher, poll_interval_secs, None)
}

/// Production entrypoint with a completion marker for the independent watchdog.
pub fn spawn_scheduler_with_heartbeat(
    store: Arc<Mutex<Store>>,
    bus: Arc<EventBus>,
    session_launcher: Arc<dyn SessionLauncher>,
    poll_interval_secs: u64,
    heartbeat: Option<LoopHeartbeat>,
) -> SchedulerHandle {
    let (tx, mut rx) = mpsc::channel::<SchedulerCommand>(16);

    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(poll_interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Check immediately on startup for catch-up
        process_due_jobs(&store, &bus, &session_launcher, heartbeat.as_ref()).await;
        if let Some(heartbeat) = &heartbeat {
            heartbeat.mark_completed();
        }

        // Issue #954: first retention run shortly after start (daemons restart
        // more often than hourly), then hourly on the scheduler's own clock.
        let mut next_retention = tokio::time::Instant::now() + RETENTION_FIRST_DELAY;
        // #1006: predicate wakes are evaluated on a fast lane so a wait for
        // jobs ends within seconds, not at the next general poll.
        let mut fast_lane =
            tokio::time::interval(tokio::time::Duration::from_secs(wake_when::FAST_LANE_SECS));
        fast_lane.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = fast_lane.tick() => {
                    process_due_wake_when(&store, &bus, &session_launcher).await;
                }
                _ = interval.tick() => {
                    process_due_jobs(&store, &bus, &session_launcher, heartbeat.as_ref()).await;
                    if tokio::time::Instant::now() >= next_retention {
                        next_retention = tokio::time::Instant::now() + RETENTION_INTERVAL;
                        run_retention_sweep(&store).await;
                    }
                    if let Some(heartbeat) = &heartbeat {
                        heartbeat.mark_completed();
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(SchedulerCommand::TriggerNow(job_id)) => {
                            fire_job_by_id(&store, &bus, &session_launcher, &job_id).await;
                            if let Some(heartbeat) = &heartbeat {
                                heartbeat.mark_completed();
                            }
                        }
                        Some(SchedulerCommand::CheckNow) => {
                            process_due_jobs(&store, &bus, &session_launcher, heartbeat.as_ref()).await;
                            if let Some(heartbeat) = &heartbeat {
                                heartbeat.mark_completed();
                            }
                        }
                        Some(SchedulerCommand::Shutdown) | None => {
                            tracing::info!("Scheduler shutting down");
                            break;
                        }
                    }
                }
            }
        }
    });

    SchedulerHandle::new(tx)
}

const RETENTION_FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const RETENTION_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const RETENTION_BATCH: usize = 200;
const RETENTION_MAX_BATCHES: usize = 50;

/// One retention run: bounded batches, each in its own IMMEDIATE transaction,
/// re-acquiring the store lock per batch so due-job firing and RPCs interleave.
/// A failed batch stops the run (it deleted nothing) and retries next hour.
async fn run_retention_sweep(store: &Arc<Mutex<Store>>) {
    let now = Utc::now();
    let mut total = RetentionSweepReport::default();
    let mut cursor = None;
    let mut batches = 0usize;
    while batches < RETENTION_MAX_BATCHES {
        batches += 1;
        let result = store
            .lock()
            .await
            .retention_sweep_batch(now, cursor, RETENTION_BATCH);
        match result {
            Ok((report, next)) => {
                total.absorb(&report);
                match next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            Err(error) => {
                tracing::warn!(%error, batches, "scheduled job retention batch failed; retrying next run");
                break;
            }
        }
        tokio::task::yield_now().await;
    }
    if total.scanned > 0 {
        tracing::info!(
            batches,
            scanned = total.scanned,
            deleted = total.deleted,
            deleted_manager_watches = total.deleted_manager_watches,
            kept_program_guards = total.kept_program_guards,
            kept_too_recent = total.kept_too_recent,
            kept_unreadable = total.kept_unreadable,
            "scheduled job retention run complete"
        );
    }
}

async fn process_due_jobs(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    launcher: &Arc<dyn SessionLauncher>,
    heartbeat: Option<&LoopHeartbeat>,
) {
    let now = Utc::now();
    let due_jobs = {
        let guard = store.lock().await;
        if let Err(error) = guard.reconcile_harness_manager_watches() {
            tracing::error!(%error, "manager watch reconciliation failed; retained notices will retry");
        }
        // #794 S3: materialize at most one keep-alive row per window for idle
        // parents whose children run long. Off unless the operator enabled it.
        let child_policy = guard.child_autonomy_policy();
        if let Err(error) = guard.reconcile_child_keepalives(now, &child_policy) {
            tracing::error!(%error, "child keep-alive reconciliation failed; will retry");
        }
        match guard.list_due_scheduled_jobs(&now) {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::error!("Failed to query due scheduled jobs: {e}");
                return;
            }
        }
    };

    for job in due_jobs {
        fire_job(store, bus, launcher, &job).await;
        if let Some(heartbeat) = heartbeat {
            heartbeat.mark_completed();
        }
    }
}

/// The fast lane: fire only the due predicate wakes.
async fn process_due_wake_when(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    launcher: &Arc<dyn SessionLauncher>,
) {
    let due = {
        let guard = store.lock().await;
        match guard.list_due_wake_when_jobs(&Utc::now()) {
            Ok(jobs) => jobs,
            Err(error) => {
                tracing::error!(%error, "failed to query due predicate wakes");
                return;
            }
        }
    };
    for job in due {
        fire_job(store, bus, launcher, &job).await;
    }
}

async fn fire_job_by_id(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    launcher: &Arc<dyn SessionLauncher>,
    job_id: &Uuid,
) {
    let job = {
        let guard = store.lock().await;
        match guard.program_guard_owner_for_job_id(job_id) {
            Ok(Some(owner_session_id)) => {
                tracing::warn!(
                    job_id = %job_id,
                    owner_session_id = %owner_session_id,
                    "manual trigger rejected for deterministic program guard"
                );
                return;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(
                    job_id = %job_id,
                    error = %error,
                    "manual trigger declined because program-guard ownership could not be resolved"
                );
                return;
            }
        }
        match guard.get_scheduled_job(job_id) {
            Ok(Some(j)) => j,
            Ok(None) => {
                tracing::warn!("Scheduled job {job_id} not found for manual trigger");
                return;
            }
            Err(e) => {
                tracing::error!("Failed to load scheduled job {job_id}: {e}");
                return;
            }
        }
    };
    fire_job_inner(store, bus, launcher, &job, true).await;
}

/// Due-list dispatch. Resume delivery is bound to the exact row, which is
/// revalidated under the target spawn guard (K2 review (a)).
async fn fire_job(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    launcher: &Arc<dyn SessionLauncher>,
    job: &rsi_common::types::ScheduledJob,
) {
    fire_job_inner(store, bus, launcher, job, false).await;
}

/// `manual` is an explicit operator trigger, which keeps its existing
/// fire-even-if-disabled Resume behavior.
async fn fire_job_inner(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    launcher: &Arc<dyn SessionLauncher>,
    job: &rsi_common::types::ScheduledJob,
    manual: bool,
) {
    // A restart or long scheduler stall must not deliver a heal outside its
    // wall-clock budget. Persist exhaustion before publishing its final notice.
    //
    // This recheck is not under the target's spawn guard. That is safe: it only
    // ever retires or exhausts the heal row, and `session/recovery.rs`
    // re-verifies the target (still Failed, still the lineage tip, no manual
    // resume/continue running) under the guard before any heal relaunch.
    let expired = {
        let guard = store.lock().await;
        match guard.transient_heal_state(job.id) {
            Ok(Some(state)) if !state.exhausted && job.enabled => {
                if !manual {
                    // A previous fire may have re-armed a new backoff epoch
                    // since this due-list snapshot was captured.
                    match guard.get_scheduled_job(&job.id) {
                        Ok(Some(current))
                            if current.enabled && current.next_fire_at == job.next_fire_at => {}
                        Ok(_) => return,
                        Err(error) => {
                            tracing::error!(%error, "heal row unavailable; wake retained");
                            return;
                        }
                    }
                }

                if state.window_expired(Utc::now()) {
                    Some(guard.defer_transient_heal(job.id, &state.last_reason, Utc::now(), true))
                } else {
                    None
                }
            }
            Ok(_) => None,
            Err(error) => {
                tracing::error!(%error, "heal state unavailable; wake retained");
                return;
            }
        }
    };
    if let Some(outcome) = expired {
        crate::session::transient_heal::publish_heal_outcome(
            bus,
            job.wake_session_id.unwrap_or(job.id),
            outcome,
        );
        return;
    }
    // Persisted manager ownership selects the guarded watch path independently
    // of mutable job fields. It captures and validates a fresh row below.
    let managed = {
        let guard = store.lock().await;
        guard.is_harness_manager_watch(job.id)
    };
    match managed {
        Ok(true) => {
            fire_terminal_watch_job(store, bus, launcher, job).await;
            return;
        }
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, "manager notice ownership unavailable");
            return;
        }
    }
    let program_guard_owner = {
        let guard = store.lock().await;
        guard.program_guard_owner_for_job_id(&job.id)
    };
    match program_guard_owner {
        Ok(Some(owner_session_id)) => {
            tracing::warn!(
                job_id = %job.id,
                owner_session_id = %owner_session_id,
                "scheduled execution rejected for deterministic program guard"
            );
            return;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::error!(
                job_id = %job.id,
                error = %error,
                "scheduled execution declined because program-guard ownership could not be resolved"
            );
            return;
        }
    }

    tracing::info!("Firing scheduled job '{}' (id={})", job.name, job.id);

    // V89 capacity rows are recognized by the incident FK before any generic
    // wake-mode branch. No malformed recognized row may fall through to Fresh
    // or AgentFresh.
    let capacity_validation = {
        let guard = store.lock().await;
        guard.validate_capacity_delivery(job, Utc::now())
    };
    match capacity_validation {
        Ok(crate::store::capacity_recovery::CapacityDeliveryValidation::NotCapacity) => {}
        Ok(crate::store::capacity_recovery::CapacityDeliveryValidation::NotDue) => {
            tracing::warn!(job_id = %job.id, "capacity wake trigger declined before persisted due slot");
            return;
        }
        Ok(crate::store::capacity_recovery::CapacityDeliveryValidation::StaleSnapshot) => {
            tracing::info!(job_id = %job.id, "stale capacity wake snapshot ignored; current due slot remains authoritative");
            return;
        }
        Ok(crate::store::capacity_recovery::CapacityDeliveryValidation::Ready(plan)) => {
            let delivery = rsi_common::daemon_message::wrap("provider-capacity", &job.message);
            match launcher
                .resume_capacity_scheduled(
                    plan.controller_session_id,
                    delivery,
                    plan.wake_job_id,
                    plan.due_slot,
                )
                .await
            {
                Ok(session_id) => {
                    tracing::info!(
                        job_id = %job.id,
                        "capacity provider launch durably confirmed"
                    );
                    let disabled = {
                        let guard = store.lock().await;
                        guard.disable_capacity_due_slot(job.id, plan.due_slot, Utc::now())
                    };
                    match disabled {
                        Ok(true) => {}
                        Ok(false) => tracing::info!(
                            job_id = %job.id,
                            "capacity wake due slot advanced during dispatch; stale disable changed nothing"
                        ),
                        Err(error) => tracing::error!(
                            job_id = %job.id,
                            error = %error,
                            "failed to compare-and-settle capacity due slot"
                        ),
                    }
                    bus.publish(DaemonEvent::ScheduledJobFired {
                        job_id: job.id,
                        job_name: job.name.clone(),
                        session_id,
                    });
                }
                // #1073: a waiting deploy holds the capacity wake; the due slot
                // stays retained and is retried once the deploy settles.
                Err(error) if crate::deploy_drain::is_draining_error(&error) => {
                    tracing::debug!(job_id = %job.id, "capacity wake held: deploy_draining");
                }
                Err(error) => {
                    let message = format!(
                        "Scheduled capacity Resume '{}' failed before durable launch confirmation: {error}",
                        job.name
                    );
                    tracing::error!(job_id = %job.id, "{message}");
                    bus.publish(DaemonEvent::SystemMessage {
                        level: "error".into(),
                        message,
                    });
                }
            }
            return;
        }
        Err(error) => {
            let disable_error = {
                let guard = store.lock().await;
                guard.disable_capacity_wake(job.id, Utc::now()).err()
            };
            let message = format!(
                "Disabled malformed recognized capacity wake '{}' (id={}): {error}{}",
                job.name,
                job.id,
                disable_error
                    .map(|failure| format!("; disable failed: {failure}"))
                    .unwrap_or_default()
            );
            tracing::error!("{message}");
            bus.publish(DaemonEvent::SystemMessage {
                level: "error".into(),
                message,
            });
            return;
        }
    }

    // A8 terminal watch: handled FIRST and returns unconditionally — an
    // OnTerminal job must never reach the Resume branch or the Fresh-launch
    // fallthrough below (T-10 pins this).
    if matches!(job.wake_mode, WakeMode::OnTerminal(_)) {
        fire_terminal_watch_job(store, bus, launcher, job).await;
        return;
    }

    // #1006: a predicate wake is evaluated by the daemon; it resumes its owner
    // only once the predicate settles (true, unsatisfiable or timed out).
    let predicate_job;
    let job = if job.wake_mode == WakeMode::Resume {
        match wake_when::gate(store, job, manual).await {
            wake_when::Gate::NotPredicate => job,
            wake_when::Gate::Pending => return,
            wake_when::Gate::Deliver(message) => {
                predicate_job = rsi_common::types::ScheduledJob {
                    message,
                    ..job.clone()
                };
                &predicate_job
            }
        }
    } else {
        job
    };

    // Branch on wake mode
    if job.wake_mode == WakeMode::Resume {
        match job.wake_session_id {
            Some(target) => {
                // #872: a global-manager message is delivered only while its
                // grant and recipient seat are current; a stale one is
                // retired without delivery, and an unreadable fence holds it.
                let deliverable = {
                    let guard = store.lock().await;
                    guard
                        .global_message_deliverable(job.id)
                        .and_then(|current| {
                            if !current {
                                guard.retire_global_message(job.id)?;
                            }
                            Ok(current)
                        })
                };
                match deliverable {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::info!(
                            job_id = %job.id,
                            target = %target,
                            "global manager message retired: grant or recipient seat changed"
                        );
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(
                            job_id = %job.id,
                            %error,
                            "global manager message fence unavailable; holding the wake"
                        );
                        return;
                    }
                }
                // #794 S3: hold a program master's wake while its children run;
                // retire a keep-alive row whose children settled. An operator
                // manual trigger bypasses both, and any read failure delivers
                // exactly as before this gate existed.
                if !manual {
                    // #1073: a waiting deploy holds a child's resume; the row
                    // stays due and is delivered once the deploy settles.
                    if launcher.deploy_holds_resume(target).await {
                        tracing::debug!(
                            job_id = %job.id,
                            target = %target,
                            "scheduled resume held: deploy_draining"
                        );
                        return;
                    }
                    // #572: an exhausted provider account holds the wake until
                    // its retry-after; it is re-armed there, never re-fired on
                    // the scheduler cadence.
                    if let Some(until) = launcher.provider_usage_hold(target).await {
                        hold_wake_for_provider_exhaustion(store, bus, job, target, until).await;
                        return;
                    }
                    let gate = {
                        let guard = store.lock().await;
                        child_gate::resume_gate(&guard, job, Utc::now())
                    };
                    match gate {
                        Ok(child_gate::ResumeGate::Deliver) => {}
                        Ok(child_gate::ResumeGate::Hold) => {
                            tracing::debug!(
                                job_id = %job.id,
                                target = %target,
                                "scheduled resume held while children run"
                            );
                            return;
                        }
                        Ok(child_gate::ResumeGate::RetireKeepalive) => {
                            let retired = {
                                let guard = store.lock().await;
                                child_gate::retire_keepalive(&guard, job)
                            };
                            if let Err(error) = retired {
                                tracing::error!(job_id = %job.id, %error, "failed to retire settled keep-alive row");
                            }
                            return;
                        }
                        Err(error) => tracing::warn!(
                            job_id = %job.id,
                            %error,
                            "child gate unavailable; delivering the wake"
                        ),
                    }
                }
                // Daemon-attributed delivery: the resume rides the user turn
                // (the only headless-CLI inbound channel), so envelope the
                // payload as daemon traffic. Slash-command payloads pass
                // through verbatim inside `wrap`.
                let delivery = rsi_common::daemon_message::wrap("scheduled-wake", &job.message);
                // Bound to this exact row: a stale due-list snapshot of a job
                // retired or disabled since capture must not resume its target.
                // #872: a global-manager message is always delivered through the
                // job-bound path, so the effect claim re-checks it atomically
                // against the actual resumed tip (an unreadable flag binds).
                let job_bound =
                    !manual || store.lock().await.is_global_message(job.id).unwrap_or(true);
                let resumed = if !job_bound {
                    launcher.resume_scheduled(target, delivery).await
                } else {
                    launcher
                        .resume_scheduled_job(target, delivery, vec![job.id])
                        .await
                };
                match resumed {
                    Ok(session_id) => {
                        tracing::info!(
                            "Scheduled resume job '{}' -> resumed session {session_id}",
                            job.name
                        );
                        // Delivery settles the row and clears the K2 retry
                        // state in one write.
                        advance_job_after_attempt_clearing_retry(store, job).await;
                        // A very short provider turn can finalize Failed while
                        // this wake is still enabled, so its hook defers to
                        // this delivery owner. After settling, reconcile that
                        // failure once; a concurrently armed heal is idempotent.
                        if store
                            .lock()
                            .await
                            .transient_heal_state(job.id)
                            .ok()
                            .flatten()
                            .is_some()
                        {
                            launcher
                                .reconcile_transient_heal_after_resume(session_id)
                                .await;
                        }

                        bus.publish(DaemonEvent::ScheduledJobFired {
                            job_id: job.id,
                            job_name: job.name.clone(),
                            session_id,
                        });
                    }
                    // K2: an operator "trigger now" is never consumed by a
                    // fence refusal; the typed code is reported and the row
                    // is left exactly as it was.
                    Err(e) if manual && continuation_fence_code(&e).is_some() => {
                        tracing::warn!(job_id = %job.id, error = %e, "Manual resume trigger refused by the continuation fence");
                        bus.publish(DaemonEvent::SystemMessage {
                            level: "warn".into(),
                            message: format!(
                                "Scheduled resume job '{}' not delivered: {e}",
                                job.name
                            ),
                        });
                    }
                    // #1073: the drain engaged between the precheck and the
                    // continuation. The row stays due, untouched (no retry
                    // attempt is spent), and is delivered after the deploy.
                    Err(e) if crate::deploy_drain::is_draining_error(&e) => {
                        tracing::debug!(
                            job_id = %job.id,
                            target = %target,
                            "scheduled resume held: deploy_draining (race with the continuation)"
                        );
                    }
                    // #572: the hold engaged between the precheck and the
                    // continuation; re-arm at its deadline, spending no attempt.
                    Err(e) if crate::provider_exhaustion::hold_error_until(&e).is_some() => {
                        let until = crate::provider_exhaustion::hold_error_until(&e)
                            .unwrap_or_else(Utc::now);
                        hold_wake_for_provider_exhaustion(store, bus, job, target, until).await;
                    }
                    // K2/#996: a retryable refusal (custody fence or transient
                    // capacity/pause) retains the wake with bounded durable
                    // backoff instead of consuming a one-shot job.
                    Err(e) if retryable_resume_refusal(&e) => {
                        settle_retryable_resume_refusal(store, bus, job, target, &e).await;
                    }
                    Err(e)
                        if matches!(
                            e,
                            DaemonError::CodexResumeTornTail
                                | DaemonError::CodexResumeToolHistory(_)
                        ) =>
                    {
                        // A provider thread with invalid persisted history will
                        // refuse every scheduled resume. Retire this exact row
                        // once, including recurring jobs, and expose recovery.
                        let retired = {
                            let guard = store.lock().await;
                            match guard.get_scheduled_job(&job.id) {
                                Ok(Some(current)) if current.enabled => guard
                                    .settle_watch_fire_with_retry(
                                        &WatchFireCapture::default(),
                                        job.id,
                                        Some(&Utc::now()),
                                        None,
                                        false,
                                        true,
                                    ),
                                Ok(_) => Ok(false),
                                Err(error) => Err(error),
                            }
                        };
                        match retired {
                            Ok(true) => {
                                let class = crate::model_control::classify_error_class(&e);
                                let notice_result = {
                                    let guard = store.lock().await;
                                    guard.record_manager_transcript_refusal_notice(target, &class)
                                };
                                if let Err(error) = notice_result {
                                    tracing::error!(job_id=%job.id, %error, "failed to record manager transcript recovery notice");
                                }
                                bus.publish(DaemonEvent::SystemMessage {
                                    level: "error".into(),
                                    message: format!(
                                        "Scheduled resume job '{}' stopped: Codex transcript cannot be resumed ({e}). Recover from a fresh conversation boundary within the active retry policy.",
                                        job.name,
                                    ),
                                });
                            }
                            Ok(false) => {}
                            Err(error) => {
                                tracing::error!(job_id=%job.id, %error, "failed to retire invalid transcript resume");
                            }
                        }
                    }
                    // #1124: the no-result continuation re-proved its worker
                    // ineligible (halted, paused, archived, or a human gate):
                    // retire the one-shot row quietly, never retry or deliver.
                    Err(e) if crate::session::worker_result_guard::is_no_result_retired(&e) => {
                        tracing::info!(
                            job_id = %job.id,
                            target = %target,
                            "no-result continuation retired: worker no longer eligible"
                        );
                        advance_job_after_attempt(store, job).await;
                    }
                    Err(e) => {
                        if crate::error::is_retryable_custody_wake_error(&e) {
                            defer_retryable_custody_wake(store, job, &WatchFireCapture::default())
                                .await;
                            return;
                        }
                        tracing::error!("Scheduled resume job '{}' failed: {e}", job.name);
                        // #652: a terminal custody refusal is also a durable
                        // manager notice, not only a bus message.
                        record_custody_refusal_notice(store, job, target, &e).await;
                        bus.publish(DaemonEvent::SystemMessage {
                            level: "error".into(),
                            message: format!("Scheduled resume job '{}' failed: {e}", job.name),
                        });
                        advance_job_after_attempt(store, job).await;
                    }
                }
                return;
            }
            None => {
                tracing::error!(
                    "Scheduled resume job '{}' has no wake_session_id; firing as Fresh instead",
                    job.name
                );
            }
        }
    }
    // Fall through to Fresh launch. `AgentFresh` carries the durable authority
    // provenance; its bound origin is used only to copy the origin's persisted
    // rotation-disable state. Legacy bound `Fresh` retains its existing
    // inheritance behavior but never gains the dedicated admission purpose.
    let initial_rotation_disabled = if matches!(
        job.wake_mode,
        WakeMode::Fresh | WakeMode::AgentFresh
    ) {
        match job.wake_session_id {
            Some(origin_id) => {
                let origin = {
                    let store = store.lock().await;
                    store.get_session(origin_id)
                };
                match origin {
                    Ok(Some(origin)) => {
                        // Issue #30 / #27 (priority 1, data integrity): decline
                        // an `AgentFresh` fire whose wake target is still live.
                        // The job's working_dir IS that live session's sandbox
                        // worktree, so launching here would run a second agent
                        // process against a tree another agent is still
                        // writing. Fail closed: do not spawn.
                        //
                        // Declined rather than converted to a resume: the
                        // Resume path (`SessionManager::resume_scheduled`)
                        // already refuses a live target with this same live
                        // set, so a conversion could not deliver anything — it
                        // would only relabel this decline as a delivery error.
                        // Declining keeps `AgentFresh` consistent with the
                        // policy Resume has always enforced.
                        if job.wake_mode == WakeMode::AgentFresh
                            && is_live_wake_target(origin.status)
                        {
                            let message = format!(
                                "Scheduled agent-fresh job '{}' (id={}) declined: wake target \
                                 {origin_id} is still live (status {:?}); launching would place a \
                                 second agent process in that session's sandbox worktree \
                                 (issue #30). Fresh/AgentFresh is a consumed, best-effort root \
                                 launch and transfers no hierarchy or lead authority; use \
                                 AgentReserveSuccessor for master turnover. Arm mode 'resume' to \
                                 wake this session later, or mode 'on_terminal' to wake it when a \
                                 watched session finishes.",
                                job.name, job.id, origin.status
                            );
                            tracing::warn!("{message}");
                            bus.publish(DaemonEvent::SystemMessage {
                                level: "warn".into(),
                                message,
                            });
                            advance_job_after_attempt(store, job).await;
                            return;
                        }
                        origin.rotation_disabled_at.is_some()
                    }
                    Ok(None) => {
                        let message = format!(
                            "Scheduled Fresh job '{}' cannot resolve bound origin {origin_id}; refusing unknown rotation state",
                            job.name
                        );
                        tracing::error!("{message}");
                        bus.publish(DaemonEvent::SystemMessage {
                            level: "error".into(),
                            message,
                        });
                        advance_job_after_attempt(store, job).await;
                        return;
                    }
                    Err(error) => {
                        let message = format!(
                            "Scheduled Fresh job '{}' failed to resolve bound origin {origin_id}: {error}",
                            job.name
                        );
                        tracing::error!("{message}");
                        bus.publish(DaemonEvent::SystemMessage {
                            level: "error".into(),
                            message,
                        });
                        advance_job_after_attempt(store, job).await;
                        return;
                    }
                }
            }
            None => false,
        }
    } else {
        false
    };

    let working_dir = job.working_dir.clone().unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"))
    });

    let model_invocation_purpose = if job.wake_mode == WakeMode::AgentFresh {
        rsi_common::model_control::ModelInvocationPurpose::AgentScheduleWakeFresh
    } else {
        rsi_common::model_control::ModelInvocationPurpose::ScheduledFresh
    };

    let launch_config = LaunchConfig {
        completion_gates: None,
        query: job.message.clone(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        working_dir: Some(working_dir),
        provider: job.provider,
        model: job.model.clone(),
        configured_context_window: None,
        max_turns: None,
        system_prompt: None,
        resume_session_id: None,
        // Scheduled Fresh successors are always interactive Standard sessions.
        // This must be explicit so a project's default worker kind cannot turn
        // a one-shot recovery successor into an A9 worker-retry session.
        session_kind: Some(SessionKind::Standard),
        project_id: job.project_id,
        rsi_session_id: None,
        rsi_socket: None,
        rsi_session_token: None,
        continued_from: None,
        openai_base_url: None,
        openai_api_key: None,
        conversation_history: None,
        workflow_id: None,
        workflow_id_override: None,
        max_retries: None,
        group_id: None,
        parent_id: None,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: Some(job.id),
        model_invocation_owner: None,
        model_invocation_dedup_key: Some(format!(
            "scheduled.fresh:{}:{}",
            job.id, job.next_fire_at
        )),
        model_invocation_request_fingerprint: Some(hash_request_fingerprint(&[
            model_invocation_purpose.as_str(),
            &job.id.to_string(),
            &job.next_fire_at.to_rfc3339(),
            job.model.as_deref().unwrap_or(""),
            &job.message,
        ])),
        skip_project_model_default: false,
        tool_policy: None,
        model_invocation_purpose,
        sandbox: None,
        cargo_target_dir: None,
        execution_scratch: None,
        // RSI-006: scheduled jobs are production launches.
        is_eval: false,
        skip_context_pipeline: false,
        // Scheduled jobs launch their configured message as-is; if it starts
        // with a `/<command>`, the RPC layer (or a future scheduler upgrade)
        // could resolve the class, but today we leave this unset so the
        // validator stays silent for automated jobs.
        capability_class: None,
        // Scheduled jobs carry no tag set at spawn time.
        tags: vec![],
        // Scheduled job launches are never topology-bound.
        topology_node_id: None,
        topology_iteration: 0,
        closure_selector: None,
    };

    match launcher
        .launch_scheduled_fresh(launch_config, initial_rotation_disabled)
        .await
    {
        Ok(session_id) => {
            tracing::info!("Scheduled job '{}' fired -> session {session_id}", job.name);
            advance_job_after_attempt(store, job).await;
            bus.publish(DaemonEvent::ScheduledJobFired {
                job_id: job.id,
                job_name: job.name.clone(),
                session_id,
            });
        }
        // Issue #12: a live session already writes the wake's tree. The
        // launcher refused before any effect, so the one-shot is NOT consumed:
        // it stays armed and is retried for a bounded window; only then is it
        // settled and the owner told.
        Err(e) if crate::session::tree_admission::is_tree_occupied_error(&e) => {
            settle_tree_occupied_fresh(store, bus, job, &e).await;
        }
        Err(e) => {
            tracing::error!("Failed to fire scheduled job '{}': {e}", job.name);
            bus.publish(DaemonEvent::SystemMessage {
                level: "error".into(),
                message: format!("Scheduled job '{}' failed to fire: {e}", job.name),
            });
            advance_job_after_attempt(store, job).await;
        }
    }
}

/// How long a fresh wake keeps retrying while its tree is occupied.
const TREE_OCCUPIED_RETRY_WINDOW: chrono::Duration = chrono::Duration::minutes(60);
/// Delay between retries while its tree is occupied.
const TREE_OCCUPIED_RETRY_DELAY: chrono::Duration = chrono::Duration::seconds(60);

/// Issue #12: handle a fresh wake whose tree is occupied.
///
/// A recurring wake skips this occurrence and stays armed for the next one
/// (the recurrence is its own retry). A one-shot is retained and re-armed for a
/// bounded window from its anchor; past that it is settled and its owner is
/// resumed with exactly ONE durable note (deterministic job id, so a repeated
/// settle can never queue a second one), so a decline is never silent.
async fn settle_tree_occupied_fresh(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    job: &rsi_common::types::ScheduledJob,
    error: &DaemonError,
) {
    let now = Utc::now();
    if !matches!(job.schedule.recurrence, Recurrence::Once) {
        let message = format!(
            "Scheduled fresh job '{}' (id={}) skipped this occurrence: {error}; it stays armed \
             for the next one",
            job.name, job.id
        );
        tracing::warn!("{message}");
        bus.publish(DaemonEvent::SystemMessage {
            level: "warn".into(),
            message,
        });
        advance_job_after_attempt(store, job).await;
        return;
    }
    let deadline = job.schedule.anchor + TREE_OCCUPIED_RETRY_WINDOW;
    if now < deadline && job.enabled {
        let rearm_at = now + TREE_OCCUPIED_RETRY_DELAY;
        let moved = {
            let guard = store.lock().await;
            guard.defer_wake_when(job.id, rearm_at)
        };
        match moved {
            Ok(()) => {
                let message = format!(
                    "Scheduled fresh job '{}' (id={}) held: {error}; retrying no earlier than \
                     {rearm_at} (gives up at {deadline})",
                    job.name, job.id
                );
                tracing::warn!("{message}");
                bus.publish(DaemonEvent::SystemMessage {
                    level: "warn".into(),
                    message,
                });
                return;
            }
            Err(move_error) => {
                tracing::error!(job_id = %job.id, %move_error, "could not re-arm an occupied fresh wake; settling it");
            }
        }
    }
    let notice = job.wake_session_id.map(|origin| {
        let notice_name = format!("fresh-wake-declined:{}", job.id);
        rsi_common::types::ScheduledJob {
            id: Uuid::new_v5(&job.id, notice_name.as_bytes()),
            name: notice_name,
            message: format!(
                "Your fresh wake '{}' (id={}) never launched: {error}. Another live session \
                 still writes that worktree. Re-arm it once the tree is quiet, or continue the \
                 work yourself.",
                job.name, job.id
            ),
            schedule: rsi_common::types::ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: now,
            },
            last_fired_at: None,
            next_fire_at: now,
            enabled: true,
            working_dir: job.working_dir.clone(),
            provider: job.provider,
            model: job.model.clone(),
            project_id: job.project_id,
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::Resume,
            wake_session_id: Some(origin),
        }
    });
    // ONE store transaction settles the exact one-shot and inserts its
    // deterministic notice, so a crash or an insertion failure can never leave
    // the wake consumed without its notice (or the notice without the
    // settlement). On failure the wake stays armed and the next poll retries.
    let settled = {
        let guard = store.lock().await;
        guard.settle_declined_fresh_wake(job.id, &now, notice.as_ref())
    };
    match settled {
        Ok(_) => {
            let message = format!(
                "Scheduled fresh job '{}' (id={}) declined after the retry window: {error}",
                job.name, job.id
            );
            tracing::warn!("{message}");
            bus.publish(DaemonEvent::SystemMessage {
                level: "warn".into(),
                message,
            });
        }
        Err(settle_error) => {
            let message = format!(
                "Scheduled fresh job '{}' (id={}) could not be settled with its owner notice: \
                 {settle_error}; it stays armed and is retried",
                job.name, job.id
            );
            tracing::error!("{message}");
            bus.publish(DaemonEvent::SystemMessage {
                level: "error".into(),
                message,
            });
        }
    }
}

/// Operator "trigger now" entry for tests (the manual branch).
#[cfg(test)]
pub(crate) async fn fire_job_manual_for_test(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    launcher: &Arc<dyn SessionLauncher>,
    job: &rsi_common::types::ScheduledJob,
) {
    fire_job_inner(store, bus, launcher, job, true).await;
}

#[cfg(test)]
pub(crate) async fn fire_job_for_test(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    launcher: &Arc<dyn SessionLauncher>,
    job: &rsi_common::types::ScheduledJob,
) {
    fire_job(store, bus, launcher, job).await;
}

/// A8: fire one terminal-watch job — the scheduler stays a dumb dispatcher.
/// All policy (predicate over persisted state, retry suppression, lineage
/// chase, coalescing, delivery) lives in `SessionLauncher::fire_watch`
/// (`SessionManager`); this fn only maps the outcome onto job rows per the
/// plan §3.4 table.
async fn fire_terminal_watch_job(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    launcher: &Arc<dyn SessionLauncher>,
    job: &rsi_common::types::ScheduledJob,
) {
    // Review F1 (defense layer a): the caller's struct may be a STALE due-list
    // snapshot — an earlier fire in the same `process_due_jobs` pass can have
    // coalesced this sibling row away (delivered + disabled). Re-read the row
    // (mirror `fire_job_by_id`) and use the FRESH row for the enabled guard
    // and everything downstream; a missing row no-ops silently.
    let (fresh, capture) = {
        let guard = store.lock().await;
        match guard.capture_watch_fire(job.id) {
            Ok(Some(captured)) => captured,
            Ok(None) => {
                tracing::debug!(
                    "Terminal watch job '{}' (id={}) row missing; skipping fire",
                    job.name,
                    job.id
                );
                return;
            }
            Err(e) => {
                tracing::error!("Failed to capture terminal watch job {}: {e}", job.id);
                return;
            }
        }
    };
    let job = &fresh;

    // Idempotency guard (on the FRESH row): a coalesced delivery (or manual
    // disarm) may have disabled this row between due-listing / trigger_now
    // and this attempt. A disabled watch no-ops — never advances, never
    // re-delivers. (Fresh/Resume jobs deliberately keep their
    // fire-even-if-disabled manual-trigger behavior; this guard is
    // OnTerminal-scoped.)
    if !job.enabled {
        tracing::debug!(
            "Terminal watch job '{}' (id={}) is disabled; skipping fire",
            job.name,
            job.id
        );
        return;
    }

    if capture.is_manager_watch(job.id) {
        let guard = store.lock().await;
        match guard.harness_manager_watch_route(job.id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                if let Err(error) = guard.settle_watch_fire(&capture, job.id, None, None, false) {
                    tracing::error!(%error, "could not disable revoked manager notice");
                }
                tracing::warn!(job_id = %job.id, "manager notice refused: scope or envelope changed");
                bus.publish(DaemonEvent::SystemMessage {
                    level: "warn".into(),
                    message:
                        "Manager notice refused: scope or recipient changed. Refresh manager scope."
                            .into(),
                });
                return;
            }
            Err(error) => {
                tracing::error!(%error, "manager notice scope unavailable; retained for retry");
                return;
            }
        }
    }

    match launcher.fire_watch(job).await {
        Ok(WatchFireOutcome::Delivered {
            session_id,
            delivered_job_ids,
            observed_job_versions,
        }) => {
            tracing::info!(
                "Terminal watch '{}' dispatched -> resumed session {session_id} ({} job(s) satisfied); \
                 awaiting provider output before retiring the watch",
                job.name,
                delivered_job_ids.len()
            );
            // Issue #12: this arm used to `disable_watch_jobs` here. That
            // retired the watch on SPAWN — `continue_session` returns once the
            // provider process exists — so a resumed turn that then died
            // without emitting anything destroyed the notification permanently,
            // with no retry and nothing surfaced. Stay armed and let the
            // confirmation gate in `plan_terminal_watch_fire` retire the row
            // once the tip has actually produced output.
            arm_watch_jobs_pending_confirmation(
                store,
                &delivered_job_ids,
                &observed_job_versions,
                &capture,
            )
            .await;
            bus.publish(DaemonEvent::ScheduledJobFired {
                job_id: job.id,
                job_name: job.name.clone(),
                session_id,
            });
        }
        Ok(WatchFireOutcome::Confirmed) => {
            if capture.is_manager_watch(job.id) {
                // Manager notices carry their own generation fence.
                disable_watch_jobs(store, std::slice::from_ref(&job.id), &capture).await;
            } else {
                // A child may have been continued/rearmed while the planner
                // awaited confirmation. Never retire that newer epoch from
                // the old row snapshot.
                match store
                    .lock()
                    .await
                    .retire_unchanged_child_watch(job, "consumed")
                {
                    Ok(true) => tracing::info!(
                        "Terminal watch '{}' (id={}) confirmed consumed; retiring",
                        job.name,
                        job.id
                    ),
                    Ok(false) => tracing::debug!(
                        job_id = %job.id,
                        "stale terminal-watch confirmation preserved a rearmed child watch"
                    ),
                    Err(error) => {
                        tracing::error!(%error, job_id = %job.id, "could not retire confirmed child watch")
                    }
                }
            }
        }
        Ok(WatchFireOutcome::NotReady) => {
            // D3 requeue-until-idle: the recurring row stays enabled and
            // re-evaluates on the next tick.
            tracing::debug!(
                "Terminal watch '{}' (id={}) not ready; staying armed",
                job.name,
                job.id
            );
            advance_job_after_attempt_with_capture(store, job, &capture).await;
        }
        Ok(WatchFireOutcome::CustodyUnavailable) => {
            defer_retryable_custody_wake(store, job, &capture).await;
        }
        Ok(WatchFireOutcome::Abandon { reason }) => {
            settle_abandoned_watch(store, bus, job, &capture, &reason).await;
        }
        Ok(WatchFireOutcome::AbandonUnconsumed(delivery)) => {
            settle_unconsumed_watch(store, bus, launcher, job, &capture, &delivery).await;
        }
        Err(e) => {
            // Transient failure (or a launcher without watch support): keep
            // the recurring row armed and surface the error — same shape as
            // the Resume error leg.
            tracing::error!("Terminal watch '{}' fire attempt failed: {e}", job.name);
            bus.publish(DaemonEvent::SystemMessage {
                level: "error".into(),
                message: format!("terminal watch '{}' fire attempt failed: {e}", job.name),
            });
            advance_job_after_attempt_with_capture(store, job, &capture).await;
        }
    }
}

/// Settle an abandoned terminal watch: warn, retire the row, and surface a
/// `SystemMessage`. A stale child snapshot that preserved a newer watch epoch
/// surfaces nothing.
async fn settle_abandoned_watch(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    job: &rsi_common::types::ScheduledJob,
    capture: &WatchFireCapture,
    reason: &str,
) {
    warn_abandoned_watch(job, reason);
    let abandoned = if capture.is_manager_watch(job.id) {
        disable_watch_jobs(store, std::slice::from_ref(&job.id), capture).await;
        true
    } else {
        let retired = store
            .lock()
            .await
            .retire_unchanged_child_watch(job, "abandoned");
        match retired {
            Ok(true) => true,
            Ok(false) => {
                tracing::debug!(job_id = %job.id, "stale terminal-watch abandonment preserved a newer watch epoch");
                false
            }
            Err(error) => {
                tracing::error!(%error, job_id = %job.id, "could not retire abandoned child watch");
                false
            }
        }
    };
    if !abandoned {
        return;
    }
    publish_abandoned_watch(bus, job, reason);
}

fn warn_abandoned_watch(job: &rsi_common::types::ScheduledJob, reason: &str) {
    tracing::warn!(
        "Terminal watch '{}' (id={}) abandoned: {reason}",
        job.name,
        job.id
    );
}

fn publish_abandoned_watch(bus: &EventBus, job: &rsi_common::types::ScheduledJob, reason: &str) {
    bus.publish(DaemonEvent::SystemMessage {
        level: "warn".into(),
        message: format!(
            "terminal watch '{}' (id={}) abandoned: {reason}",
            job.name, job.id
        ),
    });
}

/// Issue #648: settle a never-consumed delivery. Same log line and
/// `SystemMessage` as [`settle_abandoned_watch`], but the row retirement, the
/// tip's health fact and any manager notice commit in ONE store transaction.
/// On failure nothing is retired: the watch stays armed, backs off one
/// recurrence, and the next pass re-plans the same give-up. Transcript and bus
/// publication happen only after commit.
async fn settle_unconsumed_watch(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    launcher: &Arc<dyn SessionLauncher>,
    job: &rsi_common::types::ScheduledJob,
    capture: &WatchFireCapture,
    delivery: &crate::issue_tracker::poller::UnconsumedDelivery,
) {
    let reason = delivery.reason();
    warn_abandoned_watch(job, &reason);
    let sequence_floor = launcher.transcript_sequence_floor(delivery.tip).await;
    let settled = store.lock().await.abandon_unconsumed_watch(
        job,
        capture,
        delivery,
        Utc::now(),
        sequence_floor,
    );
    let record = match settled {
        Ok(Some(record)) => record,
        Ok(None) => {
            tracing::debug!(job_id = %job.id, "stale terminal-watch abandonment preserved a newer watch epoch");
            return;
        }
        Err(error) => {
            tracing::error!(
                %error,
                job_id = %job.id,
                tip = %delivery.tip,
                "could not settle abandoned watch delivery; staying armed for retry"
            );
            advance_job_after_attempt_with_capture(store, job, capture).await;
            return;
        }
    };
    publish_abandoned_watch(bus, job, &reason);
    if let Some(event) = record.health_event {
        launcher.delivery_abandoned_recorded(&event).await;
        bus.publish(DaemonEvent::ConversationEvent {
            session_id: event.session_id,
            event,
        });
    }
    if let Some(job_id) = record.notice_job {
        bus.publish(DaemonEvent::ManagerNoticeQueued { job_id });
    }
}

/// Floor on how far a provider-exhaustion hold may push a wake, so a hold
/// that is already (nearly) over cannot re-fire on the scheduler cadence.
const PROVIDER_HOLD_MIN_REARM: chrono::Duration = chrono::Duration::seconds(30);

/// #572: re-arm a due wake at the provider hold's deadline. No retry attempt is
/// spent and the row stays enabled; one operator-visible notice is published
/// when the row actually moves (it will not be due again before `until`).
async fn hold_wake_for_provider_exhaustion(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    job: &rsi_common::types::ScheduledJob,
    target: Uuid,
    until: chrono::DateTime<Utc>,
) {
    let rearm_at = until.max(Utc::now() + PROVIDER_HOLD_MIN_REARM);
    let moved = {
        let guard = store.lock().await;
        guard.defer_wake_when(job.id, rearm_at)
    };
    match moved {
        Ok(()) => {
            tracing::info!(
                job_id = %job.id,
                %target,
                %rearm_at,
                code = crate::provider_exhaustion::PROVIDER_USAGE_LIMIT_HOLD_CODE,
                "Scheduled resume held by the provider usage limit; wake re-armed"
            );
            bus.publish(DaemonEvent::SystemMessage {
                level: "warn".into(),
                message: format!(
                    "Scheduled resume job '{}' (id={}) held: {}; Codex usage limit, retrying no earlier than {rearm_at}",
                    job.name,
                    job.id,
                    crate::provider_exhaustion::PROVIDER_USAGE_LIMIT_HOLD_CODE,
                ),
            });
        }
        Err(error) => {
            tracing::error!(job_id = %job.id, %error, "could not re-arm a provider-held wake; it stays due");
        }
    }
}

/// #652: surface a terminal sandbox-custody refusal of a lead's wake as a
/// durable, typed manager notice. Best effort: the wake settles as before.
pub(crate) async fn record_custody_refusal_notice(
    store: &Arc<Mutex<Store>>,
    job: &rsi_common::types::ScheduledJob,
    target: Uuid,
    error: &DaemonError,
) {
    if let Err(record_error) = store
        .lock()
        .await
        .record_manager_custody_refusal_notice(target, error)
    {
        tracing::error!(job_id = %job.id, %record_error, "could not record custody refusal notice");
    }
}

async fn defer_retryable_custody_wake(
    store: &Arc<Mutex<Store>>,
    job: &rsi_common::types::ScheduledJob,
    capture: &WatchFireCapture,
) {
    let retry_at = Utc::now() + chrono::Duration::seconds(1);
    match store
        .lock()
        .await
        .defer_retryable_custody_wake(capture, job, &retry_at)
    {
        Ok(true) => {
            tracing::debug!(job_id = %job.id, "Custody contention deferred wake; job remains armed")
        }
        Ok(false) => {
            tracing::debug!(job_id = %job.id, "Custody contention wake changed before defer")
        }
        Err(error) => {
            tracing::error!(job_id = %job.id, %error, "Could not defer custody contention wake")
        }
    }
}

/// Stamp `last_fired_at` for a dispatched-but-unconfirmed delivery and keep the
/// rows ARMED, deferred by [`WATCH_REDELIVERY_BACKOFF`].
///
/// The stamp is what the confirmation gate compares provider output against, so
/// it must be written even though the row stays enabled. Retirement happens
/// only via `WatchFireOutcome::Confirmed` (proven consumed) or `Abandon` (gave
/// up loudly) — never as a side effect of spawning a process.
async fn arm_watch_jobs_pending_confirmation(
    store: &Arc<Mutex<Store>>,
    job_ids: &[Uuid],
    observed_job_versions: &[(Uuid, chrono::DateTime<Utc>)],
    capture: &WatchFireCapture,
) {
    let now = Utc::now();
    let retry_at = now + crate::session::WATCH_REDELIVERY_BACKOFF;
    let guard = store.lock().await;
    for jid in job_ids {
        let result = if capture.is_manager_watch(*jid) {
            guard.settle_watch_fire(capture, *jid, Some(&now), Some(&retry_at), true)
        } else if let Some((_, version)) = observed_job_versions.iter().find(|(id, _)| id == jid) {
            guard.stamp_delivered_child_watch(*jid, version, &now, &retry_at)
        } else {
            tracing::error!(job_id = %jid, "terminal-watch delivery lacks a row witness");
            continue;
        };
        match result {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(job_id = %jid, "stale terminal-watch delivery preserved a newer watch epoch")
            }
            Err(e) => tracing::error!("Failed to arm watch job {jid} pending confirmation: {e}"),
        }
    }
}

/// A8: disable watch job rows after confirmation/abandonment
/// (`enabled = false`, `last_fired_at` stamped, `next_fire_at` untouched).
async fn disable_watch_jobs(
    store: &Arc<Mutex<Store>>,
    job_ids: &[Uuid],
    capture: &WatchFireCapture,
) {
    let now = Utc::now();
    let guard = store.lock().await;
    for jid in job_ids {
        if let Err(e) = guard.settle_watch_fire(capture, *jid, Some(&now), None, false) {
            tracing::error!("Failed to disable watch job {jid}: {e}");
        }
    }
}

/// Advance or disable a scheduled job after a fire attempt (success or failure).
/// K2 settlement of a retryable continuation-fence refusal of a scheduled
/// Resume: record bounded backoff and keep the job enabled; on exhaustion
/// publish `continuation_retry_exhausted` with the last tip and settle the
/// row as an ordinary attempt (a `Once` job is disabled and stamped, never
/// deleted).
async fn settle_retryable_resume_refusal(
    store: &Arc<Mutex<Store>>,
    bus: &Arc<EventBus>,
    job: &rsi_common::types::ScheduledJob,
    target: Uuid,
    error: &crate::error::DaemonError,
) {
    let heal = {
        let guard = store.lock().await;
        match guard.transient_heal_state(job.id) {
            Ok(Some(_)) => {
                // A pause, capacity or drain refusal means the session never
                // ran: keep the backoff but spend no heal budget (#1082).
                let spend = transient_resume_refusal_code(error).is_none()
                    && !crate::deploy_drain::is_draining_error(error)
                    && !crate::store::custody_lock_order::is_lock_order_busy_error(error);
                Some(guard.defer_transient_heal(
                    job.id,
                    resume_refusal_code(error),
                    Utc::now(),
                    spend,
                ))
            }
            Ok(None) => None,
            Err(error) => {
                tracing::error!(%error, "heal state unavailable; wake retained");
                return;
            }
        }
    };
    if let Some(outcome) = heal {
        crate::session::transient_heal::publish_heal_outcome(bus, target, outcome);
        return;
    }
    let code = resume_refusal_code(error);
    let transient = transient_resume_refusal_code(error).is_some();
    let recorded = {
        let guard = store.lock().await;
        let tip = guard.published_lineage_tip(target).ok().flatten();
        guard.record_continuation_retry(job.id, code, tip, Utc::now(), true)
    };
    match recorded {
        Ok(crate::store::scheduled_jobs::ContinuationRetryOutcome::Backoff {
            attempts,
            next_fire_at,
        }) => {
            tracing::info!(
                job_id = %job.id,
                code,
                attempts,
                %next_fire_at,
                "Scheduled resume retained after a retryable continuation refusal"
            );
            // #996: a refused capacity/pause resume must be visible, not just
            // logged — the wake is retained but the operator sees why.
            if transient {
                bus.publish(DaemonEvent::SystemMessage {
                    level: "warn".into(),
                    message: format!(
                        "Scheduled resume job '{}' (id={}) deferred after transient refusal {code} (attempt {attempts}); retrying at {next_fire_at}: {error}",
                        job.name, job.id
                    ),
                });
            }
        }
        Ok(crate::store::scheduled_jobs::ContinuationRetryOutcome::Exhausted(retry)) => {
            let tip = retry
                .last_tip
                .map_or_else(|| "unknown".to_string(), |tip| tip.to_string());
            let message = format!(
                "{}: scheduled resume job '{}' (id={}) last refused {} at tip {tip} after {} attempts",
                crate::store::manager_actions::fence::CONTINUATION_RETRY_EXHAUSTED,
                job.name,
                job.id,
                retry.last_code,
                retry.attempts,
            );
            tracing::error!("{message}");
            bus.publish(DaemonEvent::SystemMessage {
                level: "error".into(),
                message,
            });
            // Review round 2: the terminal settlement and the retry reset are
            // one write. Before it commits the recorded attempts stay durable.
            #[cfg(test)]
            if crash_before_exhausted_settlement_for_test(job.id) {
                return;
            }
            advance_job_after_attempt_clearing_retry(store, job).await;
        }
        Err(record_error) => {
            tracing::error!(job_id = %job.id, error = %record_error, "failed to record continuation retry; settling as an attempt");
            advance_job_after_attempt(store, job).await;
        }
    }
}

/// Test crash point between retry exhaustion and its terminal settlement:
/// the settlement is skipped once for `job_id`, as if the daemon died there.
#[cfg(test)]
fn crash_points() -> &'static std::sync::Mutex<std::collections::HashSet<Uuid>> {
    static POINTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<Uuid>>> =
        std::sync::OnceLock::new();
    POINTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

#[cfg(test)]
pub(crate) fn install_crash_before_exhausted_settlement_for_test(job_id: Uuid) {
    crash_points()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(job_id);
}

#[cfg(test)]
fn crash_before_exhausted_settlement_for_test(job_id: Uuid) -> bool {
    crash_points()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&job_id)
}

async fn advance_job_after_attempt(
    store: &Arc<Mutex<Store>>,
    job: &rsi_common::types::ScheduledJob,
) {
    advance_job_after_attempt_with_capture(store, job, &WatchFireCapture::default()).await;
}

/// A scheduled Resume attempt that settles the row and clears the K2 retry
/// state atomically (delivery or retry exhaustion).
async fn advance_job_after_attempt_clearing_retry(
    store: &Arc<Mutex<Store>>,
    job: &rsi_common::types::ScheduledJob,
) {
    advance_job_after_attempt_inner(store, job, &WatchFireCapture::default(), true).await;
}

async fn advance_job_after_attempt_with_capture(
    store: &Arc<Mutex<Store>>,
    job: &rsi_common::types::ScheduledJob,
    capture: &WatchFireCapture,
) {
    advance_job_after_attempt_inner(store, job, capture, false).await;
}

async fn advance_job_after_attempt_inner(
    store: &Arc<Mutex<Store>>,
    job: &rsi_common::types::ScheduledJob,
    capture: &WatchFireCapture,
    clear_continuation_retry: bool,
) {
    let now = Utc::now();
    let next = next_fire_time(&job.schedule, now, now);
    let should_disable = matches!(job.schedule.recurrence, Recurrence::Once) || next.is_none();
    let guard = store.lock().await;
    // Review F1 (defense layer b): AND the written `enabled` with the row's
    // CURRENT DB value (read under the same store guard as the write) — a
    // fire attempt must never re-enable a row something else disabled
    // mid-fire (coalesced watch delivery, human disarm). A missing row is
    // left alone; a read error falls back to the caller's snapshot
    // (pre-existing behavior).
    let db_enabled = match guard.get_scheduled_job(&job.id) {
        Ok(Some(row)) => row.enabled,
        Ok(None) => {
            tracing::debug!("Scheduled job {} gone before advance; skipping", job.id);
            return;
        }
        Err(e) => {
            tracing::error!(
                "Failed to re-read scheduled job {} before advance: {e}",
                job.id
            );
            job.enabled
        }
    };
    let result =
        if matches!(job.wake_mode, WakeMode::OnTerminal(_)) && !capture.is_manager_watch(job.id) {
            guard.defer_child_watch_attempt(
                job.id,
                &job.updated_at,
                next.as_ref(),
                !should_disable && job.enabled && db_enabled,
            )
        } else {
            guard.settle_watch_fire_with_retry(
                capture,
                job.id,
                // A deferred or failed terminal watch did not reach its recipient.
                // Only the Delivered arm may stamp its confirmation witness.
                (!matches!(job.wake_mode, WakeMode::OnTerminal(_))).then_some(&now),
                next.as_ref(),
                !should_disable && job.enabled && db_enabled,
                clear_continuation_retry,
            )
        };
    if let Err(e) = result {
        tracing::error!("Failed to update scheduled job after fire: {e}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::error::DaemonError;
    use crate::store::manager_watch_settlement::tests::{manager_watch_fixture, notice_state};
    use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, Session};
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    /// Mock launcher: records every call; pops queued `fire_watch` outcomes.
    struct MockLauncher {
        launch_calls: StdMutex<usize>,
        scheduled_fresh_calls: StdMutex<Vec<bool>>,
        scheduled_fresh_purposes: StdMutex<Vec<rsi_common::model_control::ModelInvocationPurpose>>,
        resume_calls: StdMutex<Vec<Uuid>>,
        resume_outcomes: StdMutex<VecDeque<crate::error::Result<Uuid>>>,
        capacity_resume_calls: StdMutex<Vec<(Uuid, Uuid, chrono::DateTime<Utc>)>>,
        capacity_resume_outcomes: StdMutex<VecDeque<crate::error::Result<Uuid>>>,
        fire_watch_calls: StdMutex<Vec<Uuid>>,
        fire_watch_outcomes: StdMutex<VecDeque<crate::error::Result<WatchFireOutcome>>>,
        usage_hold: StdMutex<Option<chrono::DateTime<Utc>>>,
        scheduled_fresh_outcomes: StdMutex<VecDeque<crate::error::Result<Uuid>>>,
    }

    impl MockLauncher {
        fn new(outcomes: Vec<crate::error::Result<WatchFireOutcome>>) -> Arc<Self> {
            Arc::new(Self {
                launch_calls: StdMutex::new(0),
                scheduled_fresh_calls: StdMutex::new(Vec::new()),
                scheduled_fresh_purposes: StdMutex::new(Vec::new()),
                resume_calls: StdMutex::new(Vec::new()),
                resume_outcomes: StdMutex::new(VecDeque::new()),
                capacity_resume_calls: StdMutex::new(Vec::new()),
                capacity_resume_outcomes: StdMutex::new(VecDeque::new()),
                fire_watch_calls: StdMutex::new(Vec::new()),
                fire_watch_outcomes: StdMutex::new(outcomes.into()),
                usage_hold: StdMutex::new(None),
                scheduled_fresh_outcomes: StdMutex::new(VecDeque::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl SessionLauncher for MockLauncher {
        async fn launch(&self, _config: LaunchConfig) -> crate::error::Result<Uuid> {
            *self.launch_calls.lock().unwrap() += 1;
            Ok(Uuid::new_v4())
        }

        async fn launch_scheduled_fresh(
            &self,
            config: LaunchConfig,
            initial_rotation_disabled: bool,
        ) -> crate::error::Result<Uuid> {
            self.scheduled_fresh_calls
                .lock()
                .unwrap()
                .push(initial_rotation_disabled);
            self.scheduled_fresh_purposes
                .lock()
                .unwrap()
                .push(config.model_invocation_purpose);
            self.scheduled_fresh_outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(Uuid::new_v4()))
        }

        async fn resume_scheduled(
            &self,
            target: Uuid,
            _query: String,
        ) -> crate::error::Result<Uuid> {
            self.resume_calls.lock().unwrap().push(target);
            self.resume_outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(target))
        }

        async fn provider_usage_hold(&self, _target: Uuid) -> Option<chrono::DateTime<Utc>> {
            *self.usage_hold.lock().unwrap()
        }

        async fn resume_capacity_scheduled(
            &self,
            target: Uuid,
            _query: String,
            wake_job_id: Uuid,
            due_slot: chrono::DateTime<Utc>,
        ) -> crate::error::Result<Uuid> {
            self.capacity_resume_calls
                .lock()
                .unwrap()
                .push((target, wake_job_id, due_slot));
            self.capacity_resume_outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(target))
        }

        async fn fire_watch(&self, job: &ScheduledJob) -> crate::error::Result<WatchFireOutcome> {
            self.fire_watch_calls.lock().unwrap().push(job.id);
            let mut outcome = self
                .fire_watch_outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(DaemonError::Rpc("no queued outcome".into())));
            if let Ok(WatchFireOutcome::Delivered {
                observed_job_versions,
                ..
            }) = &mut outcome
                && observed_job_versions.is_empty()
            {
                observed_job_versions.push((job.id, job.updated_at));
            }
            outcome
        }
    }

    fn mk_watch_job(watched: Uuid, master: Uuid, due: bool) -> ScheduledJob {
        let now = Utc::now();
        ScheduledJob {
            id: Uuid::new_v4(),
            name: "rsi-watch".to_string(),
            message: String::new(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::EverySeconds(60),
                anchor: now - chrono::Duration::seconds(120),
            },
            last_fired_at: None,
            next_fire_at: if due {
                now - chrono::Duration::seconds(30)
            } else {
                now + chrono::Duration::seconds(3600)
            },
            enabled: true,
            working_dir: None,
            provider: None,
            model: None,
            project_id: None,
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::OnTerminal(watched),
            wake_session_id: Some(master),
        }
    }

    fn mk_fresh_job(origin: Option<Uuid>) -> ScheduledJob {
        let now = Utc::now();
        ScheduledJob {
            id: Uuid::new_v4(),
            name: "fresh-rotation-state".to_string(),
            message: "continue program".to_string(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: now - chrono::Duration::seconds(120),
            },
            last_fired_at: None,
            next_fire_at: now - chrono::Duration::seconds(30),
            enabled: true,
            working_dir: None,
            provider: None,
            model: None,
            project_id: None,
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::Fresh,
            wake_session_id: origin,
        }
    }

    fn mk_agent_fresh_job(origin: Uuid) -> ScheduledJob {
        let mut job = mk_fresh_job(Some(origin));
        job.wake_mode = WakeMode::AgentFresh;
        job
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn root_busy_resume_keeps_oneshot_armed_and_respects_disarm() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            Store::open(&directory.path().join("root-busy-resume.sqlite")).unwrap(),
        ));
        let bus = Arc::new(EventBus::new(64));
        let target = Uuid::new_v4();
        let mut job = mk_fresh_job(Some(target));
        job.wake_mode = WakeMode::Resume;
        store.lock().await.insert_scheduled_job(&job).unwrap();
        let mock = MockLauncher::new(Vec::new());
        mock.resume_outcomes
            .lock()
            .unwrap()
            .push_back(Err(retryable_custody_error(
                rsi_common::types::SandboxCustodyErrorCodeV1::RootBusy,
            )));
        let launcher: Arc<dyn SessionLauncher> = mock.clone();

        fire_job(&store, &bus, &launcher, &job).await;

        let deferred = store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .unwrap()
            .unwrap();
        assert!(deferred.enabled);
        assert_eq!(deferred.last_fired_at, None);
        assert_eq!(deferred.message, job.message);
        assert!(deferred.next_fire_at > Utc::now());
        assert_eq!(*mock.resume_calls.lock().unwrap(), vec![target]);

        store.lock().await.toggle_scheduled_job(&job.id).unwrap();
        let retry_at = Utc::now() + chrono::Duration::seconds(5);
        assert!(
            !store
                .lock()
                .await
                .defer_retryable_custody_wake(&WatchFireCapture::default(), &deferred, &retry_at)
                .unwrap()
        );
        let disarmed = store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .unwrap()
            .unwrap();
        assert!(!disarmed.enabled);
    }

    /// #572: a due 60-second Resume wake on an exhausted provider account is
    /// held and re-armed once at the provider's retry-after, not re-fired on
    /// the scheduler cadence; it is delivered once the hold has passed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn usage_limit_hold_rearms_a_due_wake_at_the_retry_after_without_a_loop() {
        let (store, bus) = fixture();
        let mut bus_rx = bus.subscribe();
        let target = Uuid::new_v4();
        let mut job = mk_fresh_job(Some(target));
        job.wake_mode = WakeMode::Resume;
        job.schedule.recurrence = Recurrence::EverySeconds(60);
        store.lock().await.insert_scheduled_job(&job).unwrap();
        let mock = MockLauncher::new(Vec::new());
        let until = Utc::now() + chrono::Duration::days(3);
        *mock.usage_hold.lock().unwrap() = Some(until);
        let launcher: Arc<dyn SessionLauncher> = mock.clone();

        // Three scheduler ticks: only the first finds the wake due.
        for _ in 0..3 {
            process_due_jobs(&store, &bus, &launcher, None).await;
        }
        assert!(mock.resume_calls.lock().unwrap().is_empty());
        let held = job_row(&store, &job.id).await;
        assert!(held.enabled);
        assert_eq!(held.last_fired_at, None);
        assert_eq!(
            held.next_fire_at, until,
            "re-armed exactly at the retry-after"
        );
        assert!(
            store
                .lock()
                .await
                .list_due_scheduled_jobs(&(Utc::now() + chrono::Duration::hours(1)))
                .unwrap()
                .is_empty(),
            "the wake is not due again before the retry-after"
        );
        let mut notices = 0;
        while let Ok(event) = bus_rx.try_recv() {
            if let DaemonEvent::SystemMessage { message, .. } = &*event
                && message.contains(crate::provider_exhaustion::PROVIDER_USAGE_LIMIT_HOLD_CODE)
            {
                notices += 1;
            }
        }
        assert_eq!(
            notices, 1,
            "one operator-visible hold notice, not one per tick"
        );

        // The hold lifts and the wake comes due: it is delivered.
        *mock.usage_hold.lock().unwrap() = None;
        store
            .lock()
            .await
            .defer_wake_when(job.id, Utc::now() - chrono::Duration::seconds(1))
            .unwrap();
        process_due_jobs(&store, &bus, &launcher, None).await;
        assert_eq!(*mock.resume_calls.lock().unwrap(), vec![target]);
    }

    /// #572: a hold that engages between the precheck and the continuation
    /// refuses with the typed hold error; the wake is re-armed at its deadline.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn usage_limit_hold_refusal_from_the_continuation_rearms_the_wake() {
        let (store, bus) = fixture();
        let target = Uuid::new_v4();
        let mut job = mk_fresh_job(Some(target));
        job.wake_mode = WakeMode::Resume;
        store.lock().await.insert_scheduled_job(&job).unwrap();
        let mock = MockLauncher::new(Vec::new());
        let until = Utc::now() + chrono::Duration::hours(5);
        mock.resume_outcomes
            .lock()
            .unwrap()
            .push_back(Err(crate::provider_exhaustion::hold_error(until)));
        let launcher: Arc<dyn SessionLauncher> = mock.clone();

        fire_job(&store, &bus, &launcher, &job).await;

        let held = job_row(&store, &job.id).await;
        assert!(held.enabled, "a held one-shot wake is not consumed");
        assert_eq!(held.last_fired_at, None);
        assert!(held.next_fire_at >= until - chrono::Duration::seconds(2));
        assert!(held.next_fire_at <= until + chrono::Duration::seconds(2));
    }

    /// #996: a generic scheduled Resume refused for a transient capacity reason
    /// is re-armed with backoff (still enabled, later next_fire_at, visible
    /// notice) instead of being consumed, and runs once capacity frees.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn capacity_refused_resume_rearms_without_consuming_the_wake() {
        let (store, bus) = fixture();
        let target = Uuid::new_v4();
        let mut job = mk_fresh_job(Some(target));
        job.wake_mode = WakeMode::Resume;
        job.schedule.recurrence = Recurrence::Once;
        store.lock().await.insert_scheduled_job(&job).unwrap();
        let mock = MockLauncher::new(Vec::new());
        mock.resume_outcomes
            .lock()
            .unwrap()
            .push_back(Err(DaemonError::PolicyDenied(
                "Invalid parameter: manager_v2_concurrency_capacity".into(),
            )));
        let launcher: Arc<dyn SessionLauncher> = mock.clone();
        let mut rx = bus.subscribe();

        fire_job(&store, &bus, &launcher, &job).await;

        let rearmed = store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .unwrap()
            .unwrap();
        assert!(
            rearmed.enabled,
            "transient refusal must keep the wake armed"
        );
        assert!(
            rearmed.next_fire_at > Utc::now(),
            "re-armed with a later next_fire_at"
        );
        assert_eq!(rearmed.last_fired_at, None);
        assert_eq!(*mock.resume_calls.lock().unwrap(), vec![target]);
        let retry = store
            .lock()
            .await
            .continuation_retry(job.id)
            .unwrap()
            .expect("bounded retry state recorded");
        assert_eq!(retry.attempts, 1);
        assert_eq!(retry.last_code, "manager_v2_concurrency_capacity");
        let mut saw_notice = false;
        while let Ok(event) = rx.try_recv() {
            if let DaemonEvent::SystemMessage { level, message } = &*event
                && level == "warn"
                && message.contains("manager_v2_concurrency_capacity")
            {
                saw_notice = true;
            }
        }
        assert!(
            saw_notice,
            "the transient refusal must be visible on the bus"
        );

        // Capacity frees: the retained wake runs and settles as a delivered
        // one-shot.
        mock.resume_outcomes.lock().unwrap().push_back(Ok(target));
        fire_job(&store, &bus, &launcher, &job).await;
        assert_eq!(
            *mock.resume_calls.lock().unwrap(),
            vec![target, target],
            "the re-armed wake must run once capacity frees"
        );
        let settled = store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .unwrap()
            .unwrap();
        assert!(!settled.enabled, "a delivered one-shot is settled/disabled");
    }

    /// #996 counter-case: a non-transient resume refusal keeps today's
    /// behaviour and consumes the one-shot wake.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn non_transient_resume_refusal_still_consumes_the_wake() {
        let (store, bus) = fixture();
        let target = Uuid::new_v4();
        let mut job = mk_fresh_job(Some(target));
        job.wake_mode = WakeMode::Resume;
        job.schedule.recurrence = Recurrence::Once;
        store.lock().await.insert_scheduled_job(&job).unwrap();
        let mock = MockLauncher::new(Vec::new());
        mock.resume_outcomes
            .lock()
            .unwrap()
            .push_back(Err(DaemonError::Rpc("provider exploded".into())));
        let launcher: Arc<dyn SessionLauncher> = mock.clone();

        fire_job(&store, &bus, &launcher, &job).await;

        let settled = store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .unwrap()
            .unwrap();
        assert!(
            !settled.enabled,
            "a non-transient refusal consumes the wake"
        );
        assert!(settled.last_fired_at.is_some());
        assert!(
            store
                .lock()
                .await
                .continuation_retry(job.id)
                .unwrap()
                .is_none(),
            "no retry state for a non-transient refusal"
        );
    }

    fn mk_origin_session(id: Uuid, rotation_disabled: bool) -> Session {
        let now = Utc::now();
        let mut session: Session = serde_json::from_value(serde_json::json!({
            "id": id,
            "status": "Completed",
            "provider": "Codex",
            "created_at": now,
            "updated_at": now,
            "query": "origin",
            "working_dir": "/tmp/rsi-scheduled-fresh",
        }))
        .expect("minimal origin session fixture deserializes");
        session.rotation_disabled_at = rotation_disabled.then_some(now);
        session
    }

    fn fixture() -> (Arc<Mutex<Store>>, Arc<EventBus>) {
        let store = Store::open_in_memory().expect("in-memory store");
        (Arc::new(Mutex::new(store)), Arc::new(EventBus::new(64)))
    }

    struct HeartbeatObservingLauncher {
        heartbeat: LoopHeartbeat,
        observed_progress: StdMutex<Vec<u64>>,
    }

    #[async_trait::async_trait]
    impl SessionLauncher for HeartbeatObservingLauncher {
        async fn launch(&self, _: LaunchConfig) -> crate::error::Result<Uuid> {
            panic!("watch test must not launch a session");
        }

        async fn fire_watch(&self, _: &ScheduledJob) -> crate::error::Result<WatchFireOutcome> {
            self.observed_progress
                .lock()
                .unwrap()
                .push(self.heartbeat.completed_at_millis());
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Ok(WatchFireOutcome::NotReady)
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn due_jobs_refresh_heartbeat_between_slow_dispatches() {
        let (store, bus) = fixture();
        {
            let guard = store.lock().await;
            for _ in 0..2 {
                guard
                    .insert_scheduled_job(&mk_watch_job(Uuid::new_v4(), Uuid::new_v4(), true))
                    .unwrap();
            }
        }
        let heartbeat = LoopHeartbeat::new();
        let launcher = Arc::new(HeartbeatObservingLauncher {
            heartbeat: heartbeat.clone(),
            observed_progress: StdMutex::new(Vec::new()),
        });
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
        process_due_jobs(&store, &bus, &dyn_launcher, Some(&heartbeat)).await;

        let observed = launcher.observed_progress.lock().unwrap();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0], 0);
        assert!(
            observed[1] > 0,
            "first dispatch advances progress before the next"
        );
    }

    async fn job_row(store: &Arc<Mutex<Store>>, id: &Uuid) -> ScheduledJob {
        store
            .lock()
            .await
            .get_scheduled_job(id)
            .expect("get job")
            .expect("job row present")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn invalid_codex_rollout_retires_recurring_resume_with_one_notice() {
        let (store, bus) = fixture();
        let target = Uuid::new_v4();
        let mut job = mk_watch_job(Uuid::new_v4(), target, true);
        job.wake_mode = WakeMode::Resume;
        job.message = "resume".into();
        job.schedule.recurrence = Recurrence::EverySeconds(60);
        store.lock().await.insert_scheduled_job(&job).unwrap();
        let launcher = MockLauncher::new(vec![]);
        launcher
            .resume_outcomes
            .lock()
            .unwrap()
            .push_back(Err(DaemonError::CodexResumeTornTail));
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
        let mut events = bus.subscribe();

        fire_job(&store, &bus, &dyn_launcher, &job).await;
        let retired = job_row(&store, &job.id).await;
        assert!(!retired.enabled);
        assert!(retired.last_fired_at.is_some());
        let notices = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| matches!(event.as_ref(), DaemonEvent::SystemMessage { message, .. } if message.contains("fresh conversation boundary")))
            .count();
        assert_eq!(notices, 1);
        assert_eq!(launcher.resume_calls.lock().unwrap().as_slice(), &[target]);
    }

    fn retryable_custody_error(code: rsi_common::types::SandboxCustodyErrorCodeV1) -> DaemonError {
        crate::error::sandbox_custody_error(rsi_common::types::SandboxCustodyErrorV1 {
            version: 1,
            code,
            session_id: None,
            transition: rsi_common::types::SandboxCustodyTransitionV1::ResumeWake,
            retryable: true,
            recovery: rsi_common::types::SandboxCustodyRecoveryV1::RetryAfterReconcile,
        })
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn transient_custody_gates_preserve_one_shot_resume_and_terminal_watch() {
        for code in [
            rsi_common::types::SandboxCustodyErrorCodeV1::ReclaimPrepared,
            rsi_common::types::SandboxCustodyErrorCodeV1::RootBusy,
        ] {
            let (store, bus) = fixture();
            let master = Uuid::new_v4();
            let watched = Uuid::new_v4();
            let mut resume = mk_watch_job(watched, master, true);
            resume.wake_mode = WakeMode::Resume;
            resume.schedule.recurrence = Recurrence::Once;
            resume.message = "resume payload".into();
            let mut watch = mk_watch_job(watched, master, true);
            watch.schedule.recurrence = Recurrence::Once;
            watch.message = "watch payload".into();
            {
                let guard = store.lock().await;
                guard.insert_scheduled_job(&resume).unwrap();
                guard.insert_scheduled_job(&watch).unwrap();
            }
            let launcher = MockLauncher::new(vec![Ok(WatchFireOutcome::CustodyUnavailable)]);
            launcher
                .resume_outcomes
                .lock()
                .unwrap()
                .push_back(Err(retryable_custody_error(code)));
            let launcher: Arc<dyn SessionLauncher> = launcher;

            fire_job(&store, &bus, &launcher, &resume).await;
            fire_terminal_watch_job(&store, &bus, &launcher, &watch).await;
            for original in [&resume, &watch] {
                let deferred = job_row(&store, &original.id).await;
                assert!(deferred.enabled, "one-shot wake remains armed");
                assert_eq!(deferred.last_fired_at, None);
                assert_eq!(deferred.message, original.message);
                assert!(deferred.next_fire_at > original.next_fire_at);
            }
        }
    }

    fn assert_pending_message_notice(store: &Store, job_id: Uuid, message_id: Uuid, sequence: i64) {
        let notice: (String, String, Option<String>) = store
            .conn
            .query_row(
                "SELECT subject_version,json_extract(state_json,'$.message_id'),settled_at
                 FROM harness_manager_notices
                 WHERE job_id=?1 AND kind='message' AND subject_id=?2",
                rusqlite::params![job_id.to_string(), message_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("exact manager message notice exists");
        assert_eq!(notice.0, sequence.to_string());
        assert_eq!(notice.1, message_id.to_string());
        assert_eq!(notice.2, None);
    }

    struct PausedWatchLauncher {
        observed: StdMutex<Option<tokio::sync::oneshot::Sender<ScheduledJob>>>,
        outcome: Mutex<tokio::sync::oneshot::Receiver<crate::error::Result<WatchFireOutcome>>>,
    }

    #[async_trait::async_trait]
    impl SessionLauncher for PausedWatchLauncher {
        async fn launch(&self, _: LaunchConfig) -> crate::error::Result<Uuid> {
            panic!("manager watch must not reach generic launch");
        }

        async fn fire_watch(&self, job: &ScheduledJob) -> crate::error::Result<WatchFireOutcome> {
            self.observed
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(job.clone())
                .unwrap();
            (&mut *self.outcome.lock().await).await.unwrap()
        }
    }

    /// Pause at the awaited planner/delivery boundary, after the scheduler's
    /// snapshot. Channel rendezvous, rather than timing, orders the new mail.
    async fn pause_watch_fire(
        store: &Arc<Mutex<Store>>,
        bus: &Arc<EventBus>,
        job: &ScheduledJob,
    ) -> (
        ScheduledJob,
        tokio::sync::oneshot::Sender<crate::error::Result<WatchFireOutcome>>,
        tokio::task::JoinHandle<()>,
    ) {
        let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
        let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
        let launcher: Arc<dyn SessionLauncher> = Arc::new(PausedWatchLauncher {
            observed: StdMutex::new(Some(observed_tx)),
            outcome: Mutex::new(outcome_rx),
        });
        let store = store.clone();
        let bus = bus.clone();
        let job = job.clone();
        let task = tokio::spawn(async move { fire_job(&store, &bus, &launcher, &job).await });
        let observed = tokio::time::timeout(std::time::Duration::from_secs(5), observed_rx)
            .await
            .expect("watch reached awaited fire")
            .unwrap();
        (observed, outcome_tx, task)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn manager_watch_stale_confirmation_preserves_new_request_after_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("watch-confirmation.sqlite");
        let store = Store::open(&path).unwrap();
        let fixture = manager_watch_fixture(&store);
        fixture.send(&store, 0, "request-a");
        let job = fixture.notice(&store, 0, false);
        let delivered_at = Utc::now() - chrono::Duration::seconds(10);
        store
            .update_scheduled_job_fired(&job.id, &delivered_at, None, true)
            .unwrap();
        let store = Arc::new(Mutex::new(store));
        let bus = Arc::new(EventBus::new(64));
        let (observed, settle, task) = pause_watch_fire(&store, &bus, &job).await;
        assert_eq!(observed.last_fired_at, Some(delivered_at));

        let (new_request, rearmed) = {
            let guard = store.lock().await;
            let request = fixture.send(&guard, 0, "request-b");
            let row = guard.get_scheduled_job(&job.id).unwrap().unwrap();
            assert!(row.enabled);
            assert_eq!(row.last_fired_at, None);
            let state = notice_state(&guard, job.id);
            assert_pending_message_notice(&guard, job.id, request.message_id, request.sequence);
            (request, state)
        };
        settle.send(Ok(WatchFireOutcome::Confirmed)).unwrap();
        task.await.unwrap();
        assert_eq!(notice_state(&*store.lock().await, job.id), rearmed);

        // Even an accidental generic advance without a witness must not claim
        // this manager row or stamp request B from request A's old snapshot.
        advance_job_after_attempt(&store, &observed).await;
        assert_eq!(notice_state(&*store.lock().await, job.id), rearmed);
        drop(store);

        let reopened = Store::open(&path).unwrap();
        assert_eq!(notice_state(&reopened, job.id), rearmed);
        assert!(
            reopened
                .list_due_scheduled_jobs(&Utc::now())
                .unwrap()
                .iter()
                .any(|due| due.id == job.id)
        );
        let inbox = reopened
            .manager_inbox(fixture.leads[0], &Default::default())
            .unwrap();
        assert!(
            inbox
                .messages
                .iter()
                .any(|message| message.message_id == new_request.message_id && !message.replied)
        );
        let exact = inbox
            .notices
            .iter()
            .find(|notice| notice.subject_id == new_request.message_id.to_string())
            .expect("the preserved request has an exact retrievable notice");
        assert_eq!(exact.subject_version, new_request.sequence.to_string());
        assert_eq!(exact.retrieved_at, exact.settled_at);
        assert!(
            !reopened
                .get_scheduled_job(&job.id)
                .unwrap()
                .unwrap()
                .enabled,
            "authorized retrieval settles the preserved request transport"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn stale_child_confirmation_preserves_continued_watch() {
        let (store, bus) = fixture();
        let job = mk_watch_job(Uuid::new_v4(), Uuid::new_v4(), true);
        let dispatched = Utc::now() - chrono::Duration::seconds(5);
        {
            let guard = store.lock().await;
            guard.insert_scheduled_job(&job).unwrap();
            guard
                .update_scheduled_job_fired(&job.id, &dispatched, None, true)
                .unwrap();
        }
        let (_, settle, task) = pause_watch_fire(&store, &bus, &job).await;
        assert!(
            store
                .lock()
                .await
                .reset_child_watch_after_continue(job.id)
                .unwrap()
        );
        settle.send(Ok(WatchFireOutcome::Confirmed)).unwrap();
        task.await.unwrap();
        let row = job_row(&store, &job.id).await;
        assert!(
            row.enabled,
            "the next child completion still has an armed watch"
        );
        assert_eq!(row.last_fired_at, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn stale_child_delivery_preserves_new_completion_and_its_next_delivery() {
        let (store, bus) = fixture();
        let master = Uuid::new_v4();
        let first = mk_watch_job(Uuid::new_v4(), master, true);
        let sibling = mk_watch_job(Uuid::new_v4(), master, true);
        {
            let guard = store.lock().await;
            guard.insert_scheduled_job(&first).unwrap();
            guard.insert_scheduled_job(&sibling).unwrap();
        }

        // The owner continuation has started for both children. The sibling
        // then enters a new turn before the old delivery is stamped.
        let (_, settle, task) = pause_watch_fire(&store, &bus, &first).await;
        assert!(
            store
                .lock()
                .await
                .reset_child_watch_after_continue(sibling.id)
                .unwrap()
        );
        settle
            .send(Ok(WatchFireOutcome::Delivered {
                session_id: master,
                delivered_job_ids: vec![first.id, sibling.id],
                observed_job_versions: vec![
                    (first.id, first.updated_at),
                    (sibling.id, sibling.updated_at),
                ],
            }))
            .unwrap();
        task.await.unwrap();

        let old_epoch = job_row(&store, &first.id).await;
        let new_epoch = job_row(&store, &sibling.id).await;
        assert!(old_epoch.enabled && old_epoch.last_fired_at.is_some());
        assert!(new_epoch.enabled);
        assert_eq!(new_epoch.last_fired_at, None);

        // The second completion earns its own continuation. One new scheduler
        // fire stamps that epoch so its later confirmation can retire the row.
        let launcher = MockLauncher::new(vec![Ok(WatchFireOutcome::Delivered {
            session_id: master,
            delivered_job_ids: vec![sibling.id],
            observed_job_versions: vec![(sibling.id, new_epoch.updated_at)],
        })]);
        let launcher_dyn: Arc<dyn SessionLauncher> = launcher.clone();
        fire_job(&store, &bus, &launcher_dyn, &new_epoch).await;
        assert_eq!(*launcher.fire_watch_calls.lock().unwrap(), vec![sibling.id]);
        assert!(job_row(&store, &sibling.id).await.last_fired_at.is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn busy_competing_attempt_does_not_erase_accepted_delivery_witness() {
        let (store, bus) = fixture();
        let job = mk_watch_job(Uuid::new_v4(), Uuid::new_v4(), true);
        store.lock().await.insert_scheduled_job(&job).unwrap();
        let (observed, settle, task) = pause_watch_fire(&store, &bus, &job).await;

        // A second invocation captured the same epoch, then found the owner
        // busy. Its NotReady settlement is ordered before the accepted one.
        let (competing, capture) = store
            .lock()
            .await
            .capture_watch_fire(job.id)
            .unwrap()
            .unwrap();
        advance_job_after_attempt_with_capture(&store, &competing, &capture).await;
        settle
            .send(Ok(WatchFireOutcome::Delivered {
                session_id: job.wake_session_id.unwrap(),
                delivered_job_ids: vec![job.id],
                observed_job_versions: vec![(job.id, observed.updated_at)],
            }))
            .unwrap();
        task.await.unwrap();

        let row = job_row(&store, &job.id).await;
        assert!(row.enabled);
        assert!(
            row.last_fired_at.is_some(),
            "accepted continuation keeps its witness"
        );
        assert!(row.next_fire_at > competing.next_fire_at);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn stale_child_abandonment_preserves_continued_watch() {
        let (store, bus) = fixture();
        let job = mk_watch_job(Uuid::new_v4(), Uuid::new_v4(), true);
        store.lock().await.insert_scheduled_job(&job).unwrap();
        let (_, settle, task) = pause_watch_fire(&store, &bus, &job).await;
        assert!(
            store
                .lock()
                .await
                .reset_child_watch_after_continue(job.id)
                .unwrap()
        );
        settle
            .send(Ok(WatchFireOutcome::Abandon {
                reason: "old child state vanished".into(),
            }))
            .unwrap();
        task.await.unwrap();

        let row = job_row(&store, &job.id).await;
        assert!(
            row.enabled,
            "the continued child's completion still has a watch"
        );
        assert_eq!(row.last_fired_at, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn manager_watch_stale_dispatch_abandon_retry_and_error_preserve_new_request() {
        let (store, bus) = fixture();
        let fixture = manager_watch_fixture(&*store.lock().await);
        fixture.send(&*store.lock().await, 0, "initial");
        let job = fixture.notice(&*store.lock().await, 0, false);
        let outcomes = [
            Ok(WatchFireOutcome::Delivered {
                session_id: fixture.leads[0],
                delivered_job_ids: vec![job.id],
                observed_job_versions: vec![],
            }),
            Ok(WatchFireOutcome::Abandon {
                reason: "old delivery timed out".into(),
            }),
            Ok(WatchFireOutcome::NotReady),
            Err(DaemonError::Rpc("old dispatch failed".into())),
        ];
        for (index, outcome) in outcomes.into_iter().enumerate() {
            let (_, settle, task) = pause_watch_fire(&store, &bus, &job).await;
            let rearmed = {
                let guard = store.lock().await;
                let receipt = fixture.send(&guard, 0, &format!("new-request-{index}"));
                let state = notice_state(&guard, job.id);
                assert_pending_message_notice(&guard, job.id, receipt.message_id, receipt.sequence);
                state
            };
            settle.send(outcome).unwrap();
            task.await.unwrap();
            assert_eq!(
                notice_state(&*store.lock().await, job.id),
                rearmed,
                "outcome {index} overwrote newer notice"
            );
            let current = job_row(&store, &job.id).await;
            assert!(current.enabled);
            assert_eq!(current.last_fired_at, None);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn manager_watch_retry_and_error_preserve_the_actual_delivery_witness() {
        let (store, bus) = fixture();
        let fixture = manager_watch_fixture(&*store.lock().await);
        fixture.send(&*store.lock().await, 0, "pending-request");
        let job = fixture.notice(&*store.lock().await, 0, false);
        let delivered_at = Utc::now() - chrono::Duration::seconds(10);
        for previous_delivery in [None, Some(delivered_at)] {
            if let Some(at) = previous_delivery {
                store
                    .lock()
                    .await
                    .update_scheduled_job_fired(&job.id, &at, None, true)
                    .unwrap();
            }
            let launcher: Arc<dyn SessionLauncher> = MockLauncher::new(vec![
                Ok(WatchFireOutcome::NotReady),
                Err(DaemonError::Rpc("transient delivery failure".into())),
            ]);
            for _ in 0..2 {
                fire_job(&store, &bus, &launcher, &job).await;
                let pending = job_row(&store, &job.id).await;
                assert!(pending.enabled);
                assert!(pending.next_fire_at > job.next_fire_at);
                assert_eq!(pending.last_fired_at, previous_delivery);
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn manager_watch_coalescing_stamps_only_observed_unchanged_notices_after_reopen() {
        use rsi_common::harness_manager::AgentManagerReplyRequestV1;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("watch-coalescing.sqlite");
        let store = Store::open(&path).unwrap();
        let fixture = manager_watch_fixture(&store);
        let requests: Vec<_> = (0..3)
            .map(|index| fixture.send(&store, index, &format!("request-{index}")))
            .collect();
        let jobs: Vec<_> = (0..3)
            .map(|index| fixture.notice(&store, index, true))
            .collect();
        // A disabled sibling is not observed, then a reply rearms it while the
        // old dispatch is paused. A later planner may include it in its bundle.
        store
            .update_scheduled_job_fired(&jobs[2].id, &Utc::now(), None, false)
            .unwrap();
        let primary_before = notice_state(&store, jobs[0].id);
        let store = Arc::new(Mutex::new(store));
        let bus = Arc::new(EventBus::new(64));
        let (_, settle, task) = pause_watch_fire(&store, &bus, &jobs[0]).await;
        let rearmed = {
            let guard = store.lock().await;
            for index in 1..3 {
                guard
                    .manager_reply(
                        fixture.leads[index],
                        &AgentManagerReplyRequestV1 {
                            request_id: requests[index].message_id,
                            message: format!("New reply for feature {index}"),
                            idempotency_key: format!("reply-{index}"),
                            still_running: false,
                        },
                    )
                    .unwrap();
            }
            [
                notice_state(&guard, jobs[1].id),
                notice_state(&guard, jobs[2].id),
            ]
        };
        settle
            .send(Ok(WatchFireOutcome::Delivered {
                session_id: fixture.manager,
                delivered_job_ids: jobs.iter().map(|job| job.id).collect(),
                observed_job_versions: vec![],
            }))
            .unwrap();
        task.await.unwrap();
        let dispatched = job_row(&store, &jobs[0].id).await;
        assert!(dispatched.enabled && dispatched.last_fired_at.is_some());
        assert!(dispatched.next_fire_at > jobs[0].next_fire_at);
        assert_eq!(
            notice_state(&*store.lock().await, jobs[0].id).1,
            primary_before.1 + 1
        );
        for index in 1..3 {
            assert_eq!(
                notice_state(&*store.lock().await, jobs[index].id),
                rearmed[index - 1]
            );
        }
        drop(store);

        let reopened = Store::open(&path).unwrap();
        for index in 1..3 {
            assert_eq!(notice_state(&reopened, jobs[index].id), rearmed[index - 1]);
            let row = reopened
                .get_scheduled_job(&jobs[index].id)
                .unwrap()
                .unwrap();
            assert!(row.enabled);
            assert_eq!(row.last_fired_at, None);
        }
        let store = Arc::new(Mutex::new(reopened));
        let launcher: Arc<dyn SessionLauncher> =
            MockLauncher::new(vec![Ok(WatchFireOutcome::Delivered {
                session_id: fixture.manager,
                delivered_job_ids: vec![jobs[1].id, jobs[2].id],
                observed_job_versions: vec![],
            })]);
        fire_job(&store, &bus, &launcher, &jobs[1]).await;
        for job in &jobs[1..] {
            let row = job_row(&store, &job.id).await;
            assert!(row.enabled && row.last_fired_at.is_some());
        }
    }

    fn capacity_fixture(due: bool) -> (Arc<Mutex<Store>>, Arc<EventBus>, ScheduledJob, Uuid) {
        let store = Store::open_in_memory().expect("capacity scheduler store");
        let mut session = rsid_store::test_support::make_test_session();
        session.id = Uuid::new_v4();
        session.provider = rsi_common::types::SessionProvider::Codex;
        session.model = Some("gpt-5.4".into());
        session.status = SessionStatus::Failed;
        session.stop_reason = Some("provider_error:codex_usage_limit".into());
        session.project_id = None;
        let controller = session.id;
        store.insert_session(&session).unwrap();
        let now = Utc::now();
        let guard_job = crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(
            crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
                message: "guard".into(),
                in_seconds: None,
                at: None,
                name: None,
                every_seconds: None,
                mode: Some("program_guard".into()),
                working_dir: session.working_dir.clone(),
                provider: Some(rsi_common::types::SessionProvider::Codex),
                model: session.model.clone(),
                project_id: None,
                origin_session_id: Some(controller),
                watch_session_id: None,
            },
        )
        .unwrap();
        let guard_id = guard_job.id;
        store.insert_scheduled_job(&guard_job).unwrap();
        let invocation = Uuid::new_v4();
        let terminal_at = if due {
            now - chrono::Duration::seconds(61)
        } else {
            now
        };
        let timestamp = terminal_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO model_invocations(
                    id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                    trigger_source,session_id,policy_snapshot_json,usage_confidence,
                    created_at,completed_at
                 ) VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable',
                          'admitted','failed','capacity-scheduler-fixture',?2,'{}','unavailable',?3,?3)",
                rusqlite::params![invocation.to_string(), controller.to_string(), timestamp],
            )
            .unwrap();
        let recovery = store
            .settle_capacity_failure(controller, controller, guard_id, invocation, 1, terminal_at)
            .unwrap();
        let job = store
            .get_scheduled_job(&recovery.wake_job_id)
            .unwrap()
            .unwrap();
        (
            Arc::new(Mutex::new(store)),
            Arc::new(EventBus::new(64)),
            job,
            controller,
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn capacity_startup_catchup_never_dispatches_before_due_and_settles_after_due() {
        let (store, bus, job, controller) = capacity_fixture(false);
        let launcher = MockLauncher::new(Vec::new());
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
        fire_job(&store, &bus, &dyn_launcher, &job).await;
        assert!(launcher.capacity_resume_calls.lock().unwrap().is_empty());
        assert!(job_row(&store, &job.id).await.enabled);

        let (store, bus, job, controller_due) = capacity_fixture(true);
        let launcher = MockLauncher::new(Vec::new());
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
        fire_job(&store, &bus, &dyn_launcher, &job).await;
        assert_eq!(
            *launcher.capacity_resume_calls.lock().unwrap(),
            vec![(controller_due, job.id, job.next_fire_at)]
        );
        assert!(launcher.resume_calls.lock().unwrap().is_empty());
        assert!(launcher.scheduled_fresh_calls.lock().unwrap().is_empty());
        assert!(!job_row(&store, &job.id).await.enabled);
        assert_ne!(controller, Uuid::nil());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn capacity_failure_before_admission_keeps_one_wake_enabled() {
        let (store, bus, job, _) = capacity_fixture(true);
        let launcher = MockLauncher::new(Vec::new());
        launcher
            .capacity_resume_outcomes
            .lock()
            .unwrap()
            .push_back(Err(DaemonError::Rpc(
                "injected pre-admission failure".into(),
            )));
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
        fire_job(&store, &bus, &dyn_launcher, &job).await;
        assert_eq!(launcher.capacity_resume_calls.lock().unwrap().len(), 1);
        assert!(job_row(&store, &job.id).await.enabled);
        assert!(launcher.resume_calls.lock().unwrap().is_empty());
        assert!(launcher.scheduled_fresh_calls.lock().unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn capacity_duplicate_admission_disables_exact_due_slot_without_second_provider() {
        let (store, bus, job, controller) = capacity_fixture(true);
        let launcher = MockLauncher::new(Vec::new());
        launcher
            .capacity_resume_outcomes
            .lock()
            .unwrap()
            .push_back(Ok(controller));
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
        fire_job(&store, &bus, &dyn_launcher, &job).await;
        assert_eq!(launcher.capacity_resume_calls.lock().unwrap().len(), 1);
        assert!(!job_row(&store, &job.id).await.enabled);
        assert!(launcher.resume_calls.lock().unwrap().is_empty());
        assert!(launcher.scheduled_fresh_calls.lock().unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn capacity_malformed_recognized_rows_never_fall_through_to_fresh_or_agent_fresh() {
        for mutation in 0..10 {
            let (store, bus, original, _) = capacity_fixture(true);
            {
                let locked = store.lock().await;
                match mutation {
                    0 => {
                        locked
                            .conn
                            .execute(
                                "UPDATE scheduled_jobs SET wake_session_id=NULL WHERE id=?1",
                                [original.id.to_string()],
                            )
                            .unwrap();
                    }
                    1 => {
                        let mut wrong = rsid_store::test_support::make_test_session();
                        wrong.id = Uuid::new_v4();
                        let wrong_id = wrong.id;
                        locked.insert_session(&wrong).unwrap();
                        locked
                            .conn
                            .execute(
                                "UPDATE scheduled_jobs SET wake_session_id=?1 WHERE id=?2",
                                rusqlite::params![wrong_id.to_string(), original.id.to_string()],
                            )
                            .unwrap();
                    }
                    2 | 3 => {
                        locked
                            .conn
                            .execute(
                                "UPDATE scheduled_jobs SET wake_mode=?1 WHERE id=?2",
                                rusqlite::params![
                                    if mutation == 2 {
                                        "fresh"
                                    } else {
                                        "agent_fresh"
                                    },
                                    original.id.to_string()
                                ],
                            )
                            .unwrap();
                    }
                    4 => {
                        locked
                            .conn
                            .execute(
                                "UPDATE scheduled_jobs SET schedule_json=?1 WHERE id=?2",
                                rusqlite::params![
                                    serde_json::to_string(&ScheduleSpec {
                                        recurrence: Recurrence::EverySeconds(60),
                                        anchor: original.schedule.anchor,
                                    })
                                    .unwrap(),
                                    original.id.to_string()
                                ],
                            )
                            .unwrap();
                    }
                    5 => {
                        locked
                            .conn
                            .execute(
                                "UPDATE scheduled_jobs SET provider=NULL WHERE id=?1",
                                [original.id.to_string()],
                            )
                            .unwrap();
                    }
                    6 => {
                        locked
                            .conn
                            .execute(
                                "UPDATE scheduled_jobs SET model='changed-model' WHERE id=?1",
                                [original.id.to_string()],
                            )
                            .unwrap();
                    }
                    7 => {
                        locked
                            .conn
                            .execute(
                                "UPDATE scheduled_jobs SET working_dir='/tmp/changed-capacity-path' WHERE id=?1",
                                [original.id.to_string()],
                            )
                            .unwrap();
                    }
                    8 => {
                        locked
                            .conn
                            .execute(
                                "UPDATE scheduled_jobs SET next_fire_at=?1 WHERE id=?2",
                                rusqlite::params![
                                    (original.next_fire_at + chrono::Duration::seconds(1))
                                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true,),
                                    original.id.to_string()
                                ],
                            )
                            .unwrap();
                    }
                    9 => {
                        locked
                            .conn
                            .execute(
                                "UPDATE scheduled_jobs SET project_id=?1 WHERE id=?2",
                                rusqlite::params![
                                    crate::store::d04_test_project_id().to_string(),
                                    original.id.to_string()
                                ],
                            )
                            .unwrap();
                    }
                    _ => unreachable!(),
                }
            }
            let snapshot = job_row(&store, &original.id).await;
            let launcher = MockLauncher::new(Vec::new());
            let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
            fire_job(&store, &bus, &dyn_launcher, &snapshot).await;
            assert!(launcher.capacity_resume_calls.lock().unwrap().is_empty());
            assert!(launcher.resume_calls.lock().unwrap().is_empty());
            assert!(launcher.scheduled_fresh_calls.lock().unwrap().is_empty());
            assert_eq!(*launcher.launch_calls.lock().unwrap(), 0);
            assert!(!job_row(&store, &snapshot.id).await.enabled);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn capacity_stale_snapshot_cannot_disable_a_rearmed_due_slot() {
        let (store, bus, mut snapshot, _) = capacity_fixture(true);
        snapshot.next_fire_at -= chrono::Duration::seconds(1);
        let launcher = MockLauncher::new(Vec::new());
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
        fire_job(&store, &bus, &dyn_launcher, &snapshot).await;
        assert!(launcher.capacity_resume_calls.lock().unwrap().is_empty());
        assert!(job_row(&store, &snapshot.id).await.enabled);
        assert!(launcher.scheduled_fresh_calls.lock().unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn capacity_closed_incident_is_disabled_without_any_launch() {
        let (store, bus, snapshot, controller) = capacity_fixture(true);
        {
            let locked = store.lock().await;
            assert!(matches!(
                locked
                    .close_capacity_incident(
                        controller,
                        crate::store::capacity_recovery::CapacityCloseKind::NonCapacitySuccess,
                        None,
                        None,
                        Utc::now(),
                    )
                    .unwrap(),
                crate::store::capacity_recovery::CapacityCloseOutcome::Closed { .. }
            ));
        }
        let launcher = MockLauncher::new(Vec::new());
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
        fire_job(&store, &bus, &dyn_launcher, &snapshot).await;
        assert!(launcher.capacity_resume_calls.lock().unwrap().is_empty());
        assert!(launcher.resume_calls.lock().unwrap().is_empty());
        assert!(launcher.scheduled_fresh_calls.lock().unwrap().is_empty());
        assert_eq!(*launcher.launch_calls.lock().unwrap(), 0);
        assert!(!job_row(&store, &snapshot.id).await.enabled);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn scheduled_fresh_inherits_disabled_origin() {
        let (store, bus) = fixture();
        let origin_id = Uuid::new_v4();
        let job = mk_agent_fresh_job(origin_id);
        {
            let store = store.lock().await;
            store
                .insert_session(&mk_origin_session(origin_id, false))
                .expect("insert disabled origin");
            store
                .toggle_session_rotation_disabled(origin_id)
                .expect("persist disabled origin");
            store.insert_scheduled_job(&job).expect("insert fresh job");
        }
        let launcher = MockLauncher::new(Vec::new());
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();

        fire_job(&store, &bus, &dyn_launcher, &job).await;

        assert_eq!(
            *launcher.scheduled_fresh_calls.lock().unwrap(),
            vec![true],
            "the persisted disabled origin must reach the scheduled Fresh seam"
        );
        assert_eq!(*launcher.launch_calls.lock().unwrap(), 0);
        assert_eq!(
            *launcher.scheduled_fresh_purposes.lock().unwrap(),
            vec![rsi_common::model_control::ModelInvocationPurpose::AgentScheduleWakeFresh]
        );
        assert!(
            !job_row(&store, &job.id).await.enabled,
            "the one-shot Fresh job should be settled after launch"
        );
    }

    /// Issue #12: an occupied tree does not consume the one-shot. The wake
    /// stays armed and is re-armed for a retry; the retry launches once the
    /// launcher admits it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn occupied_tree_keeps_fresh_wake_armed_then_launches_when_quiet() {
        let (store, bus) = fixture();
        let origin_id = Uuid::new_v4();
        let job = mk_agent_fresh_job(origin_id);
        {
            let guard = store.lock().await;
            guard
                .insert_session(&mk_origin_session(origin_id, false))
                .expect("insert settled origin");
            guard.insert_scheduled_job(&job).expect("insert job");
        }
        let launcher = MockLauncher::new(Vec::new());
        launcher
            .scheduled_fresh_outcomes
            .lock()
            .unwrap()
            .push_back(Err(crate::session::tree_admission::tree_occupied_error(
                "a neighbour writes the tree",
            )));
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
        let before = Utc::now();

        fire_job(&store, &bus, &dyn_launcher, &job).await;

        let held = job_row(&store, &job.id).await;
        assert!(
            held.enabled,
            "an occupied tree must not consume the one-shot"
        );
        assert!(
            held.next_fire_at >= before + chrono::Duration::seconds(30),
            "the wake is re-armed for a later retry, not left due"
        );
        assert!(
            store
                .lock()
                .await
                .list_scheduled_jobs()
                .unwrap()
                .iter()
                .all(|row| !row.name.starts_with("fresh-wake-declined:")),
            "no owner notice while the wake is still retrying"
        );

        fire_job(&store, &bus, &dyn_launcher, &held).await;
        assert!(
            !job_row(&store, &job.id).await.enabled,
            "once the launcher admits the launch the one-shot settles"
        );
    }

    /// Issue #12: past the retry window the wake is settled and its owner is
    /// resumed with a durable note, so the decline is never silent.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn occupied_tree_past_the_window_settles_and_notifies_the_owner() {
        let (store, bus) = fixture();
        let origin_id = Uuid::new_v4();
        let mut job = mk_agent_fresh_job(origin_id);
        job.schedule.anchor = Utc::now() - chrono::Duration::minutes(61);
        {
            let guard = store.lock().await;
            guard
                .insert_session(&mk_origin_session(origin_id, false))
                .expect("insert settled origin");
            guard.insert_scheduled_job(&job).expect("insert job");
        }
        let launcher = MockLauncher::new(Vec::new());
        launcher
            .scheduled_fresh_outcomes
            .lock()
            .unwrap()
            .push_back(Err(crate::session::tree_admission::tree_occupied_error(
                "a neighbour writes the tree",
            )));
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();

        launcher
            .scheduled_fresh_outcomes
            .lock()
            .unwrap()
            .push_back(Err(crate::session::tree_admission::tree_occupied_error(
                "a neighbour writes the tree",
            )));
        fire_job(&store, &bus, &dyn_launcher, &job).await;
        // A repeated settle of the same decline never queues a second note.
        fire_job(&store, &bus, &dyn_launcher, &job).await;

        assert!(!job_row(&store, &job.id).await.enabled);
        let notices: Vec<_> = store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|row| row.name == format!("fresh-wake-declined:{}", job.id))
            .collect();
        assert_eq!(notices.len(), 1, "exactly one durable decline note");
        let notice = &notices[0];
        assert!(notice.enabled);
        assert_eq!(notice.wake_mode, WakeMode::Resume);
        assert_eq!(notice.wake_session_id, Some(origin_id));
    }

    /// Issue #12 (F2): if the owner notice cannot be written, the one-shot is
    /// NOT consumed (the settlement rolls back with the failed insert); the
    /// next poll settles it together with its single notice.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn occupied_tree_notice_failure_leaves_the_one_shot_armed_for_retry() {
        let (store, bus) = fixture();
        let origin_id = Uuid::new_v4();
        let mut job = mk_agent_fresh_job(origin_id);
        job.schedule.anchor = Utc::now() - chrono::Duration::minutes(61);
        {
            let guard = store.lock().await;
            guard
                .insert_session(&mk_origin_session(origin_id, false))
                .expect("insert settled origin");
            guard.insert_scheduled_job(&job).expect("insert job");
            guard
                .conn
                .execute_batch(
                    "CREATE TEMP TRIGGER fail_notice BEFORE INSERT ON scheduled_jobs
                     WHEN NEW.name LIKE 'fresh-wake-declined:%'
                     BEGIN SELECT RAISE(ABORT, 'injected notice failure'); END;",
                )
                .expect("inject failure");
        }
        let launcher = MockLauncher::new(Vec::new());
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
        for _ in 0..2 {
            launcher
                .scheduled_fresh_outcomes
                .lock()
                .unwrap()
                .push_back(Err(crate::session::tree_admission::tree_occupied_error(
                    "a neighbour writes the tree",
                )));
        }

        fire_job(&store, &bus, &dyn_launcher, &job).await;
        assert!(
            job_row(&store, &job.id).await.enabled,
            "a failed notice insert must not consume the wake"
        );

        store
            .lock()
            .await
            .conn
            .execute_batch("DROP TRIGGER fail_notice;")
            .expect("clear failure");
        let current = job_row(&store, &job.id).await;
        fire_job(&store, &bus, &dyn_launcher, &current).await;
        assert!(!job_row(&store, &job.id).await.enabled);
        let notices = store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|row| row.name == format!("fresh-wake-declined:{}", job.id))
            .count();
        assert_eq!(notices, 1, "the retry settles with exactly one notice");
    }

    /// Issue #12: a recurring fresh wake skips the declined occurrence and
    /// stays armed for the next one: no retry window, no notice jobs, however
    /// old the job is and however many polls decline.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn occupied_tree_skips_a_recurring_fresh_wake_occurrence_and_stays_armed() {
        let (store, bus) = fixture();
        let origin_id = Uuid::new_v4();
        let mut job = mk_agent_fresh_job(origin_id);
        job.schedule = ScheduleSpec {
            recurrence: Recurrence::EverySeconds(60),
            anchor: Utc::now() - chrono::Duration::hours(3),
        };
        {
            let guard = store.lock().await;
            guard
                .insert_session(&mk_origin_session(origin_id, false))
                .expect("insert settled origin");
            guard.insert_scheduled_job(&job).expect("insert job");
        }
        let launcher = MockLauncher::new(Vec::new());
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
        for _ in 0..3 {
            launcher
                .scheduled_fresh_outcomes
                .lock()
                .unwrap()
                .push_back(Err(crate::session::tree_admission::tree_occupied_error(
                    "a neighbour writes the tree",
                )));
            let current = job_row(&store, &job.id).await;
            fire_job(&store, &bus, &dyn_launcher, &current).await;
        }

        let after = job_row(&store, &job.id).await;
        assert!(after.enabled, "the recurring wake stays armed");
        assert!(
            after.next_fire_at > Utc::now(),
            "armed for the next occurrence"
        );
        assert!(
            store
                .lock()
                .await
                .list_scheduled_jobs()
                .unwrap()
                .iter()
                .all(|row| !row.name.starts_with("fresh-wake-declined:")),
            "a skipped occurrence queues no decline jobs"
        );
    }

    /// Issue #30 / #27 (priority 1): an `AgentFresh` wake whose target session
    /// is still LIVE must NOT spawn. The job's working_dir is that live
    /// session's sandbox worktree, so a launch here means two uncoordinated
    /// agent processes in one tree. Covers every live status.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn agent_fresh_declines_to_spawn_a_twin_into_a_live_target() {
        for status in [
            SessionStatus::Starting,
            SessionStatus::Running,
            SessionStatus::WaitingApproval,
        ] {
            let (store, bus) = fixture();
            let origin_id = Uuid::new_v4();
            let job = mk_agent_fresh_job(origin_id);
            {
                let guard = store.lock().await;
                let mut origin = mk_origin_session(origin_id, false);
                origin.status = status;
                guard.insert_session(&origin).expect("insert live origin");
                guard.insert_scheduled_job(&job).expect("insert job");
            }
            let mut events = bus.subscribe();
            let launcher = MockLauncher::new(Vec::new());
            let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();

            fire_job(&store, &bus, &dyn_launcher, &job).await;

            assert!(
                launcher.scheduled_fresh_calls.lock().unwrap().is_empty(),
                "status {status:?}: a live wake target must never reach the Fresh launch seam"
            );
            assert_eq!(
                *launcher.launch_calls.lock().unwrap(),
                0,
                "status {status:?}: no launch of any kind"
            );
            assert!(
                launcher.resume_calls.lock().unwrap().is_empty(),
                "status {status:?}: declined, not converted into a resume"
            );

            // The decline must be observable — a silently-skipped wake is its
            // own bug.
            let mut saw_warn = false;
            while let Ok(event) = events.try_recv() {
                if let DaemonEvent::SystemMessage { level, message } = event.as_ref() {
                    if level == "warn"
                        && message.contains("still live")
                        && message.contains(&origin_id.to_string())
                    {
                        assert!(message.contains("transfers no hierarchy or lead authority"));
                        assert!(message.contains("AgentReserveSuccessor"));
                        saw_warn = true;
                    }
                }
            }
            bus.unsubscribe();
            assert!(
                saw_warn,
                "status {status:?}: the decline must surface a warn naming the live target"
            );
            assert!(
                !job_row(&store, &job.id).await.enabled,
                "status {status:?}: the one-shot job must settle without launching"
            );
        }
    }

    /// Issue #30 guard must not regress the LEGITIMATE case: `AgentFresh`
    /// against a target that has since gone terminal is the intended feature
    /// ("wake a fresh agent after I've finished") and must still launch, with
    /// its dedicated admission purpose intact. Covers every terminal status.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn agent_fresh_still_launches_when_target_is_terminal() {
        for status in [
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Interrupted,
            SessionStatus::Archived,
        ] {
            let (store, bus) = fixture();
            let origin_id = Uuid::new_v4();
            let job = mk_agent_fresh_job(origin_id);
            {
                let guard = store.lock().await;
                let mut origin = mk_origin_session(origin_id, false);
                origin.status = status;
                guard
                    .insert_session(&origin)
                    .expect("insert terminal origin");
                guard.insert_scheduled_job(&job).expect("insert job");
            }
            let launcher = MockLauncher::new(Vec::new());
            let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();

            fire_job(&store, &bus, &dyn_launcher, &job).await;

            assert_eq!(
                *launcher.scheduled_fresh_calls.lock().unwrap(),
                vec![false],
                "status {status:?}: a terminal target must still launch exactly once"
            );
            assert_eq!(
                *launcher.scheduled_fresh_purposes.lock().unwrap(),
                vec![rsi_common::model_control::ModelInvocationPurpose::AgentScheduleWakeFresh],
                "status {status:?}: the dedicated agent admission purpose must survive the guard"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn scheduled_fresh_preserves_enabled_default_and_rejects_missing_origin() {
        let (store, bus) = fixture();
        let enabled_origin_id = Uuid::new_v4();
        let missing_origin_id = Uuid::new_v4();
        let enabled_origin_job = mk_fresh_job(Some(enabled_origin_id));
        let generic_job = mk_fresh_job(None);
        let missing_origin_job = mk_agent_fresh_job(missing_origin_id);
        {
            let store = store.lock().await;
            store
                .insert_session(&mk_origin_session(enabled_origin_id, false))
                .expect("insert enabled origin");
            store
                .insert_scheduled_job(&enabled_origin_job)
                .expect("insert enabled-origin job");
            store
                .insert_scheduled_job(&generic_job)
                .expect("insert generic job");
            store
                .insert_scheduled_job(&missing_origin_job)
                .expect("insert missing-origin job");
        }
        let mut events = bus.subscribe();
        let launcher = MockLauncher::new(Vec::new());
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();

        fire_job(&store, &bus, &dyn_launcher, &enabled_origin_job).await;
        fire_job(&store, &bus, &dyn_launcher, &generic_job).await;
        fire_job(&store, &bus, &dyn_launcher, &missing_origin_job).await;

        assert_eq!(
            *launcher.scheduled_fresh_calls.lock().unwrap(),
            vec![false, false],
            "enabled and unbound Fresh jobs retain the default enabled state"
        );
        assert_eq!(*launcher.launch_calls.lock().unwrap(), 0);
        let mut saw_missing_origin_error = false;
        for _ in 0..3 {
            if let Ok(event) = events.try_recv() {
                if let DaemonEvent::SystemMessage { level, message } = event.as_ref() {
                    saw_missing_origin_error = level == "error"
                        && message.contains("cannot resolve bound origin")
                        && message.contains(&missing_origin_id.to_string());
                }
            }
        }
        bus.unsubscribe();
        assert!(
            saw_missing_origin_error,
            "a bound Fresh job with no origin row must fail closed visibly"
        );
        assert!(
            !job_row(&store, &missing_origin_job.id).await.enabled,
            "the one-shot missing-origin job must settle without launching"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn one_shot_agent_fresh_selects_dedicated_purpose_while_generic_and_legacy_stay_scheduled()
     {
        let (store, bus) = fixture();
        let agent_origin = Uuid::new_v4();
        let legacy_origin = Uuid::new_v4();
        let agent_job = mk_agent_fresh_job(agent_origin);
        let generic_job = mk_fresh_job(None);
        let legacy_job = mk_fresh_job(Some(legacy_origin));
        {
            let guard = store.lock().await;
            guard
                .insert_session(&mk_origin_session(agent_origin, false))
                .expect("insert agent origin");
            guard
                .insert_session(&mk_origin_session(legacy_origin, false))
                .expect("insert legacy origin");
            for job in [&agent_job, &generic_job, &legacy_job] {
                guard.insert_scheduled_job(job).expect("insert job");
            }
        }
        let launcher = MockLauncher::new(Vec::new());
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();

        for job in [&agent_job, &generic_job, &legacy_job] {
            fire_job(&store, &bus, &dyn_launcher, job).await;
        }

        assert_eq!(
            *launcher.scheduled_fresh_purposes.lock().unwrap(),
            vec![
                rsi_common::model_control::ModelInvocationPurpose::AgentScheduleWakeFresh,
                rsi_common::model_control::ModelInvocationPurpose::ScheduledFresh,
                rsi_common::model_control::ModelInvocationPurpose::ScheduledFresh,
            ]
        );
        for job in [&agent_job, &generic_job, &legacy_job] {
            assert!(
                !job_row(&store, &job.id).await.enabled,
                "one-shot job {} must settle after exactly one launch attempt",
                job.id
            );
        }
    }

    /// T-2: a due `OnTerminal` row fires via the plain due-poll with NO bus
    /// event anywhere in the loop — the DB-state shape of the two
    /// `SessionStatusChanged`-bypassing terminal flips (F-003/F-004).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn bypass_flip_fires_via_tick_without_bus_event() {
        let (store, bus) = fixture();
        let job = mk_watch_job(Uuid::new_v4(), Uuid::new_v4(), true);
        store
            .lock()
            .await
            .insert_scheduled_job(&job)
            .expect("insert");
        let resumed = Uuid::new_v4();
        let launcher = MockLauncher::new(vec![Ok(WatchFireOutcome::Delivered {
            session_id: resumed,
            delivered_job_ids: vec![job.id],
            observed_job_versions: vec![],
        })]);

        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();
        process_due_jobs(&store, &bus, &dyn_launcher, None).await;

        assert_eq!(*launcher.fire_watch_calls.lock().unwrap(), vec![job.id]);
        assert_eq!(*launcher.launch_calls.lock().unwrap(), 0);
        // Issue #12: dispatch stamps and re-arms; only confirmation retires.
        let row = job_row(&store, &job.id).await;
        assert!(
            row.enabled && row.last_fired_at.is_some(),
            "dispatched watch stays armed pending confirmation, with the stamp written"
        );
    }

    /// T-3: a rejected delivery (busy master) keeps the recurring row armed
    /// with `next_fire_at` advanced; a later dispatch stamps it and keeps it
    /// armed pending confirmation (D3 requeue-until-idle).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn busy_master_rejection_keeps_watch_armed() {
        let (store, bus) = fixture();
        let job = mk_watch_job(Uuid::new_v4(), Uuid::new_v4(), true);
        store
            .lock()
            .await
            .insert_scheduled_job(&job)
            .expect("insert");
        let launcher = MockLauncher::new(vec![
            Ok(WatchFireOutcome::NotReady),
            Ok(WatchFireOutcome::Delivered {
                session_id: Uuid::new_v4(),
                delivered_job_ids: vec![job.id],
                observed_job_versions: vec![],
            }),
        ]);
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();

        fire_job(&store, &bus, &dyn_launcher, &job).await;
        let after_first = job_row(&store, &job.id).await;
        assert!(after_first.enabled, "NotReady must keep the row armed");
        assert_eq!(
            after_first.last_fired_at, None,
            "a busy owner did not receive a watch delivery to confirm"
        );
        assert!(
            after_first.next_fire_at > job.next_fire_at,
            "NotReady must advance next_fire_at"
        );

        fire_job(&store, &bus, &dyn_launcher, &after_first).await;
        // Issue #12: a dispatch is NOT a delivery. `continue_session` returns as
        // soon as the provider process exists, so retiring the row here lost the
        // notification whenever the resumed turn then died without emitting
        // anything. The row stays armed, deferred by the re-delivery backoff,
        // until the confirmation gate proves the tip produced output.
        let after_dispatch = job_row(&store, &job.id).await;
        assert!(
            after_dispatch.enabled,
            "dispatch must keep the row armed pending confirmation"
        );
        assert!(
            after_dispatch.last_fired_at.is_some(),
            "dispatch must stamp last_fired_at -- the confirmation gate compares against it"
        );
        assert!(
            after_dispatch.next_fire_at > after_first.next_fire_at,
            "dispatch must defer the next attempt by the re-delivery backoff"
        );
        assert_eq!(*launcher.launch_calls.lock().unwrap(), 0);
    }

    /// Issue #12: only a proven-consumed delivery retires the watch row.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn confirmed_delivery_retires_the_watch_row() {
        let (store, bus) = fixture();
        let job = mk_watch_job(Uuid::new_v4(), Uuid::new_v4(), true);
        {
            let guard = store.lock().await;
            guard.insert_scheduled_job(&job).expect("insert");
            guard
                .update_scheduled_job_fired(&job.id, &Utc::now(), None, true)
                .expect("record earlier delivery");
        }
        let launcher = MockLauncher::new(vec![Ok(WatchFireOutcome::Confirmed)]);
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();

        fire_job(&store, &bus, &dyn_launcher, &job).await;

        assert!(
            !job_row(&store, &job.id).await.enabled,
            "a confirmed delivery must retire the row"
        );
        assert!(launcher.resume_calls.lock().unwrap().is_empty());
    }

    /// T-6 (scheduler half): one Delivered outcome naming two coalesced jobs
    /// stamps BOTH rows off a single `fire_watch` call and keeps both armed
    /// pending confirmation.
    ///
    /// Issue #12 moved sibling dedup from "eager disable on dispatch" to the
    /// confirmation gate in `plan_terminal_watch_fire`: a coalesced sibling now
    /// retires on its own next tick, which sees the tip's post-dispatch output
    /// and returns `Confirmed`. Eager disable was what destroyed the
    /// notification when the dispatched turn produced nothing, so the dedup had
    /// to move somewhere that can tell those two cases apart.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn two_terminal_children_one_master_single_delivery_stamps_both() {
        let (store, bus) = fixture();
        let master = Uuid::new_v4();
        let job_a = mk_watch_job(Uuid::new_v4(), master, true);
        let job_b = mk_watch_job(Uuid::new_v4(), master, true);
        {
            let guard = store.lock().await;
            guard.insert_scheduled_job(&job_a).expect("insert a");
            guard.insert_scheduled_job(&job_b).expect("insert b");
        }
        let launcher = MockLauncher::new(vec![Ok(WatchFireOutcome::Delivered {
            session_id: master,
            delivered_job_ids: vec![job_a.id, job_b.id],
            observed_job_versions: vec![(job_a.id, job_a.updated_at), (job_b.id, job_b.updated_at)],
        })]);
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();

        fire_job(&store, &bus, &dyn_launcher, &job_a).await;

        assert_eq!(launcher.fire_watch_calls.lock().unwrap().len(), 1);
        for (label, id) in [("a", job_a.id), ("b", job_b.id)] {
            let row = job_row(&store, &id).await;
            assert!(row.enabled, "coalesced job {label} stays armed on dispatch");
            assert!(
                row.last_fired_at.is_some(),
                "coalesced job {label} must be stamped so its gate can confirm"
            );
        }

        // Review F1 replay leg: job B fires later from the SAME stale due-list
        // snapshot. Its struct still says `enabled: true`; simulate the row
        // having since been retired (here by its own confirmation tick, as a
        // coalesced sibling now is) and pin that the fresh-row re-read no-ops:
        // no second delivery, no state change, never re-enabled.
        store
            .lock()
            .await
            .update_scheduled_job_fired(&job_b.id, &Utc::now(), None, false)
            .expect("retire b");
        let b_before = job_row(&store, &job_b.id).await;
        fire_job(&store, &bus, &dyn_launcher, &job_b).await;
        assert_eq!(
            launcher.fire_watch_calls.lock().unwrap().len(),
            1,
            "stale-snapshot replay must not reach fire_watch"
        );
        assert!(launcher.resume_calls.lock().unwrap().is_empty());
        assert_eq!(*launcher.launch_calls.lock().unwrap(), 0);
        let b_after = job_row(&store, &job_b.id).await;
        assert!(!b_after.enabled, "retired row must never be re-enabled");
        assert_eq!(b_after.next_fire_at, b_before.next_fire_at);
        assert_eq!(b_after.last_fired_at, b_before.last_fired_at);
    }

    /// Review F1(b) direct pin: `advance_job_after_attempt` ANDs the written
    /// `enabled` with the row's CURRENT DB value — a row disabled in the DB
    /// between snapshot and advance stays disabled, never re-enabled from
    /// the stale struct.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn advance_after_attempt_never_reenables_db_disabled_row() {
        let (store, _bus) = fixture();
        let job = mk_watch_job(Uuid::new_v4(), Uuid::new_v4(), true);
        store
            .lock()
            .await
            .insert_scheduled_job(&job)
            .expect("insert");
        // Something else (e.g. a coalesced delivery) disables the row
        // mid-fire.
        disable_watch_jobs(
            &store,
            std::slice::from_ref(&job.id),
            &WatchFireCapture::default(),
        )
        .await;

        // Advance from the STALE snapshot struct (`enabled: true`).
        advance_job_after_attempt(&store, &job).await;

        let row = job_row(&store, &job.id).await;
        assert!(!row.enabled, "advance must not re-enable a DB-disabled row");
    }

    /// T-7: an armed watch row fires via a freshly spawned scheduler's
    /// startup catch-up (restart survival — F-008/F-013).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn armed_watch_fires_after_scheduler_restart() {
        let (store, bus) = fixture();
        let job = mk_watch_job(Uuid::new_v4(), Uuid::new_v4(), true);
        store
            .lock()
            .await
            .insert_scheduled_job(&job)
            .expect("insert");
        let launcher = MockLauncher::new(vec![Ok(WatchFireOutcome::Delivered {
            session_id: Uuid::new_v4(),
            delivered_job_ids: vec![job.id],
            observed_job_versions: vec![],
        })]);

        // Fresh scheduler = restart shape; long poll interval so only the
        // startup catch-up can be the trigger.
        let handle = spawn_scheduler(
            Arc::clone(&store),
            Arc::clone(&bus),
            launcher.clone() as Arc<dyn SessionLauncher>,
            3600,
        );

        // Issue #12: a dispatched watch now stays armed pending confirmation, so
        // `enabled` is no longer the fired-signal. Observe the fire directly.
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        loop {
            if !launcher.fire_watch_calls.lock().unwrap().is_empty() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "startup catch-up did not fire the armed watch"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        assert_eq!(*launcher.fire_watch_calls.lock().unwrap(), vec![job.id]);
        let _ = handle.shutdown().await;
    }

    /// T-10: an `OnTerminal` job NEVER reaches the Fresh-launch fallthrough —
    /// across `Abandon`, launcher error, and `NotReady` the mock's `launch` is
    /// never called; Abandon disables, error keeps the row armed (advanced).
    /// A disabled row (post-coalesced-delivery trigger race) no-ops entirely.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn on_terminal_job_never_falls_through_to_fresh_launch() {
        let (store, bus) = fixture();
        let job = mk_watch_job(Uuid::new_v4(), Uuid::new_v4(), true);
        store
            .lock()
            .await
            .insert_scheduled_job(&job)
            .expect("insert");
        let launcher = MockLauncher::new(vec![
            Ok(WatchFireOutcome::NotReady),
            Err(DaemonError::Rpc("transient".into())),
            Ok(WatchFireOutcome::Abandon {
                reason: "watched session gone".into(),
            }),
        ]);
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();

        fire_job(&store, &bus, &dyn_launcher, &job).await; // NotReady
        assert!(job_row(&store, &job.id).await.enabled);

        fire_job(&store, &bus, &dyn_launcher, &job).await; // Err -> stays armed
        let after_err = job_row(&store, &job.id).await;
        assert!(after_err.enabled, "transient error must keep the row armed");

        fire_job(&store, &bus, &dyn_launcher, &job).await; // Abandon -> disabled
        let after_abandon = job_row(&store, &job.id).await;
        assert!(!after_abandon.enabled, "abandon must disable");

        // Disabled row: guard no-ops before fire_watch (idempotent firing).
        fire_job(&store, &bus, &dyn_launcher, &after_abandon).await;
        assert_eq!(launcher.fire_watch_calls.lock().unwrap().len(), 3);

        assert_eq!(
            *launcher.launch_calls.lock().unwrap(),
            0,
            "OnTerminal must never reach the fresh-launch leg"
        );
        assert!(
            launcher.resume_calls.lock().unwrap().is_empty(),
            "OnTerminal must never reach the Resume leg"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn closed_guard_malformed_program_guard_never_executes_by_id_or_due_path() {
        use crate::session::harness::tools::schedule_wake::{
            ScheduleWakeRequest, build_agent_scheduled_job,
        };

        let (store, bus) = fixture();
        let session_id = Uuid::new_v4();
        store
            .lock()
            .await
            .insert_session(&mk_origin_session(session_id, false))
            .expect("insert owning session");
        let mut job = build_agent_scheduled_job(ScheduleWakeRequest {
            message: "closed program identity".into(),
            in_seconds: None,
            at: None,
            name: None,
            every_seconds: None,
            mode: Some("program_guard".into()),
            working_dir: std::path::PathBuf::from("/tmp/closed-program-guard"),
            provider: None,
            model: None,
            project_id: None,
            origin_session_id: Some(session_id),
            watch_session_id: None,
        })
        .expect("build exact program sentinel");
        job.enabled = false;
        store
            .lock()
            .await
            .insert_scheduled_job(&job)
            .expect("insert closed sentinel");
        let launcher = MockLauncher::new(Vec::new());
        let dyn_launcher: Arc<dyn SessionLauncher> = launcher.clone();

        let poisoned_at = Utc::now() - chrono::Duration::minutes(5);
        let poisoned_schedule = ScheduleSpec {
            recurrence: Recurrence::EverySeconds(1),
            anchor: poisoned_at,
        };
        {
            let guard = store.lock().await;
            guard
                .conn
                .execute(
                    "UPDATE scheduled_jobs
                     SET schedule_json=?1,next_fire_at=?2,wake_mode='fresh',wake_session_id=?3
                     WHERE id=?4",
                    rusqlite::params![
                        serde_json::to_string(&poisoned_schedule).unwrap(),
                        poisoned_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                        Uuid::new_v4().to_string(),
                        job.id.to_string(),
                    ],
                )
                .expect("poison every mutable identity field");
            assert_eq!(
                guard.program_guard_owner_for_job_id(&job.id).unwrap(),
                Some(session_id)
            );
        }

        fire_job_by_id(&store, &bus, &dyn_launcher, &job.id).await;

        assert!(
            launcher.resume_calls.lock().unwrap().is_empty(),
            "closed program sentinel bypassed explicit registration"
        );
        assert!(!job_row(&store, &job.id).await.enabled);

        store
            .lock()
            .await
            .update_scheduled_job(
                &job.id,
                &crate::store::scheduled_jobs::ScheduledJobUpdate {
                    name: None,
                    message: None,
                    schedule: None,
                    enabled: Some(true),
                    next_fire_at: None,
                },
            )
            .expect("emulate legacy generic rearm");
        fire_job_by_id(&store, &bus, &dyn_launcher, &job.id).await;
        process_due_jobs(&store, &bus, &dyn_launcher, None).await;

        assert!(launcher.resume_calls.lock().unwrap().is_empty());
        assert!(launcher.scheduled_fresh_calls.lock().unwrap().is_empty());
        assert!(launcher.fire_watch_calls.lock().unwrap().is_empty());
        assert_eq!(*launcher.launch_calls.lock().unwrap(), 0);
        let guard = store.lock().await;
        assert_eq!(guard.list_scheduled_jobs().unwrap().len(), 1);
        assert!(guard.list_issues(&Default::default()).unwrap().is_empty());
    }
}
