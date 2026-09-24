//! Dreamer scheduler: background task that monitors idle conditions and
//! orchestrates Dream consolidation cycles with durable checkpoints.

use crate::bus::{DaemonEvent, EventBus};
use crate::config::RuntimeConfig;
use crate::dreamer::deduction::{DEDUCTION_SYSTEM_PROMPT, DeductionResult, DeductionSpecialist};
use crate::dreamer::extractor::{
    EXTRACTION_SYSTEM_PROMPT, build_dream_extraction_prompt, parse_dream_extraction_response,
};
use crate::dreamer::induction::{INDUCTION_SYSTEM_PROMPT, InductionResult, InductionSpecialist};
use crate::dreamer::llm_client::{DreamCallContext, DreamCallSuccess, DreamerLlmClient};
use crate::dreamer::state::{
    DreamCaps, DreamCheckpoint, DreamConsumption, DreamPhase, DreamProgress, DreamRunState,
    DreamRunStatus, DreamRuntimeState, DreamStatusSnapshot, load as load_state, save as save_state,
    snapshot as build_snapshot,
};
use crate::error::{DaemonError, Result};
use crate::model_control::AdmissionDecision;
use crate::session::types::TrackedSession;
use crate::store::Store;
use chrono::{DateTime, Duration, Utc};
use rsi_common::model_control::ModelControlMode;
use rsi_common::model_control::ModelUsageConfidence;
use rsi_common::types::{Observation, ObservationLevel};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Configuration for the dreamer scheduler.
pub struct DreamConfig {
    pub observation_threshold: u64,
    pub idle_secs: u64,
    pub cooldown_secs: u64,
    pub batch_size: usize,
    pub poll_interval_secs: u64,
    pub max_model_calls: u32,
    pub max_estimated_input_tokens: u64,
    pub max_estimated_output_tokens: u64,
    pub max_estimated_total_tokens: u64,
    pub max_wall_time_ms: u64,
}

enum DreamerCommand {
    TriggerNow {
        reply: oneshot::Sender<Result<DreamStatusSnapshot>>,
    },
    NotifyControlChange,
    Shutdown {
        reply: oneshot::Sender<Result<()>>,
    },
}

#[derive(Clone)]
pub struct DreamerHandle {
    tx: mpsc::Sender<DreamerCommand>,
    status: Arc<RwLock<DreamStatusSnapshot>>,
    active_cancellations: Arc<Mutex<HashMap<Uuid, CancellationToken>>>,
}

impl DreamerHandle {
    fn new(
        tx: mpsc::Sender<DreamerCommand>,
        status: Arc<RwLock<DreamStatusSnapshot>>,
        active_cancellations: Arc<Mutex<HashMap<Uuid, CancellationToken>>>,
    ) -> Self {
        Self {
            tx,
            status,
            active_cancellations,
        }
    }

    pub async fn trigger_now(&self) -> Result<DreamStatusSnapshot> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DreamerCommand::TriggerNow { reply: reply_tx })
            .await
            .map_err(|_| crate::error::DaemonError::ChannelClosed)?;
        reply_rx
            .await
            .map_err(|_| crate::error::DaemonError::ChannelClosed)?
    }

    pub async fn shutdown(&self) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(DreamerCommand::Shutdown { reply: reply_tx })
            .await
            .map_err(|_| crate::error::DaemonError::ChannelClosed)?;
        reply_rx
            .await
            .map_err(|_| crate::error::DaemonError::ChannelClosed)?
    }

    pub async fn status(&self) -> DreamStatusSnapshot {
        self.status.read().await.clone()
    }

    pub async fn notify_control_change(&self) -> Result<()> {
        self.tx
            .send(DreamerCommand::NotifyControlChange)
            .await
            .map_err(|_| crate::error::DaemonError::ChannelClosed)
    }

    pub async fn cancel_invocation(&self, invocation_id: Uuid) -> bool {
        let cancel = {
            self.active_cancellations
                .lock()
                .await
                .get(&invocation_id)
                .cloned()
        };
        if let Some(cancel) = cancel {
            cancel.cancel();
            true
        } else {
            false
        }
    }
}

#[derive(Clone)]
struct DreamClock {
    inner: Arc<DreamClockInner>,
}

enum DreamClockInner {
    Live {
        base_wall: DateTime<Utc>,
        base_instant: std::time::Instant,
    },
    #[cfg(test)]
    Manual(std::sync::Mutex<DateTime<Utc>>),
}

#[cfg(test)]
#[derive(Clone)]
struct DreamClockControl {
    inner: Arc<DreamClockInner>,
}

impl DreamClock {
    fn new() -> Self {
        Self {
            inner: Arc::new(DreamClockInner::Live {
                base_wall: Utc::now(),
                base_instant: std::time::Instant::now(),
            }),
        }
    }

    fn now(&self) -> DateTime<Utc> {
        match &*self.inner {
            DreamClockInner::Live {
                base_wall,
                base_instant,
            } => {
                let elapsed = std::time::Instant::now().duration_since(*base_instant);
                *base_wall + Duration::from_std(elapsed).unwrap_or_else(|_| Duration::zero())
            }
            #[cfg(test)]
            DreamClockInner::Manual(now) => *now.lock().expect("manual dream clock poisoned"),
        }
    }

    #[cfg(test)]
    fn manual(start: DateTime<Utc>) -> (Self, DreamClockControl) {
        let inner = Arc::new(DreamClockInner::Manual(std::sync::Mutex::new(start)));
        (
            Self {
                inner: Arc::clone(&inner),
            },
            DreamClockControl { inner },
        )
    }
}

#[cfg(test)]
impl DreamClockControl {
    fn advance_ms(&self, ms: i64) {
        if let DreamClockInner::Manual(now) = &*self.inner {
            let mut guard = now.lock().expect("manual dream clock poisoned");
            *guard += Duration::milliseconds(ms);
        }
    }
}

struct RunningPhaseCall {
    invocation_id: Uuid,
    cancel: CancellationToken,
    phase: DreamPhase,
    item_key: String,
    input_estimate: u64,
    _control_registration: crate::model_control::CancellationRegistration,
    rx: oneshot::Receiver<PhaseCallOutcome>,
}

enum PhaseCallOutcome {
    Success {
        raw_response: String,
        completion: crate::model_control::InvocationCompletion,
        result_hash: String,
    },
    Paused {
        reason: String,
        wall_time_ms: u64,
    },
    Failed {
        reason: String,
        wall_time_ms: u64,
    },
}

pub fn spawn_dreamer(
    store: Arc<Mutex<Store>>,
    event_bus: Arc<EventBus>,
    llm: DreamerLlmClient,
    config: DreamConfig,
    active_sessions: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    runtime_config: Arc<RuntimeConfig>,
) -> DreamerHandle {
    spawn_dreamer_with_model_control_runtime(
        store,
        event_bus,
        llm,
        config,
        active_sessions,
        runtime_config,
        crate::model_control::ModelControlRuntime::default_normal(),
    )
}

pub fn spawn_dreamer_with_model_control_runtime(
    store: Arc<Mutex<Store>>,
    event_bus: Arc<EventBus>,
    llm: DreamerLlmClient,
    config: DreamConfig,
    active_sessions: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    runtime_config: Arc<RuntimeConfig>,
    model_control_runtime: crate::model_control::ModelControlRuntime,
) -> DreamerHandle {
    spawn_dreamer_with_clock_and_runtime(
        store,
        event_bus,
        llm,
        config,
        active_sessions,
        runtime_config,
        model_control_runtime,
        DreamClock::new(),
    )
}

fn spawn_dreamer_with_clock(
    store: Arc<Mutex<Store>>,
    event_bus: Arc<EventBus>,
    llm: DreamerLlmClient,
    config: DreamConfig,
    active_sessions: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    runtime_config: Arc<RuntimeConfig>,
    clock: DreamClock,
) -> DreamerHandle {
    spawn_dreamer_with_clock_and_runtime(
        store,
        event_bus,
        llm,
        config,
        active_sessions,
        runtime_config,
        crate::model_control::ModelControlRuntime::default_normal(),
        clock,
    )
}

fn spawn_dreamer_with_clock_and_runtime(
    store: Arc<Mutex<Store>>,
    event_bus: Arc<EventBus>,
    llm: DreamerLlmClient,
    config: DreamConfig,
    active_sessions: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    runtime_config: Arc<RuntimeConfig>,
    model_control_runtime: crate::model_control::ModelControlRuntime,
    clock: DreamClock,
) -> DreamerHandle {
    let (tx, mut cmd_rx) = mpsc::channel(8);
    let active_cancellations = Arc::new(Mutex::new(HashMap::new()));
    let status = Arc::new(RwLock::new(DreamStatusSnapshot {
        enabled: runtime_config.dream_enabled.load(Ordering::Relaxed),
        active_run_id: None,
        active_owner: None,
        status: None,
        phase: None,
        progress: DreamProgress::default(),
        last_success_at: None,
        cooldown_until: None,
        recent_consumption: DreamConsumption::default(),
        reason: Some("initializing".to_string()),
    }));
    let handle = DreamerHandle::new(tx, Arc::clone(&status), Arc::clone(&active_cancellations));

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            config.poll_interval_secs.max(1),
        ));
        let mut event_rx = event_bus.subscribe();
        let mut model_control_rx = model_control_runtime.subscribe();
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;

        let mut llm = llm;
        let mut running_call: Option<RunningPhaseCall> = None;
        let mut manual_trigger_requested = false;
        let mut shutting_down = false;
        let mut state = match {
            let guard = store.lock().await;
            load_state(&guard)
        } {
            Ok(mut loaded) => {
                if let Some(run) = loaded.active_run.as_mut() {
                    run.status = DreamRunStatus::Paused;
                    run.pause_reason = Some("restart_resume".to_string());
                    run.updated_at = clock
                        .now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
                }
                loaded
            }
            Err(error) => {
                tracing::error!(error = %error, "failed to load dream state");
                // A corrupt durable checkpoint is not permission to restart
                // Dream from defaults: that can replay paid work. Preserve the
                // record for repair and disable this worker before any call.
                runtime_config.dream_enabled.store(false, Ordering::Relaxed);
                let reason = format!("Dream disabled: durable state is unreadable: {error}");
                *status.write().await = DreamStatusSnapshot {
                    enabled: false,
                    active_run_id: None,
                    active_owner: None,
                    status: None,
                    phase: None,
                    progress: DreamProgress::default(),
                    last_success_at: None,
                    cooldown_until: None,
                    recent_consumption: DreamConsumption::default(),
                    reason: Some(reason),
                };
                return;
            }
        };
        if let Err(error) =
            persist_state(&store, &state, &status, &runtime_config, &clock, None).await
        {
            tracing::error!(error = %error, "failed to persist initial dream state");
            runtime_config.dream_enabled.store(false, Ordering::Relaxed);
            *status.write().await = DreamStatusSnapshot {
                enabled: false,
                active_run_id: None,
                active_owner: None,
                status: None,
                phase: None,
                progress: DreamProgress::default(),
                last_success_at: None,
                cooldown_until: None,
                recent_consumption: DreamConsumption::default(),
                reason: Some(format!(
                    "Dream disabled: initial state persistence failed: {error}"
                )),
            };
            return;
        }

        let mut immediate_drive = true;
        loop {
            refresh_llm_from_runtime(&mut llm, &store, &runtime_config).await;
            let control = match compute_control_state(
                &store,
                &active_sessions,
                &runtime_config,
                &llm,
                &clock,
            )
            .await
            {
                Ok(control) => control,
                Err(error) => {
                    tracing::error!(error = %error, "failed to compute dream control state");
                    DreamControlState::default()
                }
            };
            let snapshot_reason = current_snapshot_reason(&state, &control);
            if let Err(error) = persist_state(
                &store,
                &state,
                &status,
                &runtime_config,
                &clock,
                snapshot_reason,
            )
            .await
            {
                tracing::error!(error = %error, "failed to persist dream status");
            }

            if let Some(call) = running_call.as_ref()
                && should_pause_active_run(&control)
            {
                call.cancel.cancel();
            }

            let mut recompute = false;
            if immediate_drive && running_call.is_none() {
                immediate_drive = false;
            } else if let Some(call) = running_call.as_mut() {
                tokio::select! {
                    _ = interval.tick() => {}
                    cmd = cmd_rx.recv() => {
                        let (keep_running, cmd_recompute, skip_drive) = handle_command(
                            cmd,
                            &runtime_config,
                            &control,
                            &status,
                            &mut manual_trigger_requested,
                            &mut shutting_down,
                        ).await;
                        if !keep_running {
                            break;
                        }
                        if skip_drive {
                            continue;
                        }
                        recompute = cmd_recompute;
                    }
                    event = event_rx.recv() => {
                        if is_control_nudge_event(event.as_ref().ok().map(|value| &**value)) {
                            recompute = true;
                        }
                    }
                    changed = model_control_rx.changed() => {
                        if changed.is_ok() {
                            recompute = true;
                        }
                    }
                    outcome = &mut call.rx => {
                        let finished = running_call.take().expect("call present");
                        let outcome = outcome.unwrap_or(PhaseCallOutcome::Paused {
                            reason: "dream task dropped".to_string(),
                            wall_time_ms: 0,
                        });
                        if let Err(error) = finalize_phase_call(
                            &store,
                            &event_bus,
                            &clock,
                            &mut state,
                            finished,
                            outcome,
                            &active_cancellations,
                        ).await {
                            tracing::error!(error = %error, "failed to finalize dream phase");
                        }
                        recompute = true;
                    }
                }
            } else {
                tokio::select! {
                    _ = interval.tick() => {}
                    cmd = cmd_rx.recv() => {
                        let (keep_running, cmd_recompute, skip_drive) = handle_command(
                            cmd,
                            &runtime_config,
                            &control,
                            &status,
                            &mut manual_trigger_requested,
                            &mut shutting_down,
                        ).await;
                        if !keep_running {
                            break;
                        }
                        if skip_drive {
                            continue;
                        }
                        recompute = cmd_recompute;
                    }
                    event = event_rx.recv() => {
                        if is_control_nudge_event(event.as_ref().ok().map(|value| &**value)) {
                            recompute = true;
                        }
                    }
                    changed = model_control_rx.changed() => {
                        if changed.is_ok() {
                            recompute = true;
                        }
                    }
                }
            }

            if recompute {
                if running_call.is_none() {
                    immediate_drive = true;
                }
                continue;
            }

            while !shutting_down && running_call.is_none() {
                immediate_drive = false;
                match drive_once(
                    &store,
                    &event_bus,
                    &clock,
                    &llm,
                    &config,
                    &control,
                    &mut state,
                    &mut manual_trigger_requested,
                    &active_sessions,
                    &runtime_config,
                    &active_cancellations,
                    &model_control_runtime,
                )
                .await
                {
                    Ok(DriveResult::NoProgress) => break,
                    Ok(DriveResult::Spawned(call)) => {
                        running_call = Some(call);
                        break;
                    }
                    Ok(DriveResult::Progressed) => {}
                    Err(error) => {
                        tracing::error!(error = %error, "dream drive failed");
                        fail_active_run(&clock, &mut state, error.to_string());
                        if let Err(persist_error) =
                            persist_state_no_reason(&store, &state, &clock).await
                        {
                            tracing::error!(error = %persist_error, "failed to persist terminal Dream failure");
                        }
                        break;
                    }
                }
            }
        }
    });

    handle
}

#[derive(Default)]
struct DreamControlState {
    enabled: bool,
    current_mode: Option<ModelControlMode>,
    has_active_foreground: bool,
    idle_blocked_until: Option<DateTime<Utc>>,
    paid_route_blocked: bool,
}

async fn refresh_llm_from_runtime(
    llm: &mut DreamerLlmClient,
    store: &Arc<Mutex<Store>>,
    runtime_config: &Arc<RuntimeConfig>,
) {
    let current_model = runtime_config.dream_model.read().clone();
    let current_provider = *runtime_config.dream_model_provider.read();
    let current_base_url = runtime_config.dream_model_base_url.read().clone();
    let current_api_key = runtime_config.dream_model_api_key.read().clone();
    let current_api_url = current_base_url.unwrap_or_default();
    if current_model != llm.model
        || current_provider != llm.provider
        || current_api_url != llm.api_url
        || current_api_key != llm.api_key
    {
        *llm = DreamerLlmClient::new(
            Arc::clone(store),
            Arc::clone(&llm.event_bus),
            current_provider,
            current_api_url,
            current_api_key,
            current_model,
        );
    }
}

async fn compute_control_state(
    store: &Arc<Mutex<Store>>,
    active_sessions: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    runtime_config: &Arc<RuntimeConfig>,
    llm: &DreamerLlmClient,
    clock: &DreamClock,
) -> Result<DreamControlState> {
    let enabled = runtime_config.dream_enabled.load(Ordering::Relaxed);
    let has_active_foreground = !active_sessions.read().await.is_empty();
    let idle_secs = runtime_config.dream_idle_secs.load(Ordering::Relaxed);
    let latest_activity = {
        let guard = store.lock().await;
        guard.latest_session_activity_at()?
    };
    let idle_blocked_until = latest_activity.map(|ts| ts + Duration::seconds(idle_secs as i64));
    let current_mode = {
        let guard = store.lock().await;
        Some(guard.current_model_control_mode()?)
    };
    let target = llm.target();
    let is_local_route = crate::memory::llm::provider_label(&target)? == "Local";
    let paid_route_blocked = matches!(
        current_mode,
        Some(ModelControlMode::DenyPaid | ModelControlMode::LocalOnly)
    ) && !is_local_route;

    let _ = clock;
    Ok(DreamControlState {
        enabled,
        current_mode,
        has_active_foreground,
        idle_blocked_until,
        paid_route_blocked,
    })
}

fn current_snapshot_reason(
    state: &DreamRuntimeState,
    control: &DreamControlState,
) -> Option<String> {
    if !control.enabled {
        return Some("disabled".to_string());
    }
    if let Some(mode) = control.current_mode {
        match mode {
            ModelControlMode::PauseBackground => {
                return Some("pause_background".to_string());
            }
            ModelControlMode::StopAll => return Some("stop_all".to_string()),
            ModelControlMode::DenyPaid if control.paid_route_blocked => {
                return Some("deny_paid".to_string());
            }
            ModelControlMode::LocalOnly if control.paid_route_blocked => {
                return Some("local_only".to_string());
            }
            _ => {}
        }
    }
    if control.has_active_foreground {
        return Some("foreground_active".to_string());
    }
    state
        .active_run
        .as_ref()
        .and_then(|run| run.pause_reason.clone())
}

fn should_pause_active_run(control: &DreamControlState) -> bool {
    !control.enabled
        || control.has_active_foreground
        || matches!(
            control.current_mode,
            Some(ModelControlMode::PauseBackground | ModelControlMode::StopAll)
        )
        || control.paid_route_blocked
}

async fn handle_command(
    cmd: Option<DreamerCommand>,
    runtime_config: &Arc<RuntimeConfig>,
    control: &DreamControlState,
    status: &Arc<RwLock<DreamStatusSnapshot>>,
    manual_trigger_requested: &mut bool,
    shutting_down: &mut bool,
) -> (bool, bool, bool) {
    match cmd {
        Some(DreamerCommand::TriggerNow { reply }) => {
            let result = if !runtime_config.dream_enabled.load(Ordering::Relaxed) {
                Err(DaemonError::PolicyDenied(
                    "dream is disabled by runtime config".to_string(),
                ))
            } else if control.has_active_foreground {
                Err(DaemonError::PolicyDenied(
                    "foreground work is active; Dream is suppressed".to_string(),
                ))
            } else if matches!(
                control.current_mode,
                Some(ModelControlMode::PauseBackground | ModelControlMode::StopAll)
            ) {
                Err(DaemonError::PolicyDenied(
                    "model control currently blocks background Dream work".to_string(),
                ))
            } else if control.paid_route_blocked {
                Err(DaemonError::PolicyDenied(
                    "current Dream route is blocked by model-control policy".to_string(),
                ))
            } else {
                *manual_trigger_requested = true;
                Ok(status.read().await.clone())
            };
            let _ = reply.send(result);
            (true, false, true)
        }
        Some(DreamerCommand::NotifyControlChange) => (true, true, false),
        Some(DreamerCommand::Shutdown { reply }) => {
            *shutting_down = true;
            let _ = reply.send(Ok(()));
            (false, false, false)
        }
        None => (false, false, false),
    }
}

enum DriveResult {
    NoProgress,
    Progressed,
    Spawned(RunningPhaseCall),
}

fn is_control_nudge_event(event: Option<&DaemonEvent>) -> bool {
    matches!(
        event,
        Some(
            DaemonEvent::SessionCreated { .. }
                | DaemonEvent::SessionStatusChanged { .. }
                | DaemonEvent::SessionDeleted { .. }
                | DaemonEvent::SessionArchived { .. }
                | DaemonEvent::SessionUnarchived { .. }
                | DaemonEvent::SessionReconciled { .. }
        )
    )
}

async fn drive_once(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    clock: &DreamClock,
    llm: &DreamerLlmClient,
    config: &DreamConfig,
    control: &DreamControlState,
    state: &mut DreamRuntimeState,
    manual_trigger_requested: &mut bool,
    active_sessions: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    runtime_config: &Arc<RuntimeConfig>,
    active_cancellations: &Arc<Mutex<HashMap<Uuid, CancellationToken>>>,
    model_control_runtime: &crate::model_control::ModelControlRuntime,
) -> Result<DriveResult> {
    let pause_reason = current_snapshot_reason(state, control);
    if let Some(run) = state.active_run.as_mut() {
        // Recovery only writes local durable bookkeeping.  It must finish
        // before mode/idle/wall-time policy can block fresh provider work;
        // otherwise a restart under StopAll can retain an active ledger row
        // forever even though no further model call is permitted.
        if run.status == DreamRunStatus::Paused
            && run.pause_reason.as_deref() == Some("restart_resume")
            && run.checkpoint.current_response.is_none()
            && run.checkpoint.current_invocation_id.is_some()
        {
            reconcile_uncheckpointed_invocation(store, clock, state, event_bus).await?;
            persist_state_no_reason(store, state, clock).await?;
            return Ok(DriveResult::Progressed);
        }

        if run.status == DreamRunStatus::Paused
            && run.pause_reason.as_deref() == Some("restart_resume")
            && run.checkpoint.current_response.is_some()
            && !run.checkpoint.current_settled
        {
            settle_checkpointed_response(store, state, event_bus).await?;
            persist_state_no_reason(store, state, clock).await?;
            return Ok(DriveResult::Progressed);
        }

        if should_pause_active_run(control) {
            if run.status != DreamRunStatus::Paused {
                run.status = DreamRunStatus::Paused;
                run.pause_reason = pause_reason.clone();
                run.updated_at = clock
                    .now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
                persist_state_no_reason(store, state, clock).await?;
                return Ok(DriveResult::Progressed);
            }
            return Ok(DriveResult::NoProgress);
        }

        if run.status == DreamRunStatus::Paused {
            run.status = DreamRunStatus::Running;
            run.pause_reason = None;
            run.updated_at = clock
                .now()
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            event_bus.publish(DaemonEvent::DreamStarted);
            persist_state_no_reason(store, state, clock).await?;
            return Ok(DriveResult::Progressed);
        }

        enforce_wall_time_cap(run, clock)?;

        if advance_phase_if_needed(store, clock, run).await? {
            persist_state_no_reason(store, state, clock).await?;
            return Ok(DriveResult::Progressed);
        }

        if run.phase == DreamPhase::Induction
            && run.checkpoint.induction_index >= run.checkpoint.induction_project_ids.len()
        {
            complete_active_run(state, clock, event_bus);
            persist_state_no_reason(store, state, clock).await?;
            return Ok(DriveResult::Progressed);
        }

        if let Some(raw_response) = run.checkpoint.current_response.clone() {
            apply_current_response(store, clock, state, raw_response).await?;
            persist_state_no_reason(store, state, clock).await?;
            return Ok(DriveResult::Progressed);
        }

        let fresh_control =
            compute_control_state(store, active_sessions, runtime_config, llm, clock).await?;
        if should_pause_active_run(&fresh_control) {
            return Ok(DriveResult::NoProgress);
        }
        let prepared = prepare_current_phase_call(
            store,
            clock,
            llm,
            state,
            active_cancellations,
            model_control_runtime,
        )
        .await?;
        match prepared {
            PreparedPhaseCall::Skip => {
                advance_current_phase_index(state, clock);
                persist_state_no_reason(store, state, clock).await?;
                Ok(DriveResult::Progressed)
            }
            PreparedPhaseCall::Spawn(call) => Ok(DriveResult::Spawned(call)),
        }
    } else {
        let now = clock.now();
        let manual = std::mem::take(manual_trigger_requested);
        if !control.enabled {
            return Ok(DriveResult::NoProgress);
        }
        if control.has_active_foreground {
            return Ok(DriveResult::NoProgress);
        }
        if control_blocks_new_run(control) {
            return Ok(DriveResult::NoProgress);
        }
        if !manual && control.idle_blocked_until.is_some_and(|ts| ts > now) {
            return Ok(DriveResult::NoProgress);
        }
        if !manual
            && state
                .cooldown_until
                .as_deref()
                .is_some_and(|raw| parse_timestamp(raw).map(|ts| ts > now).unwrap_or(false))
        {
            return Ok(DriveResult::NoProgress);
        }

        let pending_sessions = {
            let guard = store.lock().await;
            guard.sessions_needing_extraction(config.batch_size)?
        };
        let pending_projects = {
            let guard = store.lock().await;
            guard.list_observations_by_level(ObservationLevel::Explicit, 500)?
        };
        if !manual {
            let since = state
                .last_success_at
                .as_deref()
                .and_then(parse_timestamp)
                .unwrap_or_else(|| now - Duration::days(365));
            let effective_count = {
                let guard = store.lock().await;
                guard.count_observations_since(since)?
            } + (pending_sessions.len() as u64 * 5);
            if effective_count < config.observation_threshold {
                return Ok(DriveResult::NoProgress);
            }
        }

        if pending_sessions.is_empty() && pending_projects.is_empty() {
            return Ok(DriveResult::NoProgress);
        }

        let extract_count = pending_sessions.len() as u64;
        let cooldown_secs = config.cooldown_secs;
        state.active_run = Some(DreamRunState {
            run_id: Uuid::new_v4(),
            owner: format!("dream:{}", Uuid::new_v4()),
            trigger: if manual {
                "manual".to_string()
            } else {
                "automatic".to_string()
            },
            status: DreamRunStatus::Running,
            phase: DreamPhase::Extraction,
            checkpoint: DreamCheckpoint {
                extract_session_ids: pending_sessions,
                ..DreamCheckpoint::default()
            },
            progress: DreamProgress {
                total_items: extract_count,
                ..DreamProgress::default()
            },
            consumption: DreamConsumption::default(),
            caps: DreamCaps {
                max_items_per_phase: config.batch_size,
                max_model_calls: config.max_model_calls,
                max_estimated_input_tokens: config.max_estimated_input_tokens,
                max_estimated_output_tokens: config.max_estimated_output_tokens,
                max_estimated_total_tokens: config.max_estimated_total_tokens,
                max_wall_time_ms: config.max_wall_time_ms,
                max_concurrency: 1,
                cooldown_secs,
            },
            started_at: now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            updated_at: now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            completed_at: None,
            terminal_reason: None,
            pause_reason: None,
        });
        event_bus.publish(DaemonEvent::DreamStarted);
        Ok(DriveResult::Progressed)
    }
}

enum PreparedPhaseCall {
    Skip,
    Spawn(RunningPhaseCall),
}

async fn prepare_current_phase_call(
    store: &Arc<Mutex<Store>>,
    clock: &DreamClock,
    llm: &DreamerLlmClient,
    state: &mut DreamRuntimeState,
    active_cancellations: &Arc<Mutex<HashMap<Uuid, CancellationToken>>>,
    model_control_runtime: &crate::model_control::ModelControlRuntime,
) -> Result<PreparedPhaseCall> {
    let (run_id, phase, owner, item_key, system_prompt, user_prompt) = {
        let run = state
            .active_run
            .as_mut()
            .ok_or_else(|| DaemonError::Store("missing active Dream run".to_string()))?;
        let (item_key, system_prompt, user_prompt) = match run.phase {
            DreamPhase::Extraction => {
                let session_id = *run
                    .checkpoint
                    .extract_session_ids
                    .get(run.checkpoint.extract_index)
                    .ok_or_else(|| DaemonError::Store("missing extraction session".to_string()))?;
                let (events, query) = {
                    let guard = store.lock().await;
                    let events = guard.load_events(session_id)?;
                    let query = guard
                        .get_session(session_id)?
                        .as_ref()
                        .map(|session| session.query.clone())
                        .unwrap_or_default();
                    (events, query)
                };
                let Some(prompt) = build_dream_extraction_prompt(&events, &query, 4, 16_000) else {
                    return Ok(PreparedPhaseCall::Skip);
                };
                (
                    session_id.to_string(),
                    EXTRACTION_SYSTEM_PROMPT.to_string(),
                    prompt,
                )
            }
            DreamPhase::Deduction => {
                let project_id = run
                    .checkpoint
                    .deduction_project_ids
                    .get(run.checkpoint.deduction_index)
                    .cloned()
                    .ok_or_else(|| DaemonError::Store("missing deduction project".to_string()))?;
                let observations = load_explicit_project_observations(store, project_id).await?;
                if observations.is_empty() {
                    return Ok(PreparedPhaseCall::Skip);
                }
                (
                    project_key(project_id),
                    DEDUCTION_SYSTEM_PROMPT.to_string(),
                    DeductionSpecialist::build_prompt(&observations),
                )
            }
            DreamPhase::Induction => {
                let project_id = run
                    .checkpoint
                    .induction_project_ids
                    .get(run.checkpoint.induction_index)
                    .cloned()
                    .ok_or_else(|| DaemonError::Store("missing induction project".to_string()))?;
                let observations = load_all_project_observations(store, project_id).await?;
                if observations.len() < 3 {
                    return Ok(PreparedPhaseCall::Skip);
                }
                (
                    project_key(project_id),
                    INDUCTION_SYSTEM_PROMPT.to_string(),
                    InductionSpecialist::build_prompt(&observations),
                )
            }
        };
        (
            run.run_id,
            run.phase,
            run.owner.clone(),
            item_key,
            system_prompt,
            user_prompt,
        )
    };

    let input_estimate = estimate_tokens(&format!("{system_prompt}\n\n{user_prompt}"));
    {
        let run = state
            .active_run
            .as_mut()
            .ok_or_else(|| DaemonError::Store("missing active Dream run".to_string()))?;
        enforce_run_caps(run, input_estimate)?;
    }
    let dedup_key = crate::model_control::stable_dedup_key(
        "dream-phase",
        &[
            run_id.to_string().as_str(),
            phase.as_str(),
            item_key.as_str(),
            llm.model.as_str(),
            llm.api_url.as_str(),
        ],
    );
    let request_fingerprint =
        llm.request_fingerprint_for_prompts(phase, &item_key, &system_prompt, &user_prompt, run_id);
    let cancel = CancellationToken::new();
    let context = DreamCallContext {
        run_id,
        phase,
        item_key: item_key.clone(),
        owner,
        dedup_key: dedup_key.clone(),
        request_fingerprint: request_fingerprint.clone(),
        cancel: cancel.clone(),
    };
    let permit = match llm.admit_with_context(&context).await? {
        AdmissionDecision::Admitted(permit) => permit,
        AdmissionDecision::Duplicate { invocation_id } => {
            return Err(DaemonError::PolicyDenied(format!(
                "dream invocation {invocation_id} already exists without a recoverable checkpoint"
            )));
        }
    };
    {
        let run = state
            .active_run
            .as_mut()
            .ok_or_else(|| DaemonError::Store("missing active Dream run".to_string()))?;
        run.checkpoint.current_item_key = Some(item_key.clone());
        run.checkpoint.current_dedup_key = Some(dedup_key.clone());
        run.checkpoint.current_fingerprint = Some(request_fingerprint.clone());
        run.checkpoint.current_invocation_id = Some(permit.invocation_id());
        run.checkpoint.current_input_estimate = Some(input_estimate);
        run.checkpoint.current_output_estimate = None;
        run.checkpoint.current_wall_time_ms = None;
        run.checkpoint.current_result_hash = None;
        run.checkpoint.current_settled = false;
        run.checkpoint.current_consumption_recorded = false;
        run.updated_at = clock
            .now()
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    }
    persist_state_no_reason(store, state, clock).await?;

    let (tx, rx) = oneshot::channel();
    let llm = llm.clone();
    let permit_for_task = permit.clone();
    let context_for_task = context.clone();
    {
        active_cancellations
            .lock()
            .await
            .insert(permit.invocation_id(), cancel.clone());
    }
    let cancel_for_runtime = cancel.clone();
    let control_registration = model_control_runtime.register_cancellation(
        permit.invocation_id(),
        "dream_provider_call",
        Arc::new(move || cancel_for_runtime.cancel()),
    );
    tokio::spawn(async move {
        let call_started = std::time::Instant::now();
        let outcome = match llm
            .execute_with_permit(
                &permit_for_task,
                &context_for_task,
                &system_prompt,
                &user_prompt,
                4096,
                input_estimate,
            )
            .await
        {
            Ok(DreamCallSuccess {
                raw_response,
                completion,
                result_hash,
            }) => PhaseCallOutcome::Success {
                raw_response,
                completion,
                result_hash,
            },
            Err(DaemonError::ChannelClosed) => PhaseCallOutcome::Paused {
                reason: "cancelled".to_string(),
                wall_time_ms: call_started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            },
            Err(error) => PhaseCallOutcome::Failed {
                reason: error.to_string(),
                wall_time_ms: call_started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            },
        };
        let _ = tx.send(outcome);
    });

    Ok(PreparedPhaseCall::Spawn(RunningPhaseCall {
        invocation_id: permit.invocation_id(),
        cancel,
        phase,
        item_key,
        input_estimate,
        _control_registration: control_registration,
        rx,
    }))
}

fn enforce_run_caps(run: &DreamRunState, input_estimate: u64) -> Result<()> {
    let next_model_calls = run.consumption.model_calls.saturating_add(1);
    let next_input = run
        .consumption
        .estimated_input_tokens
        .saturating_add(input_estimate);
    let next_output = run.consumption.estimated_output_tokens.saturating_add(4096);
    let next_total = next_input.saturating_add(next_output);
    if next_model_calls > run.caps.max_model_calls {
        return Err(DaemonError::PolicyDenied(
            "Dream model-call cap exceeded before execution".to_string(),
        ));
    }
    if next_input > run.caps.max_estimated_input_tokens {
        return Err(DaemonError::PolicyDenied(
            "Dream input-token cap exceeded before execution".to_string(),
        ));
    }
    if next_output > run.caps.max_estimated_output_tokens {
        return Err(DaemonError::PolicyDenied(
            "Dream output-token cap exceeded before execution".to_string(),
        ));
    }
    if next_total > run.caps.max_estimated_total_tokens {
        return Err(DaemonError::PolicyDenied(
            "Dream total-token cap exceeded before execution".to_string(),
        ));
    }
    if run.caps.max_concurrency != 1 {
        return Err(DaemonError::PolicyDenied(
            "Dream concurrency must remain 1".to_string(),
        ));
    }
    Ok(())
}

fn control_blocks_new_run(control: &DreamControlState) -> bool {
    matches!(
        control.current_mode,
        Some(ModelControlMode::PauseBackground | ModelControlMode::StopAll)
    ) || control.paid_route_blocked
}

fn enforce_wall_time_cap(run: &DreamRunState, clock: &DreamClock) -> Result<()> {
    let started_at = parse_timestamp(&run.started_at).ok_or_else(|| {
        DaemonError::Store("Dream run started_at is not a valid RFC3339 timestamp".to_string())
    })?;
    let elapsed_ms = clock
        .now()
        .signed_duration_since(started_at)
        .num_milliseconds()
        .max(0) as u64;
    if elapsed_ms > run.caps.max_wall_time_ms {
        return Err(DaemonError::PolicyDenied(
            "Dream wall-time cap exceeded before execution".to_string(),
        ));
    }
    Ok(())
}

async fn settle_checkpointed_response(
    store: &Arc<Mutex<Store>>,
    state: &mut DreamRuntimeState,
    event_bus: &Arc<EventBus>,
) -> Result<()> {
    let run = state
        .active_run
        .as_mut()
        .ok_or_else(|| DaemonError::Store("missing Dream run".to_string()))?;
    let invocation_id = run
        .checkpoint
        .current_invocation_id
        .ok_or_else(|| DaemonError::Store("missing Dream invocation checkpoint".to_string()))?;
    let completion = crate::model_control::InvocationCompletion {
        input_tokens: run.checkpoint.current_input_estimate,
        output_tokens: run.checkpoint.current_output_estimate,
        wall_time_ms: run.checkpoint.current_wall_time_ms,
        confidence: Some(ModelUsageConfidence::Estimated),
        ..crate::model_control::InvocationCompletion::default()
    };
    crate::model_control::complete_invocation_by_id(store, invocation_id, completion, event_bus)
        .await?;
    run.checkpoint.current_settled = true;
    Ok(())
}

async fn reconcile_uncheckpointed_invocation(
    store: &Arc<Mutex<Store>>,
    clock: &DreamClock,
    state: &mut DreamRuntimeState,
    event_bus: &Arc<EventBus>,
) -> Result<()> {
    let invocation_id = state
        .active_run
        .as_ref()
        .and_then(|run| run.checkpoint.current_invocation_id)
        .ok_or_else(|| DaemonError::Store("missing Dream invocation checkpoint".to_string()))?;
    let completion = crate::model_control::InvocationCompletion {
        input_tokens: state
            .active_run
            .as_ref()
            .and_then(|run| run.checkpoint.current_input_estimate),
        error_class: Some("ambiguous".to_string()),
        confidence: Some(ModelUsageConfidence::Partial),
        ..crate::model_control::InvocationCompletion::default()
    };
    crate::model_control::complete_invocation_by_id(store, invocation_id, completion, event_bus)
        .await?;
    if let Some(run) = state.active_run.as_mut() {
        let input_tokens = run.checkpoint.current_input_estimate.unwrap_or_default();
        record_attempt_consumption(
            run,
            input_tokens,
            0,
            run.checkpoint.current_wall_time_ms.unwrap_or_default(),
            ModelUsageConfidence::Partial,
        );
    }
    fail_active_run(
        clock,
        state,
        format!(
            "Dream invocation {invocation_id} has no durable response checkpoint; refusing replay"
        ),
    );
    Ok(())
}

async fn finalize_phase_call(
    store: &Arc<Mutex<Store>>,
    _event_bus: &Arc<EventBus>,
    clock: &DreamClock,
    state: &mut DreamRuntimeState,
    call: RunningPhaseCall,
    outcome: PhaseCallOutcome,
    active_cancellations: &Arc<Mutex<HashMap<Uuid, CancellationToken>>>,
) -> Result<()> {
    active_cancellations
        .lock()
        .await
        .remove(&call.invocation_id);
    let Some(run) = state.active_run.as_mut() else {
        return Ok(());
    };
    match outcome {
        PhaseCallOutcome::Success {
            raw_response,
            completion,
            result_hash,
        } => {
            if call.phase != run.phase
                || Some(call.item_key.clone()) != run.checkpoint.current_item_key
            {
                return Err(DaemonError::Store(
                    "dream phase result no longer matches active checkpoint".to_string(),
                ));
            }
            record_attempt_consumption(
                run,
                completion.input_tokens.unwrap_or(call.input_estimate),
                completion.output_tokens.unwrap_or_default(),
                completion.wall_time_ms.unwrap_or_default(),
                ModelUsageConfidence::Estimated,
            );
            run.checkpoint.current_response = Some(raw_response);
            run.checkpoint.current_output_estimate = completion.output_tokens;
            run.checkpoint.current_wall_time_ms = completion.wall_time_ms;
            run.checkpoint.current_result_hash = Some(result_hash);
            run.checkpoint.current_settled = false;
            run.updated_at = clock
                .now()
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            persist_state_no_reason(store, state, clock).await
        }
        PhaseCallOutcome::Paused {
            reason,
            wall_time_ms,
        } => {
            record_attempt_consumption(
                run,
                call.input_estimate,
                0,
                wall_time_ms,
                ModelUsageConfidence::Partial,
            );
            run.status = DreamRunStatus::Paused;
            run.pause_reason = Some(reason);
            run.updated_at = clock
                .now()
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            persist_state_no_reason(store, state, clock).await
        }
        PhaseCallOutcome::Failed {
            reason,
            wall_time_ms,
        } => {
            record_attempt_consumption(
                run,
                call.input_estimate,
                0,
                wall_time_ms,
                ModelUsageConfidence::Partial,
            );
            fail_active_run(clock, state, reason);
            persist_state_no_reason(store, state, clock).await
        }
    }
}

fn record_attempt_consumption(
    run: &mut DreamRunState,
    input_tokens: u64,
    output_tokens: u64,
    wall_time_ms: u64,
    confidence: ModelUsageConfidence,
) {
    if run.checkpoint.current_consumption_recorded {
        return;
    }
    run.consumption.model_calls = run.consumption.model_calls.saturating_add(1);
    run.consumption.estimated_input_tokens = run
        .consumption
        .estimated_input_tokens
        .saturating_add(input_tokens);
    run.consumption.estimated_output_tokens = run
        .consumption
        .estimated_output_tokens
        .saturating_add(output_tokens);
    run.consumption.estimated_total_tokens = run
        .consumption
        .estimated_total_tokens
        .saturating_add(input_tokens)
        .saturating_add(output_tokens);
    run.consumption.wall_time_ms = run.consumption.wall_time_ms.saturating_add(wall_time_ms);
    run.consumption.confidence = confidence;
    run.checkpoint.current_consumption_recorded = true;
}

async fn apply_current_response(
    store: &Arc<Mutex<Store>>,
    clock: &DreamClock,
    state: &mut DreamRuntimeState,
    raw_response: String,
) -> Result<()> {
    let run = state
        .active_run
        .as_mut()
        .ok_or_else(|| DaemonError::Store("missing Dream run".to_string()))?;
    match run.phase {
        DreamPhase::Extraction => {
            let session_id = *run
                .checkpoint
                .extract_session_ids
                .get(run.checkpoint.extract_index)
                .ok_or_else(|| DaemonError::Store("missing extraction session".to_string()))?;
            let project_id = {
                let guard = store.lock().await;
                guard
                    .get_session(session_id)?
                    .and_then(|session| session.project_id)
            };
            let observations =
                parse_dream_extraction_response(session_id, project_id, &raw_response);
            if !observations.is_empty() {
                let guard = store.lock().await;
                let observations = observations
                    .into_iter()
                    .map(|observation| deterministic_explicit_observation(observation))
                    .collect::<Vec<_>>();
                guard.insert_observations_batch(&observations)?;
                run.progress.observations_extracted = run
                    .progress
                    .observations_extracted
                    .saturating_add(observations.len() as u64);
            }
            run.progress.sessions_processed = run.progress.sessions_processed.saturating_add(1);
            run.progress.completed_items = run.progress.completed_items.saturating_add(1);
            run.checkpoint.extract_index = run.checkpoint.extract_index.saturating_add(1);
        }
        DreamPhase::Deduction => {
            let project_id = run
                .checkpoint
                .deduction_project_ids
                .get(run.checkpoint.deduction_index)
                .cloned()
                .ok_or_else(|| DaemonError::Store("missing deduction project".to_string()))?;
            let observations = load_explicit_project_observations(store, project_id).await?;
            let result =
                DeductionSpecialist::parse_response(&raw_response, &observations, project_id);
            apply_deduction_result(store, project_id, &result).await?;
            run.progress.deductions_created = run
                .progress
                .deductions_created
                .saturating_add(result.new_observations.len() as u64);
            run.progress.observations_superseded = run
                .progress
                .observations_superseded
                .saturating_add(result.superseded_ids.len() as u64);
            run.progress.completed_items = run.progress.completed_items.saturating_add(1);
            run.checkpoint.deduction_index = run.checkpoint.deduction_index.saturating_add(1);
        }
        DreamPhase::Induction => {
            let project_id = run
                .checkpoint
                .induction_project_ids
                .get(run.checkpoint.induction_index)
                .cloned()
                .ok_or_else(|| DaemonError::Store("missing induction project".to_string()))?;
            let result = InductionSpecialist::parse_response(&raw_response, project_id);
            apply_induction_result(store, &result).await?;
            run.progress.patterns_identified = run
                .progress
                .patterns_identified
                .saturating_add(result.patterns.len() as u64);
            run.progress.completed_items = run.progress.completed_items.saturating_add(1);
            run.checkpoint.induction_index = run.checkpoint.induction_index.saturating_add(1);
        }
    }
    run.checkpoint.clear_current_call();
    run.updated_at = clock
        .now()
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    Ok(())
}

async fn advance_phase_if_needed(
    store: &Arc<Mutex<Store>>,
    clock: &DreamClock,
    run: &mut DreamRunState,
) -> Result<bool> {
    let changed = match run.phase {
        DreamPhase::Extraction
            if run.checkpoint.extract_index >= run.checkpoint.extract_session_ids.len() =>
        {
            run.phase = DreamPhase::Deduction;
            if run.checkpoint.deduction_project_ids.is_empty() {
                run.checkpoint.deduction_project_ids = collect_project_ids(
                    &load_observations_for_level(store, ObservationLevel::Explicit).await?,
                );
                if run.checkpoint.deduction_project_ids.len() > run.caps.max_items_per_phase {
                    return Err(DaemonError::PolicyDenied(
                        "Dream deduction project cap exceeded before execution".to_string(),
                    ));
                }
                run.progress.total_items = run
                    .progress
                    .total_items
                    .saturating_add(run.checkpoint.deduction_project_ids.len() as u64);
            }
            true
        }
        DreamPhase::Deduction
            if run.checkpoint.deduction_index >= run.checkpoint.deduction_project_ids.len() =>
        {
            run.phase = DreamPhase::Induction;
            if run.checkpoint.induction_project_ids.is_empty() {
                run.checkpoint.induction_project_ids =
                    collect_project_ids(&load_all_observations(store).await?);
                if run.checkpoint.induction_project_ids.len() > run.caps.max_items_per_phase {
                    return Err(DaemonError::PolicyDenied(
                        "Dream induction project cap exceeded before execution".to_string(),
                    ));
                }
                run.progress.total_items = run
                    .progress
                    .total_items
                    .saturating_add(run.checkpoint.induction_project_ids.len() as u64);
            }
            true
        }
        _ => false,
    };
    if changed {
        run.checkpoint.clear_current_call();
        run.updated_at = clock
            .now()
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    }
    Ok(changed)
}

fn complete_active_run(
    state: &mut DreamRuntimeState,
    clock: &DreamClock,
    event_bus: &Arc<EventBus>,
) {
    if let Some(mut run) = state.active_run.take() {
        let now = clock.now();
        run.status = DreamRunStatus::Completed;
        run.completed_at = Some(now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
        run.updated_at = run.completed_at.clone().unwrap_or_default();
        state.last_success_at = run.completed_at.clone();
        state.cooldown_until = Some(
            (now + Duration::seconds(run.caps.cooldown_secs as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        );
        state.last_terminal_reason = Some("completed".to_string());
        state.recent_consumption = run.consumption.clone();
        event_bus.publish(DaemonEvent::DreamCompleted {
            observations_extracted: run.progress.observations_extracted as usize,
            deductions_created: run.progress.deductions_created as usize,
            patterns_identified: run.progress.patterns_identified as usize,
        });
    }
}

fn fail_active_run(clock: &DreamClock, state: &mut DreamRuntimeState, reason: String) {
    if let Some(mut run) = state.active_run.take() {
        let now = clock
            .now()
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        run.status = DreamRunStatus::Failed;
        run.terminal_reason = Some(reason.clone());
        run.completed_at = Some(now.clone());
        run.updated_at = now;
        state.last_terminal_reason = Some(reason);
        state.recent_consumption = run.consumption;
    }
}

fn advance_current_phase_index(state: &mut DreamRuntimeState, clock: &DreamClock) {
    if let Some(run) = state.active_run.as_mut() {
        run.progress.completed_items = run.progress.completed_items.saturating_add(1);
        match run.phase {
            DreamPhase::Extraction => run.checkpoint.extract_index += 1,
            DreamPhase::Deduction => run.checkpoint.deduction_index += 1,
            DreamPhase::Induction => run.checkpoint.induction_index += 1,
        }
        run.updated_at = clock
            .now()
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        run.checkpoint.clear_current_call();
    }
}

async fn persist_state(
    store: &Arc<Mutex<Store>>,
    state: &DreamRuntimeState,
    status: &Arc<RwLock<DreamStatusSnapshot>>,
    runtime_config: &Arc<RuntimeConfig>,
    clock: &DreamClock,
    reason: Option<String>,
) -> Result<()> {
    let snapshot = build_snapshot(
        state,
        runtime_config.dream_enabled.load(Ordering::Relaxed),
        reason,
    );
    *status.write().await = snapshot;
    persist_state_no_reason(store, state, clock).await
}

async fn persist_state_no_reason(
    store: &Arc<Mutex<Store>>,
    state: &DreamRuntimeState,
    clock: &DreamClock,
) -> Result<()> {
    let guard = store.lock().await;
    save_state(&guard, state, clock.now())
}

async fn load_observations_for_level(
    store: &Arc<Mutex<Store>>,
    level: ObservationLevel,
) -> Result<Vec<Observation>> {
    let guard = store.lock().await;
    guard.list_observations_by_level(level, 500)
}

async fn load_all_observations(store: &Arc<Mutex<Store>>) -> Result<Vec<Observation>> {
    let guard = store.lock().await;
    let mut all = guard.list_observations_by_level(ObservationLevel::Explicit, 500)?;
    all.extend(guard.list_observations_by_level(ObservationLevel::Deductive, 500)?);
    all.extend(guard.list_observations_by_level(ObservationLevel::Inductive, 500)?);
    Ok(all)
}

async fn load_explicit_project_observations(
    store: &Arc<Mutex<Store>>,
    project_id: Option<Uuid>,
) -> Result<Vec<Observation>> {
    let observations = load_observations_for_level(store, ObservationLevel::Explicit).await?;
    Ok(observations
        .into_iter()
        .filter(|observation| observation.project_id == project_id)
        .collect())
}

async fn load_all_project_observations(
    store: &Arc<Mutex<Store>>,
    project_id: Option<Uuid>,
) -> Result<Vec<Observation>> {
    let observations = load_all_observations(store).await?;
    Ok(observations
        .into_iter()
        .filter(|observation| observation.project_id == project_id)
        .collect())
}

async fn apply_deduction_result(
    store: &Arc<Mutex<Store>>,
    project_id: Option<Uuid>,
    result: &DeductionResult,
) -> Result<()> {
    let guard = store.lock().await;
    for observation in &result.new_observations {
        guard.insert_observation(&deterministic_deductive_observation(observation.clone()))?;
    }
    for id in &result.superseded_ids {
        guard.soft_delete_observation(*id)?;
    }
    for (id1, id2, reason) in &result.contradictions {
        let contradiction = rsi_common::types::Observation {
            id: contradiction_observation_id(project_id, *id1, *id2, reason),
            session_id: Uuid::nil(),
            project_id,
            level: ObservationLevel::Contradiction,
            content: reason.clone(),
            source_ids: vec![*id1, *id2],
            confidence: None,
            times_derived: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        guard.insert_observation(&contradiction)?;
    }
    Ok(())
}

async fn apply_induction_result(store: &Arc<Mutex<Store>>, result: &InductionResult) -> Result<()> {
    let guard = store.lock().await;
    for observation in &result.patterns {
        guard.insert_observation(&deterministic_inductive_observation(observation.clone()))?;
    }
    Ok(())
}

fn project_key(project_id: Option<Uuid>) -> String {
    project_id
        .map(|id| format!("project:{id}"))
        .unwrap_or_else(|| "project:none".to_string())
}

fn collect_project_ids(observations: &[Observation]) -> Vec<Option<Uuid>> {
    let mut ids: Vec<Option<Uuid>> = observations
        .iter()
        .map(|observation| observation.project_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    ids.sort_by_key(|id| id.map(|value| value.to_string()).unwrap_or_default());
    ids
}

fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn estimate_tokens(text: &str) -> u64 {
    ((text.len() + 3) / 4) as u64
}

fn deterministic_explicit_observation(mut observation: Observation) -> Observation {
    observation.id = stable_observation_id(
        ObservationLevel::Explicit,
        observation.project_id,
        observation.session_id,
        &observation.content,
        &observation.source_ids,
        None,
    );
    observation
}

fn deterministic_deductive_observation(mut observation: Observation) -> Observation {
    observation.id = stable_observation_id(
        ObservationLevel::Deductive,
        observation.project_id,
        observation.session_id,
        &observation.content,
        &observation.source_ids,
        observation.confidence.map(|value| format!("{value:?}")),
    );
    observation
}

fn deterministic_inductive_observation(mut observation: Observation) -> Observation {
    observation.id = stable_observation_id(
        ObservationLevel::Inductive,
        observation.project_id,
        observation.session_id,
        &observation.content,
        &observation.source_ids,
        observation.confidence.map(|value| format!("{value:?}")),
    );
    observation
}

fn contradiction_observation_id(
    project_id: Option<Uuid>,
    left: Uuid,
    right: Uuid,
    reason: &str,
) -> Uuid {
    let mut source_ids = [left, right];
    source_ids.sort_unstable();
    stable_observation_id(
        ObservationLevel::Contradiction,
        project_id,
        Uuid::nil(),
        reason,
        &source_ids,
        None,
    )
}

fn stable_observation_id(
    level: ObservationLevel,
    project_id: Option<Uuid>,
    session_id: Uuid,
    content: &str,
    source_ids: &[Uuid],
    confidence: Option<String>,
) -> Uuid {
    let mut source_ids = source_ids.to_vec();
    source_ids.sort_unstable();
    let source_hash = source_ids
        .iter()
        .map(Uuid::to_string)
        .collect::<Vec<_>>()
        .join(",");
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!(
            "{}|{}|{}|{}|{}|{}",
            level_label(level),
            project_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "none".to_string()),
            session_id,
            content,
            source_hash,
            confidence.unwrap_or_default(),
        )
        .as_bytes(),
    )
}

fn level_label(level: ObservationLevel) -> &'static str {
    match level {
        ObservationLevel::Explicit => "explicit",
        ObservationLevel::Deductive => "deductive",
        ObservationLevel::Inductive => "inductive",
        ObservationLevel::Contradiction => "contradiction",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::dreamer::llm_client::DreamCompletionBackend;
    use crate::session::types::TrackedSession;
    use parking_lot::Mutex as StdMutex;
    use rsi_common::types::{
        ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
    };
    use std::collections::VecDeque;
    use std::future::Future;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::task::yield_now;

    #[test]
    fn test_dream_config_defaults() {
        let config = DreamConfig {
            observation_threshold: 50,
            idle_secs: 3600,
            cooldown_secs: 28800,
            batch_size: 20,
            poll_interval_secs: 1,
            max_model_calls: 64,
            max_estimated_input_tokens: 100_000,
            max_estimated_output_tokens: 32_768,
            max_estimated_total_tokens: 132_768,
            max_wall_time_ms: 1_800_000,
        };
        assert_eq!(config.observation_threshold, 50);
        assert_eq!(config.batch_size, 20);
    }

    enum BackendStep {
        Return(Result<String>),
        Pending,
    }

    #[derive(Default)]
    struct FakeDreamBackend {
        calls: AtomicU32,
        steps: StdMutex<VecDeque<BackendStep>>,
    }

    impl FakeDreamBackend {
        fn with_steps(steps: impl IntoIterator<Item = BackendStep>) -> Arc<Self> {
            let backend = Arc::new(Self::default());
            backend.steps.lock().extend(steps);
            backend
        }
    }

    #[async_trait::async_trait]
    impl DreamCompletionBackend for FakeDreamBackend {
        async fn execute(
            &self,
            _permit: &crate::model_control::AdmissionPermit,
            _target: &crate::memory::llm::MemoryLlmTarget,
            _prompt: &str,
            _max_tokens: u32,
            _purpose: &str,
            cancel: &CancellationToken,
        ) -> Result<String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let step = self
                .steps
                .lock()
                .pop_front()
                .unwrap_or_else(|| BackendStep::Return(Ok(String::new())));
            match step {
                BackendStep::Return(result) => result,
                BackendStep::Pending => {
                    cancel.cancelled().await;
                    Err(DaemonError::ChannelClosed)
                }
            }
        }
    }

    fn dream_config() -> DreamConfig {
        DreamConfig {
            observation_threshold: 1,
            idle_secs: 1,
            cooldown_secs: 2,
            batch_size: 8,
            poll_interval_secs: 1,
            max_model_calls: 8,
            max_estimated_input_tokens: 10_000,
            max_estimated_output_tokens: 10_000,
            max_estimated_total_tokens: 20_000,
            max_wall_time_ms: 60_000,
        }
    }

    fn dream_config_with_threshold(observation_threshold: u64) -> DreamConfig {
        let mut config = dream_config();
        config.observation_threshold = observation_threshold;
        config
    }

    fn runtime_config(enabled: bool) -> Arc<RuntimeConfig> {
        let mut config = Config::default();
        config.dream_enabled = enabled;
        config.dream_idle_secs = 1;
        config.dream_model = Some("qwen3:14b".to_string());
        config.dream_api_url = None;
        let runtime = RuntimeConfig::from_config(&config);
        *runtime.dream_model_provider.write() = SessionProvider::Local;
        runtime
    }

    fn paid_runtime_config(enabled: bool) -> Arc<RuntimeConfig> {
        let runtime = runtime_config(enabled);
        *runtime.dream_model.write() = "claude-sonnet-5".to_string();
        *runtime.dream_model_provider.write() = SessionProvider::Claude;
        runtime
    }

    fn store() -> Arc<Mutex<Store>> {
        Arc::new(Mutex::new(Store::open_in_memory().expect("store")))
    }

    fn local_llm(store: &Arc<Mutex<Store>>, backend: Arc<FakeDreamBackend>) -> DreamerLlmClient {
        DreamerLlmClient::new_with_backend(
            Arc::clone(store),
            Arc::new(EventBus::new(8)),
            SessionProvider::Local,
            String::new(),
            None,
            "qwen3:14b".to_string(),
            backend,
        )
    }

    fn paid_llm(store: &Arc<Mutex<Store>>, backend: Arc<FakeDreamBackend>) -> DreamerLlmClient {
        DreamerLlmClient::new_with_backend(
            Arc::clone(store),
            Arc::new(EventBus::new(8)),
            SessionProvider::Claude,
            "https://api.anthropic.com/v1/messages".to_string(),
            Some("test-key".to_string()),
            "claude-sonnet-5".to_string(),
            backend,
        )
    }

    fn test_session(status: SessionStatus, updated_at: DateTime<Utc>) -> Session {
        Session {
            context_fill_pct: None,
            id: Uuid::new_v4(),
            status,
            session_kind: SessionKind::Standard,
            provider: SessionProvider::Local,
            context_usage_confidence: ContextUsageConfidence::Missing,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            created_at: updated_at,
            updated_at,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            query: "dream test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            model: Some("qwen3:14b".to_string()),
            claude_session_id: None,
            project_id: None,
            continued_from: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
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
            approval_wait_ms: None,
            approval_started_at: None,
            work_time_ms: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
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

    fn tracked_session(updated_at: DateTime<Utc>) -> TrackedSession {
        TrackedSession::new_for_test(test_session(SessionStatus::Running, updated_at))
    }

    async fn insert_explicit_observation(store: &Arc<Mutex<Store>>, content: &str) {
        let observation = Observation {
            id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            project_id: None,
            level: ObservationLevel::Explicit,
            content: content.to_string(),
            source_ids: vec![],
            confidence: None,
            times_derived: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let guard = store.lock().await;
        guard.insert_observation(&observation).unwrap();
    }

    async fn drive_scheduler(handle: &DreamerHandle, clock: &DreamClockControl, ms: i64) {
        clock.advance_ms(ms);
        handle.notify_control_change().await.unwrap();
        for _ in 0..8 {
            yield_now().await;
        }
    }

    async fn drive_until<F, Fut>(
        handle: &DreamerHandle,
        clock: &DreamClockControl,
        mut predicate: F,
    ) where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        for _ in 0..64 {
            if predicate().await {
                return;
            }
            drive_scheduler(handle, clock, 0).await;
        }
        panic!("condition not met before deterministic drive budget expired");
    }

    async fn set_model_control_mode(store: &Arc<Mutex<Store>>, mode: &str) {
        let guard = store.lock().await;
        guard
            .set_daemon_setting("model_control_mode", mode)
            .unwrap();
    }

    async fn admit_dream_invocation(
        store: &Arc<Mutex<Store>>,
        dedup_key: &str,
        request_fingerprint: &str,
        provider: &str,
        model: &str,
        backend: &str,
    ) -> Uuid {
        let request = crate::model_control::ModelAdmissionRequest {
            purpose: rsi_common::model_control::ModelInvocationPurpose::DreamConsolidation,
            provider: Some(provider.to_string()),
            model: Some(model.to_string()),
            backend: Some(backend.to_string()),
            effort: None,
            trigger: "test".to_string(),
            owner: rsi_common::model_control::InvocationOwner {
                operator: Some("dream:test".to_string()),
                ..rsi_common::model_control::InvocationOwner::default()
            },
            dedup_key: Some(dedup_key.to_string()),
            request_fingerprint: Some(request_fingerprint.to_string()),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                rsi_common::model_control::ModelInvocationPurpose::DreamConsolidation,
                Some(provider),
                Some(backend),
                Some(model),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let bus = Arc::new(EventBus::new(16));
        match crate::model_control::admit_invocation(store, request, &bus)
            .await
            .unwrap()
        {
            crate::model_control::AdmissionDecision::Admitted(permit) => permit.invocation_id(),
            crate::model_control::AdmissionDecision::Duplicate { invocation_id } => invocation_id,
        }
    }

    #[tokio::test]
    async fn test_dreamer_handle_channel() {
        let status = Arc::new(RwLock::new(DreamStatusSnapshot {
            enabled: false,
            active_run_id: None,
            active_owner: None,
            status: None,
            phase: None,
            progress: DreamProgress::default(),
            last_success_at: None,
            cooldown_until: None,
            recent_consumption: DreamConsumption::default(),
            reason: None,
        }));
        let (tx, mut rx) = mpsc::channel(4);
        let handle = DreamerHandle::new(tx, status, Arc::new(Mutex::new(HashMap::new())));

        let task = tokio::spawn(async move {
            match rx.recv().await.unwrap() {
                DreamerCommand::TriggerNow { reply } => {
                    let _ = reply.send(Ok(DreamStatusSnapshot {
                        enabled: true,
                        active_run_id: None,
                        active_owner: None,
                        status: None,
                        phase: None,
                        progress: DreamProgress::default(),
                        last_success_at: None,
                        cooldown_until: None,
                        recent_consumption: DreamConsumption::default(),
                        reason: None,
                    }));
                }
                _ => panic!("unexpected command"),
            }
        });

        let snapshot = handle.trigger_now().await.unwrap();
        assert!(snapshot.enabled);
        task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn disabled_dream_produces_zero_calls() {
        let store = store();
        insert_explicit_observation(&store, "fact one").await;
        let backend = FakeDreamBackend::with_steps([BackendStep::Return(Ok(String::new()))]);
        let (clock, control) = DreamClock::manual(Utc::now());
        let handle = spawn_dreamer_with_clock(
            Arc::clone(&store),
            Arc::new(EventBus::new(16)),
            local_llm(&store, backend.clone()),
            dream_config(),
            Arc::new(RwLock::new(HashMap::new())),
            runtime_config(false),
            clock,
        );

        drive_scheduler(&handle, &control, 1200).await;
        assert!(handle.trigger_now().await.is_err());
        assert_eq!(backend.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dream_respects_idle_window_before_auto_run() {
        let store = store();
        insert_explicit_observation(&store, "fact one").await;
        {
            let guard = store.lock().await;
            guard
                .insert_session(&test_session(SessionStatus::Completed, Utc::now()))
                .unwrap();
        }
        let backend = FakeDreamBackend::with_steps([BackendStep::Return(Ok(String::new()))]);
        let (clock, control) = DreamClock::manual(Utc::now());
        let handle = spawn_dreamer_with_clock(
            Arc::clone(&store),
            Arc::new(EventBus::new(16)),
            local_llm(&store, backend.clone()),
            dream_config(),
            Arc::new(RwLock::new(HashMap::new())),
            runtime_config(true),
            clock,
        );

        drive_scheduler(&handle, &control, 400).await;
        assert_eq!(backend.calls.load(Ordering::Relaxed), 0);
        drive_scheduler(&handle, &control, 1500).await;
        drive_until(&handle, &control, || {
            let store = Arc::clone(&store);
            async move {
                let guard = store.lock().await;
                load_state(&guard).unwrap().last_success_at.is_some()
            }
        })
        .await;
        assert_eq!(backend.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn live_disable_pauses_active_dream_work() {
        let store = store();
        insert_explicit_observation(&store, "fact one").await;
        let backend = FakeDreamBackend::with_steps([BackendStep::Pending]);
        let runtime = runtime_config(true);
        let (clock, control) = DreamClock::manual(Utc::now());
        let handle = spawn_dreamer_with_clock(
            Arc::clone(&store),
            Arc::new(EventBus::new(16)),
            local_llm(&store, backend.clone()),
            dream_config(),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::clone(&runtime),
            clock,
        );

        handle.trigger_now().await.unwrap();
        drive_scheduler(&handle, &control, 1200).await;
        assert_eq!(backend.calls.load(Ordering::Relaxed), 1);

        runtime
            .update_field("dream_enabled", &serde_json::json!(false))
            .unwrap();
        handle.notify_control_change().await.unwrap();
        drive_scheduler(&handle, &control, 1).await;

        let status = handle.status().await;
        assert_eq!(status.status, Some(DreamRunStatus::Paused));
        assert_eq!(status.reason.as_deref(), Some("disabled"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn foreground_activity_pauses_active_dream_work() {
        let store = store();
        insert_explicit_observation(&store, "fact one").await;
        let backend = FakeDreamBackend::with_steps([BackendStep::Pending]);
        let active = Arc::new(RwLock::new(HashMap::new()));
        let (clock, control) = DreamClock::manual(Utc::now());
        let handle = spawn_dreamer_with_clock(
            Arc::clone(&store),
            Arc::new(EventBus::new(16)),
            local_llm(&store, backend.clone()),
            dream_config(),
            Arc::clone(&active),
            runtime_config(true),
            clock,
        );

        handle.trigger_now().await.unwrap();
        drive_scheduler(&handle, &control, 1200).await;
        active
            .write()
            .await
            .insert(Uuid::new_v4(), tracked_session(Utc::now()));
        handle.notify_control_change().await.unwrap();
        drive_scheduler(&handle, &control, 1).await;

        let status = handle.status().await;
        assert_eq!(status.status, Some(DreamRunStatus::Paused));
        assert_eq!(status.reason.as_deref(), Some("foreground_active"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restart_reuses_checkpoint_without_reexecution() {
        let store = store();
        let now = Utc::now();
        let config = dream_config();
        let runtime_state = DreamRuntimeState {
            version: 3,
            active_run: Some(DreamRunState {
                run_id: Uuid::new_v4(),
                owner: "dream:test".to_string(),
                trigger: "manual".to_string(),
                status: DreamRunStatus::Paused,
                phase: DreamPhase::Deduction,
                checkpoint: DreamCheckpoint {
                    deduction_project_ids: vec![None],
                    current_invocation_id: Some(Uuid::new_v4()),
                    current_settled: true,
                    current_item_key: Some("project:none".to_string()),
                    current_response: Some(String::new()),
                    ..DreamCheckpoint::default()
                },
                progress: DreamProgress {
                    total_items: 1,
                    ..DreamProgress::default()
                },
                consumption: DreamConsumption::default(),
                caps: DreamCaps {
                    max_items_per_phase: config.batch_size,
                    max_model_calls: config.max_model_calls,
                    max_estimated_input_tokens: config.max_estimated_input_tokens,
                    max_estimated_output_tokens: config.max_estimated_output_tokens,
                    max_estimated_total_tokens: config.max_estimated_total_tokens,
                    max_wall_time_ms: config.max_wall_time_ms,
                    max_concurrency: 1,
                    cooldown_secs: config.cooldown_secs,
                },
                started_at: now.to_rfc3339(),
                updated_at: now.to_rfc3339(),
                completed_at: None,
                terminal_reason: None,
                pause_reason: Some("restart_resume".to_string()),
            }),
            last_success_at: None,
            cooldown_until: None,
            last_terminal_reason: None,
            recent_consumption: DreamConsumption::default(),
        };
        {
            let guard = store.lock().await;
            save_state(&guard, &runtime_state, now).unwrap();
        }
        let backend =
            FakeDreamBackend::with_steps([BackendStep::Return(Ok("should not run".to_string()))]);
        let (clock, control) = DreamClock::manual(now);
        let handle = spawn_dreamer_with_clock(
            Arc::clone(&store),
            Arc::new(EventBus::new(16)),
            local_llm(&store, backend.clone()),
            config,
            Arc::new(RwLock::new(HashMap::new())),
            runtime_config(true),
            clock,
        );

        drive_scheduler(&handle, &control, 1200).await;
        assert_eq!(backend.calls.load(Ordering::Relaxed), 0);
        drive_until(&handle, &control, || {
            let store = Arc::clone(&store);
            async move {
                let guard = store.lock().await;
                load_state(&guard).unwrap().last_success_at.is_some()
            }
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_trigger_rechecks_live_disable_before_run() {
        let store = store();
        insert_explicit_observation(&store, "fact one").await;
        let backend = FakeDreamBackend::with_steps([BackendStep::Return(Ok(String::new()))]);
        let runtime = runtime_config(true);
        let config = dream_config_with_threshold(100);
        let (clock, control) = DreamClock::manual(Utc::now());
        let handle = spawn_dreamer_with_clock(
            Arc::clone(&store),
            Arc::new(EventBus::new(16)),
            local_llm(&store, backend.clone()),
            config,
            Arc::new(RwLock::new(HashMap::new())),
            Arc::clone(&runtime),
            clock,
        );

        handle.trigger_now().await.unwrap();
        runtime
            .update_field("dream_enabled", &serde_json::json!(false))
            .unwrap();
        handle.notify_control_change().await.unwrap();
        drive_scheduler(&handle, &control, 1200).await;

        assert_eq!(backend.calls.load(Ordering::Relaxed), 0);
        let guard = store.lock().await;
        assert!(load_state(&guard).unwrap().active_run.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_trigger_rechecks_model_control_modes_before_run() {
        for mode in ["pause_background", "deny_paid", "local_only", "stop_all"] {
            let store = store();
            insert_explicit_observation(&store, "fact one").await;
            let backend = FakeDreamBackend::with_steps([BackendStep::Return(Ok(String::new()))]);
            let config = dream_config_with_threshold(100);
            let (clock, control) = DreamClock::manual(Utc::now());
            let handle = spawn_dreamer_with_clock(
                Arc::clone(&store),
                Arc::new(EventBus::new(16)),
                paid_llm(&store, backend.clone()),
                config,
                Arc::new(RwLock::new(HashMap::new())),
                paid_runtime_config(true),
                clock,
            );

            handle.trigger_now().await.unwrap();
            set_model_control_mode(&store, mode).await;
            handle.notify_control_change().await.unwrap();
            drive_scheduler(&handle, &control, 1200).await;

            assert_eq!(
                backend.calls.load(Ordering::Relaxed),
                0,
                "manual trigger unexpectedly launched under mode {mode}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn checkpointed_response_settles_and_applies_without_backend_execution() {
        let store = store();
        let config = dream_config();
        let dedup_key = "dream:test:settle";
        let fingerprint = "sha256:test:settle";
        let invocation_id = admit_dream_invocation(
            &store,
            dedup_key,
            fingerprint,
            "Local",
            "qwen3:14b",
            "Local",
        )
        .await;
        let now = Utc::now();
        let runtime_state = DreamRuntimeState {
            version: 3,
            active_run: Some(DreamRunState {
                run_id: Uuid::new_v4(),
                owner: "dream:test".to_string(),
                trigger: "manual".to_string(),
                status: DreamRunStatus::Paused,
                phase: DreamPhase::Deduction,
                checkpoint: DreamCheckpoint {
                    deduction_project_ids: vec![None],
                    current_item_key: Some("project:none".to_string()),
                    current_dedup_key: Some(dedup_key.to_string()),
                    current_fingerprint: Some(fingerprint.to_string()),
                    current_invocation_id: Some(invocation_id),
                    current_input_estimate: Some(10),
                    current_output_estimate: Some(0),
                    current_wall_time_ms: Some(5),
                    current_response: Some(String::new()),
                    current_settled: false,
                    ..DreamCheckpoint::default()
                },
                progress: DreamProgress {
                    total_items: 1,
                    ..DreamProgress::default()
                },
                consumption: DreamConsumption {
                    model_calls: 1,
                    estimated_input_tokens: 10,
                    estimated_output_tokens: 0,
                    estimated_total_tokens: 10,
                    wall_time_ms: 5,
                    confidence: ModelUsageConfidence::Estimated,
                },
                caps: DreamCaps {
                    max_items_per_phase: config.batch_size,
                    max_model_calls: config.max_model_calls,
                    max_estimated_input_tokens: config.max_estimated_input_tokens,
                    max_estimated_output_tokens: config.max_estimated_output_tokens,
                    max_estimated_total_tokens: config.max_estimated_total_tokens,
                    max_wall_time_ms: config.max_wall_time_ms,
                    max_concurrency: 1,
                    cooldown_secs: config.cooldown_secs,
                },
                started_at: now.to_rfc3339(),
                updated_at: now.to_rfc3339(),
                completed_at: None,
                terminal_reason: None,
                pause_reason: Some("restart_resume".to_string()),
            }),
            last_success_at: None,
            cooldown_until: None,
            last_terminal_reason: None,
            recent_consumption: DreamConsumption::default(),
        };
        {
            let guard = store.lock().await;
            save_state(&guard, &runtime_state, now).unwrap();
        }

        let backend =
            FakeDreamBackend::with_steps([BackendStep::Return(Ok("should not run".to_string()))]);
        let (clock, control) = DreamClock::manual(now);
        let handle = spawn_dreamer_with_clock(
            Arc::clone(&store),
            Arc::new(EventBus::new(16)),
            local_llm(&store, backend.clone()),
            config,
            Arc::new(RwLock::new(HashMap::new())),
            runtime_config(true),
            clock,
        );

        drive_scheduler(&handle, &control, 1200).await;
        assert_eq!(backend.calls.load(Ordering::Relaxed), 0);
        drive_until(&handle, &control, || {
            let store = Arc::clone(&store);
            async move {
                let guard = store.lock().await;
                load_state(&guard).unwrap().last_success_at.is_some()
            }
        })
        .await;
        let guard = store.lock().await;
        let existing = crate::model_control::lookup_existing_invocation_by_dedup(&guard, dedup_key)
            .unwrap()
            .unwrap();
        assert_eq!(existing.status, "completed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn uncheckpointed_invocation_fails_without_reexecution() {
        let store = store();
        let config = dream_config();
        let dedup_key = "dream:test:ambiguous";
        let fingerprint = "sha256:test:ambiguous";
        let invocation_id = admit_dream_invocation(
            &store,
            dedup_key,
            fingerprint,
            "Local",
            "qwen3:14b",
            "Local",
        )
        .await;
        let now = Utc::now();
        let runtime_state = DreamRuntimeState {
            version: 3,
            active_run: Some(DreamRunState {
                run_id: Uuid::new_v4(),
                owner: "dream:test".to_string(),
                trigger: "manual".to_string(),
                status: DreamRunStatus::Paused,
                phase: DreamPhase::Deduction,
                checkpoint: DreamCheckpoint {
                    deduction_project_ids: vec![None],
                    current_item_key: Some("project:none".to_string()),
                    current_dedup_key: Some(dedup_key.to_string()),
                    current_fingerprint: Some(fingerprint.to_string()),
                    current_invocation_id: Some(invocation_id),
                    current_input_estimate: Some(10),
                    ..DreamCheckpoint::default()
                },
                progress: DreamProgress {
                    total_items: 1,
                    ..DreamProgress::default()
                },
                consumption: DreamConsumption::default(),
                caps: DreamCaps {
                    max_items_per_phase: config.batch_size,
                    max_model_calls: config.max_model_calls,
                    max_estimated_input_tokens: config.max_estimated_input_tokens,
                    max_estimated_output_tokens: config.max_estimated_output_tokens,
                    max_estimated_total_tokens: config.max_estimated_total_tokens,
                    max_wall_time_ms: config.max_wall_time_ms,
                    max_concurrency: 1,
                    cooldown_secs: config.cooldown_secs,
                },
                started_at: now.to_rfc3339(),
                updated_at: now.to_rfc3339(),
                completed_at: None,
                terminal_reason: None,
                pause_reason: Some("restart_resume".to_string()),
            }),
            last_success_at: None,
            cooldown_until: None,
            last_terminal_reason: None,
            recent_consumption: DreamConsumption::default(),
        };
        {
            let guard = store.lock().await;
            save_state(&guard, &runtime_state, now).unwrap();
        }

        let backend =
            FakeDreamBackend::with_steps([BackendStep::Return(Ok("should not run".to_string()))]);
        let (clock, control) = DreamClock::manual(now);
        let handle = spawn_dreamer_with_clock(
            Arc::clone(&store),
            Arc::new(EventBus::new(16)),
            local_llm(&store, backend.clone()),
            config,
            Arc::new(RwLock::new(HashMap::new())),
            runtime_config(true),
            clock,
        );

        drive_scheduler(&handle, &control, 1200).await;
        assert_eq!(backend.calls.load(Ordering::Relaxed), 0);
        drive_until(&handle, &control, || {
            let store = Arc::clone(&store);
            async move {
                let guard = store.lock().await;
                load_state(&guard).unwrap().active_run.is_none()
            }
        })
        .await;
        let guard = store.lock().await;
        let loaded = load_state(&guard).unwrap();
        assert!(loaded.active_run.is_none());
        assert!(
            loaded
                .last_terminal_reason
                .as_deref()
                .unwrap_or_default()
                .contains("refusing replay")
        );
        let existing = crate::model_control::lookup_existing_invocation_by_dedup(&guard, dedup_key)
            .unwrap()
            .unwrap();
        assert_eq!(existing.status, "failed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn extraction_application_is_idempotent_on_replay() {
        let store = store();
        let session = test_session(SessionStatus::Completed, Utc::now());
        let session_id = session.id;
        {
            let guard = store.lock().await;
            guard.insert_session(&session).unwrap();
        }
        let clock = DreamClock::new();
        let mut state = DreamRuntimeState {
            version: 3,
            active_run: Some(DreamRunState {
                run_id: Uuid::new_v4(),
                owner: "dream:test".to_string(),
                trigger: "manual".to_string(),
                status: DreamRunStatus::Running,
                phase: DreamPhase::Extraction,
                checkpoint: DreamCheckpoint {
                    extract_session_ids: vec![session_id],
                    current_response: Some("[\"fact one\"]".to_string()),
                    current_settled: true,
                    ..DreamCheckpoint::default()
                },
                progress: DreamProgress::default(),
                consumption: DreamConsumption::default(),
                caps: DreamCaps {
                    max_items_per_phase: 1,
                    max_model_calls: 1,
                    max_estimated_input_tokens: 100,
                    max_estimated_output_tokens: 100,
                    max_estimated_total_tokens: 200,
                    max_wall_time_ms: 1000,
                    max_concurrency: 1,
                    cooldown_secs: 1,
                },
                started_at: Utc::now().to_rfc3339(),
                updated_at: Utc::now().to_rfc3339(),
                completed_at: None,
                terminal_reason: None,
                pause_reason: None,
            }),
            last_success_at: None,
            cooldown_until: None,
            last_terminal_reason: None,
            recent_consumption: DreamConsumption::default(),
        };
        let replay_state = state.clone();

        apply_current_response(&store, &clock, &mut state, "[\"fact one\"]".to_string())
            .await
            .unwrap();
        let mut replay = replay_state;
        apply_current_response(&store, &clock, &mut replay, "[\"fact one\"]".to_string())
            .await
            .unwrap();

        let guard = store.lock().await;
        assert_eq!(
            guard
                .list_observations_by_level(ObservationLevel::Explicit, 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persisted_caps_cannot_be_raised_by_live_config_changes() {
        let store = store();
        let config = dream_config_with_threshold(100);
        insert_explicit_observation(&store, "fact one").await;
        let now = Utc::now();
        let runtime_state = DreamRuntimeState {
            version: 3,
            active_run: Some(DreamRunState {
                run_id: Uuid::new_v4(),
                owner: "dream:test".to_string(),
                trigger: "manual".to_string(),
                status: DreamRunStatus::Running,
                phase: DreamPhase::Deduction,
                checkpoint: DreamCheckpoint {
                    deduction_project_ids: vec![None],
                    ..DreamCheckpoint::default()
                },
                progress: DreamProgress {
                    total_items: 1,
                    ..DreamProgress::default()
                },
                consumption: DreamConsumption::default(),
                caps: DreamCaps {
                    max_items_per_phase: 1,
                    max_model_calls: 0,
                    max_estimated_input_tokens: config.max_estimated_input_tokens,
                    max_estimated_output_tokens: config.max_estimated_output_tokens,
                    max_estimated_total_tokens: config.max_estimated_total_tokens,
                    max_wall_time_ms: config.max_wall_time_ms,
                    max_concurrency: 1,
                    cooldown_secs: config.cooldown_secs,
                },
                started_at: now.to_rfc3339(),
                updated_at: now.to_rfc3339(),
                completed_at: None,
                terminal_reason: None,
                pause_reason: None,
            }),
            last_success_at: None,
            cooldown_until: None,
            last_terminal_reason: None,
            recent_consumption: DreamConsumption::default(),
        };
        {
            let guard = store.lock().await;
            save_state(&guard, &runtime_state, now).unwrap();
        }
        let runtime = runtime_config(true);
        runtime
            .update_field("dream_observation_threshold", &serde_json::json!(100))
            .unwrap();
        let backend =
            FakeDreamBackend::with_steps([BackendStep::Return(Ok("unexpected".to_string()))]);
        let (clock, control) = DreamClock::manual(now);
        let handle = spawn_dreamer_with_clock(
            Arc::clone(&store),
            Arc::new(EventBus::new(16)),
            local_llm(&store, backend.clone()),
            config,
            Arc::new(RwLock::new(HashMap::new())),
            runtime,
            clock,
        );

        drive_scheduler(&handle, &control, 1200).await;
        assert_eq!(backend.calls.load(Ordering::Relaxed), 0);
        drive_until(&handle, &control, || {
            let store = Arc::clone(&store);
            async move {
                let guard = store.lock().await;
                load_state(&guard)
                    .unwrap()
                    .last_terminal_reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains("model-call cap exceeded")
            }
        })
        .await;
        let guard = store.lock().await;
        let loaded = load_state(&guard).unwrap();
        assert!(
            loaded
                .last_terminal_reason
                .as_deref()
                .unwrap_or_default()
                .contains("model-call cap exceeded")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_registration_is_idempotent_until_settlement() {
        let store = store();
        insert_explicit_observation(&store, "fact one").await;
        let backend = FakeDreamBackend::with_steps([BackendStep::Pending]);
        let (clock, control) = DreamClock::manual(Utc::now());
        let handle = spawn_dreamer_with_clock(
            Arc::clone(&store),
            Arc::new(EventBus::new(16)),
            local_llm(&store, backend.clone()),
            dream_config(),
            Arc::new(RwLock::new(HashMap::new())),
            runtime_config(true),
            clock,
        );

        handle.trigger_now().await.unwrap();
        drive_scheduler(&handle, &control, 1200).await;
        let invocation_id = {
            let guard = store.lock().await;
            load_state(&guard)
                .unwrap()
                .active_run
                .and_then(|run| run.checkpoint.current_invocation_id)
                .unwrap()
        };

        assert!(handle.cancel_invocation(invocation_id).await);
        assert!(handle.cancel_invocation(invocation_id).await);
        drive_scheduler(&handle, &control, 1).await;
        assert!(!handle.cancel_invocation(invocation_id).await);
    }
}
