use crate::bus::EventBus;
use crate::error::Result;
use crate::memory::chunking::{chunk_markdown, enforce_max_input_tokens, remap_chunk_lines};
use crate::memory::embedding::EmbeddingProviderResult;
use crate::memory::embedding::batch::{EmbeddingControl, embed_chunks_in_batches};
use crate::memory::embedding::cache::EmbeddingCache;
use crate::memory::files::{build_file_entry, is_memory_path, list_memory_files};
use crate::memory::reindex;
use crate::memory::session_text::build_session_entry;
use crate::memory::store::MemoryStore;
use crate::memory::types::{
    MemoryChunk, MemoryConfig, MemoryFileEntry, MemoryProviderStatus, MemorySearchResult,
    MemorySource,
};
use crate::store::Store;
use rsi_common::model_control::InvocationOwner;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, warn};

/// Report returned after a sync operation completes.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    /// Number of files that were (re-)indexed because their hash changed.
    pub files_indexed: usize,
    /// Number of files whose hash matched the stored hash (skipped).
    pub files_unchanged: usize,
    /// Number of stale file records deleted (file no longer on disk).
    pub files_deleted: usize,
    /// Number of session transcripts indexed.
    pub sessions_indexed: usize,
    /// Number of session transcripts skipped because indexing failed. Non-zero
    /// means the pass completed but is incomplete — surfaced so a persistently
    /// failing session cannot fail silently forever.
    pub sessions_failed: usize,
    /// Whether a full reindex was performed (vs incremental sync).
    pub reindexed: bool,
}

pub struct MemorySyncEngine {
    config: MemoryConfig,
    memory_store: MemoryStore,
    /// Path to the main flywheel.db (opened as needed for session reads).
    main_db_path: PathBuf,
    embedding_provider: Arc<EmbeddingProviderResult>,
    embedding_cache: EmbeddingCache,
    event_bus: Arc<EventBus>,
    memory_dir: PathBuf,
    db_path: PathBuf,
    /// Set to true when the file watcher detects changes.
    dirty: bool,
}

impl MemorySyncEngine {
    pub fn new(
        config: MemoryConfig,
        memory_store: MemoryStore,
        main_db_path: PathBuf,
        embedding_provider: Arc<EmbeddingProviderResult>,
        event_bus: Arc<EventBus>,
        memory_dir: PathBuf,
        db_path: PathBuf,
    ) -> Self {
        let embedding_cache = Self::build_cache(&config, &embedding_provider);
        Self {
            config,
            memory_store,
            main_db_path,
            embedding_provider,
            embedding_cache,
            event_bus,
            memory_dir,
            db_path,
            dirty: true, // start dirty to trigger initial sync
        }
    }

    /// Open a read-only connection to the main flywheel.db.
    /// Each call opens a fresh connection to avoid Send/Sync issues with
    /// rusqlite::Connection (which is Send but not Sync).
    fn open_main_store(&self) -> Result<Store> {
        Store::open(&self.main_db_path)
    }

    fn build_cache(config: &MemoryConfig, provider: &EmbeddingProviderResult) -> EmbeddingCache {
        let provider_key = match provider.provider.as_ref() {
            Some(p) => EmbeddingCache::compute_provider_key(p.as_ref()),
            None => String::new(),
        };
        EmbeddingCache {
            provider_id: provider.provider_id().to_string(),
            model: provider.model_name().to_string(),
            provider_key,
            max_entries: config.cache_max_entries,
            enabled: config.cache_enabled,
        }
    }

    /// Mark the engine as dirty (files have changed on disk).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Main sync entry point. Checks reindex conditions, then performs
    /// either a full reindex or an incremental sync.
    pub async fn run_sync(&mut self, reason: &str, force: bool) -> Result<SyncReport> {
        debug!(reason, force, "memory sync: starting");

        let meta = self.memory_store.load_index_meta()?;
        let mut trigger =
            reindex::check_reindex_trigger(&meta, &self.embedding_provider, &self.config);

        // Schema migrations can request a forced reindex by setting the
        // `reindex_required` meta key (e.g. V3 added `project_id` columns
        // that existing chunks lack). Respect it once, then clear it after
        // a successful reindex below.
        if trigger.is_none() {
            let schema_flag =
                self.memory_store.get_meta("reindex_required")?.as_deref() == Some("1");
            if schema_flag {
                trigger = Some(reindex::ReindexTrigger::SchemaUpgrade);
            }
        }

        if force || trigger.is_some() {
            let trigger_desc = trigger
                .as_ref()
                .map(|t| format!("{t:?}"))
                .unwrap_or_else(|| "forced".to_string());
            debug!(trigger = %trigger_desc, "memory sync: full reindex required");

            let report = self.run_full_reindex().await?;
            self.dirty = false;
            return Ok(report);
        }

        // Incremental sync
        let mut report = SyncReport::default();

        if self.dirty || reason == "startup" {
            self.dirty = false;
            self.sync_memory_files(&mut report).await?;
        }

        self.sync_session_transcripts(&mut report).await?;

        // Update meta after successful sync
        self.write_current_meta()?;

        Ok(report)
    }

    /// Perform a full reindex: create temp DB, populate, atomic swap.
    async fn run_full_reindex(&mut self) -> Result<SyncReport> {
        let temp_id = uuid::Uuid::new_v4();
        let temp_path = PathBuf::from(format!("{}.tmp-{}", self.db_path.display(), temp_id));

        // Create temp database with schema
        let temp_store = MemoryStore::open(&temp_path)?;

        // Create a temporary sync engine pointing at the temp store
        let mut temp_engine = MemorySyncEngine::new(
            self.config.clone(),
            temp_store,
            self.main_db_path.clone(),
            Arc::clone(&self.embedding_provider),
            Arc::clone(&self.event_bus),
            self.memory_dir.clone(),
            temp_path.clone(),
        );

        // Run a full sync into the temp database
        let mut report = SyncReport::default();
        match async {
            temp_engine.sync_memory_files(&mut report).await?;
            temp_engine.sync_session_transcripts(&mut report).await?;
            temp_engine.write_current_meta()?;
            Ok::<(), crate::error::DaemonError>(())
        }
        .await
        {
            Ok(()) => {}
            Err(e) => {
                // Clean up temp files on failure
                drop(temp_engine);
                reindex::remove_index_files(&temp_path).await;
                return Err(e);
            }
        }
        report.reindexed = true;

        // Drop the temp engine to close its DB connection
        drop(temp_engine);

        // Atomic swap: rename temp -> target
        if let Err(e) = reindex::swap_index_files(&self.db_path, &temp_path).await {
            reindex::remove_index_files(&temp_path).await;
            return Err(e);
        }

        // Reopen the store pointing at the (now-replaced) database
        self.memory_store = MemoryStore::open(&self.db_path)?;

        Ok(report)
    }

    /// Sync memory files from disk. Compares hashes against stored records,
    /// re-indexes changed files, and prunes deleted file records.
    async fn sync_memory_files(&mut self, report: &mut SyncReport) -> Result<()> {
        // list_memory_files expects the workspace root (parent of memory/),
        // not the memory directory itself.
        let workspace_dir = self
            .memory_dir
            .parent()
            .unwrap_or(&self.memory_dir)
            .to_path_buf();
        let files = list_memory_files(&workspace_dir)?;
        let mut file_entries = Vec::new();
        for file_path in &files {
            match build_file_entry(file_path, &workspace_dir)? {
                Some(entry) => file_entries.push(entry),
                None => continue,
            }
        }

        let active_paths: HashSet<String> = file_entries.iter().map(|e| e.path.clone()).collect();

        for entry in &file_entries {
            let stored = self.memory_store.get_file(&entry.path)?;
            let stored_hash = stored.as_ref().map(|f| f.hash.as_str());

            if stored_hash == Some(&entry.hash) {
                report.files_unchanged += 1;
                continue;
            }

            // File is new or changed -- index it
            let content = match tokio::fs::read_to_string(&entry.abs_path).await {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            self.index_file(entry, &content, None).await?;
            report.files_indexed += 1;
        }

        // Prune stale records (files that no longer exist on disk)
        let stored_files = self.memory_store.list_files()?;
        for stored in &stored_files {
            if stored.source == MemorySource::Memory && !active_paths.contains(&stored.path) {
                self.memory_store
                    .delete_chunks_for_file(&stored.path, MemorySource::Memory)?;
                self.memory_store.delete_file(&stored.path)?;
                report.files_deleted += 1;
            }
        }

        Ok(())
    }

    /// Sync session transcripts. Reads conversation events from the main store,
    /// extracts text, and indexes changed sessions.
    async fn sync_session_transcripts(&mut self, report: &mut SyncReport) -> Result<()> {
        // Open a fresh read-only connection to the main store
        let main_store = match self.open_main_store() {
            Ok(s) => s,
            Err(e) => {
                warn!("memory sync: unable to open main store for session sync: {e}");
                return Ok(());
            }
        };

        let sessions = main_store.load_sessions()?;

        // Only `Completed` sessions are (re)indexed. An `Archived` session is
        // terminal and its transcript is immutable, so re-reading it on every
        // sync is pure cost: each iteration `load_events`-es the full event
        // stream just to hash it and hit the unchanged short-circuit below.
        // With thousands of archived sessions that dominated daemon RSS.
        let sessions_to_index: Vec<_> = sessions
            .iter()
            .filter(|s| matches!(s.status, rsi_common::types::SessionStatus::Completed))
            .collect();

        // Retention is deliberately WIDER than the index set: archived sessions
        // keep their already-indexed chunks so memory search still covers them.
        // Dropping them from `active_paths` would make the prune loop below
        // delete those entries. Sessions purged from the DB entirely are absent
        // here and so are still correctly pruned.
        let active_paths: HashSet<String> = sessions
            .iter()
            .filter(|s| {
                matches!(
                    s.status,
                    rsi_common::types::SessionStatus::Completed
                        | rsi_common::types::SessionStatus::Archived
                )
            })
            .map(|s| format!("sessions/{}", s.id))
            .collect();

        for session in &sessions_to_index {
            let path = format!("sessions/{}", session.id);
            let stored = self.memory_store.get_file(&path)?;
            let stored_project_id = stored.as_ref().and_then(|f| f.project_id);
            let project_changed = stored_project_id != session.project_id;

            // Fast path: decide from an indexed probe whether this transcript
            // moved at all, BEFORE paying to materialize it. The hash check
            // below is the same decision made on fully-loaded content, so
            // reaching it for an unchanged session is pure waste — and it was
            // the dominant cost of this loop across thousands of sessions.
            let watermark = main_store.event_watermark(session.id)?;
            if let Some(stored) = stored.as_ref()
                && stored.watermark.is_some()
                && stored.watermark == watermark
                && !project_changed
            {
                continue; // unchanged — events never loaded
            }

            let events = main_store.load_events(session.id)?;
            // Carry the session's project scope into the memory index so the
            // search path can filter agent-facing memory by project without
            // joining back to the main session store.
            let mut session_entry =
                match build_session_entry(session.id, session.project_id, &events) {
                    Some(entry) => entry,
                    None => continue,
                };
            session_entry.entry.watermark = watermark;

            let stored_hash = stored.as_ref().map(|f| f.hash.as_str());

            if stored_hash == Some(&session_entry.entry.hash) && !project_changed {
                // Content identical but the watermark was absent (pre-V4 row)
                // or stale. Persist the watermark so the fast path engages on
                // the next pass instead of reloading this session forever.
                self.memory_store.upsert_file(&session_entry.entry)?;
                continue;
            }

            if project_changed {
                debug!(
                    session_id = %session.id,
                    stored_project_id = ?stored_project_id,
                    new_project_id = ?session_entry.entry.project_id,
                    "memory sync: session project changed; reindexing transcript"
                );
            }

            // Per-session isolation: one session's indexing failure must not
            // abort the pass. This was previously `?`, which meant a single
            // transient provider error killed the whole sweep — and because
            // nothing downstream of the failure was ever indexed, the next pass
            // restarted from scratch and hit the same session again. A local
            // failure stays local; the session simply retries next pass.
            if let Err(e) = self
                .index_file(
                    &session_entry.entry,
                    &session_entry.content,
                    Some(&session_entry.line_map),
                )
                .await
            {
                warn!(
                    session_id = %session.id,
                    error = %e,
                    "memory sync: failed to index session transcript; skipping"
                );
                report.sessions_failed += 1;
                continue;
            }
            report.sessions_indexed += 1;
        }

        // Prune stale session records
        let stored_files = self.memory_store.list_files()?;
        for stored in &stored_files {
            if stored.source == MemorySource::Sessions && !active_paths.contains(&stored.path) {
                self.memory_store
                    .delete_chunks_for_file(&stored.path, MemorySource::Sessions)?;
                self.memory_store.delete_file(&stored.path)?;
            }
        }

        Ok(())
    }

    /// Index a single file: chunk -> embed -> write to DB.
    ///
    /// This is the core indexing pipeline that transforms raw content into
    /// searchable chunks with embeddings.
    async fn index_file(
        &mut self,
        entry: &MemoryFileEntry,
        content: &str,
        line_map: Option<&[u32]>,
    ) -> Result<()> {
        // 1. Chunk the content
        let mut chunks =
            chunk_markdown(content, self.config.chunk_tokens, self.config.chunk_overlap);
        chunks.retain(|c| !c.text.trim().is_empty());

        // 2. Enforce embedding model's max input tokens
        let max_input = self
            .embedding_provider
            .provider
            .as_ref()
            .and_then(|p| p.max_input_tokens())
            .unwrap_or(8192);
        let mut chunks = enforce_max_input_tokens(chunks, max_input);

        // 3. Remap line numbers for session transcripts
        if entry.source == MemorySource::Sessions
            && let Some(lm) = line_map
        {
            remap_chunk_lines(&mut chunks, lm);
        }

        // 4. Embed chunks (if provider available)
        let embeddings = match self.embedding_provider.provider.as_ref() {
            Some(_) => {
                let control_store = Arc::new(tokio::sync::Mutex::new(self.open_main_store()?));
                let control = EmbeddingControl {
                    store: control_store,
                    event_bus: Arc::clone(&self.event_bus),
                    owner: InvocationOwner {
                        project_id: entry.project_id,
                        ..InvocationOwner::default()
                    },
                    provider: self.embedding_provider.provider_label.clone(),
                    backend: self.embedding_provider.backend.clone(),
                    model: self.embedding_provider.model_name().to_string(),
                    base_url: self.embedding_provider.base_url.clone(),
                    trigger: "memory_embedding_index".to_string(),
                    dedup_namespace: format!("memory-embedding:{}", entry.path),
                };
                embed_chunks_in_batches(
                    self.embedding_provider.provider.as_deref().unwrap(),
                    &chunks,
                    &self.embedding_cache,
                    &self.memory_store,
                    Some(&control),
                )
                .await?
            }
            None => {
                // FTS-only mode: no embeddings
                vec![Vec::new(); chunks.len()]
            }
        };

        // 5. Determine vector dimensions and ensure vector table exists
        let vector_dims = embeddings.iter().find(|e| !e.is_empty()).map(|e| e.len());
        if let Some(dims) = vector_dims {
            self.memory_store.ensure_vector_table(dims as u32)?;
        }

        // 6. Build (chunk, embedding_json) pairs for index_file_chunks
        let model = self.embedding_provider.model_name();
        let chunks_with_embeddings: Vec<(MemoryChunk, String)> = chunks
            .into_iter()
            .zip(embeddings.iter())
            .map(|(chunk, emb)| {
                let emb_json = if emb.is_empty() {
                    String::new()
                } else {
                    serde_json::to_string(emb).unwrap_or_default()
                };
                (chunk, emb_json)
            })
            .collect();

        // 7. Write to DB using the composite operation
        self.memory_store.index_file_chunks(
            &entry.path,
            entry.source,
            model,
            &chunks_with_embeddings,
            entry,
        )?;

        Ok(())
    }

    /// Search the memory index. Delegates to the search engine (Phase 4).
    ///
    /// When `project_id` is `Some`, results are restricted to chunks whose
    /// indexed `project_id` matches. When `None`, the search is unscoped
    /// (manual / admin / debug behavior).
    pub async fn search(
        &self,
        query: &str,
        max_results: Option<usize>,
        min_score: Option<f64>,
        project_id: Option<uuid::Uuid>,
    ) -> Result<Vec<MemorySearchResult>> {
        let mut search_config = self.config.clone();
        if let Some(max) = max_results {
            search_config.max_results = max as u32;
        }
        if let Some(min) = min_score {
            search_config.min_score = min;
        }

        let pid_string = project_id.map(|p| p.to_string());
        let search_control = self
            .embedding_provider
            .provider
            .as_ref()
            .map(|_| {
                Ok::<EmbeddingControl, crate::error::DaemonError>(EmbeddingControl {
                    store: Arc::new(tokio::sync::Mutex::new(self.open_main_store()?)),
                    event_bus: Arc::clone(&self.event_bus),
                    owner: InvocationOwner {
                        project_id,
                        operator: Some("memory_search".to_string()),
                        ..InvocationOwner::default()
                    },
                    provider: self.embedding_provider.provider_label.clone(),
                    backend: self.embedding_provider.backend.clone(),
                    model: self.embedding_provider.model_name().to_string(),
                    base_url: self.embedding_provider.base_url.clone(),
                    trigger: "memory_embedding_query".to_string(),
                    dedup_namespace: format!(
                        "memory-search:{}",
                        pid_string.as_deref().unwrap_or("global")
                    ),
                })
            })
            .transpose()?;
        crate::memory::search::search(
            &self.memory_store,
            self.embedding_provider.provider.as_deref(),
            query,
            &search_config,
            pid_string.as_deref(),
            search_control.as_ref(),
        )
        .await
    }

    /// Build a status report of the memory system.
    pub fn status(&self) -> MemoryProviderStatus {
        let file_count = self.memory_store.file_count().unwrap_or(0);
        let chunk_count = self.memory_store.chunk_count().unwrap_or(0);

        MemoryProviderStatus {
            backend: "builtin".to_string(),
            provider: self.embedding_provider.provider_id().to_string(),
            model: Some(self.embedding_provider.model_name().to_string()),
            file_count,
            chunk_count,
            dirty: self.dirty,
            db_path: self.db_path.to_string_lossy().to_string(),
            fts_available: self.memory_store.fts_available(),
            vector_available: self.memory_store.vector_available(),
            vector_dims: self
                .memory_store
                .load_index_meta()
                .ok()
                .flatten()
                .and_then(|m| m.vector_dims),
            cache_entries: 0, // TODO: add cache count method
            observation_count: self.memory_store.count_observations().unwrap_or(0) as u32,
        }
    }

    /// Read a file from the memory directory.
    /// Validates the path is within memory scope before reading.
    /// `rel_path` is relative to the workspace root (e.g., "memory/test.md").
    pub async fn read_file(
        &self,
        rel_path: &str,
        from_line: Option<usize>,
        num_lines: Option<usize>,
    ) -> Result<String> {
        if !is_memory_path(rel_path) {
            return Err(crate::error::DaemonError::InvalidParam(format!(
                "path '{rel_path}' is not within memory scope"
            )));
        }

        // memory_dir is ~/.flywheel/memory/, but rel_path starts with "memory/"
        // so use the parent (workspace root) to resolve.
        let workspace_dir = self.memory_dir.parent().unwrap_or(&self.memory_dir);
        let abs_path = workspace_dir.join(rel_path);
        let content = tokio::fs::read_to_string(&abs_path).await.map_err(|e| {
            crate::error::DaemonError::InvalidParam(format!(
                "failed to read {}: {e}",
                abs_path.display()
            ))
        })?;

        match (from_line, num_lines) {
            (Some(from), Some(count)) => {
                let lines: Vec<&str> = content.lines().skip(from).take(count).collect();
                Ok(lines.join("\n"))
            }
            (Some(from), None) => {
                let lines: Vec<&str> = content.lines().skip(from).collect();
                Ok(lines.join("\n"))
            }
            _ => Ok(content),
        }
    }

    /// Write current configuration as index metadata.
    fn write_current_meta(&self) -> Result<()> {
        let meta = crate::memory::types::MemoryIndexMeta {
            model: self.embedding_provider.model_name().to_string(),
            provider: self.embedding_provider.provider_id().to_string(),
            provider_key: if self.embedding_cache.provider_key.is_empty() {
                None
            } else {
                Some(self.embedding_cache.provider_key.clone())
            },
            sources: self.config.sources.clone(),
            chunk_tokens: self.config.chunk_tokens,
            chunk_overlap: self.config.chunk_overlap,
            vector_dims: self
                .memory_store
                .load_index_meta()
                .ok()
                .flatten()
                .and_then(|m| m.vector_dims),
        };
        self.memory_store.save_index_meta(&meta)
    }

    /// Get the database path for cleanup operations.
    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }

    /// Get a shared handle to the memory store for background tasks.
    /// Wraps the store in an `Arc<Mutex>` for safe concurrent access.
    ///
    /// # Errors
    ///
    /// Returns an error if the memory database cannot be opened (disk full,
    /// file-descriptor exhaustion, permissions, or corruption).
    ///
    /// Opening a `SQLite` database is fallible (disk full, file-descriptor
    /// exhaustion, permissions, corruption), so this returns `Result` rather
    /// than panicking. It previously used `.expect(...)`, which gave a
    /// fallible operation an infallible signature: no caller *could* handle
    /// the error, and because the memory worker is an unsupervised
    /// `tokio::spawn` loop holding no `JoinHandle`, a single transient open
    /// failure silently killed memory, search, and observations for the
    /// remaining lifetime of the daemon — strictly worse than a crash, which
    /// would at least restart into a working state.
    ///
    /// NOTE: this still constructs a NEW connection per call, so the returned
    /// `Arc`s never alias one another. That is load-bearing: the
    /// `unsafe impl Sync for MemoryStore` in `memory/store.rs` justifies
    /// itself with an exclusive-ownership argument that no longer holds
    /// literally, and non-aliasing is what keeps it sound today. Caching a
    /// single shared connection here is the obvious performance fix and would
    /// make those `Arc`s alias — do not do it without first re-deriving that
    /// `unsafe impl` from the resulting code.
    pub fn store_handle(&self) -> Result<Arc<std::sync::Mutex<MemoryStore>>> {
        // The store itself is !Send, so we use std::sync::Mutex (not tokio::Mutex).
        Ok(Arc::new(std::sync::Mutex::new(MemoryStore::open(
            self.db_path.as_path(),
        )?)))
    }

    /// Get a shared handle to the main daemon store for admission/accounting work.
    /// # Errors
    ///
    /// Returns an error if the main daemon database cannot be opened.
    pub fn main_store_handle(&self) -> Result<Arc<tokio::sync::Mutex<Store>>> {
        Ok(Arc::new(tokio::sync::Mutex::new(Store::open(
            self.main_db_path.as_path(),
        )?)))
    }

    /// Get the memory config.
    pub fn config(&self) -> MemoryConfig {
        self.config.clone()
    }

    /// Get the embedding provider.
    pub fn embedding_provider(&self) -> Arc<EmbeddingProviderResult> {
        self.embedding_provider.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_fts_only_provider() -> Arc<EmbeddingProviderResult> {
        Arc::new(EmbeddingProviderResult {
            provider: None,
            requested: "none".to_string(),
            provider_label: "none".to_string(),
            backend: "none".to_string(),
            base_url: None,
            fallback_reason: None,
            unavailable_reason: None,
        })
    }

    fn setup_engine(dir: &TempDir) -> MemorySyncEngine {
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        let db_path = dir.path().join("memory.sqlite");
        let store = MemoryStore::open(&db_path).unwrap();

        let main_db = dir.path().join("flywheel.db");
        // Create the main store so it exists on disk
        let _main_store = Store::open(&main_db).unwrap();

        MemorySyncEngine::new(
            MemoryConfig {
                memory_dir: memory_dir.clone(),
                db_path: db_path.clone(),
                ..Default::default()
            },
            store,
            main_db,
            make_fts_only_provider(),
            Arc::new(EventBus::new(32)),
            memory_dir,
            db_path,
        )
    }

    /// Regression: opening the memory database is fallible, so `store_handle`
    /// must surface that as an error rather than unwinding.
    ///
    /// Before this fix the call site was `MemoryStore::open(..).expect(..)`.
    /// Because the memory worker is an unsupervised `tokio::spawn` loop whose
    /// `JoinHandle` is dropped, that panic killed the worker outright and
    /// silently disabled memory, search, and observations for the remaining
    /// lifetime of the daemon — with nothing logged to say why.
    #[test]
    fn store_handle_returns_err_instead_of_panicking_when_db_cannot_be_opened() {
        let dir = TempDir::new().unwrap();
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();

        // A genuinely openable store, so construction itself succeeds.
        let good_db = dir.path().join("memory.sqlite");
        let store = MemoryStore::open(&good_db).unwrap();
        let main_db = dir.path().join("flywheel.db");
        let _main_store = Store::open(&main_db).unwrap();

        // ...but hand the engine a `db_path` that cannot be opened as a
        // database. A directory is never a valid SQLite file, so the open
        // fails deterministically instead of silently creating a new file.
        let unopenable = dir.path().join("not-a-database");
        std::fs::create_dir_all(&unopenable).unwrap();

        let engine = MemorySyncEngine::new(
            MemoryConfig {
                memory_dir: memory_dir.clone(),
                db_path: unopenable.clone(),
                ..Default::default()
            },
            store,
            main_db,
            make_fts_only_provider(),
            Arc::new(EventBus::new(32)),
            memory_dir,
            unopenable,
        );

        let result = engine.store_handle();
        assert!(
            result.is_err(),
            "store_handle must return Err when the database cannot be opened, not panic"
        );
    }

    #[tokio::test]
    async fn test_sync_empty_directory() {
        let dir = TempDir::new().unwrap();
        let mut engine = setup_engine(&dir);
        let report = engine.run_sync("test", false).await.unwrap();
        assert_eq!(report.files_indexed, 0);
        assert_eq!(report.files_unchanged, 0);
        assert_eq!(report.files_deleted, 0);
    }

    #[tokio::test]
    async fn test_sync_single_memory_file() {
        let dir = TempDir::new().unwrap();
        let mut engine = setup_engine(&dir);

        let memory_dir = dir.path().join("memory");
        std::fs::write(memory_dir.join("test.md"), "# Test\nHello world").unwrap();

        let report = engine.run_sync("test", false).await.unwrap();
        assert_eq!(report.files_indexed, 1);
    }

    #[tokio::test]
    async fn test_sync_unchanged_file_skipped() {
        let dir = TempDir::new().unwrap();
        let mut engine = setup_engine(&dir);
        let memory_dir = dir.path().join("memory");
        std::fs::write(memory_dir.join("test.md"), "# Test\nHello world").unwrap();

        let r1 = engine.run_sync("test", false).await.unwrap();
        assert_eq!(r1.files_indexed, 1);

        engine.mark_dirty();
        let r2 = engine.run_sync("test", false).await.unwrap();
        assert_eq!(r2.files_indexed, 0);
        assert_eq!(r2.files_unchanged, 1);
    }

    #[tokio::test]
    async fn test_sync_changed_file_reindexed() {
        let dir = TempDir::new().unwrap();
        let mut engine = setup_engine(&dir);
        let memory_dir = dir.path().join("memory");
        std::fs::write(memory_dir.join("test.md"), "# Test\nVersion 1").unwrap();

        engine.run_sync("test", false).await.unwrap();

        std::fs::write(memory_dir.join("test.md"), "# Test\nVersion 2 - changed").unwrap();
        engine.mark_dirty();
        let r2 = engine.run_sync("test", false).await.unwrap();
        assert_eq!(r2.files_indexed, 1);
    }

    #[tokio::test]
    async fn test_sync_deleted_file_pruned() {
        let dir = TempDir::new().unwrap();
        let mut engine = setup_engine(&dir);
        let memory_dir = dir.path().join("memory");
        std::fs::write(memory_dir.join("test.md"), "# Test\nHello").unwrap();

        engine.run_sync("test", false).await.unwrap();

        std::fs::remove_file(memory_dir.join("test.md")).unwrap();
        engine.mark_dirty();
        let r2 = engine.run_sync("test", false).await.unwrap();
        assert_eq!(r2.files_deleted, 1);
    }

    #[tokio::test]
    async fn test_sync_multiple_files() {
        let dir = TempDir::new().unwrap();
        let mut engine = setup_engine(&dir);
        let memory_dir = dir.path().join("memory");

        std::fs::write(memory_dir.join("a.md"), "# A\nContent A").unwrap();
        std::fs::write(memory_dir.join("b.md"), "# B\nContent B").unwrap();
        std::fs::write(memory_dir.join("c.md"), "# C\nContent C").unwrap();

        let report = engine.run_sync("test", false).await.unwrap();
        assert_eq!(report.files_indexed, 3);
    }

    #[tokio::test]
    async fn test_read_file_valid_path() {
        let dir = TempDir::new().unwrap();
        let engine = setup_engine(&dir);
        let memory_dir = dir.path().join("memory");
        std::fs::write(memory_dir.join("test.md"), "line1\nline2\nline3").unwrap();

        let content = engine
            .read_file("memory/test.md", None, None)
            .await
            .unwrap();
        assert_eq!(content, "line1\nline2\nline3");
    }

    #[tokio::test]
    async fn test_read_file_invalid_scope() {
        let dir = TempDir::new().unwrap();
        let engine = setup_engine(&dir);

        let result = engine.read_file("../../etc/passwd", None, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_file_with_line_range() {
        let dir = TempDir::new().unwrap();
        let engine = setup_engine(&dir);
        let memory_dir = dir.path().join("memory");
        std::fs::write(
            memory_dir.join("test.md"),
            "line0\nline1\nline2\nline3\nline4",
        )
        .unwrap();

        let content = engine
            .read_file("memory/test.md", Some(1), Some(2))
            .await
            .unwrap();
        assert_eq!(content, "line1\nline2");
    }

    #[tokio::test]
    async fn test_status_reports_correct_counts() {
        let dir = TempDir::new().unwrap();
        let mut engine = setup_engine(&dir);
        let memory_dir = dir.path().join("memory");

        std::fs::write(memory_dir.join("a.md"), "# A\nContent A").unwrap();
        std::fs::write(memory_dir.join("b.md"), "# B\nContent B").unwrap();

        engine.run_sync("test", false).await.unwrap();

        let status = engine.status();
        assert_eq!(status.file_count, 2);
        assert!(status.chunk_count > 0);
    }

    #[tokio::test]
    async fn test_sync_marks_dirty_false() {
        let dir = TempDir::new().unwrap();
        let mut engine = setup_engine(&dir);
        assert!(engine.dirty);

        engine.run_sync("test", false).await.unwrap();
        // After run_sync, dirty should be false (it was consumed)
        // Next sync with reason != "startup" should skip file sync
        let mut report = SyncReport::default();
        // sync_memory_files won't run because dirty is false
        engine.sync_memory_files(&mut report).await.unwrap();
        // This is a direct call, it always runs. The dirty flag is checked in run_sync.
    }

    #[tokio::test]
    async fn test_search_returns_empty() {
        let dir = TempDir::new().unwrap();
        let engine = setup_engine(&dir);
        let results = engine.search("test query", None, None, None).await.unwrap();
        assert!(results.is_empty());
    }
}
