use std::collections::HashMap;
use std::path::Path;
use std::sync::Once;

type ChunkRow = (
    String,
    String,
    String,
    u32,
    u32,
    String,
    String,
    String,
    String,
    i64,
    Option<String>,
);

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::memory::types::{
    EmbeddingCacheEntry, MemoryChunk, MemoryFileEntry, MemoryIndexMeta, MemorySource, StoredChunk,
    str_to_memory_source,
};

/// Store-layer row for observation persistence.
/// Uses String IDs and JSON serialization for DB storage.
#[derive(Debug, Clone)]
pub struct ObservationRow {
    pub id: String,
    pub session_id: String,
    pub project_id: Option<String>,
    pub level: String,
    pub content: String,
    pub source_ids: String, // JSON array of UUIDs
    pub confidence: Option<String>,
    pub times_derived: u32,
    pub embedding: String, // JSON f32 array
    pub created_at: String,
    pub updated_at: String,
}

// FFI binding to the vendored sqlite-vec init function (compiled via build.rs)
unsafe extern "C" {
    fn sqlite3_vec_init(
        db: *mut rusqlite::ffi::sqlite3,
        pz_err_msg: *mut *mut std::os::raw::c_char,
        p_api: *const rusqlite::ffi::sqlite3_api_routines,
    ) -> std::os::raw::c_int;
}

static REGISTER_VEC: Once = Once::new();
static mut VEC_REGISTERED: bool = false;

/// Register the vendored sqlite-vec as a statically-linked auto-extension.
/// This is called once and applies to all future connections.
pub fn register_sqlite_vec() -> bool {
    REGISTER_VEC.call_once(|| {
        let rc = unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::os::raw::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> std::os::raw::c_int,
            >(sqlite3_vec_init as *const ())))
        };
        if rc == rusqlite::ffi::SQLITE_OK {
            tracing::info!("sqlite-vec vendored extension registered as auto-extension");
            unsafe { VEC_REGISTERED = true };
        } else {
            tracing::warn!("Failed to register vendored sqlite-vec: rc={}", rc);
        }
    });
    unsafe { VEC_REGISTERED }
}

pub struct MemoryStore {
    conn: Connection,
    fts_available: bool,
    vector_available: bool,
    vec_extension_loaded: bool,
    /// Embedding width of the live vec0 tables, once known. Tracked so a
    /// mid-process embedding-model swap is detected rather than silently
    /// writing into a table declared at the previous width.
    vector_dims: Option<u32>,
}

// SAFETY: MemoryStore is exclusively owned by MemorySyncEngine inside MemoryWorker,
// which processes commands sequentially on a single tokio task. No concurrent access
// to the Connection is possible. The Sync impl is needed because async search methods
// hold &MemoryStore across .await points (embed_query calls), and tokio::spawn requires
// Send futures. &T is Send only when T: Sync. rusqlite::Connection is Send but not Sync
// due to internal RefCell. The worker's sequential processing invariant makes this safe.
unsafe impl Sync for MemoryStore {}

impl MemoryStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        let vec_extension_loaded = Self::try_load_vec_extension(&conn);

        let mut store = Self {
            conn,
            fts_available: false,
            vector_available: false,
            vec_extension_loaded,
            vector_dims: None,
        };
        let (fts_available, fts_error) = store.init_schema()?;
        store.fts_available = fts_available;

        if let Some(err) = fts_error {
            tracing::warn!("FTS5 not available: {}", err);
        }

        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let vec_extension_loaded = Self::try_load_vec_extension(&conn);

        let mut store = Self {
            conn,
            fts_available: false,
            vector_available: false,
            vec_extension_loaded,
            vector_dims: None,
        };
        let (fts_available, _) = store.init_schema()?;
        store.fts_available = fts_available;
        Ok(store)
    }

    pub fn fts_available(&self) -> bool {
        self.fts_available
    }

    pub fn vector_available(&self) -> bool {
        self.vector_available
    }

    pub fn vec_extension_loaded(&self) -> bool {
        self.vec_extension_loaded
    }

    /// Returns a reference to the underlying SQLite connection.
    /// Used by search modules for read-only queries.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Get the modification time (Unix ms) for a tracked file by path.
    /// Returns None if the file is not in the index.
    pub fn get_file_mtime(&self, path: &str) -> Option<i64> {
        self.conn
            .query_row("SELECT mtime FROM files WHERE path = ?1", [path], |row| {
                row.get(0)
            })
            .ok()
    }

    /// Attempt to load the sqlite-vec extension.
    /// First tries the vendored statically-linked auto-extension (registered once
    /// via `register_sqlite_vec()`). Falls back to dynamic loading via
    /// `RSI_SQLITE_VEC_PATH` environment variable (legacy `MOTHERSHIP_SQLITE_VEC_PATH` /
    /// `FLYWHEEL_SQLITE_VEC_PATH` still honored).
    fn try_load_vec_extension(conn: &Connection) -> bool {
        // Try vendored auto-extension first.
        // register_sqlite_vec() sets up auto_extension for future connections,
        // but this connection is already open. Explicitly call the init function.
        if register_sqlite_vec() {
            let rc =
                unsafe { sqlite3_vec_init(conn.handle(), std::ptr::null_mut(), std::ptr::null()) };
            if rc == rusqlite::ffi::SQLITE_OK {
                // Verify it actually works
                let works = conn
                    .query_row("SELECT vec_version()", [], |_row| Ok(()))
                    .is_ok();
                if works {
                    return true;
                }
            }
            tracing::debug!("Vendored sqlite-vec init returned rc={}", rc);
        }

        // Fall back to dynamic loading via env var
        let ext_path = match rsi_common::identity::env_with_legacy(
            "RSI_SQLITE_VEC_PATH",
            &["MOTHERSHIP_SQLITE_VEC_PATH", "FLYWHEEL_SQLITE_VEC_PATH"],
        ) {
            Ok(p) if !p.is_empty() => p,
            _ => {
                tracing::debug!(
                    "sqlite-vec not available (no vendored extension, RSI_SQLITE_VEC_PATH not set)"
                );
                return false;
            }
        };

        unsafe {
            if let Err(e) = conn.load_extension_enable() {
                tracing::warn!("Failed to enable extension loading: {}", e);
                return false;
            }

            let result = conn.load_extension(&ext_path, None);

            if let Err(e) = conn.load_extension_disable() {
                tracing::warn!("Failed to disable extension loading: {}", e);
            }

            match result {
                Ok(()) => {
                    tracing::info!("sqlite-vec extension loaded from {}", ext_path);
                    true
                }
                Err(e) => {
                    tracing::warn!("Failed to load sqlite-vec from {}: {}", ext_path, e);
                    false
                }
            }
        }
    }

    fn init_schema(&self) -> Result<(bool, Option<String>)> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap_or(0);

        let mut fts_error: Option<String> = None;

        if version < 1 {
            // V1: Initial schema
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS meta (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS files (
                    path   TEXT PRIMARY KEY,
                    source TEXT NOT NULL DEFAULT 'memory',
                    hash   TEXT NOT NULL,
                    mtime  INTEGER NOT NULL,
                    size   INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS chunks (
                    id         TEXT PRIMARY KEY,
                    path       TEXT NOT NULL,
                    source     TEXT NOT NULL DEFAULT 'memory',
                    start_line INTEGER NOT NULL,
                    end_line   INTEGER NOT NULL,
                    hash       TEXT NOT NULL,
                    model      TEXT NOT NULL,
                    text       TEXT NOT NULL,
                    embedding  TEXT NOT NULL DEFAULT '',
                    updated_at INTEGER NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_chunks_path
                    ON chunks(path);
                CREATE INDEX IF NOT EXISTS idx_chunks_source
                    ON chunks(source);
                CREATE INDEX IF NOT EXISTS idx_chunks_path_source
                    ON chunks(path, source);

                CREATE TABLE IF NOT EXISTS embedding_cache (
                    provider     TEXT NOT NULL,
                    model        TEXT NOT NULL,
                    provider_key TEXT NOT NULL,
                    hash         TEXT NOT NULL,
                    embedding    TEXT NOT NULL,
                    dims         INTEGER,
                    updated_at   INTEGER NOT NULL,
                    PRIMARY KEY (provider, model, provider_key, hash)
                );

                CREATE INDEX IF NOT EXISTS idx_embedding_cache_updated_at
                    ON embedding_cache(updated_at);",
            )?;

            // FTS5: attempt creation, degrade gracefully
            let (_, err) = self.try_create_fts_table();
            fts_error = err;

            self.conn.execute("PRAGMA user_version = 1", [])?;
        }

        if version < 2 {
            // V2: Observations table for tiered observation extraction
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS observations (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    project_id TEXT,
                    level TEXT NOT NULL DEFAULT 'explicit',
                    content TEXT NOT NULL,
                    source_ids TEXT DEFAULT '[]',
                    confidence TEXT,
                    times_derived INTEGER NOT NULL DEFAULT 1,
                    embedding TEXT DEFAULT '',
                    deleted_at TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_observations_session
                    ON observations(session_id);
                CREATE INDEX IF NOT EXISTS idx_observations_project
                    ON observations(project_id);
                CREATE INDEX IF NOT EXISTS idx_observations_level
                    ON observations(level);
                CREATE INDEX IF NOT EXISTS idx_observations_created
                    ON observations(created_at);",
            )?;

            // Observations FTS: attempt creation, degrade gracefully
            let _ = self.conn.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS observations_fts USING fts5(
                    content,
                    id UNINDEXED,
                    session_id UNINDEXED,
                    project_id UNINDEXED,
                    level UNINDEXED
                );",
            );

            self.conn.execute("PRAGMA user_version = 2", [])?;
        }

        if version < 3 {
            // V3: denormalize project_id into files and chunks so memory
            // search can filter by project without joining back to the main
            // session store. `project_id` is NULL for memory files (global)
            // and for legacy pre-V3 rows that have not yet been reindexed.
            //
            // Brand-new stores (version == 0) skip the reindex_required
            // flag because there is no pre-existing data to backfill —
            // their files/chunks tables are created fresh below with the
            // V3 column layout already populated.
            let is_brand_new = version == 0;

            self.conn.execute_batch(
                "ALTER TABLE files ADD COLUMN project_id TEXT;
                 ALTER TABLE chunks ADD COLUMN project_id TEXT;
                 CREATE INDEX IF NOT EXISTS idx_files_project_id
                     ON files(project_id);
                 CREATE INDEX IF NOT EXISTS idx_chunks_project_id
                     ON chunks(project_id);
                 CREATE INDEX IF NOT EXISTS idx_chunks_project_source
                     ON chunks(project_id, source);",
            )?;

            // chunks_fts is a contentless FTS5 virtual table; ALTER TABLE
            // is not supported on virtual tables. Drop and recreate so the
            // search path can filter by project_id directly in FTS queries.
            // The reindex trigger below repopulates this on the next sync.
            let _ = self.conn.execute_batch("DROP TABLE IF EXISTS chunks_fts;");
            let _ = self.try_create_fts_table();

            // For existing stores being upgraded in place, mark the index
            // for a forced reindex on next sync so existing chunks acquire
            // project IDs (see check_reindex_trigger). Brand-new stores
            // skip this — there is nothing to backfill.
            if !is_brand_new {
                self.conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('reindex_required', '1')
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    [],
                )?;
            }

            self.conn.execute("PRAGMA user_version = 3", [])?;
        }

        if version < 4 {
            // V4: cheap change-detection watermark for session transcripts.
            //
            // Session sync previously loaded every event of every candidate
            // session on every pass just to hash the transcript and discover
            // nothing had changed. `watermark` stores an indexed-probe summary
            // (see `Store::event_watermark`) so an unchanged session can be
            // skipped without materializing its events at all.
            //
            // NULL means "unknown" — the pre-V4 state and the safe default —
            // which forces a full load exactly once per session, after which
            // the watermark is populated. No reindex flag is needed.
            self.conn
                .execute_batch("ALTER TABLE files ADD COLUMN watermark TEXT;")?;

            self.conn.execute("PRAGMA user_version = 4", [])?;
        }

        let fts_available = self.check_fts_available();
        Ok((fts_available, fts_error))
    }

    fn try_create_fts_table(&self) -> (bool, Option<String>) {
        let result = self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                text,
                id UNINDEXED,
                path UNINDEXED,
                source UNINDEXED,
                project_id UNINDEXED,
                model UNINDEXED,
                start_line UNINDEXED,
                end_line UNINDEXED
            );",
        );
        match result {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        }
    }

    fn check_fts_available(&self) -> bool {
        self.conn
            .prepare("SELECT 1 FROM chunks_fts LIMIT 0")
            .is_ok()
    }

    // -------------------------------------------------------------------------
    // Meta CRUD
    // -------------------------------------------------------------------------

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let result = self.conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        );
        match result {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_meta(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM meta WHERE key = ?1", params![key])?;
        Ok(())
    }

    pub fn load_index_meta(&self) -> Result<Option<MemoryIndexMeta>> {
        let model = self.get_meta("model")?;
        let provider = self.get_meta("provider")?;

        let model = match model {
            Some(m) => m,
            None => return Ok(None),
        };
        let provider = match provider {
            Some(p) => p,
            None => return Ok(None),
        };

        let provider_key = self.get_meta("provider_key")?;
        let sources = self
            .get_meta("sources")?
            .map(|s| {
                s.split(',')
                    .map(str_to_memory_source)
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_else(|| vec![MemorySource::Memory]);
        let chunk_tokens = self
            .get_meta("chunk_tokens")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(400);
        let chunk_overlap = self
            .get_meta("chunk_overlap")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(80);
        let vector_dims = self.get_meta("vector_dims")?.and_then(|s| s.parse().ok());

        Ok(Some(MemoryIndexMeta {
            model,
            provider,
            provider_key,
            sources,
            chunk_tokens,
            chunk_overlap,
            vector_dims,
        }))
    }

    pub fn save_index_meta(&self, meta: &MemoryIndexMeta) -> Result<()> {
        self.set_meta("model", &meta.model)?;
        self.set_meta("provider", &meta.provider)?;

        if let Some(ref key) = meta.provider_key {
            self.set_meta("provider_key", key)?;
        }

        let sources_str: String = meta
            .sources
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(",");
        self.set_meta("sources", &sources_str)?;
        self.set_meta("chunk_tokens", &meta.chunk_tokens.to_string())?;
        self.set_meta("chunk_overlap", &meta.chunk_overlap.to_string())?;

        if let Some(dims) = meta.vector_dims {
            self.set_meta("vector_dims", &dims.to_string())?;
        }

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Files CRUD
    // -------------------------------------------------------------------------

    pub fn upsert_file(&self, entry: &MemoryFileEntry) -> Result<()> {
        let project_id_str = entry.project_id.as_ref().map(|u| u.to_string());
        self.conn.execute(
            "INSERT INTO files (path, source, hash, mtime, size, project_id, watermark)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(path) DO UPDATE SET
                 source = excluded.source,
                 hash = excluded.hash,
                 mtime = excluded.mtime,
                 size = excluded.size,
                 project_id = excluded.project_id,
                 watermark = excluded.watermark",
            params![
                entry.path,
                entry.source.as_str(),
                entry.hash,
                entry.mtime_ms,
                entry.size,
                project_id_str,
                entry.watermark,
            ],
        )?;
        Ok(())
    }

    pub fn get_file(&self, path: &str) -> Result<Option<MemoryFileEntry>> {
        let result = self.conn.query_row(
            "SELECT path, source, hash, mtime, size, project_id, watermark
             FROM files WHERE path = ?1",
            params![path],
            |row| {
                Ok(FileRow {
                    path: row.get(0)?,
                    source_str: row.get(1)?,
                    hash: row.get(2)?,
                    mtime: row.get(3)?,
                    size: row.get(4)?,
                    project_id: row.get(5)?,
                    watermark: row.get(6)?,
                })
            },
        );
        match result {
            Ok(row) => Ok(Some(row.into_file_entry()?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_files(&self) -> Result<Vec<MemoryFileEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, source, hash, mtime, size, project_id, watermark
             FROM files ORDER BY path ASC",
        )?;

        let rows = stmt
            .query_map([], |row| {
                Ok(FileRow {
                    path: row.get(0)?,
                    source_str: row.get(1)?,
                    hash: row.get(2)?,
                    mtime: row.get(3)?,
                    size: row.get(4)?,
                    project_id: row.get(5)?,
                    watermark: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter().map(|r| r.into_file_entry()).collect()
    }

    pub fn delete_file(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn file_count(&self) -> Result<u32> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
        Ok(count as u32)
    }

    // -------------------------------------------------------------------------
    // Chunks CRUD
    // -------------------------------------------------------------------------

    pub fn insert_chunks(
        &self,
        path: &str,
        source: MemorySource,
        model: &str,
        chunks: &[(MemoryChunk, String)],
        project_id: Option<&str>,
    ) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp();
        let tx = self.conn.unchecked_transaction()?;

        let mut stmt = tx.prepare(
            "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, project_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                 text = excluded.text,
                 embedding = excluded.embedding,
                 model = excluded.model,
                 updated_at = excluded.updated_at,
                 project_id = excluded.project_id",
        )?;

        for (chunk, embedding_json) in chunks {
            let id = StoredChunk::make_id(path, source, chunk.start_line, &chunk.hash);
            stmt.execute(params![
                id,
                path,
                source.as_str(),
                chunk.start_line,
                chunk.end_line,
                chunk.hash,
                model,
                chunk.text,
                embedding_json,
                now,
                project_id,
            ])?;
        }

        drop(stmt);
        tx.commit()?;
        Ok(())
    }

    pub fn delete_chunks_for_file(&self, path: &str, source: MemorySource) -> Result<u64> {
        let deleted = self.conn.execute(
            "DELETE FROM chunks WHERE path = ?1 AND source = ?2",
            params![path, source.as_str()],
        )?;
        Ok(deleted as u64)
    }

    pub fn get_chunks_for_file(
        &self,
        path: &str,
        source: MemorySource,
    ) -> Result<Vec<StoredChunk>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, project_id
             FROM chunks WHERE path = ?1 AND source = ?2 ORDER BY start_line ASC",
        )?;

        let rows = stmt
            .query_map(params![path, source.as_str()], Self::map_chunk_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter().map(Self::convert_chunk_row).collect()
    }

    pub fn get_all_chunks(&self) -> Result<Vec<StoredChunk>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, project_id
             FROM chunks ORDER BY path ASC, start_line ASC",
        )?;

        let rows = stmt
            .query_map([], Self::map_chunk_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter().map(Self::convert_chunk_row).collect()
    }

    pub fn chunk_count(&self) -> Result<u32> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
        Ok(count as u32)
    }

    // -------------------------------------------------------------------------
    // Embedding Cache CRUD
    // -------------------------------------------------------------------------

    pub fn load_embedding_cache(
        &self,
        provider: &str,
        model: &str,
        provider_key: &str,
        hashes: &[&str],
    ) -> Result<HashMap<String, String>> {
        let mut result = HashMap::new();

        if hashes.is_empty() {
            return Ok(result);
        }

        // Batch into groups of 500 to stay under SQLite variable limit
        for batch in hashes.chunks(500) {
            let placeholders: String = batch
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 4))
                .collect::<Vec<_>>()
                .join(",");

            let sql = format!(
                "SELECT hash, embedding FROM embedding_cache
                 WHERE provider = ?1 AND model = ?2 AND provider_key = ?3
                 AND hash IN ({})",
                placeholders
            );

            let mut stmt = self.conn.prepare(&sql)?;

            let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
                Box::new(provider.to_string()),
                Box::new(model.to_string()),
                Box::new(provider_key.to_string()),
            ];
            for hash in batch {
                param_values.push(Box::new(hash.to_string()));
            }

            let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                param_values.iter().map(|b| b.as_ref()).collect();

            let rows = stmt
                .query_map(params_ref.as_slice(), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            for (hash, embedding) in rows {
                result.insert(hash, embedding);
            }
        }

        Ok(result)
    }

    pub fn upsert_embedding_cache(&self, entries: &[EmbeddingCacheEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare(
            "INSERT INTO embedding_cache (provider, model, provider_key, hash, embedding, dims, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(provider, model, provider_key, hash) DO UPDATE SET
                 embedding = excluded.embedding,
                 dims = excluded.dims,
                 updated_at = excluded.updated_at",
        )?;

        for entry in entries {
            stmt.execute(params![
                entry.provider,
                entry.model,
                entry.provider_key,
                entry.hash,
                entry.embedding,
                entry.dims.map(|d| d as i64),
                entry.updated_at,
            ])?;
        }

        drop(stmt);
        tx.commit()?;
        Ok(())
    }

    pub fn prune_embedding_cache(&self, max_entries: u32) -> Result<u64> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM embedding_cache", [], |row| row.get(0))?;

        if count <= max_entries as i64 {
            return Ok(0);
        }

        let to_delete = count - max_entries as i64;
        let deleted = self.conn.execute(
            "DELETE FROM embedding_cache WHERE rowid IN (
                SELECT rowid FROM embedding_cache ORDER BY updated_at ASC LIMIT ?1
            )",
            params![to_delete],
        )?;

        Ok(deleted as u64)
    }

    pub fn cache_entry_count(&self) -> Result<u32> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM embedding_cache", [], |row| row.get(0))?;
        Ok(count as u32)
    }

    // -------------------------------------------------------------------------
    // FTS5 Operations
    // -------------------------------------------------------------------------

    pub fn insert_fts_chunks(&self, chunks: &[StoredChunk]) -> Result<()> {
        if !self.fts_available {
            return Ok(());
        }

        let tx = self.conn.unchecked_transaction()?;
        let mut stmt = tx.prepare(
            "INSERT INTO chunks_fts (text, id, path, source, project_id, model, start_line, end_line)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;

        for chunk in chunks {
            stmt.execute(params![
                chunk.text,
                chunk.id,
                chunk.path,
                chunk.source.as_str(),
                chunk.project_id,
                chunk.model,
                chunk.start_line,
                chunk.end_line,
            ])?;
        }

        drop(stmt);
        tx.commit()?;
        Ok(())
    }

    pub fn delete_fts_chunks_for_file(
        &self,
        path: &str,
        source: MemorySource,
        model: &str,
    ) -> Result<()> {
        if !self.fts_available {
            return Ok(());
        }

        self.conn.execute(
            "DELETE FROM chunks_fts WHERE path = ?1 AND source = ?2 AND model = ?3",
            params![path, source.as_str(), model],
        )?;
        Ok(())
    }

    pub fn search_fts(&self, query: &str, limit: u32) -> Result<Vec<(String, f64)>> {
        if !self.fts_available {
            return Ok(vec![]);
        }

        let mut stmt = self.conn.prepare(
            "SELECT id, rank FROM chunks_fts
             WHERE chunks_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;

        let rows = stmt
            .query_map(params![query, limit], |row| {
                let id: String = row.get(0)?;
                let rank: f64 = row.get(1)?;
                Ok((id, rank))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    // -------------------------------------------------------------------------
    // Vector Table Operations
    // -------------------------------------------------------------------------

    /// Embedding width declared by an existing vec0 table, read back from the
    /// DDL recorded in `sqlite_master`. `None` when the table is absent or its
    /// declaration cannot be parsed.
    fn existing_vector_dims(&self, table: &str) -> Option<u32> {
        let ddl: String = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |row| row.get(0),
            )
            .ok()?;
        parse_vec0_dims(&ddl)
    }

    /// Create the vec0 tables at `dims`, recreating them if they already exist
    /// at a different width.
    ///
    /// A vec0 column validates vector length on insert, so a table left over
    /// from a previous embedding model can never accept the new vectors — every
    /// write would fail and vector search would silently degrade to FTS-only.
    /// The stored vectors are derived data: `ReindexTrigger::ModelChanged`
    /// forces a full reindex on any embedding-model swap, which repopulates
    /// them from the source files.
    pub fn ensure_vector_table(&mut self, dims: u32) -> Result<()> {
        if self.vector_available && self.vector_dims == Some(dims) {
            return Ok(());
        }

        if !self.vec_extension_loaded {
            tracing::debug!("sqlite-vec extension not loaded, skipping vector table creation");
            return Ok(());
        }

        // Keep these identifiers in the literal iterator so crate-wide SQL
        // provenance checks can prove every DDL target and column statically.
        for (table, id_col) in [
            ("chunks_vec", "id"),
            ("observation_vectors", "observation_id"),
        ] {
            if let Some(existing) = self.existing_vector_dims(table) {
                if existing == dims {
                    continue;
                }
                tracing::warn!(
                    table = table,
                    existing_dims = existing,
                    new_dims = dims,
                    "embedding dimensionality changed; recreating vector table \
                     (vectors are derived and repopulate on reindex)"
                );
                if let Err(e) = self
                    .conn
                    .execute_batch(&format!("DROP TABLE IF EXISTS {}", table))
                {
                    tracing::warn!("failed to drop stale vector table {}: {}", table, e);
                    if table == "chunks_vec" {
                        self.vector_available = false;
                        return Ok(()); // Degrade gracefully
                    }
                    continue;
                }
            }

            let sql = format!(
                "CREATE VIRTUAL TABLE IF NOT EXISTS {} USING vec0(
                {} TEXT PRIMARY KEY,
                embedding float[{}]
            )",
                table, id_col, dims
            );

            if let Err(e) = self.conn.execute_batch(&sql) {
                tracing::warn!("{} table creation failed: {}", table, e);
                if table == "chunks_vec" {
                    self.vector_available = false;
                    return Ok(()); // Degrade gracefully
                }
            }
        }

        self.vector_available = true;
        self.vector_dims = Some(dims);

        Ok(())
    }

    pub fn insert_vector_chunks(&self, chunks: &[(String, Vec<f32>)]) -> Result<()> {
        if !self.vector_available {
            return Ok(());
        }

        let tx = self.conn.unchecked_transaction()?;

        // vec0 virtual tables don't support UPSERT, so delete-then-insert
        {
            let mut del_stmt = tx.prepare("DELETE FROM chunks_vec WHERE id = ?1")?;
            let mut ins_stmt =
                tx.prepare("INSERT INTO chunks_vec (id, embedding) VALUES (?1, ?2)")?;

            for (id, embedding) in chunks {
                let blob = embedding_to_blob(embedding);
                del_stmt.execute(params![id])?;
                ins_stmt.execute(params![id, blob])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    pub fn delete_vector_chunks_for_file(&self, path: &str, source: MemorySource) -> Result<()> {
        if !self.vector_available {
            return Ok(());
        }

        self.conn.execute(
            "DELETE FROM chunks_vec WHERE id IN (
                SELECT id FROM chunks WHERE path = ?1 AND source = ?2
            )",
            params![path, source.as_str()],
        )?;
        Ok(())
    }

    pub fn search_vector(&self, query_embedding: &[f32], limit: u32) -> Result<Vec<(String, f64)>> {
        if !self.vector_available {
            return Ok(vec![]);
        }

        let blob = embedding_to_blob(query_embedding);
        let mut stmt = self.conn.prepare(
            "SELECT id, distance FROM chunks_vec
             WHERE embedding MATCH ?1
             ORDER BY distance
             LIMIT ?2",
        )?;

        let rows = stmt
            .query_map(params![blob, limit], |row| {
                let id: String = row.get(0)?;
                let distance: f64 = row.get(1)?;
                Ok((id, distance))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    // -------------------------------------------------------------------------
    // Observation CRUD
    // -------------------------------------------------------------------------

    /// Batch insert observations with optional embeddings.
    pub fn insert_observations(&self, observations: &[ObservationRow]) -> Result<()> {
        if observations.is_empty() {
            return Ok(());
        }

        let tx = self.conn.unchecked_transaction()?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO observations (id, session_id, project_id, level, content, source_ids, confidence, times_derived, embedding, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(id) DO UPDATE SET
                     content = excluded.content,
                     embedding = excluded.embedding,
                     times_derived = excluded.times_derived,
                     updated_at = excluded.updated_at",
            )?;

            for obs in observations {
                stmt.execute(params![
                    obs.id,
                    obs.session_id,
                    obs.project_id,
                    obs.level,
                    obs.content,
                    obs.source_ids,
                    obs.confidence,
                    obs.times_derived,
                    obs.embedding,
                    obs.created_at,
                    obs.updated_at,
                ])?;
            }
        }

        // Insert into FTS if available
        if self.check_observation_fts_available() {
            let mut fts_stmt = tx.prepare(
                "INSERT OR REPLACE INTO observations_fts (content, id, session_id, project_id, level)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for obs in observations {
                fts_stmt.execute(params![
                    obs.content,
                    obs.id,
                    obs.session_id,
                    obs.project_id,
                    obs.level,
                ])?;
            }
        }

        // Insert into vector table if available
        if self.vector_available {
            let obs_vec_available = tx
                .prepare("SELECT 1 FROM observation_vectors LIMIT 0")
                .is_ok();
            if obs_vec_available {
                let mut del_stmt =
                    tx.prepare("DELETE FROM observation_vectors WHERE observation_id = ?1")?;
                let mut ins_stmt = tx.prepare(
                    "INSERT INTO observation_vectors (observation_id, embedding) VALUES (?1, ?2)",
                )?;
                for obs in observations {
                    if !obs.embedding.is_empty() {
                        if let Ok(vec) = serde_json::from_str::<Vec<f32>>(&obs.embedding) {
                            let blob = embedding_to_blob(&vec);
                            del_stmt.execute(params![obs.id])?;
                            ins_stmt.execute(params![obs.id, blob])?;
                        }
                    }
                }
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Get observations for a specific session.
    pub fn get_observations_by_session(&self, session_id: &str) -> Result<Vec<ObservationRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, project_id, level, content, source_ids, confidence, times_derived, embedding, created_at, updated_at
             FROM observations
             WHERE session_id = ?1 AND deleted_at IS NULL
             ORDER BY created_at ASC",
        )?;

        let rows = stmt
            .query_map(params![session_id], Self::map_observation_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Get observations for a specific project.
    pub fn get_observations_by_project(&self, project_id: &str) -> Result<Vec<ObservationRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, project_id, level, content, source_ids, confidence, times_derived, embedding, created_at, updated_at
             FROM observations
             WHERE project_id = ?1 AND deleted_at IS NULL
             ORDER BY created_at ASC",
        )?;

        let rows = stmt
            .query_map(params![project_id], Self::map_observation_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// List observations with optional filters and limit.
    pub fn list_observations(
        &self,
        session_id: Option<&str>,
        project_id: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<ObservationRow>> {
        let mut sql = String::from(
            "SELECT id, session_id, project_id, level, content, source_ids, confidence, times_derived, embedding, created_at, updated_at
             FROM observations WHERE deleted_at IS NULL",
        );
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;

        if let Some(sid) = session_id {
            sql.push_str(&format!(" AND session_id = ?{param_idx}"));
            param_values.push(Box::new(sid.to_string()));
            param_idx += 1;
        }
        if let Some(pid) = project_id {
            sql.push_str(&format!(" AND project_id = ?{param_idx}"));
            param_values.push(Box::new(pid.to_string()));
            param_idx += 1;
        }
        sql.push_str(" ORDER BY created_at DESC");
        if let Some(lim) = limit {
            sql.push_str(&format!(" LIMIT ?{param_idx}"));
            param_values.push(Box::new(lim as i64));
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let params_ref: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|b| b.as_ref()).collect();

        let rows = stmt
            .query_map(params_ref.as_slice(), Self::map_observation_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Search observations by keyword (FTS).
    ///
    /// When `project_id` is `Some`, the SQL `WHERE` clause requires the
    /// matching row's `project_id`. When `None`, observations of every
    /// project (plus those with NULL project_id) are eligible.
    pub fn search_observations_by_keyword(
        &self,
        query: &str,
        max_results: usize,
        project_id: Option<&str>,
    ) -> Result<Vec<(String, f64)>> {
        if !self.check_observation_fts_available() {
            // Fallback to LIKE search if FTS not available
            let (sql, has_project) = if project_id.is_some() {
                (
                    "SELECT id, 1.0 as score FROM observations
                     WHERE content LIKE '%' || ?1 || '%' AND deleted_at IS NULL
                       AND project_id = ?2
                     LIMIT ?3",
                    true,
                )
            } else {
                (
                    "SELECT id, 1.0 as score FROM observations
                     WHERE content LIKE '%' || ?1 || '%' AND deleted_at IS NULL
                     LIMIT ?2",
                    false,
                )
            };
            let mut stmt = self.conn.prepare(sql)?;
            let map_row =
                |row: &rusqlite::Row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?));
            let rows: Vec<(String, f64)> = if has_project {
                stmt.query_map(
                    params![query, project_id.unwrap(), max_results as i64],
                    map_row,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
            } else {
                stmt.query_map(params![query, max_results as i64], map_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            };
            return Ok(rows);
        }

        // FTS path. observations_fts already mirrors `project_id`, so the
        // filter can apply directly without joining back to `observations`.
        let (sql, has_project) = if project_id.is_some() {
            (
                "SELECT id, rank FROM observations_fts
                 WHERE observations_fts MATCH ?1 AND project_id = ?2
                 ORDER BY rank
                 LIMIT ?3",
                true,
            )
        } else {
            (
                "SELECT id, rank FROM observations_fts
                 WHERE observations_fts MATCH ?1
                 ORDER BY rank
                 LIMIT ?2",
                false,
            )
        };
        let mut stmt = self.conn.prepare(sql)?;
        let map_row = |row: &rusqlite::Row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?));
        let rows: Vec<(String, f64)> = if has_project {
            stmt.query_map(
                params![query, project_id.unwrap(), max_results as i64],
                map_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![query, max_results as i64], map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };

        Ok(rows)
    }

    /// Search observations by vector similarity.
    pub fn search_observations_by_vector(
        &self,
        query_embedding: &[f32],
        max_results: usize,
    ) -> Result<Vec<(String, f64)>> {
        if !self.vector_available {
            return Ok(vec![]);
        }

        let obs_vec_available = self
            .conn
            .prepare("SELECT 1 FROM observation_vectors LIMIT 0")
            .is_ok();
        if !obs_vec_available {
            return Ok(vec![]);
        }

        let blob = embedding_to_blob(query_embedding);
        let mut stmt = self.conn.prepare(
            "SELECT observation_id, distance FROM observation_vectors
             WHERE embedding MATCH ?1
             ORDER BY distance
             LIMIT ?2",
        )?;

        let rows = stmt
            .query_map(params![blob, max_results as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    /// Count total non-deleted observations.
    pub fn count_observations(&self) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM observations WHERE deleted_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Soft-delete observations for a session.
    pub fn delete_observations_by_session(&self, session_id: &str) -> Result<u64> {
        let now = chrono::Utc::now().to_rfc3339();
        let deleted = self.conn.execute(
            "UPDATE observations SET deleted_at = ?1 WHERE session_id = ?2 AND deleted_at IS NULL",
            params![now, session_id],
        )?;
        Ok(deleted as u64)
    }

    /// Get a single observation by ID.
    pub fn get_observation(&self, id: &str) -> Result<Option<ObservationRow>> {
        let result = self.conn.query_row(
            "SELECT id, session_id, project_id, level, content, source_ids, confidence, times_derived, embedding, created_at, updated_at
             FROM observations WHERE id = ?1 AND deleted_at IS NULL",
            params![id],
            Self::map_observation_row,
        );
        match result {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn check_observation_fts_available(&self) -> bool {
        self.conn
            .prepare("SELECT 1 FROM observations_fts LIMIT 0")
            .is_ok()
    }

    fn map_observation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ObservationRow> {
        Ok(ObservationRow {
            id: row.get(0)?,
            session_id: row.get(1)?,
            project_id: row.get(2)?,
            level: row.get(3)?,
            content: row.get(4)?,
            source_ids: row.get(5)?,
            confidence: row.get(6)?,
            times_derived: row.get(7)?,
            embedding: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
        })
    }

    // -------------------------------------------------------------------------
    // Composite Operations
    // -------------------------------------------------------------------------

    pub fn index_file_chunks(
        &self,
        path: &str,
        source: MemorySource,
        model: &str,
        chunks: &[(MemoryChunk, String)],
        file_entry: &MemoryFileEntry,
    ) -> Result<()> {
        // Delete old data (order matters: vec first, then fts, then chunks)
        self.delete_vector_chunks_for_file(path, source)?;
        self.delete_fts_chunks_for_file(path, source, model)?;
        self.delete_chunks_for_file(path, source)?;

        // Project scope (denormalized from file_entry) is carried into both
        // `chunks` and `chunks_fts` so the project filter can apply directly
        // in SQL on the search path.
        let project_id_str = file_entry.project_id.as_ref().map(|u| u.to_string());

        // Insert new chunks
        self.insert_chunks(path, source, model, chunks, project_id_str.as_deref())?;

        // Build StoredChunk list for FTS insertion and parse embeddings for vector insertion
        let now = chrono::Utc::now().timestamp();
        let mut stored: Vec<StoredChunk> = Vec::with_capacity(chunks.len());
        let mut vec_entries: Vec<(String, Vec<f32>)> = Vec::new();

        for (chunk, emb_json) in chunks {
            let id = StoredChunk::make_id(path, source, chunk.start_line, &chunk.hash);
            if self.vector_available
                && !emb_json.is_empty()
                && let Ok(vec) = serde_json::from_str::<Vec<f32>>(emb_json)
            {
                vec_entries.push((id.clone(), vec));
            }
            stored.push(StoredChunk {
                id,
                path: path.to_string(),
                source,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                hash: chunk.hash.clone(),
                model: model.to_string(),
                text: chunk.text.clone(),
                embedding: emb_json.clone(),
                updated_at: now,
                project_id: project_id_str.clone(),
            });
        }

        // Insert into FTS
        self.insert_fts_chunks(&stored)?;

        // Insert into vector table
        if self.vector_available {
            self.insert_vector_chunks(&vec_entries)?;
        }

        // Upsert file entry
        self.upsert_file(file_entry)?;

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Private helpers
    // -------------------------------------------------------------------------

    fn map_chunk_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRow> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
            row.get(8)?,
            row.get(9)?,
            row.get(10)?,
        ))
    }

    fn convert_chunk_row(row: ChunkRow) -> Result<StoredChunk> {
        let (
            id,
            path,
            source_str,
            start_line,
            end_line,
            hash,
            model,
            text,
            embedding,
            updated_at,
            project_id,
        ) = row;
        let source = str_to_memory_source(&source_str)?;
        Ok(StoredChunk {
            id,
            path,
            source,
            start_line,
            end_line,
            hash,
            model,
            text,
            embedding,
            updated_at,
            project_id,
        })
    }
}

struct FileRow {
    path: String,
    source_str: String,
    hash: String,
    mtime: i64,
    size: i64,
    project_id: Option<String>,
    watermark: Option<String>,
}

impl FileRow {
    fn into_file_entry(self) -> Result<MemoryFileEntry> {
        let source = str_to_memory_source(&self.source_str)?;
        let project_id = self
            .project_id
            .as_deref()
            .and_then(|s| uuid::Uuid::parse_str(s).ok());
        Ok(MemoryFileEntry {
            path: self.path,
            abs_path: std::path::PathBuf::new(), // Not stored in DB; caller must resolve
            mtime_ms: self.mtime,
            size: self.size,
            hash: self.hash,
            source,
            project_id,
            watermark: self.watermark,
        })
    }
}

/// Parse the embedding width out of a vec0 table's DDL, e.g. the `1024` in
/// `embedding float[1024]`. Returns `None` when the declaration is absent or
/// malformed.
fn parse_vec0_dims(ddl: &str) -> Option<u32> {
    let start = ddl.find("float[")? + "float[".len();
    let rest = &ddl[start..];
    let end = rest.find(']')?;
    rest[..end].trim().parse().ok()
}

/// Convert a Vec<f32> to a byte blob for sqlite-vec storage.
fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(embedding.len() * 4);
    for &val in embedding {
        blob.extend_from_slice(&val.to_le_bytes());
    }
    blob
}

/// Convert a byte blob from sqlite-vec back to Vec<f32>.
#[allow(dead_code)]
fn blob_to_embedding(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::types::{MemoryIndexMeta, MemorySource};
    use std::path::PathBuf;

    fn make_file_entry(path: &str, hash: &str) -> MemoryFileEntry {
        MemoryFileEntry {
            path: path.to_string(),
            abs_path: PathBuf::from(format!("/fake/{}", path)),
            mtime_ms: 1_000_000,
            size: 512,
            hash: hash.to_string(),
            source: MemorySource::Memory,
            project_id: None,
            watermark: None,
        }
    }

    fn make_chunk(start: u32, end: u32, text: &str) -> (MemoryChunk, String) {
        let chunk = MemoryChunk {
            start_line: start,
            end_line: end,
            text: text.to_string(),
            hash: format!("hash_{}", start),
        };
        (chunk, String::new())
    }

    fn make_cache_entry(provider: &str, model: &str, hash: &str, emb: &str) -> EmbeddingCacheEntry {
        EmbeddingCacheEntry {
            provider: provider.to_string(),
            model: model.to_string(),
            provider_key: "key".to_string(),
            hash: hash.to_string(),
            embedding: emb.to_string(),
            dims: Some(384),
            updated_at: 1_000,
        }
    }

    // -------------------------------------------------------------------------
    // Vendored sqlite-vec registration
    // -------------------------------------------------------------------------

    #[test]
    fn test_vendored_sqlite_vec_loads() {
        let registered = register_sqlite_vec();
        eprintln!("register_sqlite_vec() returned: {}", registered);

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let result: std::result::Result<String, _> =
            conn.query_row("SELECT vec_version()", [], |row| row.get(0));
        eprintln!("vec_version() result: {:?}", result);

        assert!(
            registered,
            "vendored sqlite-vec should register successfully"
        );
        assert!(
            result.is_ok(),
            "vec_version() should work on new connections"
        );

        // Verify vec0 virtual table creation works
        conn.execute_batch(
            "CREATE VIRTUAL TABLE test_vec USING vec0(id TEXT PRIMARY KEY, v float[3])",
        )
        .unwrap();
        conn.execute_batch("DROP TABLE test_vec").unwrap();
    }

    // -------------------------------------------------------------------------
    // Store core tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_open_in_memory() {
        let store = MemoryStore::open_in_memory().unwrap();
        // meta table should exist
        store
            .conn
            .query_row("SELECT COUNT(*) FROM meta", [], |row| row.get::<_, i64>(0))
            .unwrap();
        // files table should exist
        store
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))
            .unwrap();
        // chunks table should exist
        store
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
    }

    #[test]
    fn test_open_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.sqlite");
        assert!(!db_path.exists());
        MemoryStore::open(&db_path).unwrap();
        assert!(db_path.exists());
    }

    #[test]
    fn test_schema_version() {
        let store = MemoryStore::open_in_memory().unwrap();
        let version: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        // V4 added the `files.watermark` change-detection column for session
        // transcript sync.
        assert_eq!(version, 4);
    }

    #[test]
    fn test_schema_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.sqlite");
        MemoryStore::open(&db_path).unwrap();
        // Open again — should not error
        let store2 = MemoryStore::open(&db_path).unwrap();
        let version: i32 = store2
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 4);
    }

    // -------------------------------------------------------------------------
    // Meta CRUD tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_get_meta_missing() {
        let store = MemoryStore::open_in_memory().unwrap();
        assert_eq!(store.get_meta("nonexistent").unwrap(), None);
    }

    #[test]
    fn test_set_and_get_meta() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.set_meta("model", "nomic-embed-text").unwrap();
        assert_eq!(
            store.get_meta("model").unwrap(),
            Some("nomic-embed-text".to_string())
        );
    }

    #[test]
    fn test_set_meta_overwrites() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.set_meta("k", "v1").unwrap();
        store.set_meta("k", "v2").unwrap();
        assert_eq!(store.get_meta("k").unwrap(), Some("v2".to_string()));
    }

    #[test]
    fn test_delete_meta() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.set_meta("k", "v").unwrap();
        store.delete_meta("k").unwrap();
        assert_eq!(store.get_meta("k").unwrap(), None);
    }

    #[test]
    fn test_load_index_meta_empty() {
        let store = MemoryStore::open_in_memory().unwrap();
        assert!(store.load_index_meta().unwrap().is_none());
    }

    #[test]
    fn test_save_and_load_index_meta() {
        let store = MemoryStore::open_in_memory().unwrap();
        let meta = MemoryIndexMeta {
            model: "nomic-embed-text".to_string(),
            provider: "ollama".to_string(),
            provider_key: Some("pk123".to_string()),
            sources: vec![MemorySource::Memory, MemorySource::Sessions],
            chunk_tokens: 400,
            chunk_overlap: 80,
            vector_dims: Some(384),
        };
        store.save_index_meta(&meta).unwrap();
        let loaded = store.load_index_meta().unwrap().unwrap();
        assert_eq!(loaded.model, meta.model);
        assert_eq!(loaded.provider, meta.provider);
        assert_eq!(loaded.provider_key, meta.provider_key);
        assert_eq!(loaded.sources, meta.sources);
        assert_eq!(loaded.chunk_tokens, meta.chunk_tokens);
        assert_eq!(loaded.chunk_overlap, meta.chunk_overlap);
        assert_eq!(loaded.vector_dims, meta.vector_dims);
    }

    #[test]
    fn test_load_index_meta_partial() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Only set "model", no "provider"
        store.set_meta("model", "test-model").unwrap();
        assert!(store.load_index_meta().unwrap().is_none());
    }

    // -------------------------------------------------------------------------
    // Files CRUD tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_upsert_file_new() {
        let store = MemoryStore::open_in_memory().unwrap();
        let entry = make_file_entry("MEMORY.md", "abc123");
        store.upsert_file(&entry).unwrap();
        let got = store.get_file("MEMORY.md").unwrap().unwrap();
        assert_eq!(got.path, "MEMORY.md");
        assert_eq!(got.hash, "abc123");
    }

    #[test]
    fn test_upsert_file_update() {
        let store = MemoryStore::open_in_memory().unwrap();
        let entry = make_file_entry("MEMORY.md", "abc123");
        store.upsert_file(&entry).unwrap();
        let updated = make_file_entry("MEMORY.md", "newHash");
        store.upsert_file(&updated).unwrap();
        let got = store.get_file("MEMORY.md").unwrap().unwrap();
        assert_eq!(got.hash, "newHash");
    }

    #[test]
    fn test_get_file_missing() {
        let store = MemoryStore::open_in_memory().unwrap();
        assert!(store.get_file("nonexistent.md").unwrap().is_none());
    }

    #[test]
    fn test_list_files_empty() {
        let store = MemoryStore::open_in_memory().unwrap();
        assert!(store.list_files().unwrap().is_empty());
    }

    #[test]
    fn test_list_files_ordered() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.upsert_file(&make_file_entry("c.md", "hc")).unwrap();
        store.upsert_file(&make_file_entry("a.md", "ha")).unwrap();
        store.upsert_file(&make_file_entry("b.md", "hb")).unwrap();
        let files = store.list_files().unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "a.md");
        assert_eq!(files[1].path, "b.md");
        assert_eq!(files[2].path, "c.md");
    }

    #[test]
    fn test_delete_file() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.upsert_file(&make_file_entry("x.md", "hx")).unwrap();
        store.delete_file("x.md").unwrap();
        assert!(store.get_file("x.md").unwrap().is_none());
    }

    #[test]
    fn test_file_count() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.upsert_file(&make_file_entry("a.md", "ha")).unwrap();
        store.upsert_file(&make_file_entry("b.md", "hb")).unwrap();
        store.upsert_file(&make_file_entry("c.md", "hc")).unwrap();
        assert_eq!(store.file_count().unwrap(), 3);
    }

    // -------------------------------------------------------------------------
    // Chunks CRUD tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_insert_chunks_empty() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .insert_chunks("test.md", MemorySource::Memory, "model", &[], None)
            .unwrap();
        assert_eq!(store.chunk_count().unwrap(), 0);
    }

    #[test]
    fn test_insert_and_get_chunks() {
        let store = MemoryStore::open_in_memory().unwrap();
        let chunks = vec![
            make_chunk(0, 5, "chunk a"),
            make_chunk(6, 10, "chunk b"),
            make_chunk(11, 15, "chunk c"),
        ];
        store
            .insert_chunks("test.md", MemorySource::Memory, "model", &chunks, None)
            .unwrap();
        let got = store
            .get_chunks_for_file("test.md", MemorySource::Memory)
            .unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].start_line, 0);
        assert_eq!(got[1].start_line, 6);
        assert_eq!(got[2].start_line, 11);
    }

    #[test]
    fn test_insert_chunks_upsert() {
        let store = MemoryStore::open_in_memory().unwrap();
        let chunk = MemoryChunk {
            start_line: 0,
            end_line: 5,
            text: "original".to_string(),
            hash: "h1".to_string(),
        };
        store
            .insert_chunks(
                "test.md",
                MemorySource::Memory,
                "model",
                &[(chunk.clone(), String::new())],
                None,
            )
            .unwrap();
        let updated = MemoryChunk {
            text: "updated".to_string(),
            ..chunk
        };
        store
            .insert_chunks(
                "test.md",
                MemorySource::Memory,
                "model",
                &[(updated, String::new())],
                None,
            )
            .unwrap();
        let got = store
            .get_chunks_for_file("test.md", MemorySource::Memory)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "updated");
    }

    #[test]
    fn test_delete_chunks_for_file() {
        let store = MemoryStore::open_in_memory().unwrap();
        let chunks = vec![make_chunk(0, 5, "a"), make_chunk(6, 10, "b")];
        store
            .insert_chunks("test.md", MemorySource::Memory, "model", &chunks, None)
            .unwrap();
        store
            .delete_chunks_for_file("test.md", MemorySource::Memory)
            .unwrap();
        assert!(
            store
                .get_chunks_for_file("test.md", MemorySource::Memory)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_delete_chunks_returns_count() {
        let store = MemoryStore::open_in_memory().unwrap();
        let chunks = vec![
            make_chunk(0, 5, "a"),
            make_chunk(6, 10, "b"),
            make_chunk(11, 15, "c"),
        ];
        store
            .insert_chunks("test.md", MemorySource::Memory, "model", &chunks, None)
            .unwrap();
        let deleted = store
            .delete_chunks_for_file("test.md", MemorySource::Memory)
            .unwrap();
        assert_eq!(deleted, 3);
    }

    #[test]
    fn test_get_all_chunks() {
        let store = MemoryStore::open_in_memory().unwrap();
        let chunks1 = vec![make_chunk(0, 5, "a")];
        let chunks2 = vec![make_chunk(0, 5, "b"), make_chunk(6, 10, "c")];
        store
            .insert_chunks("file1.md", MemorySource::Memory, "model", &chunks1, None)
            .unwrap();
        store
            .insert_chunks("file2.md", MemorySource::Memory, "model", &chunks2, None)
            .unwrap();
        let all = store.get_all_chunks().unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_chunk_count() {
        let store = MemoryStore::open_in_memory().unwrap();
        let chunks: Vec<_> = (0..5)
            .map(|i| make_chunk(i * 6, i * 6 + 5, "text"))
            .collect();
        store
            .insert_chunks("test.md", MemorySource::Memory, "model", &chunks, None)
            .unwrap();
        assert_eq!(store.chunk_count().unwrap(), 5);
    }

    // -------------------------------------------------------------------------
    // Embedding cache tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_load_cache_empty() {
        let store = MemoryStore::open_in_memory().unwrap();
        let result = store
            .load_embedding_cache("ollama", "model", "key", &[])
            .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_load_cache_no_matches() {
        let store = MemoryStore::open_in_memory().unwrap();
        let result = store
            .load_embedding_cache("ollama", "model", "key", &["h1", "h2"])
            .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_upsert_and_load_cache() {
        let store = MemoryStore::open_in_memory().unwrap();
        let entries = vec![
            make_cache_entry("ollama", "model", "h1", "[1.0,2.0]"),
            make_cache_entry("ollama", "model", "h2", "[3.0,4.0]"),
            make_cache_entry("ollama", "model", "h3", "[5.0,6.0]"),
        ];
        store.upsert_embedding_cache(&entries).unwrap();
        let result = store
            .load_embedding_cache("ollama", "model", "key", &["h1", "h3"])
            .unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains_key("h1"));
        assert!(result.contains_key("h3"));
        assert!(!result.contains_key("h2"));
    }

    #[test]
    fn test_upsert_cache_overwrites() {
        let store = MemoryStore::open_in_memory().unwrap();
        let e1 = make_cache_entry("ollama", "model", "h1", "[1.0]");
        let e2 = make_cache_entry("ollama", "model", "h1", "[9.0]");
        store.upsert_embedding_cache(&[e1]).unwrap();
        store.upsert_embedding_cache(&[e2]).unwrap();
        let result = store
            .load_embedding_cache("ollama", "model", "key", &["h1"])
            .unwrap();
        assert_eq!(result["h1"], "[9.0]");
    }

    #[test]
    fn test_prune_cache_under_limit() {
        let store = MemoryStore::open_in_memory().unwrap();
        let entries: Vec<_> = (0..5)
            .map(|i| make_cache_entry("ollama", "model", &format!("h{}", i), "[]"))
            .collect();
        store.upsert_embedding_cache(&entries).unwrap();
        let deleted = store.prune_embedding_cache(10).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(store.cache_entry_count().unwrap(), 5);
    }

    #[test]
    fn test_prune_cache_over_limit() {
        let store = MemoryStore::open_in_memory().unwrap();
        let entries: Vec<_> = (0..10)
            .map(|i| {
                let mut e = make_cache_entry("ollama", "model", &format!("h{}", i), "[]");
                e.updated_at = i as i64; // different timestamps for ordering
                e
            })
            .collect();
        store.upsert_embedding_cache(&entries).unwrap();
        let deleted = store.prune_embedding_cache(3).unwrap();
        assert_eq!(deleted, 7);
        assert_eq!(store.cache_entry_count().unwrap(), 3);
    }

    #[test]
    fn test_cache_entry_count() {
        let store = MemoryStore::open_in_memory().unwrap();
        let entries: Vec<_> = (0..4)
            .map(|i| make_cache_entry("ollama", "model", &format!("h{}", i), "[]"))
            .collect();
        store.upsert_embedding_cache(&entries).unwrap();
        assert_eq!(store.cache_entry_count().unwrap(), 4);
    }

    #[test]
    fn test_load_cache_batching() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Insert 600 entries
        let entries: Vec<_> = (0..600)
            .map(|i| make_cache_entry("ollama", "model", &format!("h{:04}", i), "[]"))
            .collect();
        store.upsert_embedding_cache(&entries).unwrap();

        let hashes: Vec<&str> = entries.iter().map(|e| e.hash.as_str()).collect();
        let result = store
            .load_embedding_cache("ollama", "model", "key", &hashes)
            .unwrap();
        assert_eq!(result.len(), 600);
    }

    // -------------------------------------------------------------------------
    // FTS5 tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_fts_available_after_open() {
        let store = MemoryStore::open_in_memory().unwrap();
        // SQLite bundled with rusqlite has FTS5 compiled in
        assert!(store.fts_available());
    }

    #[test]
    fn test_insert_fts_chunks() {
        let store = MemoryStore::open_in_memory().unwrap();
        if !store.fts_available() {
            return; // Skip if FTS5 not available
        }
        let chunks = vec![StoredChunk {
            id: "test:memory:0:h1".to_string(),
            path: "test.md".to_string(),
            source: MemorySource::Memory,
            start_line: 0,
            end_line: 5,
            hash: "h1".to_string(),
            model: "model".to_string(),
            text: "hello world".to_string(),
            embedding: String::new(),
            updated_at: 1_000,
            project_id: None,
        }];
        store.insert_fts_chunks(&chunks).unwrap();
    }

    #[test]
    fn test_search_fts_no_results() {
        let store = MemoryStore::open_in_memory().unwrap();
        if !store.fts_available() {
            return;
        }
        let results = store.search_fts("nonexistentterm12345", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_fts_basic() {
        let store = MemoryStore::open_in_memory().unwrap();
        if !store.fts_available() {
            return;
        }
        let chunks = vec![
            StoredChunk {
                id: "t:memory:0:h1".to_string(),
                path: "t.md".to_string(),
                source: MemorySource::Memory,
                start_line: 0,
                end_line: 5,
                hash: "h1".to_string(),
                model: "model".to_string(),
                text: "the quick brown fox".to_string(),
                embedding: String::new(),
                updated_at: 1_000,
                project_id: None,
            },
            StoredChunk {
                id: "t:memory:6:h2".to_string(),
                path: "t.md".to_string(),
                source: MemorySource::Memory,
                start_line: 6,
                end_line: 10,
                hash: "h2".to_string(),
                model: "model".to_string(),
                text: "unrelated text".to_string(),
                embedding: String::new(),
                updated_at: 1_000,
                project_id: None,
            },
        ];
        store.insert_fts_chunks(&chunks).unwrap();
        let results = store.search_fts("quick", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "t:memory:0:h1");
    }

    #[test]
    fn test_search_fts_limit() {
        let store = MemoryStore::open_in_memory().unwrap();
        if !store.fts_available() {
            return;
        }
        let chunks: Vec<StoredChunk> = (0..10)
            .map(|i| StoredChunk {
                id: format!("t:memory:{}:h{}", i, i),
                path: "t.md".to_string(),
                source: MemorySource::Memory,
                start_line: i as u32,
                end_line: i as u32 + 1,
                hash: format!("h{}", i),
                model: "model".to_string(),
                text: format!("keyword content item {}", i),
                embedding: String::new(),
                updated_at: 1_000,
                project_id: None,
            })
            .collect();
        store.insert_fts_chunks(&chunks).unwrap();
        let results = store.search_fts("keyword", 3).unwrap();
        assert!(results.len() <= 3);
    }

    #[test]
    fn test_delete_fts_chunks() {
        let store = MemoryStore::open_in_memory().unwrap();
        if !store.fts_available() {
            return;
        }
        let chunks = vec![StoredChunk {
            id: "t:memory:0:h1".to_string(),
            path: "t.md".to_string(),
            source: MemorySource::Memory,
            start_line: 0,
            end_line: 5,
            hash: "h1".to_string(),
            model: "model".to_string(),
            text: "uniqueterm12345".to_string(),
            embedding: String::new(),
            updated_at: 1_000,
            project_id: None,
        }];
        store.insert_fts_chunks(&chunks).unwrap();
        store
            .delete_fts_chunks_for_file("t.md", MemorySource::Memory, "model")
            .unwrap();
        let results = store.search_fts("uniqueterm12345", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_fts_noop_when_unavailable() {
        let mut store = MemoryStore::open_in_memory().unwrap();
        store.fts_available = false;
        let chunks = vec![StoredChunk {
            id: "t:memory:0:h1".to_string(),
            path: "t.md".to_string(),
            source: MemorySource::Memory,
            start_line: 0,
            end_line: 5,
            hash: "h1".to_string(),
            model: "model".to_string(),
            text: "test".to_string(),
            embedding: String::new(),
            updated_at: 1_000,
            project_id: None,
        }];
        // Should not error
        store.insert_fts_chunks(&chunks).unwrap();
        let results = store.search_fts("test", 10).unwrap();
        assert!(results.is_empty());
    }

    // -------------------------------------------------------------------------
    // Vector table tests (require sqlite-vec — ignored by default)
    // -------------------------------------------------------------------------

    #[test]
    fn test_ensure_vector_table() {
        let mut store = MemoryStore::open_in_memory().unwrap();
        store.ensure_vector_table(384).unwrap();
        assert!(store.vector_available());
    }

    #[test]
    fn test_ensure_vector_table_idempotent() {
        let mut store = MemoryStore::open_in_memory().unwrap();
        store.ensure_vector_table(384).unwrap();
        store.ensure_vector_table(384).unwrap();
        assert!(store.vector_available());
    }

    #[test]
    fn test_parse_vec0_dims() {
        assert_eq!(
            parse_vec0_dims(
                "CREATE VIRTUAL TABLE chunks_vec USING vec0( id TEXT PRIMARY KEY, embedding float[1024] )"
            ),
            Some(1024)
        );
        assert_eq!(parse_vec0_dims("embedding float[768]"), Some(768));
        assert_eq!(parse_vec0_dims("no vector column here"), None);
        assert_eq!(parse_vec0_dims("embedding float[bogus]"), None);
        assert_eq!(parse_vec0_dims("embedding float[12"), None);
    }

    /// Swapping embedding models changes the vector width. The vec0 column
    /// validates length on insert, so a table left at the old width would
    /// reject every write and silently degrade search to FTS-only.
    #[test]
    fn test_ensure_vector_table_recreates_on_dim_change() {
        let mut store = MemoryStore::open_in_memory().unwrap();
        if !store.vec_extension_loaded() {
            return; // sqlite-vec unavailable in this build
        }

        // nomic-embed-text era: 768-wide, populated.
        store.ensure_vector_table(768).unwrap();
        assert_eq!(store.existing_vector_dims("chunks_vec"), Some(768));
        store
            .insert_vector_chunks(&[("old".to_string(), vec![0.5f32; 768])])
            .unwrap();

        // Swap to qwen3-embedding:0.6b (1024-wide).
        store.ensure_vector_table(1024).unwrap();
        assert!(store.vector_available());
        assert_eq!(store.existing_vector_dims("chunks_vec"), Some(1024));
        assert_eq!(
            store.existing_vector_dims("observation_vectors"),
            Some(1024)
        );

        // The new width must actually accept writes and be searchable.
        store
            .insert_vector_chunks(&[("new".to_string(), vec![0.25f32; 1024])])
            .unwrap();
        let results = store.search_vector(&vec![0.25f32; 1024], 1).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "new");
    }

    #[test]
    fn test_insert_vector_chunks() {
        let mut store = MemoryStore::open_in_memory().unwrap();
        store.ensure_vector_table(3).unwrap();
        let chunks = vec![("id1".to_string(), vec![1.0f32, 2.0, 3.0])];
        store.insert_vector_chunks(&chunks).unwrap();
    }

    #[test]
    fn test_search_vector_basic() {
        let mut store = MemoryStore::open_in_memory().unwrap();
        store.ensure_vector_table(3).unwrap();
        let chunks = vec![
            ("id1".to_string(), vec![1.0f32, 0.0, 0.0]),
            ("id2".to_string(), vec![0.0f32, 1.0, 0.0]),
        ];
        store.insert_vector_chunks(&chunks).unwrap();
        let results = store.search_vector(&[1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "id1");
    }

    #[test]
    fn test_delete_vector_chunks() {
        let mut store = MemoryStore::open_in_memory().unwrap();
        store.ensure_vector_table(3).unwrap();
        let chunk = MemoryChunk {
            start_line: 0,
            end_line: 5,
            text: "t".to_string(),
            hash: "h1".to_string(),
        };
        store
            .insert_chunks(
                "t.md",
                MemorySource::Memory,
                "model",
                &[(chunk, String::new())],
                None,
            )
            .unwrap();
        let emb_chunks = vec![("t.md:memory:0:h1".to_string(), vec![1.0f32, 0.0, 0.0])];
        store.insert_vector_chunks(&emb_chunks).unwrap();
        store
            .delete_vector_chunks_for_file("t.md", MemorySource::Memory)
            .unwrap();
        let results = store.search_vector(&[1.0, 0.0, 0.0], 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_vector_noop_when_unavailable() {
        let store = MemoryStore::open_in_memory().unwrap();
        // vector_available is false by default
        let chunks = vec![("id1".to_string(), vec![1.0f32, 2.0])];
        store.insert_vector_chunks(&chunks).unwrap(); // no error
        let results = store.search_vector(&[1.0, 2.0], 10).unwrap();
        assert!(results.is_empty());
        store
            .delete_vector_chunks_for_file("t.md", MemorySource::Memory)
            .unwrap(); // no error
    }

    // -------------------------------------------------------------------------
    // Composite operation tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_index_file_chunks_fresh() {
        let store = MemoryStore::open_in_memory().unwrap();
        let file_entry = make_file_entry("doc.md", "filehash");
        let chunks = vec![
            make_chunk(0, 5, "hello world"),
            make_chunk(6, 10, "foo bar"),
        ];
        store
            .index_file_chunks(
                "doc.md",
                MemorySource::Memory,
                "model",
                &chunks,
                &file_entry,
            )
            .unwrap();
        assert_eq!(store.chunk_count().unwrap(), 2);
        assert!(store.get_file("doc.md").unwrap().is_some());
    }

    #[test]
    fn test_index_file_chunks_reindex() {
        let store = MemoryStore::open_in_memory().unwrap();
        let file1 = make_file_entry("doc.md", "hash1");
        let chunks1 = vec![make_chunk(0, 5, "old content")];
        store
            .index_file_chunks("doc.md", MemorySource::Memory, "model", &chunks1, &file1)
            .unwrap();

        let file2 = make_file_entry("doc.md", "hash2");
        let chunks2 = vec![
            make_chunk(0, 3, "new content a"),
            make_chunk(4, 7, "new content b"),
        ];
        store
            .index_file_chunks("doc.md", MemorySource::Memory, "model", &chunks2, &file2)
            .unwrap();

        let got = store
            .get_chunks_for_file("doc.md", MemorySource::Memory)
            .unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|c| c.text == "new content a"));
        assert!(got.iter().any(|c| c.text == "new content b"));
        assert!(!got.iter().any(|c| c.text == "old content"));

        let file = store.get_file("doc.md").unwrap().unwrap();
        assert_eq!(file.hash, "hash2");
    }

    #[test]
    fn test_index_file_chunks_no_embeddings() {
        let store = MemoryStore::open_in_memory().unwrap();
        let file_entry = make_file_entry("doc.md", "fh");
        let chunks = vec![make_chunk(0, 5, "no embedding chunk")];
        store
            .index_file_chunks(
                "doc.md",
                MemorySource::Memory,
                "model",
                &chunks,
                &file_entry,
            )
            .unwrap();
        assert_eq!(store.chunk_count().unwrap(), 1);
        // vector_available is false, so no vector table operations happened
        assert!(!store.vector_available());
    }

    // -------------------------------------------------------------------------
    // Embedding blob conversion tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_embedding_to_blob_roundtrip() {
        let embedding = vec![1.0f32, 2.5, -3.5, 0.0];
        let blob = embedding_to_blob(&embedding);
        let recovered = blob_to_embedding(&blob);
        assert_eq!(embedding.len(), recovered.len());
        for (a, b) in embedding.iter().zip(recovered.iter()) {
            assert!((a - b).abs() < 1e-7);
        }
    }

    #[test]
    fn test_embedding_to_blob_empty() {
        let blob = embedding_to_blob(&[]);
        assert!(blob.is_empty());
        let recovered = blob_to_embedding(&blob);
        assert!(recovered.is_empty());
    }
}
