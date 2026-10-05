//! Deadline accounting for the daemon's independent liveness watchdog.
//!
//! The probe runner lives outside Tokio so it can still act when every
//! executor worker is blocked. This module keeps its decision policy separate
//! from process exit and persistence, which also makes failure cases testable.

use chrono::{DateTime, Utc};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::store::Store;
pub use crate::store_support::restart_record::{
    FailedProbe, RestartRecord, import_pending_restart_records, pending_restart_record_paths,
    read_restart_record, sidecar_path, write_restart_record,
};

pub const WATCHDOG_EXIT_CODE: i32 = 75;
const PROBE_INTERVAL: Duration = Duration::from_secs(30);
const PROBE_DEADLINE: Duration = Duration::from_secs(5);
const PROBE_KEY: &str = "internal_watchdog_probe_nonce";

/// A completion marker shared by a daemon loop and the watchdog thread.
#[derive(Clone, Debug)]
pub struct LoopHeartbeat {
    started: Arc<Instant>,
    last_completed_millis: Arc<AtomicU64>,
}

impl Default for LoopHeartbeat {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopHeartbeat {
    pub fn new() -> Self {
        Self {
            started: Arc::new(Instant::now()),
            last_completed_millis: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn mark_completed(&self) {
        let elapsed = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        self.last_completed_millis.store(elapsed, Ordering::Release);
    }

    pub fn age(&self) -> Duration {
        let now = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let completed = self.last_completed_millis.load(Ordering::Acquire);
        Duration::from_millis(now.saturating_sub(completed))
    }

    #[cfg(test)]
    pub(crate) fn completed_at_millis(&self) -> u64 {
        self.last_completed_millis.load(Ordering::Acquire)
    }
}

/// A responsive Store can report an operational error without being wedged.
/// Only a missed deadline is evidence for a watchdog restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreProbeOutcome {
    Responsive,
    Error,
    TimedOut,
}

#[derive(Clone, Debug)]
pub struct ProbeObservation {
    pub observed_at: DateTime<Utc>,
    pub uptime: Duration,
    pub rpc_ok: bool,
    pub store_probe: StoreProbeOutcome,
    pub scheduler_age: Option<Duration>,
    pub reconciliation_age: Option<Duration>,
}

#[derive(Clone, Debug)]
pub struct WatchdogPolicy {
    pub startup_grace: Duration,
    pub scheduler_max_age: Duration,
    pub reconciliation_max_age: Duration,
    pub consecutive_failures: u8,
}

impl WatchdogPolicy {
    /// Loop bounds scale with operator-selected poll intervals. The minimum
    /// leaves room for a legitimate slow job without hiding an hour-long stall.
    pub fn from_intervals(scheduler_secs: u64, reconciliation_secs: u64) -> Self {
        Self {
            // Counted from watchdog start, which is after `request_ready`: the
            // third #1166 wedge began 20 s after a restart and a 5-minute grace
            // delayed its trip past 6 minutes.
            startup_grace: Duration::from_secs(90),
            scheduler_max_age: Duration::from_secs(scheduler_secs.saturating_mul(3).max(300)),
            reconciliation_max_age: Duration::from_secs(
                reconciliation_secs.saturating_mul(3).max(360),
            ),
            consecutive_failures: 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchdogTrip {
    pub failed: Vec<FailedProbe>,
    pub last_healthy_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
}

pub struct WatchdogDecision {
    policy: WatchdogPolicy,
    consecutive_unhealthy: u8,
    last_healthy_at: DateTime<Utc>,
}

fn restart_record_from_trip(trip: &WatchdogTrip) -> RestartRecord {
    RestartRecord {
        version: 1,
        id: Uuid::new_v4(),
        observed_at: trip.observed_at,
        last_healthy_at: trip.last_healthy_at,
        failed_probes: trip
            .failed
            .iter()
            .map(|probe| probe.code().to_owned())
            .collect(),
    }
}

pub struct WatchdogHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WatchdogHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// Run from a dedicated OS thread. One Tokio task at most is admitted for the
/// Store challenge, even when the Store mutex never becomes available.
pub fn start_watchdog(
    socket_path: PathBuf,
    store: Arc<Mutex<Store>>,
    scheduler: Option<LoopHeartbeat>,
    reconciliation: Option<LoopHeartbeat>,
    policy: WatchdogPolicy,
    data_dir: PathBuf,
) -> std::io::Result<WatchdogHandle> {
    let runtime = tokio::runtime::Handle::current();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("rsid-watchdog".to_owned())
        .spawn(move || {
            let started = Instant::now();
            let mut decision = WatchdogDecision::new(policy, Utc::now());
            let rpc_probe_running = Arc::new(AtomicBool::new(false));
            let store_probe_running = Arc::new(AtomicBool::new(false));
            while !stop_thread.load(Ordering::Acquire) {
                std::thread::park_timeout(PROBE_INTERVAL);
                if stop_thread.load(Ordering::Acquire) {
                    break;
                }
                let rpc_ok = rpc_probe_single_flight(
                    &socket_path,
                    &rpc_probe_running,
                    PROBE_DEADLINE,
                );
                let store_outcome =
                    store_probe(&runtime, &store, &store_probe_running, PROBE_DEADLINE);
                let observation = ProbeObservation {
                    observed_at: Utc::now(),
                    uptime: started.elapsed(),
                    rpc_ok,
                    store_probe: store_outcome,
                    scheduler_age: scheduler.as_ref().map(LoopHeartbeat::age),
                    reconciliation_age: reconciliation.as_ref().map(LoopHeartbeat::age),
                };
                if let Some(trip) = decision.observe(&observation) {
                    let record = restart_record_from_trip(&trip);
                    let metrics = runtime.metrics();
                    let thread_waits =
                        bounded_thread_waits(Duration::from_secs(2), thread_wait_snapshot);
                    tracing::error!(
                        restart_id = %record.id,
                        failed_probes = ?record.failed_probes,
                        last_healthy_at = %record.last_healthy_at,
                        scheduler_age_ms = ?observation.scheduler_age.map(|age| age.as_millis()),
                        reconciliation_age_ms = ?observation.reconciliation_age.map(|age| age.as_millis()),
                        store_lock_contended = store.try_lock().is_err(),
                        store_probe_pending = store_probe_running.load(Ordering::Acquire),
                        rpc_probe_pending = rpc_probe_running.load(Ordering::Acquire),
                        tokio_workers = metrics.num_workers(),
                        tokio_alive_tasks = metrics.num_alive_tasks(),
                        tokio_global_queue_depth = metrics.global_queue_depth(),
                        thread_waits = ?thread_waits,
                        launch_phases = ?crate::launch_breadcrumbs::snapshot(),
                        "daemon watchdog tripped"
                    );
                    if let Err(error) =
                        write_restart_record(&sidecar_path(&data_dir, record.id), &record)
                    {
                        tracing::error!(%error, "watchdog restart evidence could not be persisted");
                    }
                    write_thread_stacks(&data_dir, record.id);
                    std::process::exit(WATCHDOG_EXIT_CODE);
                }
            }
        })?;
    Ok(WatchdogHandle {
        stop,
        thread: Some(thread),
    })
}

/// Persist every thread's kernel wait state next to the restart record so a
/// wedge tells a thread blocked on a lock from an idle park. The collection and
/// the write run on a detached thread and the watchdog waits at most 2 s of wall
/// clock: `/proc` reads of a wedged thread can block in the kernel, and this must
/// never stop the restart that follows.
fn write_thread_stacks(data_dir: &Path, restart_id: Uuid) {
    let path = data_dir.join(format!("daemon-watchdog-stacks-{restart_id}.txt"));
    let shown = path.display().to_string();
    let deadline = Duration::from_secs(2);
    let written = crate::thread_stacks::write_snapshot_bounded(path, deadline, move || {
        format!(
            "# launch phases (id phase age)\n{}\n{}",
            crate::launch_breadcrumbs::snapshot().join("\n"),
            crate::thread_stacks::thread_wait_report(deadline)
        )
    });
    if written {
        tracing::error!(restart_id = %restart_id, path = %shown, "watchdog thread wait snapshot written");
    }
}

/// The legacy comm/wchan snapshot for the trip log, collected on a detached
/// thread under a hard wall-clock deadline: a `/proc` read that blocks in the
/// kernel must never delay the restart (#1173).
fn bounded_thread_waits(
    deadline: Duration,
    collect: impl FnOnce() -> Vec<String> + Send + 'static,
) -> Vec<String> {
    crate::thread_stacks::bounded_call("watchdog-waits", deadline, collect)
        .unwrap_or_else(|| vec!["<thread wait snapshot timed out>".to_owned()])
}

#[cfg(target_os = "linux")]
fn thread_wait_snapshot() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    let mut ids = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .collect::<Vec<_>>();
    ids.sort();
    ids.into_iter()
        .take(128)
        .map(|id| {
            let task = Path::new("/proc/self/task").join(&id);
            let name = std::fs::read_to_string(task.join("comm")).unwrap_or_default();
            let wait = std::fs::read_to_string(task.join("wchan")).unwrap_or_default();
            format!("{}:{}:{}", id.to_string_lossy(), name.trim(), wait.trim())
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn thread_wait_snapshot() -> Vec<String> {
    Vec::new()
}

fn rpc_probe(socket_path: &Path, deadline: Duration) -> bool {
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket_path) else {
        return false;
    };
    if stream.set_read_timeout(Some(deadline)).is_err()
        || stream.set_write_timeout(Some(deadline)).is_err()
        || stream
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"GetHealthStatus\",\"params\":null}\n",
            )
            .is_err()
    {
        return false;
    }
    let mut response = String::new();
    if std::io::BufReader::new(stream)
        .read_line(&mut response)
        .is_err()
        || response.len() > 64 * 1024
    {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&response) else {
        return false;
    };
    value.get("id") == Some(&serde_json::Value::from(1)) && value.get("result").is_some()
}

fn rpc_probe_single_flight(
    socket_path: &Path,
    running: &Arc<AtomicBool>,
    deadline: Duration,
) -> bool {
    if running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    let socket_path = socket_path.to_path_buf();
    let worker_running = Arc::clone(running);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    if std::thread::Builder::new()
        .name("rsid-watchdog-rpc".to_owned())
        .spawn(move || {
            let ok = rpc_probe(&socket_path, deadline);
            let _ = tx.send(ok);
            worker_running.store(false, Ordering::Release);
        })
        .is_err()
    {
        running.store(false, Ordering::Release);
        return false;
    }
    rx.recv_timeout(deadline).unwrap_or(false)
}

fn store_probe(
    runtime: &tokio::runtime::Handle,
    store: &Arc<Mutex<Store>>,
    running: &Arc<AtomicBool>,
    deadline: Duration,
) -> StoreProbeOutcome {
    if running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return StoreProbeOutcome::TimedOut;
    }
    let store = Arc::clone(store);
    let running = Arc::clone(running);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    runtime.spawn(async move {
        let nonce = Uuid::new_v4().to_string();
        let outcome = {
            let guard = store.lock().await;
            match guard.set_daemon_setting(PROBE_KEY, &nonce) {
                Ok(()) => match guard.get_daemon_setting(PROBE_KEY) {
                    Ok(Some(value)) if value == nonce => StoreProbeOutcome::Responsive,
                    Ok(_) => {
                        tracing::warn!("watchdog Store challenge returned unexpected data");
                        StoreProbeOutcome::Error
                    }
                    Err(error) => {
                        tracing::warn!(%error, "watchdog Store challenge read failed");
                        StoreProbeOutcome::Error
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "watchdog Store challenge write failed");
                    StoreProbeOutcome::Error
                }
            }
        };
        let _ = tx.send(outcome);
        running.store(false, Ordering::Release);
    });
    match rx.recv_timeout(deadline) {
        Ok(outcome) => outcome,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => StoreProbeOutcome::TimedOut,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => StoreProbeOutcome::Error,
    }
}

impl WatchdogDecision {
    pub fn new(policy: WatchdogPolicy, started_at: DateTime<Utc>) -> Self {
        Self {
            policy,
            consecutive_unhealthy: 0,
            last_healthy_at: started_at,
        }
    }

    pub fn observe(&mut self, observation: &ProbeObservation) -> Option<WatchdogTrip> {
        if observation.uptime < self.policy.startup_grace {
            return None;
        }
        let mut failed = Vec::with_capacity(4);
        if !observation.rpc_ok {
            failed.push(FailedProbe::Rpc);
        }
        if observation.store_probe == StoreProbeOutcome::TimedOut {
            failed.push(FailedProbe::Store);
        }
        if observation
            .scheduler_age
            .is_some_and(|age| age > self.policy.scheduler_max_age)
        {
            failed.push(FailedProbe::Scheduler);
        }
        if observation
            .reconciliation_age
            .is_some_and(|age| age > self.policy.reconciliation_max_age)
        {
            failed.push(FailedProbe::Reconciliation);
        }
        if failed.is_empty() {
            self.consecutive_unhealthy = 0;
            self.last_healthy_at = observation.observed_at;
            return None;
        }
        self.consecutive_unhealthy = self.consecutive_unhealthy.saturating_add(1);
        (self.consecutive_unhealthy >= self.policy.consecutive_failures).then(|| WatchdogTrip {
            failed,
            last_healthy_at: self.last_healthy_at,
            observed_at: observation.observed_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(at: DateTime<Utc>, uptime_secs: u64) -> ProbeObservation {
        ProbeObservation {
            observed_at: at,
            uptime: Duration::from_secs(uptime_secs),
            rpc_ok: true,
            store_probe: StoreProbeOutcome::Responsive,
            scheduler_age: Some(Duration::ZERO),
            reconciliation_age: Some(Duration::ZERO),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn a_stalled_legacy_wait_snapshot_cannot_delay_the_trip_log() {
        let started = Instant::now();
        let waits = bounded_thread_waits(Duration::from_millis(300), || {
            loop {
                std::thread::park();
            }
        });
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "the trip path waited {:?} for a stalled collector",
            started.elapsed()
        );
        assert_eq!(waits, vec!["<thread wait snapshot timed out>".to_owned()]);
        assert_eq!(
            bounded_thread_waits(Duration::from_secs(2), || vec!["tid:comm:futex".into()]),
            vec!["tid:comm:futex".to_owned()]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn persistent_store_timeout_trips_after_grace_and_records_last_health() {
        let started = Utc::now();
        let mut decision = WatchdogDecision::new(WatchdogPolicy::from_intervals(60, 120), started);
        let mut before_grace = observation(started, 89);
        before_grace.store_probe = StoreProbeOutcome::TimedOut;
        assert!(decision.observe(&before_grace).is_none());

        let healthy_at = started + chrono::Duration::seconds(90);
        assert!(decision.observe(&observation(healthy_at, 90)).is_none());
        for seconds in [120_u64, 150] {
            let mut failed =
                observation(started + chrono::Duration::seconds(seconds as i64), seconds);
            failed.store_probe = StoreProbeOutcome::TimedOut;
            assert!(decision.observe(&failed).is_none());
        }
        let mut failed = observation(started + chrono::Duration::seconds(180), 180);
        failed.store_probe = StoreProbeOutcome::TimedOut;
        let trip = decision.observe(&failed).expect("third missed probe trips");
        assert_eq!(trip.failed, vec![FailedProbe::Store]);
        assert_eq!(trip.last_healthy_at, healthy_at);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn responsive_store_errors_do_not_trigger_a_restart() {
        let started = Utc::now();
        let mut decision = WatchdogDecision::new(WatchdogPolicy::from_intervals(60, 120), started);
        for seconds in [300_u64, 330, 360, 390] {
            let mut errored =
                observation(started + chrono::Duration::seconds(seconds as i64), seconds);
            errored.store_probe = StoreProbeOutcome::Error;
            assert!(decision.observe(&errored).is_none());
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn recovery_resets_failures_and_disabled_loops_are_omitted() {
        let started = Utc::now();
        let mut decision = WatchdogDecision::new(WatchdogPolicy::from_intervals(60, 120), started);
        let mut failed = observation(started + chrono::Duration::seconds(300), 300);
        failed.rpc_ok = false;
        failed.scheduler_age = None;
        failed.reconciliation_age = None;
        assert!(decision.observe(&failed).is_none());
        let recovered_at = started + chrono::Duration::seconds(330);
        let mut recovered = observation(recovered_at, 330);
        recovered.scheduler_age = None;
        recovered.reconciliation_age = None;
        assert!(decision.observe(&recovered).is_none());
        for seconds in [360, 390] {
            failed.observed_at = started + chrono::Duration::seconds(seconds);
            failed.uptime = Duration::from_secs(seconds as u64);
            assert!(decision.observe(&failed).is_none());
        }
        failed.observed_at = started + chrono::Duration::seconds(420);
        failed.uptime = Duration::from_secs(420);
        let trip = decision
            .observe(&failed)
            .expect("three consecutive failures");
        assert_eq!(trip.failed, vec![FailedProbe::Rpc]);
        assert_eq!(trip.last_healthy_at, recovered_at);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn stale_scheduler_tick_trips_while_other_probes_answer() {
        let started = Utc::now();
        let mut decision = WatchdogDecision::new(WatchdogPolicy::from_intervals(60, 120), started);
        let mut stale = observation(started + chrono::Duration::seconds(600), 600);
        stale.scheduler_age = Some(Duration::from_secs(301));
        assert!(decision.observe(&stale).is_none());
        assert!(decision.observe(&stale).is_none());
        assert_eq!(
            decision
                .observe(&stale)
                .expect("stale scheduler trips")
                .failed,
            vec![FailedProbe::Scheduler]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn restart_sidecar_is_atomic_and_roundtrips() {
        let directory = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let record = RestartRecord {
            version: 1,
            id: Uuid::new_v4(),
            observed_at: now,
            last_healthy_at: now - chrono::Duration::seconds(30),
            failed_probes: vec![FailedProbe::Store.code().to_owned()],
        };
        let path = sidecar_path(directory.path(), record.id);
        assert_eq!(read_restart_record(&path).unwrap(), None);
        write_restart_record(&path, &record).unwrap();
        assert_eq!(read_restart_record(&path).unwrap(), Some(record.clone()));
        assert_eq!(
            write_restart_record(&path, &record).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert_eq!(
            pending_restart_record_paths(directory.path()).unwrap(),
            vec![path]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn malformed_restart_record_remains_for_diagnosis() {
        let directory = tempfile::tempdir().unwrap();
        let path = sidecar_path(directory.path(), Uuid::new_v4());
        std::fs::write(&path, b"{").unwrap();
        assert!(read_restart_record(&path).is_err());
        assert_eq!(
            pending_restart_record_paths(directory.path()).unwrap(),
            vec![path]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn restart_record_identity_matches_its_sidecar_name() {
        let directory = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let record = RestartRecord {
            version: 1,
            id: Uuid::new_v4(),
            observed_at: now,
            last_healthy_at: now,
            failed_probes: vec![FailedProbe::Rpc.code().to_owned()],
        };
        let path = sidecar_path(directory.path(), Uuid::new_v4());
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(read_restart_record(&path).is_err());
        assert_eq!(
            pending_restart_record_paths(directory.path()).unwrap(),
            vec![path]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn restart_import_retains_invalid_records_and_unlinks_only_after_commit() {
        let directory = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let record = RestartRecord {
            version: 1,
            id: Uuid::new_v4(),
            observed_at: now,
            last_healthy_at: now,
            failed_probes: vec![FailedProbe::Store.code().to_owned()],
        };
        let valid = sidecar_path(directory.path(), record.id);
        write_restart_record(&valid, &record).unwrap();
        let invalid = sidecar_path(directory.path(), Uuid::new_v4());
        std::fs::write(&invalid, b"{").unwrap();

        let failed = import_pending_restart_records(directory.path(), |_| {
            Err(std::io::Error::other("commit failed"))
        });
        assert!(failed.is_err());
        assert!(valid.exists());
        assert!(invalid.exists());

        let mut persisted = Vec::new();
        let imported = import_pending_restart_records(directory.path(), |record| {
            persisted.push(record.id);
            Ok(())
        })
        .unwrap();
        assert_eq!(imported, vec![record.clone()]);
        assert_eq!(persisted, vec![record.id]);
        assert!(!valid.exists());
        assert!(invalid.exists());
        assert!(
            import_pending_restart_records(directory.path(), |_| Ok(()))
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn rpc_challenge_requires_a_matching_response() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("probe.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            let mut reader = std::io::BufReader::new(stream);
            reader.read_line(&mut request).unwrap();
            assert!(request.contains("GetHealthStatus"));
            reader
                .get_mut()
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n")
                .unwrap();
        });
        let running = Arc::new(AtomicBool::new(false));
        assert!(rpc_probe_single_flight(
            &socket,
            &running,
            Duration::from_secs(1)
        ));
        server.join().unwrap();
        assert!(!rpc_probe_single_flight(
            &socket,
            &running,
            Duration::from_millis(20),
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn blocked_store_probe_keeps_one_pending_challenge() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let guard = store.lock().await;
        let runtime = tokio::runtime::Handle::current();
        let running = Arc::new(AtomicBool::new(false));
        let first = {
            let store = Arc::clone(&store);
            let running = Arc::clone(&running);
            let runtime = runtime.clone();
            tokio::task::spawn_blocking(move || {
                store_probe(&runtime, &store, &running, Duration::from_millis(20))
            })
            .await
            .unwrap()
        };
        assert_eq!(first, StoreProbeOutcome::TimedOut);
        assert!(running.load(Ordering::Acquire));
        let second = {
            let store = Arc::clone(&store);
            let running = Arc::clone(&running);
            tokio::task::spawn_blocking(move || {
                store_probe(&runtime, &store, &running, Duration::from_millis(20))
            })
            .await
            .unwrap()
        };
        assert_eq!(second, StoreProbeOutcome::TimedOut);
        drop(guard);
        tokio::time::timeout(Duration::from_secs(1), async {
            while running.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let runtime = tokio::runtime::Handle::current();
        let third = tokio::task::spawn_blocking(move || {
            store_probe(&runtime, &store, &running, Duration::from_secs(1))
        })
        .await
        .unwrap();
        assert_eq!(third, StoreProbeOutcome::Responsive);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn fast_store_write_error_is_reported_without_a_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("watchdog.sqlite");
        let store = Arc::new(Mutex::new(Store::open(&database).unwrap()));
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER watchdog_probe_write_failure
                 BEFORE INSERT ON daemon_settings
                 WHEN NEW.key = 'internal_watchdog_probe_nonce'
                 BEGIN SELECT RAISE(ABORT, 'simulated write failure'); END;",
            )
            .unwrap();
        let runtime = tokio::runtime::Handle::current();
        let running = Arc::new(AtomicBool::new(false));
        let outcome = tokio::task::spawn_blocking(move || {
            store_probe(&runtime, &store, &running, Duration::from_secs(1))
        })
        .await
        .unwrap();
        assert_eq!(outcome, StoreProbeOutcome::Error);
    }
}
