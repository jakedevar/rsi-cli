//! Deadline accounting for the daemon's independent liveness watchdog.
//!
//! The probe runner lives outside Tokio so it can still act when every
//! executor worker is blocked. This module keeps its decision policy separate
//! from process exit and persistence, which also makes failure cases testable.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::store::Store;

pub const WATCHDOG_EXIT_CODE: i32 = 75;
const PROBE_INTERVAL: Duration = Duration::from_secs(30);
const PROBE_DEADLINE: Duration = Duration::from_secs(5);
const PROBE_KEY: &str = "internal_watchdog_probe_nonce";
const SIDECAR_PREFIX: &str = "daemon-watchdog-restart-";

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailedProbe {
    Rpc,
    Store,
    Scheduler,
    Reconciliation,
}

/// A responsive Store can report an operational error without being wedged.
/// Only a missed deadline is evidence for a watchdog restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreProbeOutcome {
    Responsive,
    Error,
    TimedOut,
}

impl FailedProbe {
    pub fn code(self) -> &'static str {
        match self {
            Self::Rpc => "rpc_timeout",
            Self::Store => "store_probe_timeout",
            Self::Scheduler => "scheduler_tick_stale",
            Self::Reconciliation => "reconciliation_tick_stale",
        }
    }
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
            startup_grace: Duration::from_secs(300),
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

/// A small crash record written without using the possibly wedged Store.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RestartRecord {
    pub version: u8,
    pub id: Uuid,
    pub observed_at: DateTime<Utc>,
    pub last_healthy_at: DateTime<Utc>,
    pub failed_probes: Vec<String>,
}

impl RestartRecord {
    fn from_trip(trip: &WatchdogTrip) -> Self {
        Self {
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
}

pub fn sidecar_path(data_dir: &Path, id: Uuid) -> PathBuf {
    data_dir.join(format!("{SIDECAR_PREFIX}{id}.json"))
}

pub fn pending_restart_record_paths(data_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(data_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix(SIDECAR_PREFIX)
            .and_then(|suffix| suffix.strip_suffix(".json"))
            .is_some_and(|id| Uuid::parse_str(id).is_ok())
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

/// Rename and sync the containing directory so a Store deadlock cannot erase
/// the reason for the next boot. The caller must hold the daemon instance lease.
pub fn write_restart_record(path: &Path, record: &RestartRecord) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("sidecar has no parent"))?;
    if path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "watchdog restart record already exists",
        ));
    }
    let temporary = parent.join(format!(".{SIDECAR_PREFIX}{}.tmp", record.id));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn read_restart_record(path: &Path) -> std::io::Result<Option<RestartRecord>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let record: RestartRecord = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    if record.version != 1
        || path.file_name() != sidecar_path(Path::new(""), record.id).file_name()
        || record.failed_probes.is_empty()
        || record.last_healthy_at > record.observed_at
        || record.failed_probes.iter().any(|code| {
            ![
                FailedProbe::Rpc.code(),
                FailedProbe::Store.code(),
                FailedProbe::Scheduler.code(),
                FailedProbe::Reconciliation.code(),
            ]
            .contains(&code.as_str())
        })
    {
        return Err(std::io::Error::other("invalid watchdog restart record"));
    }
    Ok(Some(record))
}

/// Replay crash sidecars after the Store has opened. `persist` must return
/// only after its insert transaction commits; its insert must be idempotent by
/// restart ID because an unlink or directory sync can fail after that commit.
/// Invalid records remain on disk so the operator can inspect the raw bytes.
pub fn import_pending_restart_records(
    data_dir: &Path,
    mut persist: impl FnMut(&RestartRecord) -> std::io::Result<()>,
) -> std::io::Result<Vec<RestartRecord>> {
    let mut imported = Vec::new();
    for path in pending_restart_record_paths(data_dir)? {
        let record = match read_restart_record(&path) {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "invalid watchdog restart sidecar retained");
                continue;
            }
        };
        persist(&record)?;
        std::fs::remove_file(&path)?;
        imported.push(record);
    }
    if !imported.is_empty() {
        std::fs::File::open(data_dir)?.sync_all()?;
    }
    Ok(imported)
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
                    let record = RestartRecord::from_trip(&trip);
                    let metrics = runtime.metrics();
                    let thread_waits = thread_wait_snapshot();
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
                        "daemon watchdog tripped"
                    );
                    if let Err(error) =
                        write_restart_record(&sidecar_path(&data_dir, record.id), &record)
                    {
                        tracing::error!(%error, "watchdog restart evidence could not be persisted");
                    }
                    std::process::exit(WATCHDOG_EXIT_CODE);
                }
            }
        })?;
    Ok(WatchdogHandle {
        stop,
        thread: Some(thread),
    })
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

    #[test]
    fn persistent_store_timeout_trips_after_grace_and_records_last_health() {
        let started = Utc::now();
        let mut decision = WatchdogDecision::new(WatchdogPolicy::from_intervals(60, 120), started);
        let mut before_grace = observation(started, 299);
        before_grace.store_probe = StoreProbeOutcome::TimedOut;
        assert!(decision.observe(&before_grace).is_none());

        let healthy_at = started + chrono::Duration::seconds(300);
        assert!(decision.observe(&observation(healthy_at, 300)).is_none());
        for seconds in [330_u64, 360] {
            let mut failed =
                observation(started + chrono::Duration::seconds(seconds as i64), seconds);
            failed.store_probe = StoreProbeOutcome::TimedOut;
            assert!(decision.observe(&failed).is_none());
        }
        let mut failed = observation(started + chrono::Duration::seconds(390), 390);
        failed.store_probe = StoreProbeOutcome::TimedOut;
        let trip = decision.observe(&failed).expect("third missed probe trips");
        assert_eq!(trip.failed, vec![FailedProbe::Store]);
        assert_eq!(trip.last_healthy_at, healthy_at);
    }

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
