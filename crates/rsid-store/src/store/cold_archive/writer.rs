//! The archive file: an append-only `SQLite` database of per-session zstd chunks
//! (plan §3.1) and the writer/reader for it.
//!
//! One file per UTC month of the session's creation: `rsi-cold-YYYY-MM.sqlite`.
//! A chunk is never updated or deleted (triggers refuse both); a re-archive of a
//! session writes a new `generation`. Pragmas are set at creation:
//! `page_size=65536`, `journal_mode=DELETE` (one file at rest),
//! `synchronous=FULL` (a COMMIT fsyncs the database and its journal),
//! `application_id=0x52534943` and `user_version=1` (the format version). After
//! creating a new file the writer fsyncs the parent directory on Unix; Windows
//! has no directory fsync, which is a documented no-op.

use super::format::{self, EncodedRows};
use super::{ColdError, ColdResult, failpoint};
use chrono::{DateTime, Datelike, Utc};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `PRAGMA application_id` of every archive file (`RSIC`).
pub const ARCHIVE_APPLICATION_ID: i64 = 0x5253_4943;
/// `PRAGMA user_version` and `archive_meta.format_version` of this format.
pub const ARCHIVE_FORMAT_VERSION: i64 = 1;

const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// The two tables a chunk can hold.
pub const EVENTS_TABLE_NAME: &str = "conversation_events";
pub const PROVENANCE_TABLE_NAME: &str = "conversation_event_provenance";

const ARCHIVE_SCHEMA: &str = "
CREATE TABLE archive_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE chunks(
  chunk_id INTEGER PRIMARY KEY,
  session_id TEXT NOT NULL,
  generation INTEGER NOT NULL CHECK(generation >= 1),
  table_name TEXT NOT NULL CHECK(table_name IN ('conversation_events','conversation_event_provenance')),
  source_schema_version INTEGER NOT NULL,
  columns_json TEXT NOT NULL,
  row_count INTEGER NOT NULL CHECK(row_count >= 0),
  min_id INTEGER, max_id INTEGER, min_sequence INTEGER, max_sequence INTEGER,
  content_len INTEGER NOT NULL,
  raw_bytes INTEGER NOT NULL,
  raw_sha256 TEXT NOT NULL CHECK(length(raw_sha256) = 64),
  codec TEXT NOT NULL CHECK(codec = 'zstd'),
  codec_level INTEGER NOT NULL,
  payload BLOB NOT NULL,
  written_at TEXT NOT NULL,
  UNIQUE(session_id, generation, table_name));
CREATE TABLE session_snapshots(
  session_id TEXT NOT NULL,
  generation INTEGER NOT NULL,
  session_json TEXT NOT NULL,
  PRIMARY KEY(session_id, generation)) WITHOUT ROWID;
CREATE TRIGGER chunks_append_only_update BEFORE UPDATE ON chunks
  BEGIN SELECT RAISE(ABORT, 'cold archive is append-only'); END;
CREATE TRIGGER chunks_append_only_delete BEFORE DELETE ON chunks
  BEGIN SELECT RAISE(ABORT, 'cold archive is append-only'); END;
CREATE TRIGGER session_snapshots_append_only_update BEFORE UPDATE ON session_snapshots
  BEGIN SELECT RAISE(ABORT, 'cold archive is append-only'); END;
CREATE TRIGGER session_snapshots_append_only_delete BEFORE DELETE ON session_snapshots
  BEGIN SELECT RAISE(ABORT, 'cold archive is append-only'); END;
";

/// `rsi-cold-YYYY-MM.sqlite`.
#[must_use]
pub fn archive_file_name(month: &str) -> String {
    format!("rsi-cold-{month}.sqlite")
}

/// The archive month (`YYYY-MM`, UTC) of a session created at `created_at`.
#[must_use]
pub fn archive_month(created_at: DateTime<Utc>) -> String {
    format!("{:04}-{:02}", created_at.year(), created_at.month())
}

fn valid_month(month: &str) -> bool {
    let bytes = month.as_bytes();
    bytes.len() == 7
        && bytes[4] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..].iter().all(u8::is_ascii_digit)
}

/// Fsyncs a directory so a newly created file survives a crash. Windows has no
/// directory fsync; there it is a no-op.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// One chunk to write: a table's rows for one session.
pub struct ChunkInput<'a> {
    pub table: &'static str,
    pub columns_json: &'a str,
    pub encoded: &'a EncodedRows,
    pub sha256: &'a str,
    pub source_schema_version: i64,
}

/// What a write left in the archive for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenChunks {
    pub generation: i64,
    pub events_chunk_id: i64,
    pub provenance_chunk_id: Option<i64>,
    /// Compressed bytes of the session's chunks (new or reused).
    pub compressed_bytes: i64,
    /// True when the chunks were already in the archive with the same digest.
    pub reused: bool,
}

/// A chunk as it sits in an archive file. (`chunk_id` and `source_schema_version`
/// are read by audits and by the restore path of a later slice.)
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ChunkRecord {
    pub chunk_id: i64,
    pub session_id: String,
    pub generation: i64,
    pub table_name: String,
    pub source_schema_version: i64,
    pub columns_json: String,
    pub row_count: i64,
    pub min_id: Option<i64>,
    pub max_id: Option<i64>,
    pub min_sequence: Option<i64>,
    pub max_sequence: Option<i64>,
    pub content_len: i64,
    pub raw_bytes: i64,
    pub raw_sha256: String,
    pub codec: String,
    pub payload: Vec<u8>,
}

/// An archive file open for writing.
pub struct ArchiveFile {
    path: PathBuf,
    conn: Connection,
}

impl ArchiveFile {
    /// Opens (creating when absent) the archive file of `month` under `dir`.
    /// Returns the file and whether this call created it.
    pub fn open_or_create(
        dir: &Path,
        month: &str,
        source_schema_version: i64,
    ) -> ColdResult<(Self, bool)> {
        if !valid_month(month) {
            return Err(ColdError::new(
                "cold_archive_bad_month",
                format!("{month:?} is not a YYYY-MM archive month"),
            ));
        }
        rsid_core::private_state::create_private_dir_all(dir).map_err(|error| {
            ColdError::new(
                "cold_archive_dir_unusable",
                format!("cannot create {}: {error}", dir.display()),
            )
        })?;
        let path = dir.join(archive_file_name(month));
        // An archive file is never a symlink: opening one would write wherever
        // the link points, outside the directory the pass just checked (#2435).
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(ColdError::new(
                "cold_archive_dir_forbidden",
                format!("{} is a symlink", path.display()),
            ));
        }
        // Creates the file 0600 when absent; an existing file keeps its content.
        rsid_core::private_state::create_private_file(&path).map_err(|error| {
            ColdError::new(
                "cold_archive_dir_unusable",
                format!("cannot create {}: {error}", path.display()),
            )
        })?;
        let conn = Connection::open(&path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let initialized: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'archive_meta')",
            [],
            |row| row.get(0),
        )?;
        let mut created = false;
        if !initialized {
            let foreign: bool =
                conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master)", [], |row| {
                    row.get(0)
                })?;
            if foreign {
                return Err(ColdError::new(
                    "cold_archive_bad_format",
                    format!("{} is not a cold-archive file", path.display()),
                ));
            }
            Self::initialize(&conn, month, source_schema_version)?;
            created = true;
        } else {
            Self::check_format(&conn, &path)?;
        }
        // Per-connection durability settings; the journal mode is persistent but
        // is stated again so a hand-edited file cannot weaken it silently.
        conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        if created {
            sync_dir(dir)?;
        }
        Ok((Self { path, conn }, created))
    }

    fn initialize(conn: &Connection, month: &str, source_schema_version: i64) -> ColdResult<()> {
        // The literals below are page size 65536, `ARCHIVE_APPLICATION_ID` and
        // `ARCHIVE_FORMAT_VERSION` (a test pins them): every SQL string at a sink
        // must be statically known to the protected-DML source audit.
        conn.execute_batch(
            "PRAGMA page_size=65536;
             PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=FULL;
             PRAGMA application_id=1381189955;
             PRAGMA user_version=1;",
        )?;
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        tx.execute_batch(ARCHIVE_SCHEMA)?;
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        for (key, value) in [
            ("format_version", ARCHIVE_FORMAT_VERSION.to_string()),
            ("month", month.to_string()),
            ("created_at", now),
            ("writer_build", env!("CARGO_PKG_VERSION").to_string()),
            ("codec", "zstd".to_string()),
            ("row_codec", format::ROW_CODEC.to_string()),
            (
                "source_schema_version_at_creation",
                source_schema_version.to_string(),
            ),
        ] {
            tx.execute(
                "INSERT INTO archive_meta(key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn check_format(conn: &Connection, path: &Path) -> ColdResult<()> {
        let application_id: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
        let format_version: Option<String> = conn
            .query_row(
                "SELECT value FROM archive_meta WHERE key = 'format_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let expected_version = ARCHIVE_FORMAT_VERSION.to_string();
        if application_id != ARCHIVE_APPLICATION_ID
            || format_version.as_deref() != Some(expected_version.as_str())
        {
            return Err(ColdError::new(
                "cold_archive_bad_format",
                format!(
                    "{} has application_id {application_id} and format {format_version:?}; \
                     this build reads format {ARCHIVE_FORMAT_VERSION}",
                    path.display()
                ),
            ));
        }
        Ok(())
    }

    /// Number of chunks in the file and its size on disk.
    pub fn stats(&self) -> ColdResult<(i64, i64)> {
        let chunks: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
        let bytes = std::fs::metadata(&self.path)?.len();
        Ok((chunks, i64::try_from(bytes).unwrap_or(i64::MAX)))
    }

    fn find_chunk(
        tx: &Transaction<'_>,
        session_id: &str,
        generation: i64,
        table: &str,
    ) -> ColdResult<Option<(i64, String, i64)>> {
        Ok(tx
            .query_row(
                "SELECT chunk_id, raw_sha256, LENGTH(payload) FROM chunks
                 WHERE session_id = ?1 AND generation = ?2 AND table_name = ?3",
                params![session_id, generation, table],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?)
    }

    fn insert_chunk(
        tx: &Transaction<'_>,
        session_id: &str,
        generation: i64,
        input: &ChunkInput<'_>,
        written_at: &str,
    ) -> ColdResult<(i64, i64)> {
        let payload = format::compress(&input.encoded.raw)?;
        let compressed = i64::try_from(payload.len()).unwrap_or(i64::MAX);
        let raw_bytes = i64::try_from(input.encoded.raw.len()).unwrap_or(i64::MAX);
        tx.execute(
            "INSERT INTO chunks(session_id, generation, table_name, source_schema_version,
                columns_json, row_count, min_id, max_id, min_sequence, max_sequence,
                content_len, raw_bytes, raw_sha256, codec, codec_level, payload, written_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'zstd', ?14, ?15, ?16)",
            params![
                session_id,
                generation,
                input.table,
                input.source_schema_version,
                input.columns_json,
                input.encoded.row_count,
                input.encoded.min_id,
                input.encoded.max_id,
                input.encoded.min_sequence,
                input.encoded.max_sequence,
                input.encoded.content_len,
                raw_bytes,
                input.sha256,
                format::ZSTD_LEVEL,
                payload,
                written_at,
            ],
        )?;
        Ok((tx.last_insert_rowid(), compressed))
    }

    /// Writes one session's chunks (events, optional provenance) and its
    /// snapshot in a single transaction (P4). The COMMIT fsyncs the file.
    ///
    /// When generation `start_generation` already holds a chunk for the session:
    /// the same digest means an earlier pass wrote it (a crash before the index
    /// recorded it), so it is reused; a different digest means the session
    /// changed, and the chunks go to the next generation.
    pub fn write_session(
        &mut self,
        session_id: &str,
        start_generation: i64,
        events: &ChunkInput<'_>,
        provenance: Option<&ChunkInput<'_>>,
        snapshot_json: &str,
    ) -> ColdResult<WrittenChunks> {
        let written_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let mut generation = start_generation;
        loop {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let existing_events = Self::find_chunk(&tx, session_id, generation, EVENTS_TABLE_NAME)?;
            let existing_provenance =
                Self::find_chunk(&tx, session_id, generation, PROVENANCE_TABLE_NAME)?;
            match existing_events {
                None if existing_provenance.is_none() => {
                    let (events_chunk_id, events_bytes) =
                        Self::insert_chunk(&tx, session_id, generation, events, &written_at)?;
                    let mut provenance_chunk_id = None;
                    let mut provenance_bytes = 0;
                    if let Some(provenance) = provenance {
                        let (id, bytes) = Self::insert_chunk(
                            &tx,
                            session_id,
                            generation,
                            provenance,
                            &written_at,
                        )?;
                        provenance_chunk_id = Some(id);
                        provenance_bytes = bytes;
                    }
                    tx.execute(
                        "INSERT INTO session_snapshots(session_id, generation, session_json)
                         VALUES (?1, ?2, ?3)",
                        params![session_id, generation, snapshot_json],
                    )?;
                    // C2: a crash here leaves a journal; the next open rolls the
                    // partial transaction back and the chunk is written again.
                    failpoint::hit("write_before_commit")?;
                    tx.commit()?;
                    return Ok(WrittenChunks {
                        generation,
                        events_chunk_id,
                        provenance_chunk_id,
                        compressed_bytes: events_bytes.saturating_add(provenance_bytes),
                        reused: false,
                    });
                }
                Some((events_chunk_id, sha, events_bytes)) if sha == events.sha256 => {
                    let provenance_matches = match (provenance, &existing_provenance) {
                        (None, None) => Some((None, 0)),
                        (Some(input), Some((id, sha, bytes))) if sha == input.sha256 => {
                            Some((Some(*id), *bytes))
                        }
                        _ => None,
                    };
                    if let Some((provenance_chunk_id, provenance_bytes)) = provenance_matches {
                        return Ok(WrittenChunks {
                            generation,
                            events_chunk_id,
                            provenance_chunk_id,
                            compressed_bytes: events_bytes.saturating_add(provenance_bytes),
                            reused: true,
                        });
                    }
                }
                _ => {}
            }
            // The generation holds other data for this session: write the next.
            drop(tx);
            generation += 1;
        }
    }
}

/// Opens an archive file read-only (`mode=ro`) for verification and reads. A
/// crashed writer can leave a hot journal that a read-only connection cannot
/// roll back, so a leftover journal is recovered through a writable open first.
pub fn open_archive_readonly(path: &Path) -> ColdResult<Connection> {
    let journal = PathBuf::from(format!("{}-journal", path.display()));
    if journal.exists() {
        let writable = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        writable.busy_timeout(BUSY_TIMEOUT)?;
        writable.query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        })?;
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

/// Reads one chunk by id.
pub fn read_chunk(conn: &Connection, chunk_id: i64) -> ColdResult<Option<ChunkRecord>> {
    Ok(conn
        .query_row(
            "SELECT chunk_id, session_id, generation, table_name, source_schema_version,
                    columns_json, row_count, min_id, max_id, min_sequence, max_sequence,
                    content_len, raw_bytes, raw_sha256, codec, payload
             FROM chunks WHERE chunk_id = ?1",
            [chunk_id],
            |row| {
                Ok(ChunkRecord {
                    chunk_id: row.get(0)?,
                    session_id: row.get(1)?,
                    generation: row.get(2)?,
                    table_name: row.get(3)?,
                    source_schema_version: row.get(4)?,
                    columns_json: row.get(5)?,
                    row_count: row.get(6)?,
                    min_id: row.get(7)?,
                    max_id: row.get(8)?,
                    min_sequence: row.get(9)?,
                    max_sequence: row.get(10)?,
                    content_len: row.get(11)?,
                    raw_bytes: row.get(12)?,
                    raw_sha256: row.get(13)?,
                    codec: row.get(14)?,
                    payload: row.get(15)?,
                })
            },
        )
        .optional()?)
}

/// Every chunk id in the file with its compressed size, for orphan accounting.
pub fn list_chunk_ids(conn: &Connection) -> ColdResult<Vec<(i64, i64)>> {
    let mut stmt =
        conn.prepare("SELECT chunk_id, LENGTH(payload) FROM chunks ORDER BY chunk_id")?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}
