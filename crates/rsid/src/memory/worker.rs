use crate::bus::EventBus;
use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::memory::embedding::EmbeddingProviderResult;
use crate::memory::store::MemoryStore;
use crate::memory::sync::{MemorySyncEngine, SyncReport};
use crate::memory::types::{MemoryConfig, MemoryProviderStatus, MemorySearchResult};
use crate::memory::watcher::MemoryFileWatcher;
use crate::store::archive_cleanup::{ArchiveProjectionConsumer, ArchiveProjectionConsumerState};
use rsi_common::types::{ConversationEvent, Observation, ObservationSearchResult};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

const MEMORY_QUEUE_CAPACITY: usize = 64;

/// Commands sent to the memory worker over the mpsc channel.
#[derive(Debug)]
pub enum MemoryCommand {
    /// Trigger an immediate sync. If `force` is true, performs a full reindex.
    SyncNow { force: bool, reason: String },

    /// Memory files changed on disk and require an incremental file scan.
    MemoryFilesChanged,

    /// Synchronize the durable archive projection and report worker-side
    /// completion for the exact stable projection identity.
    SyncArchiveProjection {
        projection_id: Uuid,
        reason: String,
        reply_tx: oneshot::Sender<Result<MemoryProjectionSyncCompletion>>,
    },

    /// Search the memory index. Results are sent back over the reply channel.
    ///
    /// When `project_id` is `Some`, the search is restricted to chunks whose
    /// indexed `project_id` matches (strict project isolation for agent memory).
    /// When `None`, the search is unscoped (global behavior preserved for manual
    /// search, debug tooling, and reindex paths).
    Search {
        query: String,
        max_results: Option<usize>,
        min_score: Option<f64>,
        project_id: Option<Uuid>,
        reply_tx: oneshot::Sender<Result<Vec<MemorySearchResult>>>,
    },

    /// Query the current status of the memory system.
    Status {
        reply_tx: oneshot::Sender<MemoryProviderStatus>,
    },

    /// Read a memory file's content with optional line range.
    ReadFile {
        path: String,
        from_line: Option<usize>,
        num_lines: Option<usize>,
        reply_tx: oneshot::Sender<Result<String>>,
    },

    /// Extract observations from a completed session's conversation events.
    ExtractObservations {
        session_id: Uuid,
        project_id: Option<Uuid>,
        query: String,
        events: Vec<ConversationEvent>,
    },

    /// List observations with optional filters.
    ListObservations {
        session_id: Option<Uuid>,
        project_id: Option<Uuid>,
        limit: Option<usize>,
        reply_tx: oneshot::Sender<Result<Vec<Observation>>>,
    },

    /// Search observations by keyword or vector similarity.
    ///
    /// When `project_id` is `Some`, search is restricted to observations whose
    /// `project_id` matches. When `None`, the search is unscoped (global).
    SearchObservations {
        query: String,
        max_results: Option<usize>,
        project_id: Option<Uuid>,
        reply_tx: oneshot::Sender<Result<Vec<ObservationSearchResult>>>,
    },

    /// Count total observations.
    ObservationCount {
        reply_tx: oneshot::Sender<Result<u64>>,
    },

    /// Gracefully shut down the memory worker.
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryProjectionSyncDisposition {
    Applied,
    AlreadyDelivered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryProjectionSyncCompletion {
    pub projection_id: Uuid,
    pub disposition: MemoryProjectionSyncDisposition,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryProjectionSyncFaultPoint {
    BeforeSynchronization,
    AfterSynchronizationBeforeAcknowledgement,
}

#[cfg(test)]
static MEMORY_PROJECTION_SYNC_FAILURES: std::sync::Mutex<
    Vec<(Uuid, MemoryProjectionSyncFaultPoint)>,
> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
struct MemoryProjectionSyncPauseRequest {
    projection_id: Uuid,
    point: MemoryProjectionSyncFaultPoint,
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
static MEMORY_PROJECTION_SYNC_PAUSES: std::sync::Mutex<Vec<MemoryProjectionSyncPauseRequest>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) struct MemoryProjectionSyncPause {
    reached: Arc<tokio::sync::Notify>,
    _release: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
impl MemoryProjectionSyncPause {
    pub(crate) async fn wait_until_reached(&self) {
        self.reached.notified().await;
    }
}

#[cfg(test)]
pub(crate) fn install_memory_projection_sync_failure(
    projection_id: Uuid,
    point: MemoryProjectionSyncFaultPoint,
) {
    MEMORY_PROJECTION_SYNC_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((projection_id, point));
}

#[cfg(test)]
pub(crate) fn install_memory_projection_sync_pause(
    projection_id: Uuid,
    point: MemoryProjectionSyncFaultPoint,
) -> MemoryProjectionSyncPause {
    let pause = MemoryProjectionSyncPause {
        reached: Arc::new(tokio::sync::Notify::new()),
        _release: Arc::new(tokio::sync::Notify::new()),
    };
    MEMORY_PROJECTION_SYNC_PAUSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(MemoryProjectionSyncPauseRequest {
            projection_id,
            point,
            reached: Arc::clone(&pause.reached),
            release: Arc::clone(&pause._release),
        });
    pause
}

#[cfg(test)]
fn fail_memory_projection_sync_if_requested(
    projection_id: Uuid,
    point: MemoryProjectionSyncFaultPoint,
) -> Result<()> {
    let mut requested = MEMORY_PROJECTION_SYNC_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(index) = requested
        .iter()
        .position(|requested| *requested == (projection_id, point))
    {
        requested.swap_remove(index);
        return Err(DaemonError::Store(format!(
            "injected memory projection sync failure at {point:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
async fn pause_memory_projection_sync_if_requested(
    projection_id: Uuid,
    point: MemoryProjectionSyncFaultPoint,
) {
    let request = {
        let mut requested = MEMORY_PROJECTION_SYNC_PAUSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        requested
            .iter()
            .position(|requested| {
                requested.projection_id == projection_id && requested.point == point
            })
            .map(|index| requested.swap_remove(index))
    };
    if let Some(request) = request {
        request.reached.notify_one();
        request.release.notified().await;
    }
}

/// Cloneable handle for sending commands to the memory worker.
#[derive(Clone)]
pub struct MemoryHandle {
    tx: mpsc::Sender<MemoryCommand>,
}

impl MemoryHandle {
    pub fn new(tx: mpsc::Sender<MemoryCommand>) -> Self {
        Self { tx }
    }

    /// Trigger an immediate sync.
    pub async fn sync_now(&self, force: bool, reason: &str) -> Result<()> {
        self.tx
            .send(MemoryCommand::SyncNow {
                force,
                reason: reason.to_string(),
            })
            .await
            .map_err(|_| DaemonError::ChannelClosed)
    }

    /// Mark memory files dirty and trigger an incremental sync.
    pub(crate) async fn memory_files_changed(&self) -> Result<()> {
        self.tx
            .send(MemoryCommand::MemoryFilesChanged)
            .await
            .map_err(|_| DaemonError::ChannelClosed)
    }

    /// Synchronize one durable archive projection and wait until the worker
    /// has both applied the effect and acknowledged its exact consumer row.
    pub async fn sync_archive_projection(
        &self,
        projection_id: Uuid,
        reason: &str,
    ) -> Result<MemoryProjectionSyncCompletion> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MemoryCommand::SyncArchiveProjection {
                projection_id,
                reason: reason.to_string(),
                reply_tx,
            })
            .await
            .map_err(|_| DaemonError::ChannelClosed)?;
        let completion = reply_rx.await.map_err(|_| DaemonError::ChannelClosed)??;
        if completion.projection_id != projection_id {
            return Err(DaemonError::Store(
                "memory projection completion identity mismatch".into(),
            ));
        }
        Ok(completion)
    }

    /// Search the memory index with optional project scope.
    ///
    /// When `project_id` is `Some`, only chunks indexed under that project
    /// (or session transcripts owned by it) are returned. When `None`, the
    /// global index is searched (manual / admin / debug behavior).
    pub async fn search(
        &self,
        query: &str,
        max_results: Option<usize>,
        min_score: Option<f64>,
        project_id: Option<Uuid>,
    ) -> Result<Vec<MemorySearchResult>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MemoryCommand::Search {
                query: query.to_string(),
                max_results,
                min_score,
                project_id,
                reply_tx,
            })
            .await
            .map_err(|_| DaemonError::ChannelClosed)?;
        reply_rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    /// Get the current status of the memory system.
    pub async fn status(&self) -> Result<MemoryProviderStatus> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MemoryCommand::Status { reply_tx })
            .await
            .map_err(|_| DaemonError::ChannelClosed)?;
        reply_rx.await.map_err(|_| DaemonError::ChannelClosed)
    }

    /// Read a memory file's content.
    pub async fn read_file(
        &self,
        path: &str,
        from_line: Option<usize>,
        num_lines: Option<usize>,
    ) -> Result<String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MemoryCommand::ReadFile {
                path: path.to_string(),
                from_line,
                num_lines,
                reply_tx,
            })
            .await
            .map_err(|_| DaemonError::ChannelClosed)?;
        reply_rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    /// Trigger observation extraction for a completed session.
    pub async fn extract_observations(
        &self,
        session_id: Uuid,
        project_id: Option<Uuid>,
        query: String,
        events: Vec<ConversationEvent>,
    ) -> Result<()> {
        self.tx
            .send(MemoryCommand::ExtractObservations {
                session_id,
                project_id,
                query,
                events,
            })
            .await
            .map_err(|_| DaemonError::ChannelClosed)
    }

    /// List observations with optional filters.
    pub async fn list_observations(
        &self,
        session_id: Option<Uuid>,
        project_id: Option<Uuid>,
        limit: Option<usize>,
    ) -> Result<Vec<Observation>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MemoryCommand::ListObservations {
                session_id,
                project_id,
                limit,
                reply_tx,
            })
            .await
            .map_err(|_| DaemonError::ChannelClosed)?;
        reply_rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    /// Search observations by keyword with optional project scope.
    ///
    /// When `project_id` is `Some`, only observations whose `project_id`
    /// matches are returned. When `None`, the search is unscoped (global).
    pub async fn search_observations(
        &self,
        query: &str,
        max_results: Option<usize>,
        project_id: Option<Uuid>,
    ) -> Result<Vec<ObservationSearchResult>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MemoryCommand::SearchObservations {
                query: query.to_string(),
                max_results,
                project_id,
                reply_tx,
            })
            .await
            .map_err(|_| DaemonError::ChannelClosed)?;
        reply_rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    /// Get total observation count.
    pub async fn observation_count(&self) -> Result<u64> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MemoryCommand::ObservationCount { reply_tx })
            .await
            .map_err(|_| DaemonError::ChannelClosed)?;
        reply_rx.await.map_err(|_| DaemonError::ChannelClosed)?
    }

    /// Shut down the memory worker gracefully.
    pub async fn shutdown(&self) -> Result<()> {
        self.tx
            .send(MemoryCommand::Shutdown)
            .await
            .map_err(|_| DaemonError::ChannelClosed)
    }
}

pub struct MemoryWorker {
    rx: mpsc::Receiver<MemoryCommand>,
    sync_engine: MemorySyncEngine,
    main_store: Arc<tokio::sync::Mutex<crate::store::Store>>,
    watcher: Option<MemoryFileWatcher>,
    bus: Arc<EventBus>,
    runtime_config: Arc<RuntimeConfig>,
}

impl MemoryWorker {
    pub fn new(
        rx: mpsc::Receiver<MemoryCommand>,
        sync_engine: MemorySyncEngine,
        main_store: Arc<tokio::sync::Mutex<crate::store::Store>>,
        watcher: Option<MemoryFileWatcher>,
        bus: Arc<EventBus>,
        runtime_config: Arc<RuntimeConfig>,
    ) -> Self {
        Self {
            rx,
            sync_engine,
            main_store,
            watcher,
            bus,
            runtime_config,
        }
    }

    /// Run the worker loop. Processes commands until Shutdown or channel close.
    pub async fn run(&mut self) {
        // On startup: clean up stale temp files from interrupted reindexes
        if let Err(e) =
            crate::memory::reindex::cleanup_stale_temp_files(self.sync_engine.db_path()).await
        {
            warn!("memory worker: failed to clean stale temp files: {e}");
        }

        // Run initial sync
        match self.sync_engine.run_sync("startup", false).await {
            Ok(report) => {
                info!(
                    files_indexed = report.files_indexed,
                    files_unchanged = report.files_unchanged,
                    files_deleted = report.files_deleted,
                    sessions_indexed = report.sessions_indexed,
                    sessions_failed = report.sessions_failed,
                    reindexed = report.reindexed,
                    "memory worker: initial sync complete"
                );
                self.publish_index_updated(&report);
            }
            Err(e) => {
                error!("memory worker: initial sync failed: {e}");
            }
        }

        loop {
            let Some(cmd) = self.rx.recv().await else {
                debug!("memory worker: channel closed, exiting");
                break;
            };

            match cmd {
                MemoryCommand::Shutdown => {
                    debug!("memory worker: shutdown received");
                    if let Some(watcher) = self.watcher.take() {
                        watcher.stop();
                    }
                    self.drain_remaining().await;
                    break;
                }
                MemoryCommand::SyncNow { force, reason } => {
                    self.handle_sync(force, &reason).await;
                }
                MemoryCommand::MemoryFilesChanged => {
                    self.handle_memory_files_changed().await;
                }
                MemoryCommand::SyncArchiveProjection {
                    projection_id,
                    reason,
                    reply_tx,
                } => {
                    let result = self
                        .handle_archive_projection_sync(projection_id, &reason)
                        .await;
                    let _ = reply_tx.send(result);
                }
                MemoryCommand::Search {
                    query,
                    max_results,
                    min_score,
                    project_id,
                    reply_tx,
                } => {
                    let result = self
                        .sync_engine
                        .search(&query, max_results, min_score, project_id)
                        .await;
                    let _ = reply_tx.send(result);
                }
                MemoryCommand::Status { reply_tx } => {
                    let status = self.sync_engine.status();
                    let _ = reply_tx.send(status);
                }
                MemoryCommand::ReadFile {
                    path,
                    from_line,
                    num_lines,
                    reply_tx,
                } => {
                    let result = self
                        .sync_engine
                        .read_file(&path, from_line, num_lines)
                        .await;
                    let _ = reply_tx.send(result);
                }
                MemoryCommand::ExtractObservations {
                    session_id,
                    project_id,
                    query,
                    events,
                } => {
                    let bus = self.bus.clone();
                    // Degrade this one extraction rather than taking down the
                    // worker: `run()` drives every memory command, and this
                    // loop is spawned without a retained `JoinHandle`, so an
                    // unwind here would silently disable memory, search, and
                    // observations for the rest of the daemon's lifetime.
                    let store = match self.sync_engine.store_handle() {
                        Ok(store) => store,
                        Err(error) => {
                            tracing::error!(
                                %error,
                                ?session_id,
                                "memory worker: failed to open memory store for observation \
                                 extraction; skipping this extraction"
                            );
                            continue;
                        }
                    };
                    let main_store = Arc::clone(&self.main_store);
                    let config = self.sync_engine.config();
                    let embedding_provider = self.sync_engine.embedding_provider();
                    let runtime_config = Arc::clone(&self.runtime_config);
                    tokio::spawn(async move {
                        Self::run_observation_extraction(
                            session_id,
                            project_id,
                            query,
                            events,
                            store,
                            main_store,
                            config,
                            embedding_provider,
                            bus,
                            runtime_config,
                        )
                        .await;
                    });
                }
                MemoryCommand::ListObservations {
                    session_id,
                    project_id,
                    limit,
                    reply_tx,
                } => {
                    let result = self.handle_list_observations(session_id, project_id, limit);
                    let _ = reply_tx.send(result);
                }
                MemoryCommand::SearchObservations {
                    query,
                    max_results,
                    project_id,
                    reply_tx,
                } => {
                    let result = self.handle_search_observations(&query, max_results, project_id);
                    let _ = reply_tx.send(result);
                }
                MemoryCommand::ObservationCount { reply_tx } => {
                    let result = self.handle_observation_count();
                    let _ = reply_tx.send(result);
                }
            }
        }
    }

    async fn handle_sync(&mut self, force: bool, reason: &str) {
        match self.sync_engine.run_sync(reason, force).await {
            Ok(report) => {
                debug!(
                    files_indexed = report.files_indexed,
                    files_unchanged = report.files_unchanged,
                    sessions_indexed = report.sessions_indexed,
                    sessions_failed = report.sessions_failed,
                    reason,
                    "memory worker: sync complete"
                );
                if report.sessions_failed > 0 {
                    warn!(
                        sessions_failed = report.sessions_failed,
                        reason, "memory worker: sync completed with unindexed sessions"
                    );
                }
                self.publish_index_updated(&report);
            }
            Err(e) => {
                warn!("memory worker: sync failed ({reason}): {e}");
            }
        }
    }

    async fn handle_memory_files_changed(&mut self) {
        self.sync_engine.mark_dirty();
        self.handle_sync(false, "watch").await;
    }

    async fn handle_archive_projection_sync(
        &mut self,
        projection_id: Uuid,
        reason: &str,
    ) -> Result<MemoryProjectionSyncCompletion> {
        let state = self
            .main_store
            .lock()
            .await
            .archive_cleanup_projection_consumer_state(
                projection_id,
                ArchiveProjectionConsumer::Memory,
            )?;
        match state {
            Some(ArchiveProjectionConsumerState::Delivered) => {
                return Ok(MemoryProjectionSyncCompletion {
                    projection_id,
                    disposition: MemoryProjectionSyncDisposition::AlreadyDelivered,
                });
            }
            Some(ArchiveProjectionConsumerState::Delivering) => {}
            Some(ArchiveProjectionConsumerState::Pending) => {
                return Err(DaemonError::Store(
                    "archive memory projection was not durably claimed".into(),
                ));
            }
            None => {
                return Err(DaemonError::Store(
                    "archive memory projection consumer is missing".into(),
                ));
            }
        }

        #[cfg(test)]
        {
            pause_memory_projection_sync_if_requested(
                projection_id,
                MemoryProjectionSyncFaultPoint::BeforeSynchronization,
            )
            .await;
            fail_memory_projection_sync_if_requested(
                projection_id,
                MemoryProjectionSyncFaultPoint::BeforeSynchronization,
            )?;
        }

        let report = self.sync_engine.run_sync(reason, false).await?;
        if report.sessions_failed > 0 {
            return Err(DaemonError::Store(format!(
                "archive memory projection sync incomplete: {} session(s) failed",
                report.sessions_failed
            )));
        }
        debug!(
            %projection_id,
            files_indexed = report.files_indexed,
            files_unchanged = report.files_unchanged,
            sessions_indexed = report.sessions_indexed,
            sessions_failed = report.sessions_failed,
            reason,
            "memory worker: archive projection sync complete"
        );

        #[cfg(test)]
        {
            pause_memory_projection_sync_if_requested(
                projection_id,
                MemoryProjectionSyncFaultPoint::AfterSynchronizationBeforeAcknowledgement,
            )
            .await;
            fail_memory_projection_sync_if_requested(
                projection_id,
                MemoryProjectionSyncFaultPoint::AfterSynchronizationBeforeAcknowledgement,
            )?;
        }

        self.main_store
            .lock()
            .await
            .complete_archive_cleanup_projection_consumer(
                projection_id,
                ArchiveProjectionConsumer::Memory,
            )?;
        self.publish_index_updated(&report);
        Ok(MemoryProjectionSyncCompletion {
            projection_id,
            disposition: MemoryProjectionSyncDisposition::Applied,
        })
    }

    fn publish_index_updated(&self, report: &SyncReport) {
        self.bus
            .publish(crate::bus::DaemonEvent::MemoryIndexUpdated {
                file_count: report.files_indexed + report.files_unchanged,
                chunk_count: 0, // Updated in Phase 7 when SyncReport tracks chunk count
            });
    }

    /// Run the full observation extraction pipeline in a background task.
    /// Extracts observations via LLM, computes embeddings, stores in DB, publishes bus event.
    async fn run_observation_extraction(
        session_id: Uuid,
        project_id: Option<Uuid>,
        query: String,
        events: Vec<ConversationEvent>,
        store: Arc<std::sync::Mutex<MemoryStore>>,
        main_store: Arc<tokio::sync::Mutex<crate::store::Store>>,
        config: MemoryConfig,
        embedding_provider: Arc<EmbeddingProviderResult>,
        bus: Arc<EventBus>,
        runtime_config: Arc<RuntimeConfig>,
    ) {
        if !config.observation_extraction_enabled {
            debug!(
                session_id = %session_id,
                "Observation extraction disabled, skipping"
            );
            return;
        }

        let observations = match crate::observation::extract_observations(
            &main_store,
            &bus,
            session_id,
            project_id,
            &query,
            &events,
            config.observation_min_events,
            config.observation_max_input_chars,
            &runtime_config,
        )
        .await
        {
            Ok(obs) => obs,
            Err(e) => {
                warn!(
                    error = %e,
                    session_id = %session_id,
                    "Observation extraction failed"
                );
                return;
            }
        };

        if observations.is_empty() {
            debug!(
                session_id = %session_id,
                "No observations extracted"
            );
            return;
        }

        // Compute embeddings for observation content
        let texts: Vec<String> = observations.iter().map(|o| o.content.clone()).collect();
        let embeddings = if let Some(ref provider) = embedding_provider.provider {
            let control = crate::memory::embedding::batch::EmbeddingControl {
                store: Arc::clone(&main_store),
                event_bus: Arc::clone(&bus),
                owner: rsi_common::model_control::InvocationOwner {
                    session_id: Some(session_id),
                    project_id,
                    ..rsi_common::model_control::InvocationOwner::default()
                },
                provider: embedding_provider.provider_label.clone(),
                backend: embedding_provider.backend.clone(),
                model: embedding_provider.model_name().to_string(),
                base_url: embedding_provider.base_url.clone(),
                trigger: "memory_observation_embedding".to_string(),
                dedup_namespace: format!("memory-observation-embedding:{session_id}"),
            };
            match crate::memory::embedding::batch::embed_text_batch(
                provider.as_ref(),
                &texts,
                Some(&control),
            )
            .await
            {
                Ok(embs) => embs,
                Err(e) => {
                    warn!(
                        error = %e,
                        session_id = %session_id,
                        "Observation embedding failed, storing without embeddings"
                    );
                    vec![vec![]; observations.len()]
                }
            }
        } else {
            vec![vec![]; observations.len()]
        };

        // Convert to ObservationRows for storage
        let rows: Vec<crate::memory::store::ObservationRow> = observations
            .iter()
            .zip(embeddings.iter())
            .map(|(obs, emb)| {
                let embedding_json = if emb.is_empty() {
                    String::new()
                } else {
                    serde_json::to_string(emb).unwrap_or_default()
                };
                crate::memory::store::ObservationRow {
                    id: obs.id.to_string(),
                    session_id: obs.session_id.to_string(),
                    project_id: obs.project_id.map(|p| p.to_string()),
                    level: format!("{:?}", obs.level).to_lowercase(),
                    content: obs.content.clone(),
                    source_ids: serde_json::to_string(&obs.source_ids)
                        .unwrap_or_else(|_| "[]".to_string()),
                    confidence: obs.confidence.map(|c| format!("{:?}", c).to_lowercase()),
                    times_derived: obs.times_derived,
                    embedding: embedding_json,
                    created_at: obs.created_at.to_rfc3339(),
                    updated_at: obs.updated_at.to_rfc3339(),
                }
            })
            .collect();

        let count = rows.len();

        // Store in DB
        if let Ok(store) = store.lock() {
            if let Err(e) = store.insert_observations(&rows) {
                error!(
                    error = %e,
                    session_id = %session_id,
                    "Failed to store observations"
                );
                return;
            }
        } else {
            error!(
                session_id = %session_id,
                "Failed to acquire store lock for observation insert"
            );
            return;
        }

        info!(
            session_id = %session_id,
            count,
            "Stored observations from session"
        );

        bus.publish(crate::bus::DaemonEvent::ObservationsExtracted { session_id, count });
    }

    fn handle_list_observations(
        &self,
        session_id: Option<Uuid>,
        project_id: Option<Uuid>,
        limit: Option<usize>,
    ) -> Result<Vec<Observation>> {
        let store = self.sync_engine.store_handle()?;
        let store = store
            .lock()
            .map_err(|_| DaemonError::Store("Store lock poisoned".to_string()))?;
        let rows = store.list_observations(
            session_id.as_ref().map(|u| u.to_string()).as_deref(),
            project_id.as_ref().map(|u| u.to_string()).as_deref(),
            limit,
        )?;
        Ok(rows
            .into_iter()
            .filter_map(observation_row_to_common)
            .collect())
    }

    fn handle_search_observations(
        &self,
        query: &str,
        max_results: Option<usize>,
        project_id: Option<Uuid>,
    ) -> Result<Vec<ObservationSearchResult>> {
        let store = self.sync_engine.store_handle()?;
        let store = store
            .lock()
            .map_err(|_| DaemonError::Store("Store lock poisoned".to_string()))?;
        let project_filter = project_id.map(|p| p.to_string());
        let results = store.search_observations_by_keyword(
            query,
            max_results.unwrap_or(20),
            project_filter.as_deref(),
        )?;

        // Fetch full observation data for each result
        let mut search_results = Vec::with_capacity(results.len());
        for (id, score) in results {
            if let Ok(Some(row)) = store.get_observation(&id)
                && let Some(obs) = observation_row_to_common(row)
            {
                search_results.push(ObservationSearchResult {
                    observation: obs,
                    score,
                });
            }
        }
        Ok(search_results)
    }

    fn handle_observation_count(&self) -> Result<u64> {
        let store = self.sync_engine.store_handle()?;
        let store = store
            .lock()
            .map_err(|_| DaemonError::Store("Store lock poisoned".to_string()))?;
        store.count_observations()
    }

    async fn drain_remaining(&mut self) {
        let mut count = 0u64;
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                MemoryCommand::Shutdown => continue,
                MemoryCommand::SyncNow { force, reason } => {
                    self.handle_sync(force, &reason).await;
                    count += 1;
                }
                MemoryCommand::MemoryFilesChanged => {
                    self.handle_memory_files_changed().await;
                    count += 1;
                }
                MemoryCommand::SyncArchiveProjection {
                    projection_id,
                    reason,
                    reply_tx,
                } => {
                    let result = self
                        .handle_archive_projection_sync(projection_id, &reason)
                        .await;
                    let _ = reply_tx.send(result);
                    count += 1;
                }
                MemoryCommand::Search { reply_tx, .. } => {
                    drop(reply_tx);
                    count += 1;
                }
                MemoryCommand::Status { reply_tx } => {
                    drop(reply_tx);
                    count += 1;
                }
                MemoryCommand::ReadFile { reply_tx, .. } => {
                    drop(reply_tx);
                    count += 1;
                }
                MemoryCommand::ExtractObservations { .. } => {
                    // Drop silently on shutdown - extraction is best-effort
                    count += 1;
                }
                MemoryCommand::ListObservations { reply_tx, .. } => {
                    drop(reply_tx);
                    count += 1;
                }
                MemoryCommand::SearchObservations { reply_tx, .. } => {
                    drop(reply_tx);
                    count += 1;
                }
                MemoryCommand::ObservationCount { reply_tx } => {
                    drop(reply_tx);
                    count += 1;
                }
            }
        }
        if count > 0 {
            debug!(
                count,
                "memory worker: drained remaining commands on shutdown"
            );
        }
    }
}

/// Convert an `ObservationRow` (store-layer) to a `rsi_common::types::Observation`.
fn observation_row_to_common(row: crate::memory::store::ObservationRow) -> Option<Observation> {
    let id = Uuid::parse_str(&row.id).ok()?;
    let session_id = Uuid::parse_str(&row.session_id).ok()?;
    let project_id = row
        .project_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok());
    let level = match row.level.as_str() {
        "explicit" => rsi_common::types::ObservationLevel::Explicit,
        "deductive" => rsi_common::types::ObservationLevel::Deductive,
        "inductive" => rsi_common::types::ObservationLevel::Inductive,
        "contradiction" => rsi_common::types::ObservationLevel::Contradiction,
        _ => rsi_common::types::ObservationLevel::Explicit,
    };
    let source_ids: Vec<Uuid> = serde_json::from_str(&row.source_ids).unwrap_or_default();
    let confidence = row.confidence.as_deref().and_then(|c| match c {
        "low" => Some(rsi_common::types::ObservationConfidence::Low),
        "medium" => Some(rsi_common::types::ObservationConfidence::Medium),
        "high" => Some(rsi_common::types::ObservationConfidence::High),
        _ => None,
    });
    let created_at = chrono::DateTime::parse_from_rfc3339(&row.created_at)
        .ok()?
        .with_timezone(&chrono::Utc);
    let updated_at = chrono::DateTime::parse_from_rfc3339(&row.updated_at)
        .ok()?
        .with_timezone(&chrono::Utc);

    Some(Observation {
        id,
        session_id,
        project_id,
        level,
        content: row.content,
        source_ids,
        confidence,
        times_derived: row.times_derived,
        created_at,
        updated_at,
    })
}

/// Spawn the memory worker on a dedicated tokio task and return a cloneable handle.
pub fn spawn_memory_worker(
    config: MemoryConfig,
    memory_store: MemoryStore,
    main_db_path: PathBuf,
    main_store: Arc<tokio::sync::Mutex<crate::store::Store>>,
    embedding_provider: Arc<EmbeddingProviderResult>,
    bus: Arc<EventBus>,
    memory_dir: PathBuf,
    db_path: PathBuf,
    runtime_config: Arc<RuntimeConfig>,
) -> MemoryHandle {
    let (tx, rx) = mpsc::channel(MEMORY_QUEUE_CAPACITY);

    let sync_engine = MemorySyncEngine::new(
        config.clone(),
        memory_store,
        main_db_path,
        embedding_provider,
        Arc::clone(&bus),
        memory_dir.clone(),
        db_path,
    );

    // Create the file watcher (sends SyncNow commands through the channel)
    let watcher_handle = MemoryHandle::new(tx.clone());
    let watcher = if config.watch_enabled {
        match MemoryFileWatcher::new(memory_dir, config.watch_debounce_ms, watcher_handle) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!("memory worker: file watcher unavailable: {e}");
                None
            }
        }
    } else {
        None
    };

    let mut worker = MemoryWorker::new(rx, sync_engine, main_store, watcher, bus, runtime_config);
    tokio::spawn(async move {
        worker.run().await;
    });

    MemoryHandle::new(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_worker_parts(
        dir: &TempDir,
    ) -> (
        mpsc::Receiver<MemoryCommand>,
        MemorySyncEngine,
        Arc<tokio::sync::Mutex<crate::store::Store>>,
        Arc<EventBus>,
        mpsc::Sender<MemoryCommand>,
    ) {
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        let db_path = dir.path().join("memory.sqlite");
        let store = MemoryStore::open(&db_path).unwrap();

        let main_db = dir.path().join("flywheel.db");
        // Create the main store so it exists on disk
        let main_store = Arc::new(tokio::sync::Mutex::new(
            crate::store::Store::open(&main_db).unwrap(),
        ));

        let provider = Arc::new(EmbeddingProviderResult {
            provider: None,
            requested: "none".to_string(),
            provider_label: "none".to_string(),
            backend: "none".to_string(),
            base_url: None,
            fallback_reason: None,
            unavailable_reason: None,
        });

        let bus = Arc::new(EventBus::new(16));
        let sync_engine = MemorySyncEngine::new(
            MemoryConfig {
                memory_dir: memory_dir.clone(),
                db_path: db_path.clone(),
                watch_enabled: false,
                ..Default::default()
            },
            store,
            main_db,
            provider,
            Arc::clone(&bus),
            memory_dir,
            db_path,
        );

        let (tx, rx) = mpsc::channel(MEMORY_QUEUE_CAPACITY);
        (rx, sync_engine, main_store, bus, tx)
    }

    #[tokio::test]
    async fn test_memory_handle_sync_now() {
        let dir = TempDir::new().unwrap();
        let (rx, sync_engine, main_store, bus, tx) = setup_worker_parts(&dir);
        let handle = MemoryHandle::new(tx);

        let rt_cfg = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        let mut worker = MemoryWorker::new(rx, sync_engine, main_store, None, bus, rt_cfg);
        let worker_task = tokio::spawn(async move { worker.run().await });

        handle.sync_now(false, "test").await.unwrap();
        handle.shutdown().await.unwrap();
        worker_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_memory_file_change_reindexes_after_startup() {
        let dir = TempDir::new().unwrap();
        let (rx, sync_engine, main_store, bus, tx) = setup_worker_parts(&dir);
        let memory_file = dir.path().join("memory/changed.md");
        std::fs::write(&memory_file, "# Before\nwatcherfreshalpha").unwrap();
        let handle = MemoryHandle::new(tx);

        let rt_cfg = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        let mut worker = MemoryWorker::new(rx, sync_engine, main_store, None, bus, rt_cfg);
        let worker_task = tokio::spawn(async move { worker.run().await });

        let initial = handle
            .search("watcherfreshalpha", None, None, None)
            .await
            .unwrap();
        assert!(
            !initial.is_empty(),
            "startup sync should index initial content"
        );

        std::fs::write(&memory_file, "# After\nwatcherfreshbeta").unwrap();
        handle.memory_files_changed().await.unwrap();

        let updated = handle
            .search("watcherfreshbeta", None, None, None)
            .await
            .unwrap();
        assert!(
            !updated.is_empty(),
            "file-change sync should index replacement content"
        );

        handle.shutdown().await.unwrap();
        worker_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_memory_handle_shutdown() {
        let dir = TempDir::new().unwrap();
        let (rx, sync_engine, main_store, bus, tx) = setup_worker_parts(&dir);
        let handle = MemoryHandle::new(tx);

        let rt_cfg = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        let mut worker = MemoryWorker::new(rx, sync_engine, main_store, None, bus, rt_cfg);
        let worker_task = tokio::spawn(async move { worker.run().await });

        handle.shutdown().await.unwrap();
        worker_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_memory_handle_status() {
        let dir = TempDir::new().unwrap();
        let (rx, sync_engine, main_store, bus, tx) = setup_worker_parts(&dir);
        let handle = MemoryHandle::new(tx);

        let rt_cfg = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        let mut worker = MemoryWorker::new(rx, sync_engine, main_store, None, bus, rt_cfg);
        let worker_task = tokio::spawn(async move { worker.run().await });

        let status = handle.status().await.unwrap();
        assert_eq!(status.backend, "builtin");

        handle.shutdown().await.unwrap();
        worker_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_memory_handle_read_file_valid() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join("test.md"), "hello world").unwrap();

        let (rx, sync_engine, main_store, bus, tx) = setup_worker_parts(&dir);
        let handle = MemoryHandle::new(tx);

        let rt_cfg = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        let mut worker = MemoryWorker::new(rx, sync_engine, main_store, None, bus, rt_cfg);
        let worker_task = tokio::spawn(async move { worker.run().await });

        let content = handle
            .read_file("memory/test.md", None, None)
            .await
            .unwrap();
        assert_eq!(content, "hello world");

        handle.shutdown().await.unwrap();
        worker_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_memory_handle_read_file_invalid_path() {
        let dir = TempDir::new().unwrap();
        let (rx, sync_engine, main_store, bus, tx) = setup_worker_parts(&dir);
        let handle = MemoryHandle::new(tx);

        let rt_cfg = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        let mut worker = MemoryWorker::new(rx, sync_engine, main_store, None, bus, rt_cfg);
        let worker_task = tokio::spawn(async move { worker.run().await });

        let result = handle.read_file("../../etc/passwd", None, None).await;
        assert!(result.is_err());

        handle.shutdown().await.unwrap();
        worker_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_memory_handle_search_returns_empty() {
        let dir = TempDir::new().unwrap();
        let (rx, sync_engine, main_store, bus, tx) = setup_worker_parts(&dir);
        let handle = MemoryHandle::new(tx);

        let rt_cfg = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        let mut worker = MemoryWorker::new(rx, sync_engine, main_store, None, bus, rt_cfg);
        let worker_task = tokio::spawn(async move { worker.run().await });

        let results = handle.search("test query", None, None, None).await.unwrap();
        assert!(results.is_empty());

        handle.shutdown().await.unwrap();
        worker_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_memory_handle_clone_sends_to_same_worker() {
        let dir = TempDir::new().unwrap();
        let (rx, sync_engine, main_store, bus, tx) = setup_worker_parts(&dir);
        let handle1 = MemoryHandle::new(tx);
        let handle2 = handle1.clone();

        let rt_cfg = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        let mut worker = MemoryWorker::new(rx, sync_engine, main_store, None, bus, rt_cfg);
        let worker_task = tokio::spawn(async move { worker.run().await });

        // Both handles should reach the same worker
        let s1 = handle1.status().await.unwrap();
        let s2 = handle2.status().await.unwrap();
        assert_eq!(s1.backend, s2.backend);

        handle1.shutdown().await.unwrap();
        worker_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_spawn_memory_worker_returns_handle() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        let db_path = dir.path().join("memory.sqlite");
        let store = MemoryStore::open(&db_path).unwrap();

        let main_db = dir.path().join("flywheel.db");
        let main_store = Arc::new(tokio::sync::Mutex::new(
            crate::store::Store::open(&main_db).unwrap(),
        ));

        let provider = Arc::new(EmbeddingProviderResult {
            provider: None,
            requested: "none".to_string(),
            provider_label: "none".to_string(),
            backend: "none".to_string(),
            base_url: None,
            fallback_reason: None,
            unavailable_reason: None,
        });

        let bus = Arc::new(EventBus::new(16));

        let rt_config =
            crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        let handle = spawn_memory_worker(
            MemoryConfig {
                memory_dir: memory_dir.clone(),
                db_path: db_path.clone(),
                watch_enabled: false,
                ..Default::default()
            },
            store,
            main_db,
            main_store,
            provider,
            bus,
            memory_dir,
            db_path,
            rt_config,
        );

        // Allow the worker to start and run initial sync
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let status = handle.status().await.unwrap();
        assert_eq!(status.backend, "builtin");

        handle.shutdown().await.unwrap();
    }
}
