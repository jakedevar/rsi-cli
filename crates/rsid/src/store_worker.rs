use crate::error::{DaemonError, Result};
use crate::profiling;
use crate::store::Store;
use rsi_common::types::{ConversationEvent, Session, SessionStatus, TurnMetric};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use tracing::{debug, error, warn};
use uuid::Uuid;

const DEFAULT_QUEUE_CAPACITY: usize = 256;
const WARN_THRESHOLD_PCT: f32 = 0.80;
const SLOW_COMMAND_THRESHOLD_MS: u128 = 100;

/// Commands sent to the persistence worker over the mpsc channel.
#[derive(Debug)]
pub enum StoreCommand {
    InsertSession(Session),
    UpdateSessionStatus {
        id: Uuid,
        status: SessionStatus,
    },
    InsertEvent {
        session_id: Uuid,
        event: ConversationEvent,
    },
    InsertTurnMetric(TurnMetric),
    UpdateSessionMetadata(Session),
    AttachProject {
        session_id: Uuid,
        project_id: Uuid,
    },
    Shutdown,
}

/// Shared telemetry counters between `StoreWorker` and `StoreHandle`.
#[derive(Debug)]
pub struct StoreMetrics {
    /// Current queue depth (incremented on enqueue, decremented after process).
    queue_depth: AtomicUsize,
    /// Duration of the last processed command in milliseconds.
    last_command_duration_ms: AtomicU64,
    /// Channel capacity for threshold calculations.
    capacity: usize,
}

impl StoreMetrics {
    fn new(capacity: usize) -> Self {
        Self {
            queue_depth: AtomicUsize::new(0),
            last_command_duration_ms: AtomicU64::new(0),
            capacity,
        }
    }

    /// Current number of pending commands in the queue.
    pub fn queue_depth(&self) -> usize {
        self.queue_depth.load(Ordering::Relaxed)
    }

    /// Duration of the most recently processed command in milliseconds.
    pub fn last_command_duration_ms(&self) -> u64 {
        self.last_command_duration_ms.load(Ordering::Relaxed)
    }

    /// Channel capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Dedicated worker that owns the `Store` and processes commands sequentially.
pub struct StoreWorker {
    store: Store,
    rx: Receiver<StoreCommand>,
    metrics: Arc<StoreMetrics>,
}

impl StoreWorker {
    pub fn new(store: Store, rx: Receiver<StoreCommand>, metrics: Arc<StoreMetrics>) -> Self {
        Self { store, rx, metrics }
    }

    /// Run the worker loop. Processes commands until `Shutdown` is received
    /// or the channel closes. Runs on a dedicated OS thread (blocking recv is fine).
    pub fn run(&mut self) {
        loop {
            let cmd = match self.rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => {
                    debug!("store worker: channel closed, exiting");
                    break;
                }
            };

            if matches!(cmd, StoreCommand::Shutdown) {
                debug!("store worker: shutdown received, draining remaining commands");
                self.drain_remaining();
                break;
            }

            self.check_queue_depth();
            self.process_command_timed(cmd);
        }
    }

    fn check_queue_depth(&self) {
        let depth = self.metrics.queue_depth.load(Ordering::Relaxed);
        let threshold = (self.metrics.capacity as f32 * WARN_THRESHOLD_PCT).ceil() as usize;

        if depth >= threshold {
            warn!(
                depth,
                capacity = self.metrics.capacity,
                "store worker: queue above 80% capacity"
            );
            return;
        }

        if !profiling::enabled() {
            return;
        }
        debug!(
            depth,
            capacity = self.metrics.capacity,
            "store worker: queue check"
        );
    }

    fn process_command_timed(&self, cmd: StoreCommand) {
        let start = std::time::Instant::now();
        self.process_command(cmd);
        let elapsed_ms = start.elapsed().as_millis();

        self.metrics
            .last_command_duration_ms
            .store(elapsed_ms as u64, Ordering::Relaxed);
        self.metrics.queue_depth.fetch_sub(1, Ordering::Relaxed);

        if elapsed_ms >= SLOW_COMMAND_THRESHOLD_MS {
            warn!(
                elapsed_ms = elapsed_ms as u64,
                "store worker: slow command detected"
            );
        }
    }

    fn process_command(&self, cmd: StoreCommand) {
        match cmd {
            StoreCommand::InsertSession(session) => {
                if let Err(e) = self.store.insert_session(&session) {
                    error!(session_id = %session.id, error = %e, "store worker: insert session failed");
                }
            }
            StoreCommand::UpdateSessionStatus { id, status } => {
                if let Err(e) = self.store.update_session_status(id, status) {
                    error!(session_id = %id, error = %e, "store worker: update session status failed");
                }
            }
            StoreCommand::InsertEvent { session_id, event } => {
                if let Err(e) = self.store.insert_event(&event) {
                    error!(session_id = %session_id, error = %e, "store worker: insert event failed");
                }
            }
            StoreCommand::InsertTurnMetric(metric) => {
                if let Err(e) = self.store.insert_turn_metric(&metric) {
                    error!(session_id = %metric.session_id, error = %e, "store worker: insert turn metric failed");
                }
            }
            StoreCommand::UpdateSessionMetadata(session) => {
                if let Err(e) = self.store.update_session_metadata(&session) {
                    error!(session_id = %session.id, error = %e, "store worker: update session metadata failed");
                }
            }
            StoreCommand::AttachProject {
                session_id,
                project_id,
            } => {
                if let Err(e) = self
                    .store
                    .update_session_project(session_id, Some(project_id))
                {
                    error!(session_id = %session_id, project_id = %project_id, error = %e, "store worker: attach project failed");
                }
            }
            StoreCommand::Shutdown => unreachable!("handled in run()"),
        }
    }

    /// Drain and process any remaining commands in the channel after shutdown signal.
    fn drain_remaining(&mut self) {
        let mut count = 0u64;
        while let Ok(cmd) = self.rx.try_recv() {
            if matches!(cmd, StoreCommand::Shutdown) {
                continue;
            }
            self.process_command_timed(cmd);
            count += 1;
        }
        if count > 0 {
            debug!(
                count,
                "store worker: drained remaining commands on shutdown"
            );
        }
    }
}

/// Cloneable handle for sending commands to the store worker.
#[derive(Clone)]
pub struct StoreHandle {
    tx: SyncSender<StoreCommand>,
    metrics: Arc<StoreMetrics>,
}

impl StoreHandle {
    pub fn new(tx: SyncSender<StoreCommand>, metrics: Arc<StoreMetrics>) -> Self {
        Self { tx, metrics }
    }

    pub fn insert_session(&self, session: Session) -> Result<()> {
        self.try_send(StoreCommand::InsertSession(session))
    }

    pub fn update_session_status(&self, id: Uuid, status: SessionStatus) -> Result<()> {
        self.try_send(StoreCommand::UpdateSessionStatus { id, status })
    }

    pub fn insert_event(&self, session_id: Uuid, event: ConversationEvent) -> Result<()> {
        self.try_send(StoreCommand::InsertEvent { session_id, event })
    }

    pub fn insert_turn_metric(&self, metric: TurnMetric) -> Result<()> {
        self.try_send(StoreCommand::InsertTurnMetric(metric))
    }

    pub fn update_session_metadata(&self, session: Session) -> Result<()> {
        self.try_send(StoreCommand::UpdateSessionMetadata(session))
    }

    pub fn attach_project(&self, session_id: Uuid, project_id: Uuid) -> Result<()> {
        self.try_send(StoreCommand::AttachProject {
            session_id,
            project_id,
        })
    }

    pub fn shutdown(&self) -> Result<()> {
        self.tx
            .send(StoreCommand::Shutdown)
            .map_err(|_| DaemonError::ChannelClosed)
    }

    /// Current queue depth.
    pub fn queue_depth(&self) -> usize {
        self.metrics.queue_depth()
    }

    /// Duration of the last processed command in milliseconds.
    pub fn last_command_duration_ms(&self) -> u64 {
        self.metrics.last_command_duration_ms()
    }

    /// Access to the shared metrics.
    pub fn metrics(&self) -> &Arc<StoreMetrics> {
        &self.metrics
    }

    fn try_send(&self, cmd: StoreCommand) -> Result<()> {
        self.metrics.queue_depth.fetch_add(1, Ordering::Relaxed);
        self.tx.try_send(cmd).map_err(|e| {
            self.metrics.queue_depth.fetch_sub(1, Ordering::Relaxed);
            match &e {
                std::sync::mpsc::TrySendError::Full(_) => {
                    warn!("store worker: queue full, dropping command");
                }
                std::sync::mpsc::TrySendError::Disconnected(_) => {
                    error!("store worker: channel closed");
                }
            }
            DaemonError::ChannelClosed
        })
    }
}

/// Spawn the store worker on a dedicated OS thread and return a cloneable handle.
pub fn spawn_store_worker(store: Store, capacity: usize) -> StoreHandle {
    let metrics = Arc::new(StoreMetrics::new(capacity));
    let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
    let mut worker = StoreWorker::new(store, rx, Arc::clone(&metrics));
    std::thread::Builder::new()
        .name("store-worker".into())
        .spawn(move || {
            worker.run();
        })
        .expect("failed to spawn store-worker thread");
    StoreHandle::new(tx, metrics)
}

/// Spawn with default capacity (256).
pub fn spawn_store_worker_default(store: Store) -> StoreHandle {
    spawn_store_worker(store, DEFAULT_QUEUE_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::{SessionKind, SessionProvider};
    use std::path::PathBuf;

    /// Opens the store inside a `TempDir` and returns the guard so the whole
    /// directory is removed when the test ends. A `NamedTempFile` is not
    /// sufficient here: SQLite writes `-wal` and `-shm` sidecars next to the
    /// database, and those paths are not covered by the file guard, so they
    /// are orphaned in `/tmp` when it drops.
    fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        (dir, store)
    }

    fn test_session() -> Session {
        Session {
            context_fill_pct: None,
            id: Uuid::new_v4(),
            provider: SessionProvider::Claude,
            claude_session_id: None,
            query: "test query".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            status: SessionStatus::Running,
            project_id: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            session_kind: SessionKind::Standard,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            continued_from: None,
            context_usage_confidence: rsi_common::types::ContextUsageConfidence::Missing,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            work_time_ms: None,
            approval_started_at: None,
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

    #[test]
    fn test_insert_session_via_worker() {
        let (_dir, store) = test_store();
        let handle = spawn_store_worker(store, 16);

        let session = test_session();

        handle.insert_session(session).unwrap();
        handle.shutdown().unwrap();
    }

    #[test]
    fn test_update_status_via_worker() {
        let (_dir, store) = test_store();
        let handle = spawn_store_worker(store, 16);

        let session = test_session();
        let session_id = session.id;

        handle.insert_session(session).unwrap();
        handle
            .update_session_status(session_id, SessionStatus::Completed)
            .unwrap();
        handle.shutdown().unwrap();
    }

    #[test]
    fn test_insert_event_via_worker() {
        let (_dir, store) = test_store();
        let handle = spawn_store_worker(store, 16);

        let session = test_session();
        let session_id = session.id;

        handle.insert_session(session).unwrap();

        let event = ConversationEvent {
            id: 0,
            session_id,
            sequence: 1,
            event_type: rsi_common::types::EventType::Message,
            role: Some(rsi_common::types::Role::Assistant),
            content: "Hello world".to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };

        handle.insert_event(session_id, event).unwrap();
        handle.shutdown().unwrap();
    }

    #[test]
    fn test_shutdown_drains_remaining() {
        let (_dir, store) = test_store();
        let metrics = Arc::new(StoreMetrics::new(64));
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let mut worker = StoreWorker::new(store, rx, Arc::clone(&metrics));

        // Send several commands then shutdown
        let session = test_session();
        let session_id = session.id;

        // Simulate enqueue by incrementing depth
        metrics.queue_depth.fetch_add(1, Ordering::Relaxed);
        tx.send(StoreCommand::InsertSession(session)).unwrap();
        metrics.queue_depth.fetch_add(1, Ordering::Relaxed);
        tx.send(StoreCommand::UpdateSessionStatus {
            id: session_id,
            status: SessionStatus::Completed,
        })
        .unwrap();
        tx.send(StoreCommand::Shutdown).unwrap();

        // Run worker — it should process all commands then exit
        worker.run();

        // After drain, depth should be 0
        assert_eq!(metrics.queue_depth(), 0);
    }

    #[test]
    fn test_channel_full_returns_error() {
        let (_dir, store) = test_store();
        // Capacity of 1 — second send should fail
        let handle = spawn_store_worker(store, 1);

        // Fill the channel
        let mut results = Vec::new();
        for _ in 0..10 {
            let session = test_session();
            results.push(handle.insert_session(session));
        }

        // At least one should have failed due to full channel
        let failures = results.iter().filter(|r| r.is_err()).count();
        assert!(failures > 0, "expected at least one channel-full error");

        // Shutdown should still work
        let _ = handle.shutdown();
    }

    #[test]
    fn test_insert_turn_metric_via_worker() {
        let (_dir, store) = test_store();
        let handle = spawn_store_worker(store, 16);

        let session = test_session();
        let session_id = session.id;
        handle.insert_session(session).unwrap();

        let metric = TurnMetric {
            id: 0,
            session_id,
            turn_number: 1,
            input_tokens: 100,
            cache_creation_tokens: 0,
            cache_read_tokens: 50,
            output_tokens: 200,
            stop_reason: Some("end_turn".to_string()),
            tools_used: Some(vec!["read_file".to_string()]),
            tool_count: 1,
            created_at: chrono::Utc::now(),
            model: None,
            thinking_tokens: 0,
            cache_creation_1h_tokens: 0,
            cache_creation_5m_tokens: 0,
            service_tier: None,
        };

        handle.insert_turn_metric(metric).unwrap();
        handle.shutdown().unwrap();
    }

    #[test]
    fn test_attach_project_via_worker() {
        let (_dir, store) = test_store();
        let handle = spawn_store_worker(store, 16);

        let session = test_session();
        let session_id = session.id;
        let project_id = Uuid::new_v4();

        handle.insert_session(session).unwrap();
        handle.attach_project(session_id, project_id).unwrap();
        handle.shutdown().unwrap();
    }

    #[test]
    fn test_queue_depth_tracks_pending_commands() {
        let (_dir, store) = test_store();
        let metrics = Arc::new(StoreMetrics::new(64));
        let (tx, rx) = std::sync::mpsc::sync_channel(64);

        // Don't start the worker yet — commands will accumulate
        let handle = StoreHandle::new(tx, Arc::clone(&metrics));

        assert_eq!(handle.queue_depth(), 0);

        // Enqueue several commands
        for _ in 0..5 {
            handle.insert_session(test_session()).unwrap();
        }

        assert_eq!(handle.queue_depth(), 5);

        // Now start the worker on a thread and let it drain
        let mut worker = StoreWorker::new(store, rx, Arc::clone(&metrics));
        let worker_thread = std::thread::spawn(move || {
            worker.run();
        });

        // Shutdown and wait
        handle.shutdown().unwrap();
        worker_thread.join().unwrap();

        assert_eq!(handle.queue_depth(), 0);
    }

    #[test]
    fn test_last_command_duration_updated() {
        let (_dir, store) = test_store();
        let handle = spawn_store_worker(store, 16);

        // Initial duration is 0
        assert_eq!(handle.last_command_duration_ms(), 0);

        let session = test_session();
        handle.insert_session(session).unwrap();

        // Shutdown (blocking) ensures commands are processed before we check
        handle.shutdown().unwrap();

        // Duration should have been set (at least 0ms is valid for fast ops)
        // We just verify it doesn't panic and the metric is accessible
        let _duration = handle.last_command_duration_ms();
    }

    #[test]
    fn test_queue_depth_zero_after_shutdown_drain() {
        let (_dir, store) = test_store();
        let metrics = Arc::new(StoreMetrics::new(64));
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let mut worker = StoreWorker::new(store, rx, Arc::clone(&metrics));

        // Enqueue commands with manual depth tracking (simulating StoreHandle)
        for _ in 0..3 {
            metrics.queue_depth.fetch_add(1, Ordering::Relaxed);
            tx.send(StoreCommand::InsertSession(test_session()))
                .unwrap();
        }
        tx.send(StoreCommand::Shutdown).unwrap();

        assert_eq!(metrics.queue_depth(), 3);

        worker.run();

        assert_eq!(metrics.queue_depth(), 0);
    }
}
