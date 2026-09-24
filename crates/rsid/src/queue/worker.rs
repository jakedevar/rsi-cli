//! Queue worker: polling loop, work unit claiming, task dispatch.

use super::processor::TaskProcessor;
use super::types::{QueueConfig, TaskType};
use crate::bus::EventBus;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, error, info, warn};

const QUEUE_CHANNEL_CAPACITY: usize = 128;

/// Commands sent to the queue worker.
#[derive(Debug)]
pub enum QueueCommand {
    /// Enqueue a new task.
    Enqueue {
        work_unit_key: String,
        task_type: String,
        session_id: Option<String>,
        project_id: Option<String>,
        payload: String,
        token_count: i64,
        priority: i32,
    },
    /// Nudge the worker to check for eligible work immediately.
    Wake,
    /// Shut down gracefully.
    Shutdown,
}

/// Cloneable handle for interacting with the queue worker.
#[derive(Clone)]
pub struct QueueHandle {
    tx: mpsc::Sender<QueueCommand>,
}

impl QueueHandle {
    pub fn new(tx: mpsc::Sender<QueueCommand>) -> Self {
        Self { tx }
    }

    /// Enqueue a task and nudge the worker.
    pub async fn enqueue(
        &self,
        task_type: TaskType,
        session_id: &str,
        project_id: Option<&str>,
        payload: &str,
        token_count: i64,
    ) -> Result<()> {
        let work_unit_key = super::types::make_work_unit_key(task_type, project_id, session_id);
        self.tx
            .send(QueueCommand::Enqueue {
                work_unit_key,
                task_type: task_type.as_str().to_string(),
                session_id: Some(session_id.to_string()),
                project_id: project_id.map(|s| s.to_string()),
                payload: payload.to_string(),
                token_count,
                priority: 0,
            })
            .await
            .map_err(|_| DaemonError::ChannelClosed)
    }

    /// Wake the worker to process eligible items immediately.
    pub async fn wake(&self) -> Result<()> {
        self.tx
            .send(QueueCommand::Wake)
            .await
            .map_err(|_| DaemonError::ChannelClosed)
    }

    /// Shut down the queue worker gracefully.
    pub async fn shutdown(&self) -> Result<()> {
        self.tx
            .send(QueueCommand::Shutdown)
            .await
            .map_err(|_| DaemonError::ChannelClosed)
    }
}

/// Background queue worker. Runs as a tokio task.
pub struct QueueWorker {
    rx: mpsc::Receiver<QueueCommand>,
    store: Arc<Mutex<Store>>,
    #[allow(dead_code)]
    bus: Arc<EventBus>,
    config: QueueConfig,
    processor: Box<dyn TaskProcessor>,
}

impl QueueWorker {
    pub fn new(
        rx: mpsc::Receiver<QueueCommand>,
        store: Arc<Mutex<Store>>,
        bus: Arc<EventBus>,
        config: QueueConfig,
        processor: Box<dyn TaskProcessor>,
    ) -> Self {
        Self {
            rx,
            store,
            bus,
            config,
            processor,
        }
    }

    /// Run the worker loop.
    pub async fn run(&mut self) {
        info!(
            "queue worker: started (poll_interval={}s, token_threshold={})",
            self.config.poll_interval_secs, self.config.default_token_threshold,
        );

        // Clean stale claims on startup
        self.clean_stale_claims().await;

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            self.config.poll_interval_secs,
        ));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.process_eligible().await;
                    self.clean_stale_claims().await;
                    self.purge_completed().await;
                }
                cmd = self.rx.recv() => {
                    match cmd {
                        Some(QueueCommand::Enqueue {
                            work_unit_key, task_type, session_id,
                            project_id, payload, token_count, priority,
                        }) => {
                            self.handle_enqueue(
                                &work_unit_key, &task_type,
                                session_id.as_deref(), project_id.as_deref(),
                                &payload, token_count, priority,
                            ).await;
                        }
                        Some(QueueCommand::Wake) => {
                            self.process_eligible().await;
                        }
                        Some(QueueCommand::Shutdown) => {
                            info!("queue worker: shutdown received");
                            self.drain_enqueue_commands().await;
                            break;
                        }
                        None => {
                            debug!("queue worker: channel closed, exiting");
                            break;
                        }
                    }
                }
            }
        }

        info!("queue worker: stopped");
    }

    async fn handle_enqueue(
        &self,
        work_unit_key: &str,
        task_type: &str,
        session_id: Option<&str>,
        project_id: Option<&str>,
        payload: &str,
        token_count: i64,
        priority: i32,
    ) {
        let store = self.store.lock().await;
        match store.enqueue_task(
            work_unit_key,
            task_type,
            session_id,
            project_id,
            payload,
            token_count,
            priority,
        ) {
            Ok(id) => {
                debug!(
                    id,
                    work_unit_key, task_type, token_count, "queue worker: task enqueued"
                );
            }
            Err(e) => {
                error!(
                    error = %e,
                    work_unit_key,
                    task_type,
                    "queue worker: failed to enqueue task"
                );
            }
        }
    }

    async fn process_eligible(&self) {
        let eligible = {
            let store = self.store.lock().await;
            match store.list_eligible_work_units(self.config.default_token_threshold) {
                Ok(units) => units,
                Err(e) => {
                    error!(error = %e, "queue worker: failed to list eligible work units");
                    return;
                }
            }
        };

        if eligible.is_empty() {
            return;
        }

        debug!(
            count = eligible.len(),
            "queue worker: found eligible work units"
        );

        for unit in eligible {
            let task_type = match TaskType::from_str(&unit.task_type) {
                Some(t) => t,
                None => {
                    warn!(
                        task_type = %unit.task_type,
                        "queue worker: unknown task type, skipping"
                    );
                    continue;
                }
            };

            if !self.processor.handles(task_type) {
                debug!(
                    task_type = %task_type,
                    "queue worker: no processor for task type, skipping"
                );
                continue;
            }

            // Claim the work unit
            let items = {
                let store = self.store.lock().await;
                match store.claim_work_unit(&unit.work_unit_key) {
                    Ok(items) => items,
                    Err(e) => {
                        error!(
                            error = %e,
                            work_unit_key = %unit.work_unit_key,
                            "queue worker: failed to claim work unit"
                        );
                        continue;
                    }
                }
            };

            if items.is_empty() {
                continue; // Another iteration claimed it
            }

            // Process the work unit
            match self.processor.process(task_type, &items).await {
                Ok(()) => {
                    let store = self.store.lock().await;
                    if let Err(e) = store.complete_work_unit(&unit.work_unit_key) {
                        error!(
                            error = %e,
                            work_unit_key = %unit.work_unit_key,
                            "queue worker: failed to mark work unit complete"
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        work_unit_key = %unit.work_unit_key,
                        task_type = %task_type,
                        "queue worker: task processing failed"
                    );
                    let store = self.store.lock().await;
                    if let Err(e2) = store.fail_work_unit(&unit.work_unit_key, &e.to_string()) {
                        error!(
                            error = %e2,
                            work_unit_key = %unit.work_unit_key,
                            "queue worker: failed to record failure"
                        );
                    }
                }
            }
        }
    }

    async fn clean_stale_claims(&self) {
        let store = self.store.lock().await;
        match store.release_stale_claims(self.config.stale_claim_timeout_secs) {
            Ok(count) if count > 0 => {
                info!(count, "queue worker: released stale claims");
            }
            Err(e) => {
                warn!(error = %e, "queue worker: failed to clean stale claims");
            }
            _ => {}
        }
    }

    async fn purge_completed(&self) {
        let store = self.store.lock().await;
        match store.purge_completed_items(self.config.completed_retention_secs) {
            Ok(count) if count > 0 => {
                debug!(count, "queue worker: purged completed items");
            }
            Err(e) => {
                warn!(error = %e, "queue worker: failed to purge completed items");
            }
            _ => {}
        }
    }

    /// Drain remaining enqueue commands on shutdown (process them into SQLite).
    async fn drain_enqueue_commands(&mut self) {
        let mut count = 0u64;
        while let Ok(cmd) = self.rx.try_recv() {
            if let QueueCommand::Enqueue {
                work_unit_key,
                task_type,
                session_id,
                project_id,
                payload,
                token_count,
                priority,
            } = cmd
            {
                self.handle_enqueue(
                    &work_unit_key,
                    &task_type,
                    session_id.as_deref(),
                    project_id.as_deref(),
                    &payload,
                    token_count,
                    priority,
                )
                .await;
                count += 1;
            }
        }
        if count > 0 {
            debug!(count, "queue worker: drained enqueue commands on shutdown");
        }
    }
}

/// Spawn the queue worker and return a cloneable handle.
pub fn spawn_queue_worker(
    store: Arc<Mutex<Store>>,
    bus: Arc<EventBus>,
    config: QueueConfig,
    processor: Box<dyn TaskProcessor>,
) -> QueueHandle {
    let (tx, rx) = mpsc::channel(QUEUE_CHANNEL_CAPACITY);
    let mut worker = QueueWorker::new(rx, store, bus, config, processor);
    tokio::spawn(async move {
        worker.run().await;
    });
    QueueHandle::new(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::processor::NoOpProcessor;
    use crate::queue::types::TaskType;

    /// Returns the `TempDir` guard alongside the store so the temporary
    /// directory is removed when the test ends rather than orphaned in `/tmp`.
    fn open_test_store() -> (tempfile::TempDir, Arc<Mutex<Store>>) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let store = Store::open(&db_path).unwrap();
        (dir, Arc::new(Mutex::new(store)))
    }

    fn test_bus() -> Arc<EventBus> {
        Arc::new(EventBus::new(10))
    }

    fn fast_config() -> QueueConfig {
        QueueConfig {
            poll_interval_secs: 1,
            default_token_threshold: 0, // Process immediately
            stale_claim_timeout_secs: 300,
            max_attempts: 5,
            completed_retention_secs: 86400,
            enabled: true,
        }
    }

    #[tokio::test]
    async fn test_spawn_and_shutdown() {
        let (_dir, store) = open_test_store();
        let bus = test_bus();
        let handle = spawn_queue_worker(store, bus, fast_config(), Box::new(NoOpProcessor));
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_enqueue_via_handle() {
        let (_dir, store) = open_test_store();
        let bus = test_bus();
        let store_clone = Arc::clone(&store);
        let handle = spawn_queue_worker(store, bus, fast_config(), Box::new(NoOpProcessor));

        handle
            .enqueue(
                TaskType::ExtractObservations,
                "sess-1",
                Some("proj-1"),
                r#"{"test": true}"#,
                500,
            )
            .await
            .unwrap();

        // Give the worker time to process the enqueue command
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify item was persisted
        let s = store_clone.lock().await;
        let metrics = s.queue_metrics().unwrap();
        // The item was either processed (completed) or still pending -- both are valid
        assert!(metrics.pending + metrics.completed >= 1);

        drop(s);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_wake_triggers_processing() {
        let (_dir, store) = open_test_store();
        let bus = test_bus();
        let store_clone = Arc::clone(&store);

        // Use a long poll interval so only wake triggers processing
        let config = QueueConfig {
            poll_interval_secs: 3600,
            default_token_threshold: 0,
            ..fast_config()
        };
        let handle = spawn_queue_worker(store, bus, config, Box::new(NoOpProcessor));

        // Enqueue a task
        handle
            .enqueue(TaskType::Dream, "sess-2", None, "{}", 100)
            .await
            .unwrap();

        // Wait for enqueue to be processed
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Wake the worker
        handle.wake().await.unwrap();

        // Wait for processing
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let s = store_clone.lock().await;
        let metrics = s.queue_metrics().unwrap();
        assert_eq!(metrics.completed, 1, "Task should be completed after wake");

        drop(s);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_noop_processor_completes() {
        let (_dir, store) = open_test_store();
        let bus = test_bus();
        let store_clone = Arc::clone(&store);
        let handle = spawn_queue_worker(store, bus, fast_config(), Box::new(NoOpProcessor));

        handle
            .enqueue(TaskType::Reconcile, "sess-3", None, "{}", 100)
            .await
            .unwrap();

        // Wait for enqueue + processing
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        handle.wake().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let s = store_clone.lock().await;
        let metrics = s.queue_metrics().unwrap();
        assert_eq!(metrics.completed, 1);
        assert_eq!(metrics.pending, 0);

        drop(s);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_drain_on_shutdown() {
        let (_dir, store) = open_test_store();
        let bus = test_bus();
        let store_clone = Arc::clone(&store);

        // Use a very long poll interval so items don't get processed by the timer
        let config = QueueConfig {
            poll_interval_secs: 3600,
            default_token_threshold: 0,
            ..fast_config()
        };
        let handle = spawn_queue_worker(store, bus, config, Box::new(NoOpProcessor));

        // Enqueue then immediately shutdown -- drain should persist it
        handle
            .enqueue(TaskType::UpdateCard, "sess-4", None, "{}", 50)
            .await
            .unwrap();
        handle.shutdown().await.unwrap();

        // Give the worker a moment to drain and exit
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let s = store_clone.lock().await;
        let metrics = s.queue_metrics().unwrap();
        // The enqueued item should have been persisted during drain
        assert!(
            metrics.pending + metrics.completed >= 1,
            "Drained enqueue should be persisted"
        );
    }
}
