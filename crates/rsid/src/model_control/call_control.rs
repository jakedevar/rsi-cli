use crate::bus::EventBus;
use crate::error::{DaemonError, Result};
use crate::model_control::live_calls::{self, CallLiveness};
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::model_control::{
    AdmissionDecision, AdmissionPermit, InvocationCompletion, ModelAdmissionRequest,
    ModelExecutionCapability, admit_invocation, complete_invocation, complete_invocation_locked,
    hash_request_fingerprint,
};
use crate::store::Store;
use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose, ModelUsageConfidence};
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex, mpsc as std_mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[cfg(not(test))]
const SETTLEMENT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const SETTLEMENT_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(not(test))]
const SETTLEMENT_PRODUCER_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const SETTLEMENT_PRODUCER_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(not(test))]
const SETTLEMENT_RETRY_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
const SETTLEMENT_RETRY_INTERVAL: Duration = Duration::from_millis(50);
/// A settlement that cannot write for this long is abandoned with the typed
/// failure below, so its ledger row (and lead capacity slot) stops depending
/// on a completion write that will never succeed (#940).
const SETTLEMENT_MAX_RETAINED_AGE: Duration = Duration::from_secs(300);
/// Sweep cadence and staleness bound for background-helper ledger rows.
const HELPER_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const HELPER_STALE_AGE: Duration = Duration::from_secs(900);
const HELPER_ORPHAN_GRACE: Duration = Duration::from_secs(120);
/// Invocations whose settlement (even the minimal abandonment) could not be
/// written to the database. Manager capacity accounting excludes these ids so
/// a dead database write cannot pin a lead slot forever (#1080); the row is
/// still settled in the ledger once writes recover, and the id is then
/// removed. Process-wide because the daemon opens several `Store` handles
/// over one database.
static CAPACITY_RELEASED: std::sync::OnceLock<StdMutex<std::collections::HashSet<uuid::Uuid>>> =
    std::sync::OnceLock::new();

fn capacity_released() -> &'static StdMutex<std::collections::HashSet<uuid::Uuid>> {
    CAPACITY_RELEASED.get_or_init(Default::default)
}

fn release_capacity_in_memory(invocation_id: uuid::Uuid) {
    lock_unpoisoned(capacity_released()).insert(invocation_id);
}

fn restore_capacity_accounting(invocation_id: uuid::Uuid) {
    lock_unpoisoned(capacity_released()).remove(&invocation_id);
}

/// JSON array of invocation ids capacity accounting must ignore, for
/// `json_each` in the manager resource queries.
pub(crate) fn capacity_released_ids_json() -> String {
    let ids: Vec<String> = lock_unpoisoned(capacity_released())
        .iter()
        .map(ToString::to_string)
        .collect();
    serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_string())
}

/// Typed failure recorded when a settlement is given up.
pub(crate) const SETTLEMENT_ABANDONED_ERROR_CLASS: &str = "settlement_abandoned";
/// Typed failure recorded when a stale background-helper row is swept.
pub(crate) const HELPER_STALE_SWEPT_ERROR_CLASS: &str = "helper_call_stale_swept";

/// Timing knobs of the settlement worker. Production uses [`Default`];
/// tests inject small bounds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SettlementPolicy {
    pub retry_interval: Duration,
    pub max_retained_age: Duration,
    pub sweep_interval: Duration,
    /// A live handle that has shown no activity for this long is expired.
    pub helper_stale_age: Duration,
    /// A running helper row with no live in-process handle is orphaned once
    /// it has been observed so for this long (monotonic).
    pub helper_orphan_grace: Duration,
}

impl Default for SettlementPolicy {
    fn default() -> Self {
        Self {
            retry_interval: SETTLEMENT_RETRY_INTERVAL,
            max_retained_age: SETTLEMENT_MAX_RETAINED_AGE,
            sweep_interval: HELPER_SWEEP_INTERVAL,
            helper_stale_age: HELPER_STALE_AGE,
            helper_orphan_grace: HELPER_ORPHAN_GRACE,
        }
    }
}

struct AbandonedCallSettlement {
    permit: AdmissionPermit,
    completion: InvocationCompletion,
}

struct RetainedSettlement {
    job: AbandonedCallSettlement,
    error: String,
    first_failed: Instant,
    /// The original completion was replaced by the minimal typed failure.
    abandoned: bool,
}

#[derive(Default)]
struct SettlementState {
    retained: Vec<RetainedSettlement>,
    worker_error: Option<String>,
}

enum SettlementCommand {
    Settle(AbandonedCallSettlement),
    #[cfg(test)]
    Drain(std_mpsc::SyncSender<Result<()>>),
    Shutdown,
}

struct SettlementDispatcher {
    accepting: bool,
    producers_open: bool,
    tx: std_mpsc::Sender<SettlementCommand>,
}

struct SettlementQueueInner {
    dispatcher: StdMutex<SettlementDispatcher>,
    state: Arc<StdMutex<SettlementState>>,
    producer_count: AtomicUsize,
}

#[derive(Default)]
struct SettlementWorkerCompletion {
    result: Option<std::result::Result<(), String>>,
}

#[derive(Default)]
struct SettlementWorkerLifecycle {
    completion: StdMutex<SettlementWorkerCompletion>,
    ready: Condvar,
}

/// Non-blocking handoff used by model-call `Drop` paths.
///
/// The unbounded standard-library channel cannot saturate. The short
/// dispatcher mutex serializes handoff with shutdown, but no path here waits
/// for the Tokio store mutex or for the settlement worker.
pub(crate) struct ModelCallSettlementHandle {
    inner: Arc<SettlementQueueInner>,
    tracks_producer: bool,
}

/// Daemon-owned worker that serializes abandoned model-call settlement.
///
/// `SessionManager` owns one worker and explicitly drains it after provider
/// tasks stop. Tests can own the same primitive without a Tokio runtime.
pub(crate) struct ModelCallSettlementWorker {
    handle: ModelCallSettlementHandle,
    join: Arc<StdMutex<Option<JoinHandle<()>>>>,
    lifecycle: Arc<SettlementWorkerLifecycle>,
}

fn lock_unpoisoned<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn retained_error(state: &SettlementState) -> Result<()> {
    if state.retained.is_empty() && state.worker_error.is_none() {
        return Ok(());
    }

    let retained = state.retained.len();
    let detail = state
        .worker_error
        .as_deref()
        .or_else(|| state.retained.last().map(|entry| entry.error.as_str()))
        .unwrap_or("unknown settlement worker failure");
    Err(DaemonError::Store(format!(
        "model-call settlement worker retained {retained} unsettled invocation(s): {detail}"
    )))
}

fn retain_settlement(
    state: &Arc<StdMutex<SettlementState>>,
    job: AbandonedCallSettlement,
    error: impl Into<String>,
) {
    let error = error.into();
    let mut state = lock_unpoisoned(state);
    state.retained.push(RetainedSettlement {
        job,
        error: error.clone(),
        first_failed: Instant::now(),
        abandoned: false,
    });
    state.worker_error.get_or_insert(error);
}

fn settle_one(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    job: AbandonedCallSettlement,
) -> std::result::Result<(), RetainedSettlement> {
    let invocation_id = job.permit.invocation_id();
    let guard = store.blocking_lock();
    match complete_invocation_locked(&guard, invocation_id, &job.completion, event_bus) {
        Ok(()) => Ok(()),
        Err(error) => Err(RetainedSettlement {
            job,
            error: error.to_string(),
            first_failed: Instant::now(),
            abandoned: false,
        }),
    }
}

/// Give up on a settlement that has not written within `max_age`: swap its
/// completion for the minimal typed failure (no usage payload, so a write
/// that failed on the original payload can succeed) exactly once.
fn abandon_if_expired(entry: &mut RetainedSettlement, max_age: Duration) {
    if entry.abandoned || entry.first_failed.elapsed() < max_age {
        return;
    }
    tracing::warn!(
        invocation_id = %entry.job.permit.invocation_id(),
        last_error = %entry.error,
        "model-call settlement never wrote; abandoning with typed failure {SETTLEMENT_ABANDONED_ERROR_CLASS}"
    );
    entry.job.completion = InvocationCompletion {
        wall_time_ms: entry.job.completion.wall_time_ms,
        error_class: Some(SETTLEMENT_ABANDONED_ERROR_CLASS.to_string()),
        confidence: Some(ModelUsageConfidence::Unavailable),
        ..InvocationCompletion::default()
    };
    entry.abandoned = true;
}

fn retry_retained(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    state: &Arc<StdMutex<SettlementState>>,
    policy: &SettlementPolicy,
) {
    let pending = {
        let mut state = lock_unpoisoned(state);
        std::mem::take(&mut state.retained)
    };
    let mut failed = Vec::new();
    for mut entry in pending {
        abandon_if_expired(&mut entry, policy.max_retained_age);
        let (first_failed, abandoned, prior_error) =
            (entry.first_failed, entry.abandoned, entry.error);
        let invocation_id = entry.job.permit.invocation_id();
        match settle_one(store, event_bus, entry.job) {
            Ok(()) => restore_capacity_accounting(invocation_id),
            Err(mut retained) => {
                if retained.error.is_empty() {
                    retained.error = prior_error;
                }
                retained.first_failed = first_failed;
                retained.abandoned = abandoned;
                // Even the minimal abandonment cannot be written: release the
                // slot in memory now and keep retrying the row (#1080).
                if abandoned {
                    release_capacity_in_memory(invocation_id);
                }
                failed.push(retained);
            }
        }
    }
    let mut state = lock_unpoisoned(state);
    state.retained.extend(failed);
    if state.retained.is_empty() {
        state.worker_error = None;
    } else {
        state.worker_error = state.retained.last().map(|entry| entry.error.clone());
    }
}

/// Whether a still-`running` helper row may be swept. Purely monotonic
/// (`Instant`) inputs, so a wall-clock jump cannot change the answer (#1080):
/// a live handle is swept only once expired and inactive, and an unhandled
/// (orphaned) row only after it has been observed orphaned for the grace.
fn helper_sweepable(
    liveness: CallLiveness,
    orphan_since: &mut Option<Instant>,
    policy: &SettlementPolicy,
) -> bool {
    match liveness {
        CallLiveness::Live { idle } => {
            *orphan_since = None;
            idle >= policy.helper_stale_age
        }
        CallLiveness::Orphaned => {
            orphan_since.get_or_insert_with(Instant::now).elapsed() >= policy.helper_orphan_grace
        }
    }
}

/// Settle orphaned or expired-and-inactive background-helper rows as typed
/// failures. Failures here are logged and retried on the next sweep.
fn sweep_stale_helper_invocations(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    policy: &SettlementPolicy,
    orphan_seen: &mut HashMap<uuid::Uuid, Instant>,
) {
    let guard = store.blocking_lock();
    let running = match guard.running_background_helper_invocation_ids() {
        Ok(running) => running,
        Err(error) => {
            tracing::warn!(%error, "background-helper invocation sweep failed");
            return;
        }
    };
    orphan_seen.retain(|id, _| running.contains(id));
    for invocation_id in running {
        let mut since = orphan_seen.get(&invocation_id).copied();
        let sweep = helper_sweepable(live_calls::liveness(invocation_id), &mut since, policy);
        match since {
            Some(since) => orphan_seen.insert(invocation_id, since),
            None => orphan_seen.remove(&invocation_id),
        };
        if !sweep {
            continue;
        }
        let completion = InvocationCompletion {
            error_class: Some(HELPER_STALE_SWEPT_ERROR_CLASS.to_string()),
            confidence: Some(ModelUsageConfidence::Unavailable),
            ..InvocationCompletion::default()
        };
        if let Err(error) =
            complete_invocation_locked(&guard, invocation_id, &completion, event_bus)
        {
            tracing::warn!(%invocation_id, %error, "stale background-helper settlement failed");
        }
    }
}

fn settlement_worker_loop(
    store: Arc<Mutex<Store>>,
    event_bus: Arc<EventBus>,
    state: Arc<StdMutex<SettlementState>>,
    rx: std_mpsc::Receiver<SettlementCommand>,
    policy: SettlementPolicy,
) -> Result<()> {
    let mut retry_at: Option<Instant> = None;
    let mut next_sweep = Instant::now() + policy.sweep_interval;
    let mut orphan_seen: HashMap<uuid::Uuid, Instant> = HashMap::new();
    loop {
        let deadline = retry_at.map_or(next_sweep, |retry| retry.min(next_sweep));
        let received = match Instant::now() >= deadline {
            true => Err(std_mpsc::RecvTimeoutError::Timeout),
            false => rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
        };
        let command = match received {
            Ok(command) => command,
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= next_sweep {
                    sweep_stale_helper_invocations(&store, &event_bus, &policy, &mut orphan_seen);
                    next_sweep = Instant::now() + policy.sweep_interval;
                }
                if retry_at.is_some_and(|retry| Instant::now() >= retry) {
                    retry_retained(&store, &event_bus, &state, &policy);
                    retry_at = (!lock_unpoisoned(&state).retained.is_empty())
                        .then(|| Instant::now() + policy.retry_interval);
                }
                continue;
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                let mut state = lock_unpoisoned(&state);
                state.worker_error.get_or_insert_with(|| {
                    "model-call settlement command channel closed before explicit shutdown"
                        .to_string()
                });
                return retained_error(&state);
            }
        };
        match command {
            SettlementCommand::Settle(job) => {
                match settle_one(&store, &event_bus, job) {
                    Ok(()) if retry_at.is_some() => {
                        retry_retained(&store, &event_bus, &state, &policy);
                        retry_at = None;
                    }
                    Ok(()) => {}
                    Err(retained) => {
                        let mut state = lock_unpoisoned(&state);
                        state.worker_error = Some(retained.error.clone());
                        state.retained.push(retained);
                    }
                }
                if !lock_unpoisoned(&state).retained.is_empty() {
                    retry_at.get_or_insert_with(|| Instant::now() + policy.retry_interval);
                }
            }
            #[cfg(test)]
            SettlementCommand::Drain(reply) => {
                retry_retained(&store, &event_bus, &state, &policy);
                retry_at = (!lock_unpoisoned(&state).retained.is_empty())
                    .then(|| Instant::now() + policy.retry_interval);
                let result = retained_error(&lock_unpoisoned(&state));
                let _ = reply.send(result);
            }
            SettlementCommand::Shutdown => {
                retry_retained(&store, &event_bus, &state, &policy);
                return retained_error(&lock_unpoisoned(&state));
            }
        }
    }
}

impl ModelCallSettlementHandle {
    fn enqueue(&self, job: AbandonedCallSettlement) {
        let send_result = {
            let dispatcher = lock_unpoisoned(&self.inner.dispatcher);
            if dispatcher.accepting {
                dispatcher.tx.send(SettlementCommand::Settle(job))
            } else {
                drop(dispatcher);
                retain_settlement(
                    &self.inner.state,
                    job,
                    "model-call settlement worker is not accepting new jobs",
                );
                return;
            }
        };

        if let Err(std_mpsc::SendError(SettlementCommand::Settle(job))) = send_result {
            retain_settlement(
                &self.inner.state,
                job,
                "model-call settlement worker command channel disconnected",
            );
        }
    }

    #[cfg(test)]
    fn request_drain_blocking(&self) -> Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::sync_channel(1);
        {
            let dispatcher = lock_unpoisoned(&self.inner.dispatcher);
            if !dispatcher.accepting {
                return retained_error(&lock_unpoisoned(&self.inner.state)).and_then(|()| {
                    Err(DaemonError::Store(
                        "model-call settlement worker is already shutting down".to_string(),
                    ))
                });
            }
            if dispatcher
                .tx
                .send(SettlementCommand::Drain(reply_tx))
                .is_err()
            {
                let mut state = lock_unpoisoned(&self.inner.state);
                state.worker_error =
                    Some("model-call settlement worker command channel disconnected".to_string());
                return retained_error(&state);
            }
        }

        reply_rx
            .recv_timeout(SETTLEMENT_DRAIN_TIMEOUT)
            .map_err(|error| {
                DaemonError::Store(format!(
                    "timed out waiting for model-call settlement worker: {error}"
                ))
            })?
    }

    fn request_shutdown(&self) {
        let mut dispatcher = lock_unpoisoned(&self.inner.dispatcher);
        if !dispatcher.accepting {
            return;
        }
        dispatcher.accepting = false;
        if dispatcher.tx.send(SettlementCommand::Shutdown).is_err() {
            let mut state = lock_unpoisoned(&self.inner.state);
            state.worker_error =
                Some("model-call settlement worker command channel disconnected".to_string());
        }
    }

    #[cfg(test)]
    pub(crate) fn drain_blocking(&self) -> Result<()> {
        self.request_drain_blocking()
    }
}

impl Clone for ModelCallSettlementHandle {
    fn clone(&self) -> Self {
        if self.tracks_producer {
            self.inner.producer_count.fetch_add(1, Ordering::AcqRel);
        }
        Self {
            inner: Arc::clone(&self.inner),
            tracks_producer: self.tracks_producer,
        }
    }
}

impl Drop for ModelCallSettlementHandle {
    fn drop(&mut self) {
        if self.tracks_producer {
            self.inner.producer_count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl ModelCallSettlementWorker {
    pub(crate) fn new(store: Arc<Mutex<Store>>, event_bus: Arc<EventBus>) -> Result<Self> {
        Self::with_policy(store, event_bus, SettlementPolicy::default())
    }

    pub(crate) fn with_policy(
        store: Arc<Mutex<Store>>,
        event_bus: Arc<EventBus>,
        policy: SettlementPolicy,
    ) -> Result<Self> {
        let (tx, rx) = std_mpsc::channel();
        let state = Arc::new(StdMutex::new(SettlementState::default()));
        let worker_state = Arc::clone(&state);
        let lifecycle = Arc::new(SettlementWorkerLifecycle::default());
        let worker_lifecycle = Arc::clone(&lifecycle);
        let join = std::thread::Builder::new()
            .name("model-call-settlement".to_string())
            .spawn(move || {
                let result = match catch_unwind(AssertUnwindSafe(|| {
                    settlement_worker_loop(store, event_bus, worker_state, rx, policy)
                })) {
                    Ok(result) => result.map_err(|error| error.to_string()),
                    Err(_) => Err("model-call settlement worker panicked".to_string()),
                };
                let mut completion = lock_unpoisoned(&worker_lifecycle.completion);
                completion.result = Some(result);
                worker_lifecycle.ready.notify_all();
            })
            .map_err(|error| {
                DaemonError::Store(format!(
                    "failed to start model-call settlement worker: {error}"
                ))
            })?;
        Ok(Self {
            handle: ModelCallSettlementHandle {
                inner: Arc::new(SettlementQueueInner {
                    dispatcher: StdMutex::new(SettlementDispatcher {
                        accepting: true,
                        producers_open: true,
                        tx,
                    }),
                    state,
                    producer_count: AtomicUsize::new(0),
                }),
                tracks_producer: false,
            },
            join: Arc::new(StdMutex::new(Some(join))),
            lifecycle,
        })
    }

    pub(crate) fn handle(&self) -> Result<ModelCallSettlementHandle> {
        let dispatcher = lock_unpoisoned(&self.handle.inner.dispatcher);
        if !dispatcher.producers_open {
            return Err(DaemonError::Store(
                "model-call settlement worker is sealing producer ownership".to_string(),
            ));
        }
        self.handle
            .inner
            .producer_count
            .fetch_add(1, Ordering::AcqRel);
        Ok(ModelCallSettlementHandle {
            inner: Arc::clone(&self.handle.inner),
            tracks_producer: true,
        })
    }

    fn seal_producers_and_wait_blocking(&self) -> Result<()> {
        {
            let mut dispatcher = lock_unpoisoned(&self.handle.inner.dispatcher);
            dispatcher.producers_open = false;
        }
        let start = Instant::now();
        loop {
            let remaining = self.handle.inner.producer_count.load(Ordering::Acquire);
            if remaining == 0 {
                return Ok(());
            }
            if start.elapsed() >= SETTLEMENT_PRODUCER_TIMEOUT {
                return Err(DaemonError::Store(format!(
                    "timed out waiting for {remaining} model-call settlement producer(s) to quiesce"
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_completion_blocking(&self) -> Result<std::result::Result<(), String>> {
        let deadline = Instant::now() + SETTLEMENT_DRAIN_TIMEOUT;
        let mut completion = lock_unpoisoned(&self.lifecycle.completion);
        loop {
            if let Some(result) = completion.result.clone() {
                return Ok(result);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(DaemonError::Store(
                    "timed out waiting for model-call settlement worker completion".to_string(),
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next, timeout) = self
                .lifecycle
                .ready
                .wait_timeout(completion, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            completion = next;
            if timeout.timed_out() && completion.result.is_none() {
                return Err(DaemonError::Store(
                    "timed out waiting for model-call settlement worker completion".to_string(),
                ));
            }
        }
    }

    fn join_completed_worker_blocking(&self) -> Result<()> {
        let start = Instant::now();
        loop {
            let join = {
                let mut join_slot = lock_unpoisoned(&self.join);
                match join_slot.as_ref() {
                    None => return Ok(()),
                    Some(join) if join.is_finished() => join_slot.take(),
                    Some(_) => None,
                }
            };
            if let Some(join) = join {
                return join.join().map_err(|_| {
                    DaemonError::Store("model-call settlement worker panicked".to_string())
                });
            }
            if start.elapsed() >= SETTLEMENT_DRAIN_TIMEOUT {
                return Err(DaemonError::Store(
                    "timed out waiting to join completed model-call settlement worker".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn shutdown_blocking_inner(&self) -> Result<()> {
        self.seal_producers_and_wait_blocking()?;
        self.handle.request_shutdown();
        let result = self.wait_for_completion_blocking()?;
        self.join_completed_worker_blocking()?;
        result.map_err(DaemonError::Store)
    }

    #[cfg(test)]
    pub(crate) async fn drain(&self) -> Result<()> {
        let handle = self.handle.clone();
        tokio::task::spawn_blocking(move || handle.drain_blocking())
            .await
            .map_err(|error| {
                DaemonError::Store(format!(
                    "failed to join model-call settlement drain: {error}"
                ))
            })?
    }

    #[cfg(test)]
    pub(crate) fn drain_blocking(&self) -> Result<()> {
        self.handle.drain_blocking()
    }

    #[cfg(test)]
    pub(crate) fn shutdown_blocking(&self) -> Result<()> {
        self.shutdown_blocking_inner()
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let handle = self.handle.clone();
        let join_slot = Arc::clone(&self.join);
        let lifecycle = Arc::clone(&self.lifecycle);
        tokio::task::spawn_blocking(move || {
            let worker = ModelCallSettlementWorker {
                handle,
                join: join_slot,
                lifecycle,
            };
            worker.shutdown_blocking_inner()
        })
        .await
        .map_err(|error| {
            DaemonError::Store(format!(
                "failed to join model-call settlement shutdown: {error}"
            ))
        })?
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCallKind {
    Primary,
    Compaction,
}

#[derive(Debug, Clone, Default)]
pub struct ModelCallUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub embedding_input_count: Option<u64>,
    pub wall_time_ms: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
    pub confidence: Option<ModelUsageConfidence>,
}

/// One admitted model attempt before its authority is split for execution.
///
/// The settlement token remains with the caller while the non-clone execution
/// capability is moved into the provider request boundary.
pub struct AdmittedModelCall {
    settlement: ModelCallSettlement,
    execution: ModelExecutionCapability,
}

impl AdmittedModelCall {
    pub fn invocation_id(&self) -> Option<uuid::Uuid> {
        self.settlement.invocation_id()
    }

    pub(crate) fn into_parts(self) -> (ModelCallSettlement, ModelExecutionCapability) {
        (self.settlement, self.execution)
    }

    pub(crate) fn real(
        permit: AdmissionPermit,
        boundary: RuntimeExecutionRoute,
        settlements: ModelCallSettlementHandle,
    ) -> Result<Self> {
        let settlement = ModelCallSettlement {
            permit: Some(permit),
            recovery: Some(ModelCallSettlementRecovery { settlements }),
        };
        let execution = settlement
            .permit()
            .expect("new model-call settlement retains its permit")
            .claim_model_execution(boundary)?;
        Ok(Self {
            settlement,
            execution,
        })
    }

    #[cfg(test)]
    fn noop() -> Self {
        Self {
            settlement: ModelCallSettlement {
                permit: None,
                recovery: None,
            },
            execution: ModelExecutionCapability::for_test(
                RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
            ),
        }
    }
}

/// Non-clone settlement identity retained after execution authority moves into
/// the backend. If its task is aborted or the owning provider session is
/// dropped, `Drop` completes a fail-closed terminal settlement before releasing
/// ownership.
pub struct ModelCallSettlement {
    permit: Option<AdmissionPermit>,
    recovery: Option<ModelCallSettlementRecovery>,
}

struct ModelCallSettlementRecovery {
    settlements: ModelCallSettlementHandle,
}

fn settle_abandoned_call(permit: AdmissionPermit, recovery: ModelCallSettlementRecovery) {
    recovery.settlements.enqueue(AbandonedCallSettlement {
        permit,
        completion: InvocationCompletion {
            error_class: Some("model_call_task_exited".to_string()),
            confidence: Some(ModelUsageConfidence::Unavailable),
            ..InvocationCompletion::default()
        },
    });
}

impl ModelCallSettlement {
    pub fn invocation_id(&self) -> Option<uuid::Uuid> {
        self.permit.as_ref().map(AdmissionPermit::invocation_id)
    }

    fn permit(&self) -> Option<&AdmissionPermit> {
        self.permit.as_ref()
    }

    fn disarm(&mut self) {
        self.recovery = None;
        self.permit = None;
    }

    pub(crate) async fn settle_result<T>(
        mut self,
        store: &Arc<Mutex<Store>>,
        event_bus: &Arc<EventBus>,
        completion: InvocationCompletion,
        result: Result<T>,
        context: &str,
    ) -> Result<T> {
        let permit = self
            .permit()
            .expect("real model-call settlement retains its permit");
        let invocation_id = permit.invocation_id();
        let settlement = complete_invocation(store, permit, completion, event_bus).await;
        if settlement.is_ok() {
            self.disarm();
        }
        match (result, settlement) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(settle_error)) => Err(DaemonError::Store(format!(
                "{context} completed but failed to settle model invocation {invocation_id}: {settle_error}"
            ))),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(settle_error)) => Err(DaemonError::Store(format!(
                "{context} failed with {error}; additionally failed to settle model invocation {invocation_id}: {settle_error}"
            ))),
        }
    }
}

impl Drop for ModelCallSettlement {
    fn drop(&mut self) {
        let (Some(permit), Some(recovery)) = (self.permit.take(), self.recovery.take()) else {
            return;
        };
        settle_abandoned_call(permit, recovery);
    }
}

#[async_trait::async_trait]
pub trait ModelCallControl: Send + Sync {
    async fn admit(
        &self,
        kind: ModelCallKind,
        dedup_hint: &str,
        fingerprint_hint: &str,
        retry_of_invocation_id: Option<uuid::Uuid>,
    ) -> Result<AdmittedModelCall>;

    async fn complete(&self, call: ModelCallSettlement, usage: ModelCallUsage) -> Result<()>;

    async fn fail(&self, call: ModelCallSettlement, error_class: &str) -> Result<()>;

    /// Fail the carried initial admission before execution authority is
    /// claimed. Cancellation paths use this before any provider send.
    async fn fail_pending(&self, error_class: &str) -> Result<()>;
}

#[cfg(test)]
#[derive(Default)]
pub struct NoopModelCallControl;

#[cfg(test)]
#[async_trait::async_trait]
impl ModelCallControl for NoopModelCallControl {
    async fn admit(
        &self,
        _kind: ModelCallKind,
        _dedup_hint: &str,
        _fingerprint_hint: &str,
        _retry_of_invocation_id: Option<uuid::Uuid>,
    ) -> Result<AdmittedModelCall> {
        Ok(AdmittedModelCall::noop())
    }

    async fn complete(&self, _call: ModelCallSettlement, _usage: ModelCallUsage) -> Result<()> {
        Ok(())
    }

    async fn fail(&self, _call: ModelCallSettlement, _error_class: &str) -> Result<()> {
        Ok(())
    }

    async fn fail_pending(&self, _error_class: &str) -> Result<()> {
        Ok(())
    }
}

pub struct StoreBackedModelCallControl {
    store: Arc<Mutex<Store>>,
    event_bus: Arc<EventBus>,
    settlements: ModelCallSettlementHandle,
    owner: InvocationOwner,
    provider: String,
    model: Option<String>,
    backend: String,
    effort: Option<String>,
    trigger: String,
    primary_purpose: ModelInvocationPurpose,
    compaction_purpose: Option<ModelInvocationPurpose>,
    execution_boundary: RuntimeExecutionRoute,
    state: Mutex<StoreBackedState>,
}

struct StoreBackedState {
    root_invocation_id: uuid::Uuid,
    initial_primary_permit: Option<AdmissionPermit>,
    next_primary_ordinal: u32,
    next_compaction_ordinal: u32,
}

impl StoreBackedModelCallControl {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: Arc<Mutex<Store>>,
        event_bus: Arc<EventBus>,
        settlements: ModelCallSettlementHandle,
        owner: InvocationOwner,
        provider: impl Into<String>,
        model: Option<String>,
        backend: impl Into<String>,
        effort: Option<String>,
        trigger: impl Into<String>,
        initial_primary_permit: AdmissionPermit,
        primary_purpose: ModelInvocationPurpose,
        compaction_purpose: Option<ModelInvocationPurpose>,
        execution_boundary: RuntimeExecutionRoute,
    ) -> Self {
        Self {
            store,
            event_bus,
            settlements,
            owner,
            provider: provider.into(),
            model,
            backend: backend.into(),
            effort,
            trigger: trigger.into(),
            primary_purpose,
            compaction_purpose,
            execution_boundary,
            state: Mutex::new(StoreBackedState {
                root_invocation_id: initial_primary_permit.invocation_id(),
                initial_primary_permit: Some(initial_primary_permit),
                next_primary_ordinal: 0,
                next_compaction_ordinal: 0,
            }),
        }
    }

    fn owner_scope_label(&self, root_invocation_id: uuid::Uuid) -> String {
        self.owner
            .session_id
            .map(|id| id.to_string())
            .or_else(|| self.owner.project_id.map(|id| id.to_string()))
            .or_else(|| self.owner.workflow_id.map(|id| id.to_string()))
            .or_else(|| self.owner.operator.clone())
            .unwrap_or_else(|| root_invocation_id.to_string())
    }
}

impl Drop for StoreBackedModelCallControl {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        let Some(permit) = state.initial_primary_permit.take() else {
            return;
        };
        settle_abandoned_call(
            permit,
            ModelCallSettlementRecovery {
                settlements: self.settlements.clone(),
            },
        );
    }
}

#[async_trait::async_trait]
impl ModelCallControl for StoreBackedModelCallControl {
    async fn admit(
        &self,
        kind: ModelCallKind,
        dedup_hint: &str,
        fingerprint_hint: &str,
        retry_of_invocation_id: Option<uuid::Uuid>,
    ) -> Result<AdmittedModelCall> {
        let (permit, purpose, ordinal, root_invocation_id) = {
            let mut state = self.state.lock().await;
            if matches!(kind, ModelCallKind::Primary)
                && retry_of_invocation_id.is_none()
                && let Some(permit) = state.initial_primary_permit.take()
            {
                state.next_primary_ordinal = 1;
                (
                    Some(permit),
                    self.primary_purpose,
                    state.next_primary_ordinal,
                    state.root_invocation_id,
                )
            } else {
                let purpose = match kind {
                    ModelCallKind::Primary => self.primary_purpose,
                    ModelCallKind::Compaction => self.compaction_purpose.ok_or_else(|| {
                        DaemonError::PolicyDenied(
                            "model compaction purpose not registered for this boundary".to_string(),
                        )
                    })?,
                };
                let ordinal = match kind {
                    ModelCallKind::Primary => {
                        state.next_primary_ordinal += 1;
                        state.next_primary_ordinal
                    }
                    ModelCallKind::Compaction => {
                        state.next_compaction_ordinal += 1;
                        state.next_compaction_ordinal
                    }
                };
                (None, purpose, ordinal, state.root_invocation_id)
            }
        };
        if let Some(permit) = permit {
            return AdmittedModelCall::real(
                permit,
                self.execution_boundary,
                self.settlements.clone(),
            );
        }

        let owner_scope = self.owner_scope_label(root_invocation_id);
        let retry_lineage = retry_of_invocation_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "root".to_string());
        // The ordinal and `dedup_hint` restart with every controller, so a
        // continued or rotated session (a new run of the same owner) would
        // otherwise rebuild the previous run's keys and hit a dedup conflict
        // on its second model call (#1039). The run's root invocation id is
        // unique per controller and stable for its own retries.
        let dedup_key = format!(
            "{}:{}:{}:{}:{}:run:{}",
            purpose.as_str(),
            owner_scope,
            ordinal,
            retry_lineage,
            dedup_hint,
            root_invocation_id
        );
        let fingerprint = hash_request_fingerprint(&[
            purpose.as_str(),
            &owner_scope,
            &ordinal.to_string(),
            &retry_lineage,
            fingerprint_hint,
        ]);
        let request = ModelAdmissionRequest {
            purpose,
            provider: Some(self.provider.clone()),
            model: self.model.clone(),
            backend: Some(self.backend.clone()),
            effort: self.effort.clone(),
            trigger: self.trigger.clone(),
            owner: self.owner.clone(),
            dedup_key: Some(dedup_key),
            request_fingerprint: Some(fingerprint),
            parent_invocation_id: Some(root_invocation_id),
            retry_of_invocation_id,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                purpose,
                Some(self.provider.as_str()),
                Some(self.backend.as_str()),
                self.model.as_deref(),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        match admit_invocation(&self.store, request, &self.event_bus).await? {
            AdmissionDecision::Admitted(permit) => {
                AdmittedModelCall::real(permit, self.execution_boundary, self.settlements.clone())
            }
            AdmissionDecision::Duplicate { invocation_id } => Err(DaemonError::PolicyDenied(
                format!("duplicate boundary admission blocked backend execution: {invocation_id}"),
            )),
        }
    }

    async fn complete(&self, mut call: ModelCallSettlement, usage: ModelCallUsage) -> Result<()> {
        let Some(permit) = call.permit() else {
            call.disarm();
            return Ok(());
        };
        let result = complete_invocation(
            &self.store,
            permit,
            InvocationCompletion {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_creation_tokens: usage.cache_creation_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                reasoning_tokens: usage.reasoning_tokens,
                embedding_input_count: usage.embedding_input_count,
                wall_time_ms: usage.wall_time_ms,
                estimated_cost_usd: crate::model_control::cost::resolve_invocation_cost(
                    &self.provider,
                    self.model.as_deref(),
                    &usage,
                ),
                confidence: usage.confidence.or(Some(ModelUsageConfidence::Measured)),
                ..InvocationCompletion::default()
            },
            &self.event_bus,
        )
        .await;
        if result.is_ok() {
            call.disarm();
        }
        result
    }

    async fn fail(&self, mut call: ModelCallSettlement, error_class: &str) -> Result<()> {
        let Some(permit) = call.permit() else {
            call.disarm();
            return Ok(());
        };
        let result = complete_invocation(
            &self.store,
            permit,
            InvocationCompletion {
                error_class: Some(error_class.to_string()),
                confidence: Some(ModelUsageConfidence::Unavailable),
                ..InvocationCompletion::default()
            },
            &self.event_bus,
        )
        .await;
        if result.is_ok() {
            call.disarm();
        }
        result
    }

    async fn fail_pending(&self, error_class: &str) -> Result<()> {
        let permit = {
            let mut state = self.state.lock().await;
            state.initial_primary_permit.take()
        };
        let Some(permit) = permit else {
            return Ok(());
        };
        let mut call = ModelCallSettlement {
            permit: Some(permit),
            recovery: Some(ModelCallSettlementRecovery {
                settlements: self.settlements.clone(),
            }),
        };
        let permit = call
            .permit()
            .expect("pending model call retains its admission permit");
        let result = complete_invocation(
            &self.store,
            permit,
            InvocationCompletion {
                error_class: Some(error_class.to_string()),
                confidence: Some(ModelUsageConfidence::Unavailable),
                ..InvocationCompletion::default()
            },
            &self.event_bus,
        )
        .await;
        if result.is_ok() {
            call.disarm();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_control::{AdmissionDecision, ExpectedUsage, complete_invocation_by_id};
    use crate::store::manager_resources::tests::fixture;
    use rsi_common::harness_manager_v2::ManagerPolicyV2;
    use rsi_common::model_control::ModelInvocationPurpose;
    use std::time::Duration;

    async fn root_permit(
        store: &Arc<Mutex<Store>>,
        event_bus: &Arc<EventBus>,
        session_id: uuid::Uuid,
    ) -> AdmissionPermit {
        purpose_permit(
            store,
            event_bus,
            session_id,
            ModelInvocationPurpose::SessionLaunchFresh,
            &format!("root:{session_id}"),
        )
        .await
    }

    async fn purpose_permit(
        store: &Arc<Mutex<Store>>,
        event_bus: &Arc<EventBus>,
        session_id: uuid::Uuid,
        purpose: ModelInvocationPurpose,
        dedup_key: &str,
    ) -> AdmissionPermit {
        // Paid background work is denied by default, so a helper purpose is
        // admitted through the local (free) backend like the real callers.
        let (provider, model, backend) = if purpose.is_background_helper() {
            ("Local", "qwen3:4b", "ollama")
        } else {
            ("Harness", "gpt-5.4", "Harness")
        };
        let request = ModelAdmissionRequest {
            purpose,
            provider: Some(provider.to_string()),
            model: Some(model.to_string()),
            backend: Some(backend.to_string()),
            effort: Some("high".to_string()),
            trigger: "test_model_call_settlement".to_string(),
            owner: InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            dedup_key: Some(dedup_key.to_string()),
            request_fingerprint: Some(format!("sha256:{dedup_key}")),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(ExpectedUsage {
                input_tokens: 1,
                output_tokens: 1,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                reasoning_tokens: 0,
                embedding_input_count: 0,
                wall_time_ms: 1_000,
            }),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        match admit_invocation(store, request, event_bus)
            .await
            .expect("admission")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                panic!("unexpected duplicate admission: {invocation_id}")
            }
        }
    }

    fn control(
        store: &Arc<Mutex<Store>>,
        event_bus: &Arc<EventBus>,
        settlements: ModelCallSettlementHandle,
        session_id: uuid::Uuid,
        permit: AdmissionPermit,
        trigger: &str,
    ) -> StoreBackedModelCallControl {
        StoreBackedModelCallControl::new(
            Arc::clone(store),
            Arc::clone(event_bus),
            settlements,
            InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            "Harness",
            Some("gpt-5.4".to_string()),
            "Harness",
            Some("high".to_string()),
            trigger,
            permit,
            ModelInvocationPurpose::SessionHarnessTurn,
            Some(ModelInvocationPurpose::SessionHarnessCompaction),
            RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
        )
    }

    fn invocation_row(store: &Store, invocation_id: uuid::Uuid) -> (String, Option<String>) {
        store
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations WHERE id = ?1",
                rusqlite::params![invocation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("invocation row")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn dropped_settlement_identity_fails_the_row_exactly_once() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let event_bus = Arc::new(EventBus::new(8));
        let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
            .expect("settlement worker");
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, &event_bus, session_id).await;
        let invocation_id = permit.invocation_id();
        let control = control(
            &store,
            &event_bus,
            worker.handle().expect("settlement producer"),
            session_id,
            permit,
            "test_model_call_settlement",
        );

        let admitted = control
            .admit(ModelCallKind::Primary, "turn:0", "request", None)
            .await
            .expect("admitted call");
        let (settlement, execution) = admitted.into_parts();
        drop(execution);
        drop(settlement);
        worker.drain().await.expect("drop settlement drains");

        let guard = store.lock().await;
        let row = invocation_row(&guard, invocation_id);
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("model_call_task_exited"));
        drop(guard);

        let second = crate::model_control::complete_invocation_by_id(
            &store,
            invocation_id,
            InvocationCompletion {
                error_class: Some("second_settlement".to_string()),
                ..InvocationCompletion::default()
            },
            &event_bus,
        )
        .await;
        assert!(second.is_ok(), "terminal settlement is idempotent");
        let guard = store.lock().await;
        let error_class: Option<String> = guard
            .conn
            .query_row(
                "SELECT error_class FROM model_invocations WHERE id = ?1",
                rusqlite::params![invocation_id.to_string()],
                |row| row.get(0),
            )
            .expect("settled invocation");
        assert_eq!(
            error_class.as_deref(),
            Some("model_call_task_exited"),
            "a repeated settlement cannot rewrite the terminal row"
        );
        drop(guard);
        drop(control);
        worker.shutdown().await.expect("settlement worker shutdown");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transient_settlement_failure_retries_without_shutdown_and_releases_cohort_capacity() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let (config, lead) = {
            let guard = store.lock().await;
            fixture(&guard, ManagerPolicyV2::default())
        };
        let event_bus = Arc::new(EventBus::new(8));
        let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
            .expect("settlement worker");
        let permit = root_permit(&store, &event_bus, lead.id).await;
        let invocation_id = permit.invocation_id();
        let control = control(
            &store,
            &event_bus,
            worker.handle().expect("settlement producer"),
            lead.id,
            permit,
            "test_transient_settlement_failure",
        );
        let admitted = control
            .admit(ModelCallKind::Primary, "turn:0", "request", None)
            .await
            .expect("admitted call");
        let (settlement, execution) = admitted.into_parts();
        drop(execution);
        {
            let guard = store.lock().await;
            guard
                .conn
                .execute_batch(&format!(
                    "CREATE TEMP TRIGGER fail_model_settlement
                     BEFORE UPDATE OF status ON model_invocations
                     WHEN NEW.id = '{invocation_id}' AND NEW.status = 'failed'
                     BEGIN SELECT RAISE(FAIL, 'forced model settlement failure'); END;"
                ))
                .expect("install transient settlement failure");
            assert_eq!(
                guard.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
                1
            );
        }
        drop(settlement);
        drop(control);

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !lock_unpoisoned(&worker.handle.inner.state)
                    .retained
                    .is_empty()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("initial settlement failure is retained");
        {
            let guard = store.lock().await;
            assert_eq!(invocation_row(&guard, invocation_id).0, "running");
            assert_eq!(
                guard.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
                1
            );
            guard
                .conn
                .execute_batch("DROP TRIGGER fail_model_settlement")
                .expect("clear transient settlement failure");
        }

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let guard = store.lock().await;
                if invocation_row(&guard, invocation_id).0 == "failed" {
                    assert_eq!(
                        guard.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
                        0
                    );
                    break;
                }
                drop(guard);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("timer retries settlement without shutdown or a later job");
        assert!(
            lock_unpoisoned(&worker.handle.inner.state)
                .retained
                .is_empty()
        );
        worker.shutdown().await.expect("settlement worker shutdown");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn background_helper_calls_do_not_hold_lead_cohort_capacity() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let (config, lead) = {
            let guard = store.lock().await;
            fixture(
                &guard,
                ManagerPolicyV2 {
                    max_active_sessions: 1,
                    ..ManagerPolicyV2::default()
                },
            )
        };
        let event_bus = Arc::new(EventBus::new(8));
        // Four helper calls in flight (a failed, unsettled helper stays
        // `running`) against a cap of one.
        let helpers = [
            ModelInvocationPurpose::SessionTitle,
            ModelInvocationPurpose::SessionSummary,
            ModelInvocationPurpose::MemoryObservationExtract,
            ModelInvocationPurpose::MemoryEmbeddingIndex,
        ];
        let mut helper_ids = Vec::new();
        for (index, purpose) in helpers.into_iter().enumerate() {
            let permit = purpose_permit(
                &store,
                &event_bus,
                lead.id,
                purpose,
                &format!("helper:{index}"),
            )
            .await;
            helper_ids.push(permit.invocation_id());
        }
        {
            let guard = store.lock().await;
            for id in &helper_ids {
                assert_eq!(invocation_row(&guard, *id).0, "running");
            }
            assert_eq!(
                guard.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
                0,
                "in-flight helper calls are not lead capacity"
            );
        }
        // A lead call is still admitted, and is the only counted slot.
        let lead_permit = root_permit(&store, &event_bus, lead.id).await;
        {
            let guard = store.lock().await;
            assert_eq!(
                invocation_row(&guard, lead_permit.invocation_id()).0,
                "running"
            );
            assert_eq!(
                guard.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
                1
            );
        }
        // With the lead cap full, another helper call is still admitted.
        let late_helper = purpose_permit(
            &store,
            &event_bus,
            lead.id,
            ModelInvocationPurpose::SessionTitle,
            "helper:late",
        )
        .await;
        let guard = store.lock().await;
        assert_eq!(
            invocation_row(&guard, late_helper.invocation_id()).0,
            "running"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permanently_failing_settlement_is_abandoned_with_a_typed_failure_and_frees_capacity() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let (config, lead) = {
            let guard = store.lock().await;
            fixture(&guard, ManagerPolicyV2::default())
        };
        let event_bus = Arc::new(EventBus::new(8));
        let worker = ModelCallSettlementWorker::with_policy(
            Arc::clone(&store),
            Arc::clone(&event_bus),
            SettlementPolicy {
                retry_interval: Duration::from_millis(20),
                max_retained_age: Duration::from_millis(300),
                ..SettlementPolicy::default()
            },
        )
        .expect("settlement worker");
        let permit = root_permit(&store, &event_bus, lead.id).await;
        let invocation_id = permit.invocation_id();
        let control = control(
            &store,
            &event_bus,
            worker.handle().expect("settlement producer"),
            lead.id,
            permit,
            "test_permanent_settlement_failure",
        );
        let admitted = control
            .admit(ModelCallKind::Primary, "turn:0", "request", None)
            .await
            .expect("admitted call");
        let (settlement, execution) = admitted.into_parts();
        drop(execution);
        {
            // The original completion can never be written; only the minimal
            // typed abandonment failure can.
            let guard = store.lock().await;
            guard
                .conn
                .execute_batch(&format!(
                    "CREATE TEMP TRIGGER fail_model_settlement
                     BEFORE UPDATE OF status ON model_invocations
                     WHEN NEW.id = '{invocation_id}' AND NEW.status = 'failed'
                       AND COALESCE(NEW.error_class, '') <> '{SETTLEMENT_ABANDONED_ERROR_CLASS}'
                     BEGIN SELECT RAISE(FAIL, 'permanent model settlement failure'); END;"
                ))
                .expect("install permanent settlement failure");
            assert_eq!(
                guard.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
                1
            );
        }
        drop(settlement);
        drop(control);

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let guard = store.lock().await;
                let (status, error_class) = invocation_row(&guard, invocation_id);
                if status == "failed" {
                    assert_eq!(
                        error_class.as_deref(),
                        Some(SETTLEMENT_ABANDONED_ERROR_CLASS)
                    );
                    assert_eq!(
                        guard.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
                        0,
                        "the abandoned settlement freed its capacity slot"
                    );
                    break;
                }
                drop(guard);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a settlement that never writes gives up after the bound");
        assert!(
            lock_unpoisoned(&worker.handle.inner.state)
                .retained
                .is_empty()
        );
        worker.shutdown().await.expect("settlement worker shutdown");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unwritable_abandonment_releases_capacity_in_memory_and_settles_once_on_recovery() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let (config, lead) = {
            let guard = store.lock().await;
            fixture(&guard, ManagerPolicyV2::default())
        };
        let event_bus = Arc::new(EventBus::new(8));
        let worker = ModelCallSettlementWorker::with_policy(
            Arc::clone(&store),
            Arc::clone(&event_bus),
            SettlementPolicy {
                retry_interval: Duration::from_millis(20),
                max_retained_age: Duration::from_millis(200),
                ..SettlementPolicy::default()
            },
        )
        .expect("settlement worker");
        let permit = root_permit(&store, &event_bus, lead.id).await;
        let invocation_id = permit.invocation_id();
        let control = control(
            &store,
            &event_bus,
            worker.handle().expect("settlement producer"),
            lead.id,
            permit,
            "test_unwritable_abandonment",
        );
        let admitted = control
            .admit(ModelCallKind::Primary, "turn:0", "request", None)
            .await
            .expect("admitted call");
        let (settlement, execution) = admitted.into_parts();
        drop(execution);
        {
            // Unconditional: no completion of any class can be written.
            let guard = store.lock().await;
            guard
                .conn
                .execute_batch(&format!(
                    "CREATE TEMP TRIGGER fail_every_model_settlement
                     BEFORE UPDATE OF status ON model_invocations
                     WHEN NEW.id = '{invocation_id}' AND NEW.status = 'failed'
                     BEGIN SELECT RAISE(FAIL, 'database refuses every settlement'); END;"
                ))
                .expect("install unconditional settlement failure");
            assert_eq!(
                guard.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
                1
            );
        }
        drop(settlement);
        drop(control);

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let guard = store.lock().await;
                let released = capacity_released_ids_json().contains(&invocation_id.to_string());
                if released {
                    assert_eq!(
                        invocation_row(&guard, invocation_id).0,
                        "running",
                        "the row cannot be written yet"
                    );
                    assert_eq!(
                        guard.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
                        0,
                        "capacity is released in memory within the bound"
                    );
                    break;
                }
                drop(guard);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("capacity is released within the bound");
        assert!(
            !lock_unpoisoned(&worker.handle.inner.state)
                .retained
                .is_empty(),
            "the row stays retained for reconciliation"
        );

        {
            let guard = store.lock().await;
            guard
                .conn
                .execute_batch("DROP TRIGGER fail_every_model_settlement")
                .expect("database recovers");
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let guard = store.lock().await;
                let (status, error_class) = invocation_row(&guard, invocation_id);
                if status == "failed" {
                    assert_eq!(
                        error_class.as_deref(),
                        Some(SETTLEMENT_ABANDONED_ERROR_CLASS)
                    );
                    break;
                }
                drop(guard);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the row settles once writes recover");
        assert!(
            lock_unpoisoned(&worker.handle.inner.state)
                .retained
                .is_empty()
        );
        assert!(
            !capacity_released_ids_json().contains(&invocation_id.to_string()),
            "reconciled row no longer needs the in-memory release"
        );
        let guard = store.lock().await;
        assert_eq!(
            guard.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
            0
        );
        drop(guard);
        worker.shutdown().await.expect("settlement worker shutdown");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn orphaned_helper_rows_are_swept_but_live_and_lead_calls_are_not() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let (_config, lead) = {
            let guard = store.lock().await;
            fixture(&guard, ManagerPolicyV2::default())
        };
        let event_bus = Arc::new(EventBus::new(8));
        // Orphaned: the only permit handle is gone but the row is still running.
        let orphan = purpose_permit(
            &store,
            &event_bus,
            lead.id,
            ModelInvocationPurpose::SessionTitle,
            "helper:orphan",
        )
        .await
        .invocation_id();
        // Live: a handle is held, and its ledger row looks two hours old (the
        // wall clock jumped forward).
        let live_permit = purpose_permit(
            &store,
            &event_bus,
            lead.id,
            ModelInvocationPurpose::MemoryObservationExtract,
            "helper:live",
        )
        .await;
        let live = live_permit.invocation_id();
        let lead_call = root_permit(&store, &event_bus, lead.id)
            .await
            .invocation_id();
        {
            let guard = store.lock().await;
            let old = (chrono::Utc::now() - chrono::Duration::hours(2))
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            for id in [orphan, live, lead_call] {
                guard
                    .conn
                    .execute(
                        "UPDATE model_invocations SET started_at = ?2, created_at = ?2 WHERE id = ?1",
                        rusqlite::params![id.to_string(), old],
                    )
                    .expect("backdate invocation");
            }
        }
        let worker = ModelCallSettlementWorker::with_policy(
            Arc::clone(&store),
            Arc::clone(&event_bus),
            SettlementPolicy {
                sweep_interval: Duration::from_millis(30),
                helper_stale_age: Duration::from_secs(900),
                helper_orphan_grace: Duration::from_millis(100),
                ..SettlementPolicy::default()
            },
        )
        .expect("settlement worker");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let guard = store.lock().await;
                if invocation_row(&guard, orphan).0 == "failed" {
                    break;
                }
                drop(guard);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("orphaned helper row is swept within the bound");
        // Several more sweeps pass; the live and lead rows are untouched.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let guard = store.lock().await;
        assert_eq!(
            invocation_row(&guard, orphan).1.as_deref(),
            Some(HELPER_STALE_SWEPT_ERROR_CLASS)
        );
        assert_eq!(
            invocation_row(&guard, live).0,
            "running",
            "a live helper is not swept by a forward wall-clock jump"
        );
        assert_eq!(
            invocation_row(&guard, lead_call).0,
            "running",
            "the sweep never touches non-helper calls"
        );
        drop(guard);
        drop(live_permit);
        worker.shutdown().await.expect("settlement worker shutdown");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_healthy_helper_stream_is_not_swept_until_it_goes_inactive() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let (_config, lead) = {
            let guard = store.lock().await;
            fixture(&guard, ManagerPolicyV2::default())
        };
        let event_bus = Arc::new(EventBus::new(8));
        let stream = purpose_permit(
            &store,
            &event_bus,
            lead.id,
            ModelInvocationPurpose::SessionSummary,
            "helper:stream",
        )
        .await;
        let stream_id = stream.invocation_id();
        let worker = ModelCallSettlementWorker::with_policy(
            Arc::clone(&store),
            Arc::clone(&event_bus),
            SettlementPolicy {
                sweep_interval: Duration::from_millis(30),
                helper_stale_age: Duration::from_millis(300),
                helper_orphan_grace: Duration::from_millis(100),
                ..SettlementPolicy::default()
            },
        )
        .expect("settlement worker");
        // The call runs for over three times the stale age, but keeps
        // receiving activity, so it is never swept.
        for _ in 0..25 {
            stream.touch_activity();
            tokio::time::sleep(Duration::from_millis(40)).await;
            let guard = store.lock().await;
            assert_eq!(
                invocation_row(&guard, stream_id).0,
                "running",
                "a live, active stream must not be swept"
            );
        }
        // Once the stream goes silent past the stale age it is expired and
        // inactive, and is swept even though its handle is still held.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let guard = store.lock().await;
                if invocation_row(&guard, stream_id).0 == "failed" {
                    assert_eq!(
                        invocation_row(&guard, stream_id).1.as_deref(),
                        Some(HELPER_STALE_SWEPT_ERROR_CLASS)
                    );
                    break;
                }
                drop(guard);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("an inactive expired helper is swept");
        drop(stream);
        worker.shutdown().await.expect("settlement worker shutdown");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn controller_drop_without_a_runtime_settles_the_unused_initial_row() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let (store, invocation_id, control, worker) = runtime.block_on(async {
            let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
            let event_bus = Arc::new(EventBus::new(8));
            let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
                .expect("settlement worker");
            let session_id = uuid::Uuid::new_v4();
            let permit = root_permit(&store, &event_bus, session_id).await;
            let invocation_id = permit.invocation_id();
            let control = control(
                &store,
                &event_bus,
                worker.handle().expect("settlement producer"),
                session_id,
                permit,
                "test_no_runtime_controller_drop",
            );
            (store, invocation_id, control, worker)
        });
        drop(runtime);

        drop(control);
        worker
            .drain_blocking()
            .expect("settlement drains without a runtime");

        let guard = store.blocking_lock();
        let row = invocation_row(&guard, invocation_id);
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("model_call_task_exited"));
        drop(guard);
        worker
            .shutdown_blocking()
            .expect("settlement worker shutdown");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn runtime_shutdown_settles_an_in_flight_call_before_task_ownership_is_released() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let (store, invocation_id, worker) = runtime.block_on(async {
            let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
            let event_bus = Arc::new(EventBus::new(8));
            let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
                .expect("settlement worker");
            let session_id = uuid::Uuid::new_v4();
            let permit = root_permit(&store, &event_bus, session_id).await;
            let invocation_id = permit.invocation_id();
            let control = Arc::new(control(
                &store,
                &event_bus,
                worker.handle().expect("settlement producer"),
                session_id,
                permit,
                "test_runtime_shutdown",
            ));
            let admitted = control
                .admit(ModelCallKind::Primary, "turn:0", "request", None)
                .await
                .expect("admitted call");
            let (settlement, execution) = admitted.into_parts();
            runtime.spawn(async move {
                let _settlement = settlement;
                let _execution = execution;
                std::future::pending::<()>().await;
            });
            (store, invocation_id, worker)
        });

        runtime.shutdown_timeout(Duration::from_secs(1));
        worker
            .drain_blocking()
            .expect("runtime shutdown settlement drains");

        let guard = store.blocking_lock();
        let row = invocation_row(&guard, invocation_id);
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("model_call_task_exited"));
        drop(guard);
        worker
            .shutdown_blocking()
            .expect("settlement worker shutdown");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn explicit_completion_wins_a_race_with_fallback_drop_settlement() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let event_bus = Arc::new(EventBus::new(8));
        let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
            .expect("settlement worker");
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, &event_bus, session_id).await;
        let invocation_id = permit.invocation_id();
        let control = control(
            &store,
            &event_bus,
            worker.handle().expect("settlement producer"),
            session_id,
            permit,
            "test_explicit_completion_race",
        );
        let admitted = control
            .admit(ModelCallKind::Primary, "turn:0", "request", None)
            .await
            .expect("admitted call");
        let (settlement, execution) = admitted.into_parts();
        drop(execution);

        complete_invocation_by_id(
            &store,
            invocation_id,
            InvocationCompletion {
                error_class: Some("explicit_terminal_result".to_string()),
                confidence: Some(ModelUsageConfidence::Unavailable),
                ..InvocationCompletion::default()
            },
            &event_bus,
        )
        .await
        .expect("explicit completion");
        drop(settlement);
        worker
            .drain()
            .await
            .expect("fallback observes terminal completion");

        let guard = store.lock().await;
        let row = invocation_row(&guard, invocation_id);
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("explicit_terminal_result"));
        drop(guard);
        drop(control);
        worker.shutdown().await.expect("settlement worker shutdown");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn settlement_drop_returns_while_store_mutex_is_contended_then_drains() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let event_bus = Arc::new(EventBus::new(8));
        let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
            .expect("settlement worker");
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, &event_bus, session_id).await;
        let invocation_id = permit.invocation_id();
        let control = control(
            &store,
            &event_bus,
            worker.handle().expect("settlement producer"),
            session_id,
            permit,
            "test_contended_settlement_drop",
        );
        let admitted = control
            .admit(ModelCallKind::Primary, "turn:0", "request", None)
            .await
            .expect("admitted call");
        let (settlement, execution) = admitted.into_parts();
        drop(execution);

        let guard = store.lock().await;
        let drop_task = tokio::spawn(async move {
            drop(settlement);
        });
        tokio::time::timeout(Duration::from_millis(250), drop_task)
            .await
            .expect("Drop must not wait for the contended Tokio store mutex")
            .expect("drop task");
        drop(guard);

        worker.drain().await.expect("contended settlement drains");
        let guard = store.lock().await;
        let row = invocation_row(&guard, invocation_id);
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("model_call_task_exited"));
        drop(guard);
        drop(control);
        worker.shutdown().await.expect("settlement worker shutdown");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn controller_drop_returns_while_store_mutex_is_contended_then_drains() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let event_bus = Arc::new(EventBus::new(8));
        let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
            .expect("settlement worker");
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, &event_bus, session_id).await;
        let invocation_id = permit.invocation_id();
        let control = control(
            &store,
            &event_bus,
            worker.handle().expect("settlement producer"),
            session_id,
            permit,
            "test_contended_controller_drop",
        );

        let guard = store.lock().await;
        let drop_task = tokio::spawn(async move {
            drop(control);
        });
        tokio::time::timeout(Duration::from_millis(250), drop_task)
            .await
            .expect("controller Drop must not wait for the contended Tokio store mutex")
            .expect("drop task");
        drop(guard);

        worker.drain().await.expect("contended settlement drains");
        let guard = store.lock().await;
        let row = invocation_row(&guard, invocation_id);
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("model_call_task_exited"));
        drop(guard);
        worker.shutdown().await.expect("settlement worker shutdown");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn shutdown_waits_for_a_late_settlement_owner_before_returning_success() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let event_bus = Arc::new(EventBus::new(8));
        let worker = Arc::new(
            ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
                .expect("settlement worker"),
        );
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, &event_bus, session_id).await;
        let invocation_id = permit.invocation_id();
        let control = control(
            &store,
            &event_bus,
            worker.handle().expect("settlement producer"),
            session_id,
            permit,
            "test_shutdown_waits_for_late_owner",
        );
        let admitted = control
            .admit(ModelCallKind::Primary, "turn:0", "request", None)
            .await
            .expect("admitted call");
        let (settlement, execution) = admitted.into_parts();
        drop(execution);
        drop(control);

        let shutdown = {
            let worker = Arc::clone(&worker);
            tokio::spawn(async move { worker.shutdown().await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !shutdown.is_finished(),
            "shutdown must wait while a settlement producer is still live"
        );
        assert!(
            worker.handle().is_err(),
            "shutdown must seal new top-level producer acquisition"
        );
        let late_producer = settlement
            .recovery
            .as_ref()
            .expect("settlement recovery")
            .settlements
            .clone();
        drop(settlement);
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            !shutdown.is_finished(),
            "a producer cloned after sealing must still delay shutdown"
        );
        drop(late_producer);

        tokio::time::timeout(Duration::from_secs(1), shutdown)
            .await
            .expect("shutdown completes after producer quiescence")
            .expect("shutdown task")
            .expect("settlement worker shutdown");
        let guard = store.lock().await;
        let row = invocation_row(&guard, invocation_id);
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("model_call_task_exited"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn shutdown_reports_a_live_producer_instead_of_abandoning_its_row() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let event_bus = Arc::new(EventBus::new(8));
        let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
            .expect("settlement worker");
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, &event_bus, session_id).await;
        let invocation_id = permit.invocation_id();
        let control = control(
            &store,
            &event_bus,
            worker.handle().expect("settlement producer"),
            session_id,
            permit,
            "test_shutdown_reports_live_producer",
        );
        let admitted = control
            .admit(ModelCallKind::Primary, "turn:0", "request", None)
            .await
            .expect("admitted call");
        let (settlement, execution) = admitted.into_parts();
        drop(execution);
        drop(control);

        let error = worker
            .shutdown()
            .await
            .expect_err("shutdown must surface a live settlement producer");
        assert!(
            error
                .to_string()
                .contains("timed out waiting for 1 model-call settlement producer"),
            "unexpected settlement error: {error}"
        );
        let guard = store.lock().await;
        let row = invocation_row(&guard, invocation_id);
        assert_eq!(row.0, "running");
        assert_eq!(row.1, None);
        drop(guard);

        drop(settlement);
        worker
            .shutdown()
            .await
            .expect("second shutdown drains the released producer");
        let guard = store.lock().await;
        let row = invocation_row(&guard, invocation_id);
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("model_call_task_exited"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_reply_timeout_retries_boundedly_then_returns_cached_success() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let event_bus = Arc::new(EventBus::new(8));
        let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
            .expect("settlement worker");
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, &event_bus, session_id).await;
        let invocation_id = permit.invocation_id();
        let control = control(
            &store,
            &event_bus,
            worker.handle().expect("settlement producer"),
            session_id,
            permit,
            "test_shutdown_reply_timeout_retry",
        );
        let admitted = control
            .admit(ModelCallKind::Primary, "turn:0", "request", None)
            .await
            .expect("admitted call");
        let (settlement, execution) = admitted.into_parts();
        drop(execution);
        drop(control);

        let store_guard = store.lock().await;
        drop(settlement);

        let first_error = worker
            .shutdown()
            .await
            .expect_err("the blocked settlement worker must time out");
        assert!(
            first_error
                .to_string()
                .contains("timed out waiting for model-call settlement worker completion"),
            "unexpected first shutdown error: {first_error}"
        );

        let retry_start = Instant::now();
        let retry_error = worker
            .shutdown()
            .await
            .expect_err("a still-blocked retry must remain bounded");
        assert!(
            retry_error
                .to_string()
                .contains("timed out waiting for model-call settlement worker completion"),
            "unexpected retry shutdown error: {retry_error}"
        );
        assert!(
            retry_start.elapsed() < Duration::from_secs(1),
            "retry must not block indefinitely in JoinHandle::join"
        );

        drop(store_guard);
        worker
            .shutdown()
            .await
            .expect("retry joins the completed worker and returns cached success");
        worker
            .shutdown()
            .await
            .expect("shutdown success remains idempotently cached");

        let guard = store.lock().await;
        let row = invocation_row(&guard, invocation_id);
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("model_call_task_exited"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_returns_the_cached_worker_error_after_join() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let event_bus = Arc::new(EventBus::new(8));
        let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
            .expect("settlement worker");
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, &event_bus, session_id).await;
        let control = control(
            &store,
            &event_bus,
            worker.handle().expect("settlement producer"),
            session_id,
            permit,
            "test_shutdown_cached_worker_error",
        );
        let admitted = control
            .admit(ModelCallKind::Primary, "turn:0", "request", None)
            .await
            .expect("admitted call");
        let (settlement, execution) = admitted.into_parts();
        drop(execution);
        drop(control);

        let guard = store.lock().await;
        guard
            .conn
            .execute_batch("DROP TABLE model_invocations")
            .expect("remove invocation table to force terminal persistence failure");
        drop(settlement);
        drop(guard);

        let first_error = worker
            .shutdown()
            .await
            .expect_err("worker must surface retained persistence failure");
        let retry_start = Instant::now();
        let cached_error = worker
            .shutdown()
            .await
            .expect_err("joined worker must preserve its terminal error");
        assert_eq!(cached_error.to_string(), first_error.to_string());
        assert!(
            retry_start.elapsed() < Duration::from_secs(1),
            "cached worker failure must return without another join wait"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aborted_fail_pending_hands_its_taken_permit_to_settlement_recovery() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let event_bus = Arc::new(EventBus::new(8));
        let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
            .expect("settlement worker");
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, &event_bus, session_id).await;
        let invocation_id = permit.invocation_id();
        let control = Arc::new(control(
            &store,
            &event_bus,
            worker.handle().expect("settlement producer"),
            session_id,
            permit,
            "test_aborted_fail_pending",
        ));

        let store_guard = store.lock().await;
        let fail_task = {
            let control = Arc::clone(&control);
            tokio::spawn(async move { control.fail_pending("cancelled_before_send").await })
        };
        tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                if control.state.lock().await.initial_primary_permit.is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fail_pending takes the permit before waiting on the store");
        fail_task.abort();
        assert!(
            fail_task
                .await
                .expect_err("fail_pending task is aborted")
                .is_cancelled(),
            "expected task cancellation"
        );
        drop(store_guard);

        worker
            .drain()
            .await
            .expect("aborted fail_pending settlement drains");
        let guard = store.lock().await;
        let row = invocation_row(&guard, invocation_id);
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("model_call_task_exited"));
        drop(guard);
        drop(control);
        worker.shutdown().await.expect("settlement worker shutdown");
    }

    /// Settles one call through the real controller for `provider`/`model` and
    /// returns the `estimated_cost_usd` the row recorded (#584/#585).
    async fn settled_cost(provider: &str, model: &str, usage: ModelCallUsage) -> Option<f64> {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let event_bus = Arc::new(EventBus::new(8));
        let worker = ModelCallSettlementWorker::new(Arc::clone(&store), Arc::clone(&event_bus))
            .expect("settlement worker");
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, &event_bus, session_id).await;
        let invocation_id = permit.invocation_id();
        let control = StoreBackedModelCallControl::new(
            Arc::clone(&store),
            Arc::clone(&event_bus),
            worker.handle().expect("settlement producer"),
            InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            provider,
            Some(model.to_string()),
            provider,
            Some("high".to_string()),
            "test_cost_resolution",
            permit,
            ModelInvocationPurpose::SessionHarnessTurn,
            None,
            RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
        );
        let admitted = control
            .admit(ModelCallKind::Primary, "turn:0", "request", None)
            .await
            .expect("admitted call");
        let (settlement, execution) = admitted.into_parts();
        drop(execution);
        control
            .complete(settlement, usage)
            .await
            .expect("completion");
        let cost = {
            let guard = store.lock().await;
            let cost = guard
                .conn
                .query_row(
                    "SELECT estimated_cost_usd FROM model_invocations WHERE id = ?1",
                    rusqlite::params![invocation_id.to_string()],
                    |row| row.get::<_, Option<f64>>(0),
                )
                .expect("invocation cost");
            assert_eq!(
                guard
                    .session_model_invocation_cost_total(session_id)
                    .expect("session total"),
                cost,
                "the session total is the sum of its invocation costs"
            );
            cost
        };
        drop(control);
        worker.shutdown().await.expect("settlement worker shutdown");
        cost
    }

    fn token_usage(input: u64, output: u64) -> ModelCallUsage {
        ModelCallUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            ..ModelCallUsage::default()
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn openrouter_provider_reported_cost_is_recorded_verbatim() {
        let mut usage = token_usage(1_000_000, 1_000_000);
        usage.estimated_cost_usd = Some(0.0421);
        let cost = settled_cost("OpenRouter", "vendor/unpriced-model", usage).await;
        assert_eq!(cost, Some(0.0421));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn harness_openrouter_estimates_cost_from_tokens_with_a_documented_price() {
        // minimax/minimax-m3: $0.30 in / $1.20 out per 1M tokens.
        let cost = settled_cost(
            "Harness",
            "minimax/minimax-m3",
            token_usage(1_000_000, 500_000),
        )
        .await
        .expect("known price estimates");
        assert!((cost - 0.90).abs() < 1e-9, "cost was {cost}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn codex_and_harness_models_without_a_price_keep_cost_null() {
        for (provider, model) in [
            ("Codex", "gpt-6-astra"),
            ("CodexAppServer", "gpt-5.4"),
            ("Harness", "gpt-5.4"),
        ] {
            let cost = settled_cost(provider, model, token_usage(10_000, 2_000)).await;
            assert_eq!(cost, None, "{provider}/{model} has no documented price");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn codex_provider_reported_cost_is_recorded_verbatim() {
        let mut usage = token_usage(10_000, 2_000);
        usage.estimated_cost_usd = Some(0.5);
        let cost = settled_cost("CodexAppServer", "gpt-6-astra", usage).await;
        assert_eq!(cost, Some(0.5));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test]
    async fn local_model_settles_a_real_zero_cost_not_null() {
        let cost = settled_cost("Local", "qwen3:14b", token_usage(10_000, 2_000)).await;
        assert_eq!(cost, Some(0.0));
    }
}
