//! A cold-archive pass: P1 select -> P3 build -> P2 stage -> P4 write -> P5
//! verify, session by session (plan §3.3), and the verify-only pass.
//!
//! The store mutex is never held across the heavy steps. A pass runs on its own
//! thread with a separate read-only connection to the live database
//! (`open_read_connection`), and takes the store (`ColdStoreAccess`) only for
//! the short index writes. A stage pass stops after verification and deletes
//! nothing; a move pass ([`run_move_pass`], slice S4) then hands each session
//! whose chunks it verified in this same pass to P6 ([`super::mover`]), in
//! batches, after the fresh-backup gate ([`super::backup`]).

use super::backup::{BACKUP_STALE_CODE, FreshBackup, admit_backup};
use super::candidates::{
    AGE_UNPROVEN_CODE, CANDIDATE_PAGE, Candidate, EVENT_COLUMNS, EVENT_ROLES, EVENTS_TABLE,
    HoldContext, MOVABLE_COUNT_SQL, MOVABLE_EVENTS_SQL, MOVABLE_PROVENANCE_SQL, PROVENANCE_COLUMNS,
    PROVENANCE_ROLES, PROVENANCE_TABLE, RECENTLY_RESTORED_CODE, TOTAL_EVENTS_SQL,
    TOTAL_PROVENANCE_SQL, candidate_page, check_hold_classification, ensure_expected_columns,
    prove_age, recently_restored, session_hold,
};
use super::format::{
    ColumnRoles, RowEncoder, columns_json, decode_rows, decompress_checked, parse_columns_json,
    summarize_decoded, table_columns,
};
use super::index::{ColdSessionRow, RunSummary, StageIntent};
use super::mover::{GUARD_BUSY_CODE, MoveContext, MoveVerdict, NOT_ADMITTED_CODE, VerifiedChunks};
use super::writer::{
    ARCHIVE_FORMAT_VERSION, ArchiveFile, ChunkInput, archive_file_name, archive_month,
    list_chunk_ids, open_archive_readonly, read_chunk,
};
use super::{ColdError, ColdResult, ColdSettings, check_archive_dir, failpoint};
use crate::store::Store;
use crate::store::custody_lock_order::BlockingStoreLockExt;
use crate::store_support::spawn_single_flight::{SpawnGuard, try_acquire_spawn_guard};
use chrono::{DateTime, Utc};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

/// Errors a pass returns instead of recording when a test killed it on purpose.
const INJECTED_FAILURE: &str = "cold_archive_injected_failure";
/// Consecutive session failures after which a pass gives up (the archive or the
/// live database is broken, not one session).
const MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// Failures listed in a report.
const MAX_LISTED_ERRORS: usize = 20;

/// Short access to the store between the heavy steps of a pass.
pub trait ColdStoreAccess {
    #[track_caller]
    fn with_store<R>(&self, f: impl FnOnce(&Store) -> ColdResult<R>) -> ColdResult<R>;
}

/// Access to a store the caller already holds.
pub struct DirectStoreAccess<'a>(pub &'a Store);

impl ColdStoreAccess for DirectStoreAccess<'_> {
    #[track_caller]
    fn with_store<R>(&self, f: impl FnOnce(&Store) -> ColdResult<R>) -> ColdResult<R> {
        f(self.0)
    }
}

/// The daemon's store behind its mutex. For a pass thread that holds no
/// custody root lock or repository mutex.
impl ColdStoreAccess for Arc<rsid_core::store_lock::Mutex<Store>> {
    #[track_caller]
    fn with_store<R>(&self, f: impl FnOnce(&Store) -> ColdResult<R>) -> ColdResult<R> {
        let guard = self
            .blocking_lock_checked()
            .map_err(|error| ColdError::new("cold_archive_store_busy", error.to_string()))?;
        f(&guard)
    }
}

/// Opens the read connection a pass reads the live database through.
pub fn open_read_connection(database_path: &Path) -> ColdResult<Connection> {
    let conn = Connection::open_with_flags(
        database_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(10))?;
    Store::register_sql_functions(&conn)
        .map_err(|error| ColdError::new("cold_archive_sqlite", error.to_string()))?;
    #[cfg(any(test, feature = "test-seam"))]
    super::live_connections::track(&conn, database_path)?;
    Ok(conn)
}

/// Lowers the calling thread to the lowest CPU and idle I/O priority, so a pass
/// never competes with the operator's desktop or the daemon's own work. Linux
/// only; elsewhere a pass relies on its batch sleeps. Call it from the pass's
/// own thread, never from a pooled worker.
pub fn lower_current_thread_priority() {
    #[cfg(target_os = "linux")]
    {
        use nix::libc;
        // SAFETY: gettid, setpriority and ioprio_set take plain integers and
        // touch only the calling thread's scheduling state.
        unsafe {
            let tid = libc::syscall(libc::SYS_gettid);
            if tid > 0 {
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let who = tid as libc::id_t;
                libc::setpriority(libc::PRIO_PROCESS, who, 19);
                // IOPRIO_WHO_PROCESS = 1; IOPRIO_CLASS_IDLE = 3, shifted by 13.
                let ioprio_idle: libc::c_long = 3 << 13;
                let who_process: libc::c_long = 1;
                libc::syscall(libc::SYS_ioprio_set, who_process, tid, ioprio_idle);
            }
        }
    }
}

/// Bounds of one pass.
#[derive(Debug, Clone)]
pub struct PassLimits {
    /// Sessions per batch before the pass yields.
    pub batch_max_sessions: usize,
    /// Raw bytes per batch before the pass yields.
    pub batch_max_raw_bytes: u64,
    /// Wall-clock budget; the next pass resumes where this one stopped.
    pub wall_budget: Duration,
    /// Sleep between batches.
    pub batch_sleep: Duration,
    /// Sleep between checks of a closed batch gate.
    pub gate_poll: Duration,
}

impl Default for PassLimits {
    fn default() -> Self {
        Self {
            batch_max_sessions: 32,
            batch_max_raw_bytes: 64 * 1024 * 1024,
            wall_budget: Duration::from_secs(10 * 60),
            batch_sleep: Duration::from_millis(250),
            gate_poll: Duration::from_secs(5),
        }
    }
}

/// Everything a pass needs besides the store and the read connection.
pub struct PassParams<'a> {
    pub run_id: Uuid,
    pub settings: &'a ColdSettings,
    pub limits: PassLimits,
    /// Asked before each batch: `false` holds the pass (host load).
    pub batch_gate: Option<&'a dyn Fn() -> bool>,
    /// Asked at every safe boundary (between sessions, between batches, while
    /// the gate is closed): `true` stops the pass there. The daemon raises it
    /// when a supervised restart starts to drain, so the pass settles and its
    /// SQLite handles close before the compaction helper opens the database. A
    /// cancelled pass is a partial pass (`budget_exhausted` and `cancelled`),
    /// never a failure: what it staged and moved stays, the rest waits.
    pub cancel: Option<&'a dyn Fn() -> bool>,
}

impl PassParams<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|cancel| cancel())
    }
}

/// One failed session in a report.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionError {
    pub session_id: String,
    pub code: String,
}

/// What a stage pass did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StageReport {
    pub sessions_examined: u64,
    /// Sessions whose chunks are written and verified by this pass.
    pub sessions_staged: u64,
    /// Of those, the ones whose chunks an earlier pass had already written.
    pub sessions_resumed: u64,
    /// Sessions skipped because nothing changed since their last verification.
    pub sessions_unchanged: u64,
    /// Sessions with no movable row.
    pub sessions_empty: u64,
    pub sessions_failed: u64,
    pub unreadable_timestamps: u64,
    /// Held sessions by reason (`HOLD_CODES`), plus `state_busy`.
    pub holds: BTreeMap<String, u64>,
    pub events_staged: u64,
    pub provenance_staged: u64,
    pub raw_bytes: u64,
    pub compressed_bytes: u64,
    /// Chunks this pass added to an archive file.
    pub chunks_written: u64,
    pub verify_errors: u64,
    pub batches: u64,
    pub budget_exhausted: bool,
    /// The pass stopped at a safe boundary because the daemon asked it to
    /// ([`PassParams::cancel`]); implies `budget_exhausted`.
    pub cancelled: bool,
    pub errors: Vec<SessionError>,
    /// What P6 did (a move pass only).
    pub moved: MoveStats,
    /// Why a move pass stopped deleting after it had already moved sessions
    /// (no qualifying backup before a later transaction). The pass keeps what it
    /// committed and reports this as its error; a pass that moved nothing
    /// returns the refusal as its error instead.
    pub move_refused: Option<MoveRefusal>,
}

/// A later deleting transaction the pass refused to open.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MoveRefusal {
    pub code: String,
    pub detail: String,
}

/// What P6 did in a move pass.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MoveStats {
    pub sessions_moved: u64,
    pub events_moved: u64,
    pub provenance_moved: u64,
    /// Sessions an earlier pass staged whose chunks this pass read back again
    /// before it moved them (GM condition 1).
    pub sessions_reverified: u64,
    pub move_batches: u64,
    /// Sessions this pass verified (or re-verified) that were not admitted to a
    /// delete transaction (the host-load gate was closed, the wall budget or a
    /// cancel ended the pass, or no backup qualified). They stay `staged` with
    /// every row intact; the next pass reads them back again before it moves
    /// them.
    pub sessions_deferred: u64,
    /// The longest time one move batch held the store, in milliseconds.
    pub max_batch_lock_ms: u64,
    /// The backup the pass relied on, and whether it took that copy itself.
    pub backup: Option<String>,
    pub backup_produced: bool,
}

impl StageReport {
    /// The run row for this report.
    #[must_use]
    pub fn run_summary(&self, live_bytes_after: Option<i64>) -> RunSummary {
        let clean =
            !self.budget_exhausted && self.sessions_failed == 0 && self.move_refused.is_none();
        RunSummary {
            outcome: if clean { "completed" } else { "partial" },
            sessions_staged: i64::try_from(self.sessions_staged).unwrap_or(i64::MAX),
            sessions_moved: i64::try_from(self.moved.sessions_moved).unwrap_or(i64::MAX),
            events_moved: i64::try_from(self.moved.events_moved).unwrap_or(i64::MAX),
            provenance_moved: i64::try_from(self.moved.provenance_moved).unwrap_or(i64::MAX),
            pages_reclaimed: 0,
            raw_bytes: i64::try_from(self.raw_bytes).unwrap_or(i64::MAX),
            compressed_bytes: i64::try_from(self.compressed_bytes).unwrap_or(i64::MAX),
            holds_json: serde_json::to_string(&self.holds).unwrap_or_else(|_| "{}".into()),
            live_bytes_after,
            error_code: self
                .move_refused
                .as_ref()
                .map(|refusal| refusal.code.clone())
                .or_else(|| self.errors.first().map(|error| error.code.clone())),
            error_detail: self
                .move_refused
                .as_ref()
                .map(|refusal| refusal.detail.clone())
                .or_else(|| {
                    self.errors.first().map(|error| {
                        format!(
                            "{} session(s) failed; first: {} ({})",
                            self.sessions_failed, error.session_id, error.code
                        )
                    })
                }),
        }
    }

    fn hold(&mut self, reason: &str) {
        *self.holds.entry(reason.to_string()).or_default() += 1;
    }

    /// The pass ends before it ran out of work: its wall budget is spent, or the
    /// daemon asked it to stop.
    fn stop_early(&mut self, params: &PassParams<'_>) {
        self.budget_exhausted = true;
        self.cancelled |= params.cancelled();
    }

    fn fail(&mut self, session_id: &str, code: &str) {
        self.sessions_failed += 1;
        if self.errors.len() < MAX_LISTED_ERRORS {
            self.errors.push(SessionError {
                session_id: session_id.to_string(),
                code: code.to_string(),
            });
        }
    }
}

/// What a verify pass did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct VerifyReport {
    pub sessions_checked: u64,
    pub sessions_ok: u64,
    pub sessions_failed: u64,
    pub files_checked: u64,
    pub files_missing: u64,
    /// Chunks no index row refers to (append-only garbage of a crashed pass).
    pub orphan_chunks: u64,
    pub orphan_bytes: u64,
    pub raw_bytes: u64,
    pub compressed_bytes: u64,
    /// Batches the pass yielded after (the same throttle the stage pass obeys).
    pub batches: u64,
    pub budget_exhausted: bool,
    /// Stopped at a safe boundary on the daemon's request; implies
    /// `budget_exhausted`.
    pub cancelled: bool,
    pub errors: Vec<SessionError>,
}

impl VerifyReport {
    /// The run row for this report.
    #[must_use]
    pub fn run_summary(&self, live_bytes_after: Option<i64>) -> RunSummary {
        let clean = !self.budget_exhausted && self.sessions_failed == 0 && self.files_missing == 0;
        RunSummary {
            outcome: if clean { "completed" } else { "partial" },
            sessions_staged: i64::try_from(self.sessions_ok).unwrap_or(i64::MAX),
            sessions_moved: 0,
            events_moved: 0,
            provenance_moved: 0,
            pages_reclaimed: 0,
            raw_bytes: i64::try_from(self.raw_bytes).unwrap_or(i64::MAX),
            compressed_bytes: i64::try_from(self.compressed_bytes).unwrap_or(i64::MAX),
            holds_json: "{}".into(),
            live_bytes_after,
            error_code: self.errors.first().map(|error| error.code.clone()),
            error_detail: (!clean).then(|| {
                format!(
                    "{} session(s) failed, {} file(s) missing, {} orphan chunk(s)",
                    self.sessions_failed, self.files_missing, self.orphan_chunks
                )
            }),
        }
    }
}

/// One session's movable rows, encoded.
pub(super) struct BuiltSession {
    pub(super) pre_event_count: i64,
    pub(super) pre_provenance_count: i64,
    pub(super) events: super::format::EncodedRows,
    pub(super) events_sha: String,
    pub(super) provenance: Option<super::format::EncodedRows>,
    pub(super) provenance_sha: Option<String>,
    pub(super) snapshot_json: String,
}

fn blob_hex(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// The session row as JSON (the archive's `session_snapshots`).
fn session_snapshot_json(conn: &Connection, session_id: &str) -> ColdResult<String> {
    let mut stmt = conn.prepare("SELECT * FROM sessions WHERE id = ?1")?;
    let names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    let mut rows = stmt.query([session_id])?;
    let Some(row) = rows.next()? else {
        return Err(ColdError::new(
            "cold_archive_session_missing",
            format!("{session_id} has no sessions row"),
        ));
    };
    let mut map = serde_json::Map::new();
    for (index, name) in names.iter().enumerate() {
        let value = match row.get_ref(index)? {
            ValueRef::Null => serde_json::Value::Null,
            ValueRef::Integer(number) => serde_json::Value::from(number),
            ValueRef::Real(number) => serde_json::Number::from_f64(number)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            ValueRef::Text(bytes) => {
                serde_json::Value::String(String::from_utf8_lossy(bytes).into_owned())
            }
            ValueRef::Blob(bytes) => serde_json::Value::String(blob_hex(bytes)),
        };
        map.insert(name.clone(), value);
    }
    Ok(serde_json::Value::Object(map).to_string())
}

/// Reads and encodes the movable rows of one session (P3) in one read snapshot.
fn build_session(read: &Connection, session_id: &str) -> ColdResult<BuiltSession> {
    let tx = Transaction::new_unchecked(read, TransactionBehavior::Deferred)?;
    let built = build_session_in(&tx, session_id)?;
    drop(tx);
    Ok(built)
}

/// [`build_session`] on a connection that already holds its snapshot (the move
/// seam digests the live rows inside its own write transaction).
pub(super) fn build_session_in(tx: &Connection, session_id: &str) -> ColdResult<BuiltSession> {
    let pre_event_count: i64 = tx.query_row(TOTAL_EVENTS_SQL, [session_id], |row| row.get(0))?;
    let pre_provenance_count: i64 =
        tx.query_row(TOTAL_PROVENANCE_SQL, [session_id], |row| row.get(0))?;

    let mut encoder = RowEncoder::new(EVENT_COLUMNS.len(), EVENT_ROLES);
    {
        let mut stmt = tx.prepare_cached(MOVABLE_EVENTS_SQL)?;
        let mut rows = stmt.query([session_id])?;
        while let Some(row) = rows.next()? {
            encoder.push_row(row)?;
        }
    }
    let events = encoder.finish();
    let events_sha = events.sha256();

    let mut provenance = None;
    let mut provenance_sha = None;
    if events.row_count > 0 {
        let mut encoder = RowEncoder::new(PROVENANCE_COLUMNS.len(), PROVENANCE_ROLES);
        {
            let mut stmt = tx.prepare_cached(MOVABLE_PROVENANCE_SQL)?;
            let mut rows = stmt.query([session_id])?;
            while let Some(row) = rows.next()? {
                encoder.push_row(row)?;
            }
        }
        let encoded = encoder.finish();
        if encoded.row_count > 0 {
            provenance_sha = Some(encoded.sha256());
            provenance = Some(encoded);
        }
    }
    let snapshot_json = session_snapshot_json(tx, session_id)?;
    Ok(BuiltSession {
        pre_event_count,
        pre_provenance_count,
        events,
        events_sha,
        provenance,
        provenance_sha,
        snapshot_json,
    })
}

/// Cheap change test for a staged, verified session: the row counts and the
/// movable set's size and sequence range. (A content rewrite in place is caught
/// by the digest compare before any delete, in the move slice.)
fn fingerprint_matches(read: &Connection, row: &ColdSessionRow) -> ColdResult<bool> {
    let session_id = row.session_id.as_str();
    let total: i64 = read.query_row(TOTAL_EVENTS_SQL, [session_id], |r| r.get(0))?;
    if total != row.pre_event_count {
        return Ok(false);
    }
    let provenance_total: i64 = read.query_row(TOTAL_PROVENANCE_SQL, [session_id], |r| r.get(0))?;
    if provenance_total != row.pre_provenance_count {
        return Ok(false);
    }
    let (count, min_sequence, max_sequence): (i64, Option<i64>, Option<i64>) =
        read.query_row(MOVABLE_COUNT_SQL, [session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    Ok(count == row.archived_event_count
        && min_sequence == row.archived_min_sequence
        && max_sequence == row.archived_max_sequence)
}

/// What a chunk must look like on read-back (P5).
struct ChunkExpectation<'a> {
    chunk_id: i64,
    session_id: &'a str,
    generation: i64,
    table: &'static str,
    roles: ColumnRoles,
    columns_json: Option<&'a str>,
    sha256: &'a str,
    row_count: i64,
    content_len: i64,
    min_id: Option<i64>,
    max_id: Option<i64>,
    min_sequence: Option<i64>,
    max_sequence: Option<i64>,
}

fn mismatch(
    code: &'static str,
    what: &str,
    expected: impl std::fmt::Debug,
    actual: impl std::fmt::Debug,
) -> ColdError {
    ColdError::new(
        code,
        format!("{what}: the manifest records {expected:?}, the chunk holds {actual:?}"),
    )
}

/// Reads one chunk back and checks it against what was written (P5): the row's
/// identity and manifest, the decompressed length and `SHA-256`, and that the
/// stream decodes to the recorded number of rows with the recorded content
/// length and id and sequence range. Returns `(raw_bytes, compressed_bytes)`.
fn verify_chunk(ro: &Connection, expect: &ChunkExpectation<'_>) -> ColdResult<(i64, i64)> {
    let record = read_chunk(ro, expect.chunk_id)?.ok_or_else(|| {
        ColdError::new(
            "cold_archive_verify_missing",
            format!("chunk {} is not in the archive file", expect.chunk_id),
        )
    })?;
    if record.session_id != expect.session_id
        || record.generation != expect.generation
        || record.table_name != expect.table
    {
        return Err(ColdError::new(
            "cold_archive_verify_identity",
            format!(
                "chunk {} is {}/{}/{}, expected {}/{}/{}",
                expect.chunk_id,
                record.session_id,
                record.generation,
                record.table_name,
                expect.session_id,
                expect.generation,
                expect.table
            ),
        ));
    }
    if record.codec != "zstd" {
        return Err(ColdError::new(
            "cold_archive_verify_codec",
            format!("chunk {} uses codec {}", expect.chunk_id, record.codec),
        ));
    }
    if let Some(columns) = expect.columns_json
        && record.columns_json != columns
    {
        return Err(mismatch(
            "cold_archive_verify_columns",
            "columns",
            columns,
            &record.columns_json,
        ));
    }
    if record.raw_sha256 != expect.sha256 {
        return Err(mismatch(
            "cold_archive_verify_sha_mismatch",
            "sha256",
            expect.sha256,
            &record.raw_sha256,
        ));
    }
    let raw = decompress_checked(&record.payload, record.raw_bytes, &record.raw_sha256)?;
    let columns = parse_columns_json(&record.columns_json)?;
    let rows = decode_rows(&raw, columns.len())?;
    let summary = summarize_decoded(&rows, expect.roles);
    if summary.row_count != record.row_count || summary.row_count != expect.row_count {
        return Err(mismatch(
            "cold_archive_verify_count",
            "row count",
            expect.row_count,
            summary.row_count,
        ));
    }
    if summary.content_len != record.content_len || summary.content_len != expect.content_len {
        return Err(mismatch(
            "cold_archive_verify_content_len",
            "content length",
            expect.content_len,
            summary.content_len,
        ));
    }
    for (what, expected, recorded, actual) in [
        ("min id", expect.min_id, record.min_id, summary.min_id),
        ("max id", expect.max_id, record.max_id, summary.max_id),
        (
            "min sequence",
            expect.min_sequence,
            record.min_sequence,
            summary.min_sequence,
        ),
        (
            "max sequence",
            expect.max_sequence,
            record.max_sequence,
            summary.max_sequence,
        ),
    ] {
        if recorded != actual || expected.is_some_and(|value| Some(value) != actual) {
            return Err(mismatch(
                "cold_archive_verify_range",
                what,
                expected,
                actual,
            ));
        }
    }
    let compressed = i64::try_from(record.payload.len()).unwrap_or(i64::MAX);
    Ok((record.raw_bytes, compressed))
}

fn now_string() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn is_injected(error: &ColdError) -> bool {
    error.code == INJECTED_FAILURE
}

struct StagePass<'a, A: ColdStoreAccess> {
    access: &'a A,
    read: &'a Connection,
    dir: PathBuf,
    now: DateTime<Utc>,
    cutoff: DateTime<Utc>,
    schema_version: i64,
    events_columns_json: String,
    provenance_columns_json: String,
    files: HashMap<String, ArchiveFile>,
    readers: HashMap<String, Connection>,
    /// `file_id` -> recorded path, for read-backs of earlier passes' chunks.
    file_paths: HashMap<i64, PathBuf>,
    /// Read-only connections by archive path, for those read-backs.
    verify_readers: HashMap<String, Connection>,
}

enum SessionOutcome {
    Held(&'static str),
    Skipped(&'static str),
    Unchanged,
    Empty,
    Staged {
        events: u64,
        provenance: u64,
        raw_bytes: u64,
        compressed_bytes: u64,
        reused: bool,
        chunks_written: u64,
    },
}

impl<A: ColdStoreAccess> StagePass<'_, A> {
    fn archive_file(&mut self, month: &str) -> ColdResult<&mut ArchiveFile> {
        if !self.files.contains_key(month) {
            let (file, _created) =
                ArchiveFile::open_or_create(&self.dir, month, self.schema_version)?;
            self.files.insert(month.to_string(), file);
        }
        self.files
            .get_mut(month)
            .ok_or_else(|| ColdError::new("cold_archive_io", "archive file handle vanished"))
    }

    fn reader(&mut self, month: &str, path: &Path) -> ColdResult<&Connection> {
        if !self.readers.contains_key(month) {
            self.readers
                .insert(month.to_string(), open_archive_readonly(path)?);
        }
        self.readers
            .get(month)
            .ok_or_else(|| ColdError::new("cold_archive_io", "archive reader vanished"))
    }

    #[allow(clippy::too_many_lines)]
    fn stage_one(&mut self, candidate: &Candidate) -> ColdResult<SessionOutcome> {
        let session_id = candidate.session_id.as_str();
        let existing = self
            .access
            .with_store(|store| store.cold_session_row(session_id))?;
        if let Some(row) = &existing
            && !matches!(row.state.as_str(), "staged" | "restored")
        {
            return Ok(SessionOutcome::Skipped("state_busy"));
        }
        if let Some(code) = session_hold(self.read, session_id, HoldContext { now: self.now })? {
            return Ok(SessionOutcome::Held(code));
        }
        if let Some(row) = &existing
            && row.state == "restored"
            && recently_restored(self.read, session_id, self.cutoff)?
        {
            return Ok(SessionOutcome::Held(RECENTLY_RESTORED_CODE));
        }
        if let Some(row) = &existing
            && row.state == "staged"
            && row.verified_at.is_some()
            && row.last_error_code.is_none()
            && fingerprint_matches(self.read, row)?
        {
            return Ok(SessionOutcome::Unchanged);
        }

        // The age proof (plan §2.2): every row of the session is provably old.
        if !prove_age(self.read, session_id, self.cutoff)? {
            return Ok(SessionOutcome::Skipped(AGE_UNPROVEN_CODE));
        }

        // P3: build.
        let built = build_session(self.read, session_id)?;
        if built.events.row_count == 0 {
            return Ok(SessionOutcome::Empty);
        }
        let month = archive_month(candidate.created_at);
        let path = self.dir.join(archive_file_name(&month));
        let path_text = path.to_string_lossy().into_owned();
        let generation = existing.as_ref().map_or(1, |row| row.generation);
        let now = now_string();
        let archived_provenance = built.provenance.as_ref().map_or(0, |p| p.row_count);

        // P2: stage. Recorded before anything is written to the archive.
        let file_id = self.access.with_store(|store| {
            let file_id =
                store.cold_register_archive_file(&path_text, &month, ARCHIVE_FORMAT_VERSION)?;
            store.cold_stage_session(&StageIntent {
                session_id,
                generation,
                file_id,
                pre_event_count: built.pre_event_count,
                pre_provenance_count: built.pre_provenance_count,
                archived_event_count: built.events.row_count,
                archived_provenance_count: archived_provenance,
                archived_content_len: built.events.content_len,
                archived_min_sequence: built.events.min_sequence,
                archived_max_sequence: built.events.max_sequence,
                events_sha256: &built.events_sha,
                provenance_sha256: built.provenance_sha.as_deref(),
                raw_bytes: i64::try_from(
                    built.events.raw.len() + built.provenance.as_ref().map_or(0, |p| p.raw.len()),
                )
                .unwrap_or(i64::MAX),
                now: &now,
            })?;
            Ok(file_id)
        })?;
        failpoint::hit("after_stage")?;

        // P4: write and fsync.
        let events_columns = self.events_columns_json.clone();
        let provenance_columns = self.provenance_columns_json.clone();
        let schema_version = self.schema_version;
        let events_input = ChunkInput {
            table: EVENTS_TABLE,
            columns_json: &events_columns,
            encoded: &built.events,
            sha256: &built.events_sha,
            source_schema_version: schema_version,
        };
        let provenance_input = built.provenance.as_ref().map(|encoded| ChunkInput {
            table: PROVENANCE_TABLE,
            columns_json: &provenance_columns,
            encoded,
            sha256: built.provenance_sha.as_deref().unwrap_or_default(),
            source_schema_version: schema_version,
        });
        // A chunk an earlier pass wrote that no longer reads back is not trusted:
        // while the live rows still exist (this slice never deletes them), the
        // session is written again under the next generation.
        let mut start_generation = generation;
        let mut attempt = 0;
        let written = loop {
            attempt += 1;
            let written = self.archive_file(&month)?.write_session(
                session_id,
                start_generation,
                &events_input,
                provenance_input.as_ref(),
                &built.snapshot_json,
            )?;
            failpoint::hit("after_write")?;
            let (chunk_count, file_bytes) = self.archive_file(&month)?.stats()?;
            let recorded_at = now_string();
            self.access.with_store(|store| {
                store.cold_record_written(session_id, &written, &recorded_at)?;
                store.cold_update_file_size(file_id, file_bytes, chunk_count, &recorded_at)
            })?;

            // P5: read back through a separate read-only connection and verify.
            let verified = {
                let ro = self.reader(&month, &path)?;
                let events_expect = ChunkExpectation {
                    chunk_id: written.events_chunk_id,
                    session_id,
                    generation: written.generation,
                    table: EVENTS_TABLE,
                    roles: EVENT_ROLES,
                    columns_json: Some(&events_columns),
                    sha256: &built.events_sha,
                    row_count: built.events.row_count,
                    content_len: built.events.content_len,
                    min_id: built.events.min_id,
                    max_id: built.events.max_id,
                    min_sequence: built.events.min_sequence,
                    max_sequence: built.events.max_sequence,
                };
                verify_chunk(ro, &events_expect).and_then(|_| {
                    match (&built.provenance, written.provenance_chunk_id) {
                        (Some(encoded), Some(chunk_id)) => verify_chunk(
                            ro,
                            &ChunkExpectation {
                                chunk_id,
                                session_id,
                                generation: written.generation,
                                table: PROVENANCE_TABLE,
                                roles: PROVENANCE_ROLES,
                                columns_json: Some(&provenance_columns),
                                sha256: built.provenance_sha.as_deref().unwrap_or_default(),
                                row_count: encoded.row_count,
                                content_len: 0,
                                min_id: encoded.min_id,
                                max_id: encoded.max_id,
                                min_sequence: None,
                                max_sequence: None,
                            },
                        )
                        .map(|_| ()),
                        (None, None) => Ok(()),
                        _ => Err(ColdError::new(
                            "cold_archive_verify_identity",
                            "the provenance chunk does not match the session's provenance rows",
                        )),
                    }
                })
            };
            match verified {
                Ok(()) => break written,
                Err(error) if written.reused && attempt == 1 => {
                    tracing::warn!(
                        session_id,
                        generation = written.generation,
                        error = %error,
                        "cold archive: a chunk written earlier does not read back; writing a new generation"
                    );
                    start_generation = written.generation + 1;
                }
                Err(error) => {
                    let at = now_string();
                    let code = error.code;
                    self.access.with_store(|store| {
                        store.cold_mark_file_verified(file_id, &at, Some(&error.to_string()))?;
                        store.cold_record_session_error(session_id, code, &at)
                    })?;
                    return Err(error);
                }
            }
        };
        failpoint::hit("after_verify")?;
        let verified_at = now_string();
        self.access.with_store(|store| {
            store.cold_record_verified(session_id, &verified_at)?;
            store.cold_mark_file_verified(file_id, &verified_at, None)
        })?;

        Ok(SessionOutcome::Staged {
            events: u64::try_from(built.events.row_count).unwrap_or(0),
            provenance: u64::try_from(archived_provenance).unwrap_or(0),
            raw_bytes: u64::try_from(
                built.events.raw.len() + built.provenance.as_ref().map_or(0, |p| p.raw.len()),
            )
            .unwrap_or(0),
            compressed_bytes: u64::try_from(written.compressed_bytes).unwrap_or(0),
            reused: written.reused,
            chunks_written: if written.reused {
                0
            } else if written.provenance_chunk_id.is_some() {
                2
            } else {
                1
            },
        })
    }
}

/// One batch of work between yields. Both passes count their work through it,
/// so the stage and verify passes obey the same batch size, the same sleep
/// between batches and the same host-load gate (#2437).
#[derive(Debug, Default)]
struct BatchThrottle {
    sessions: usize,
    raw_bytes: u64,
}

impl BatchThrottle {
    fn note(&mut self, raw_bytes: u64) {
        self.sessions += 1;
        self.raw_bytes += raw_bytes;
    }

    /// Called after each item. When the batch is full it is counted, the pass
    /// sleeps and waits for the gate; `false` means the budget ran out while
    /// waiting.
    fn yield_if_full(
        &mut self,
        timer: &PassTimer<'_>,
        params: &PassParams<'_>,
        batches: &mut u64,
    ) -> bool {
        if self.sessions < params.limits.batch_max_sessions
            && self.raw_bytes < params.limits.batch_max_raw_bytes
        {
            return true;
        }
        self.sessions = 0;
        self.raw_bytes = 0;
        *batches += 1;
        sleep_unless_cancelled(params.limits.batch_sleep, params) && wait_for_gate(timer, params)
    }

    /// Counts the trailing partial batch.
    fn finish(&mut self, batches: &mut u64) {
        if self.sessions > 0 {
            *batches += 1;
        }
    }
}

/// The longest the pass sleeps without looking at its cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// Sleeps `total`, looking at the cancel flag every [`CANCEL_POLL`]; `false`
/// when the pass was cancelled meanwhile.
fn sleep_unless_cancelled(total: Duration, params: &PassParams<'_>) -> bool {
    let until = Instant::now() + total;
    loop {
        if params.cancelled() {
            return false;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(CANCEL_POLL));
    }
}

/// Waits while the batch gate is closed; `false` when the budget ran out (or
/// the pass was cancelled) first.
fn wait_for_gate(timer: &PassTimer<'_>, params: &PassParams<'_>) -> bool {
    loop {
        if params.cancelled() || timer.elapsed() >= params.limits.wall_budget {
            return false;
        }
        match params.batch_gate {
            Some(gate) if !gate() => {
                if !sleep_unless_cancelled(params.limits.gate_poll, params) {
                    return false;
                }
            }
            _ => return true,
        }
    }
}

/// The pass's own monotonic clock: the real time since it started plus a skew
/// (zero in production; a test adds to it so a spent wall budget needs no wait).
#[derive(Clone, Copy)]
struct PassTimer<'c> {
    started: Instant,
    skew: &'c dyn Fn() -> Duration,
}

fn no_skew() -> Duration {
    Duration::ZERO
}

fn no_hook() {}

impl<'c> PassTimer<'c> {
    fn new(skew: &'c dyn Fn() -> Duration) -> Self {
        Self {
            started: Instant::now(),
            skew,
        }
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed() + (self.skew)()
    }
}

/// The clocks and handshakes of one move pass. Production runs with
/// [`MoveSeams::real`]: the system clock, no skew, no-op handshakes. A test moves
/// the clocks and uses the two handshakes to hold the pass inside a wait it must
/// survive: `before_store` runs after the backup qualified and before the store
/// is requested; `in_writer` runs with the store lock and `SQLite`'s write lock
/// held, just before the transaction's admission check; `after_qualify` runs in
/// that check, after the backup was read again and before the final load, cancel
/// and budget answers.
#[derive(Clone, Copy)]
pub(super) struct MoveSeams<'c> {
    /// The wall clock of every backup check.
    pub(super) wall: &'c dyn Fn() -> SystemTime,
    /// Added to the pass's monotonic clock (the wall budget).
    pub(super) skew: &'c dyn Fn() -> Duration,
    pub(super) before_store: &'c dyn Fn(),
    pub(super) in_writer: &'c dyn Fn(),
    pub(super) after_qualify: &'c dyn Fn(),
}

impl MoveSeams<'static> {
    pub(super) fn real() -> Self {
        Self {
            wall: &SystemTime::now,
            skew: &no_skew,
            before_store: &no_hook,
            in_writer: &no_hook,
            after_qualify: &no_hook,
        }
    }
}

/// How many times one attempt goes back outside the locks to admit itself again
/// when the check at the transaction boundary refuses it.
const MAX_BOUNDARY_ROUNDS: u32 = 3;

/// P6 state of a move pass: the proofs of sessions this pass verified, waiting
/// for their batch, and the backup the last deleting transaction relied on.
struct Mover<'p> {
    database_path: &'p Path,
    cutoff: DateTime<Utc>,
    /// The clocks and test handshakes of this pass.
    seams: MoveSeams<'p>,
    pending: Vec<VerifiedChunks>,
    pending_raw: u64,
    /// The backup the previous deleting transaction relied on. It is only a
    /// starting point: `admit_backup` opens it again before the next one, so a
    /// backup that aged out or disappeared meanwhile is never relied on.
    backup: Option<FreshBackup>,
    /// Why the pass stopped deleting: no qualifying backup before a transaction.
    refusal: Option<ColdError>,
}

/// How a flush of the queued proofs ended.
enum FlushEnd {
    /// Every queued proof had its turn (moved, skipped, or failed on its own).
    Done,
    /// The host-load gate, the wall budget or a cancel kept some proofs out of a
    /// transaction; they stay `staged`.
    Deferred,
    /// No backup qualified; the pass stops deleting.
    Refused,
}

/// How one attempt at a delete transaction ended.
enum Attempt {
    Done(Vec<MoveVerdict>),
    /// Not admitted: the gate stayed closed until the budget ran out, or the pass
    /// was cancelled. No transaction was opened.
    Deferred,
    /// Not admitted: no backup qualifies. No transaction was opened.
    Refused(ColdError),
    /// The transaction failed and rolled back (or an injected process death).
    Failed(ColdError),
}

impl Mover<'_> {
    /// Queues a session whose chunks this pass just read back equal to `row`.
    fn push(&mut self, row: &ColdSessionRow) {
        self.pending_raw += u64::try_from(row.raw_bytes).unwrap_or(0);
        self.pending.push(VerifiedChunks::of_row(row));
    }

    fn full(&self, limits: &PassLimits) -> bool {
        self.pending.len() >= limits.batch_max_sessions
            || self.pending_raw >= limits.batch_max_raw_bytes
    }

    fn account(report: &mut StageReport, session_id: &str, verdict: &MoveVerdict) {
        match verdict {
            MoveVerdict::Moved { events, provenance } => {
                report.moved.sessions_moved += 1;
                report.moved.events_moved += u64::try_from(*events).unwrap_or(0);
                report.moved.provenance_moved += u64::try_from(*provenance).unwrap_or(0);
            }
            MoveVerdict::Skipped(reason) => {
                tracing::debug!(session_id, reason, "cold archive: session not moved");
                report.hold(reason);
            }
        }
    }

    /// One delete transaction for `batch`, admitted twice (#2384 review). Outside
    /// the locks: the host-load gate must be open (waiting for it here never holds
    /// the store), the wall budget and cancel flag clear, and a backup must
    /// qualify (GM condition 2; any `VACUUM INTO` happens here). Then the pass
    /// waits for the store and for `SQLite`'s write lock, which can take long, so
    /// it asks again at the transaction boundary
    /// ([`admit_at_boundary`], run by `cold_move_batch_admitted` once both locks
    /// are held and before the first delete): cancel, budget, host load, and the
    /// backup file itself, now with the reserve re-established. A refusal there
    /// releases the locks and goes round again, outside them, up to
    /// [`MAX_BOUNDARY_ROUNDS`] times; nothing was deleted.
    fn attempt<A: ColdStoreAccess>(
        &mut self,
        access: &A,
        read: &Connection,
        batch: &[(VerifiedChunks, &SpawnGuard)],
        params: &PassParams<'_>,
        timer: &PassTimer<'_>,
        report: &mut StageReport,
    ) -> Attempt {
        let seams = self.seams;
        let mut refused_at_boundary: Option<ColdError> = None;
        for _round in 0..MAX_BOUNDARY_ROUNDS {
            if !wait_for_gate(timer, params) {
                return Attempt::Deferred;
            }
            let backup = match admit_backup(
                self.backup.as_ref(),
                read,
                self.database_path,
                (seams.wall)(),
            ) {
                Ok(backup) => backup,
                Err(error) => return Attempt::Refused(error),
            };
            report.moved.backup = Some(backup.path.display().to_string());
            report.moved.backup_produced |= backup.produced;
            self.backup = Some(backup.clone());
            (seams.before_store)();
            let context = MoveContext {
                now: Utc::now(),
                cutoff: self.cutoff,
            };
            let boundary = || {
                (seams.in_writer)();
                admit_at_boundary(params, timer, &backup, &seams)
            };
            let locked = Instant::now();
            let result = access
                .with_store(|store| store.cold_move_batch_admitted(batch, context, &boundary));
            let lock_ms = u64::try_from(locked.elapsed().as_millis()).unwrap_or(u64::MAX);
            report.moved.max_batch_lock_ms = report.moved.max_batch_lock_ms.max(lock_ms);
            tracing::info!(
                sessions = batch.len(),
                cold_archive_batch_lock_ms = lock_ms,
                "cold archive: move batch"
            );
            match result {
                Ok(verdicts) => return Attempt::Done(verdicts),
                Err(error)
                    if error.code == NOT_ADMITTED_CODE || error.code == BACKUP_STALE_CODE =>
                {
                    refused_at_boundary = Some(error);
                }
                Err(error) => return Attempt::Failed(error),
            }
        }
        match refused_at_boundary {
            Some(error) if error.code == BACKUP_STALE_CODE => Attempt::Refused(error),
            _ => Attempt::Deferred,
        }
    }

    /// Runs P6 for the queued proofs. `Err` stops the pass: an injected process
    /// death. A batch that fails is rolled back and retried one session at a
    /// time; the failing session keeps its rows and records why. Every attempt,
    /// the retries and the trailing flush included, is admitted by
    /// [`Self::attempt`]; proofs that are not admitted stay `staged` for the
    /// next pass (`sessions_deferred`).
    fn flush<A: ColdStoreAccess>(
        &mut self,
        access: &A,
        read: &Connection,
        params: &PassParams<'_>,
        timer: &PassTimer<'_>,
        report: &mut StageReport,
    ) -> ColdResult<FlushEnd> {
        if self.pending.is_empty() {
            return Ok(FlushEnd::Done);
        }
        let proofs = std::mem::take(&mut self.pending);
        self.pending_raw = 0;
        // The guards are taken before the store mutex and never awaited: a
        // session whose guard is busy is skipped as `guard_busy`.
        let mut held: Vec<(VerifiedChunks, SpawnGuard)> = Vec::with_capacity(proofs.len());
        for proof in proofs {
            let guard = Uuid::parse_str(&proof.session_id)
                .ok()
                .and_then(try_acquire_spawn_guard);
            match guard {
                Some(guard) => held.push((proof, guard)),
                None => report.hold(GUARD_BUSY_CODE),
            }
        }
        if held.is_empty() {
            return Ok(FlushEnd::Done);
        }
        let batch: Vec<(VerifiedChunks, &SpawnGuard)> = held
            .iter()
            .map(|(proof, guard)| (proof.clone(), guard))
            .collect();
        match self.attempt(access, read, &batch, params, timer, report) {
            Attempt::Done(verdicts) => {
                report.moved.move_batches += 1;
                for ((proof, _), verdict) in batch.iter().zip(&verdicts) {
                    Self::account(report, &proof.session_id, verdict);
                }
                Ok(FlushEnd::Done)
            }
            Attempt::Deferred => {
                report.moved.sessions_deferred += count_of(batch.len());
                Ok(FlushEnd::Deferred)
            }
            Attempt::Refused(error) => {
                report.moved.sessions_deferred += count_of(batch.len());
                self.refusal = Some(error);
                Ok(FlushEnd::Refused)
            }
            Attempt::Failed(error) if is_injected(&error) => Err(error),
            Attempt::Failed(error) => {
                report.moved.move_batches += 1;
                tracing::warn!(error = %error, "cold archive: a move batch failed; retrying one session at a time");
                self.retry_singly(access, read, &batch, params, timer, report)
            }
        }
    }

    /// The isolated retries of a failed batch, each admitted on its own.
    fn retry_singly<A: ColdStoreAccess>(
        &mut self,
        access: &A,
        read: &Connection,
        batch: &[(VerifiedChunks, &SpawnGuard)],
        params: &PassParams<'_>,
        timer: &PassTimer<'_>,
        report: &mut StageReport,
    ) -> ColdResult<FlushEnd> {
        for (index, entry) in batch.iter().enumerate() {
            let single = std::slice::from_ref(entry);
            match self.attempt(access, read, single, params, timer, report) {
                Attempt::Done(verdicts) => {
                    for verdict in &verdicts {
                        Self::account(report, &entry.0.session_id, verdict);
                    }
                }
                Attempt::Deferred => {
                    report.moved.sessions_deferred += count_of(batch.len() - index);
                    return Ok(FlushEnd::Deferred);
                }
                Attempt::Refused(error) => {
                    report.moved.sessions_deferred += count_of(batch.len() - index);
                    self.refusal = Some(error);
                    return Ok(FlushEnd::Refused);
                }
                Attempt::Failed(error) if is_injected(&error) => return Err(error),
                Attempt::Failed(error) => {
                    let session_id = entry.0.session_id.as_str();
                    tracing::warn!(session_id, error = %error, "cold archive: session move failed");
                    report.fail(session_id, error.code);
                    let at = now_string();
                    let code = error.code;
                    let _ = access
                        .with_store(|store| store.cold_record_session_error(session_id, code, &at));
                }
            }
        }
        Ok(FlushEnd::Done)
    }
}

fn count_of(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// Why a delete may not start right now, if it may not: the daemon asked the pass
/// to stop, its wall budget is spent, or the host-load gate is closed. Each is
/// read at this instant and none waits.
fn refusal_now(params: &PassParams<'_>, timer: &PassTimer<'_>) -> Option<&'static str> {
    if params.cancelled() {
        return Some("the daemon asked the pass to stop");
    }
    if timer.elapsed() >= params.limits.wall_budget {
        return Some("the pass's wall budget is spent");
    }
    if params.batch_gate.is_some_and(|gate| !gate()) {
        return Some("the host-load gate closed");
    }
    None
}

/// What a delete transaction asks again once it holds the store lock and
/// `SQLite`'s write lock, before its first delete. The pass may have waited for
/// either since it last looked, so every answer is current. The backup file is
/// read first, and it never waits (a backup another connection has locked is not
/// admitted, see `FreshBackup::requalify`); the cancel flag, the wall budget and
/// the host-load gate are read last, after the backup, so nothing that changed
/// while the backup was read can start a delete. Nonblocking throughout: a closed
/// gate refuses here and is waited out by the caller outside the locks.
fn admit_at_boundary(
    params: &PassParams<'_>,
    timer: &PassTimer<'_>,
    backup: &FreshBackup,
    seams: &MoveSeams<'_>,
) -> ColdResult<()> {
    backup.requalify((seams.wall)())?;
    (seams.after_qualify)();
    match refusal_now(params, timer) {
        Some(reason) => Err(ColdError::new(NOT_ADMITTED_CODE, reason)),
        None => Ok(()),
    }
}

impl<A: ColdStoreAccess> StagePass<'_, A> {
    /// GM condition 1 for a session an earlier pass staged and verified: read
    /// its chunks back again now. `Some(row)` when they still equal the index
    /// row; a failure is recorded on the row (verified again before any move).
    fn reverify(
        &mut self,
        session_id: &str,
        settings: &ColdSettings,
    ) -> ColdResult<Option<ColdSessionRow>> {
        let Some(row) = self
            .access
            .with_store(|store| store.cold_session_row(session_id))?
        else {
            return Ok(None);
        };
        if row.state != "staged" || row.events_chunk_id <= 0 {
            return Ok(None);
        }
        let path = match self.file_paths.get(&row.file_id) {
            Some(path) => path.clone(),
            None => {
                let files = self.access.with_store(|store| store.cold_archive_files())?;
                for file in files {
                    self.file_paths
                        .insert(file.file_id, PathBuf::from(file.path));
                }
                self.file_paths.get(&row.file_id).cloned().ok_or_else(|| {
                    ColdError::new(
                        "cold_archive_file_missing",
                        format!("file {} is not in the index", row.file_id),
                    )
                })?
            }
        };
        let checked = if let Some(refusal) = recorded_path_refusal(&path, settings) {
            Err(ColdError::new("cold_archive_dir_forbidden", refusal))
        } else {
            let key = path.to_string_lossy().into_owned();
            if !self.verify_readers.contains_key(&key) {
                self.verify_readers
                    .insert(key.clone(), open_archive_readonly(&path)?);
            }
            // The column metadata is part of the proof (#2384 review): the payload
            // digest covers only the encoded values, so a renamed or reordered
            // `columns_json` would still pass the digest and then read back into
            // other fields. The live tables' own column lists are the only mapping
            // this build trusts for rows it is about to delete.
            let live = LiveColumns {
                events: &self.events_columns_json,
                provenance: &self.provenance_columns_json,
            };
            match self.verify_readers.get(&key) {
                Some(ro) => verify_indexed_session(ro, &row, Some(live)).map(|_| ()),
                None => Err(ColdError::new("cold_archive_io", "archive reader vanished")),
            }
        };
        let at = now_string();
        match checked {
            Ok(()) => {
                self.access
                    .with_store(|store| store.cold_record_verified(session_id, &at))?;
                Ok(Some(row))
            }
            Err(error) => {
                let code = error.code;
                self.access
                    .with_store(|store| store.cold_record_session_error(session_id, code, &at))?;
                Err(error)
            }
        }
    }
}

/// Runs one stage pass: writes and verifies archive chunks for the cold set.
/// Deletes nothing. `Err` means the pass could not run or was killed (an
/// injected failure); per-session failures are recorded and listed in the
/// report. The caller owns the run row (`Store::cold_begin_run`).
pub fn run_stage_pass<A: ColdStoreAccess>(
    access: &A,
    read: &Connection,
    params: &PassParams<'_>,
) -> ColdResult<StageReport> {
    run_pass(access, read, params, None)
}

/// Runs one move pass (slice S4): a stage pass that then moves every session
/// whose chunks it read back and verified in this same pass (a session staged
/// by an earlier pass is read back again first, columns included), in batches
/// through P6. Before **every** delete transaction it requires the host-load
/// gate and the wall budget to admit it and a backup of `database_path` that
/// qualifies at that moment. With no backup before its first transaction the
/// pass fails `cold_archive_backup_stale` and nothing is deleted; with none
/// before a later one it stops, keeps what it committed and reports the
/// refusal in [`StageReport::move_refused`].
pub fn run_move_pass<A: ColdStoreAccess>(
    access: &A,
    read: &Connection,
    params: &PassParams<'_>,
    database_path: &Path,
) -> ColdResult<StageReport> {
    run_move_pass_with_seams(access, read, params, database_path, &MoveSeams::real())
}

/// [`run_move_pass`] with the clocks and handshakes of [`MoveSeams`] (a test
/// moves them).
pub(super) fn run_move_pass_with_seams<A: ColdStoreAccess>(
    access: &A,
    read: &Connection,
    params: &PassParams<'_>,
    database_path: &Path,
    seams: &MoveSeams<'_>,
) -> ColdResult<StageReport> {
    let cutoff = Utc::now() - chrono::Duration::days(i64::from(params.settings.after_days));
    let mut mover = Mover {
        database_path,
        cutoff,
        seams: *seams,
        pending: Vec::new(),
        pending_raw: 0,
        backup: None,
        refusal: None,
    };
    run_pass(access, read, params, Some(&mut mover))
}

#[allow(clippy::too_many_lines)]
fn run_pass<A: ColdStoreAccess>(
    access: &A,
    read: &Connection,
    params: &PassParams<'_>,
    mut mover: Option<&mut Mover<'_>>,
) -> ColdResult<StageReport> {
    // The pass's own monotonic clock; a move pass may carry an injected skew (tests).
    let skew: &dyn Fn() -> Duration = match mover.as_ref() {
        Some(mover) => mover.seams.skew,
        None => &no_skew,
    };
    let timer = PassTimer::new(skew);
    let now = Utc::now();
    let cutoff = mover.as_ref().map_or_else(
        || now - chrono::Duration::days(i64::from(params.settings.after_days)),
        |mover| mover.cutoff,
    );
    access.with_store(|store| store.cold_fail_stale_runs(params.run_id).map(|_| ()))?;
    // Resolve the directory the way the operating system will (symlinks, `..`)
    // and refuse a sandbox root or `.git` before any file is created (#2435).
    let dir = check_archive_dir(&params.settings.dir, &params.settings.forbidden_roots)
        .map_err(|detail| ColdError::new("cold_archive_dir_forbidden", detail))?;
    ensure_expected_columns(read)?;
    check_hold_classification(read)?;
    let schema_version = access.with_store(|store| {
        store
            .schema_version()
            .map(i64::from)
            .map_err(|error| ColdError::new("cold_archive_sqlite", error.to_string()))
    })?;
    let mut pass = StagePass {
        access,
        read,
        dir,
        now,
        cutoff,
        schema_version,
        events_columns_json: columns_json(&table_columns(read, EVENTS_TABLE)?)?,
        provenance_columns_json: columns_json(&table_columns(read, PROVENANCE_TABLE)?)?,
        files: HashMap::new(),
        readers: HashMap::new(),
        file_paths: HashMap::new(),
        verify_readers: HashMap::new(),
    };
    let mut report = StageReport::default();
    let mut after: Option<String> = None;
    let mut throttle = BatchThrottle::default();
    let mut consecutive_failures = 0u32;
    let mut last_error: Option<ColdError> = None;

    'walk: loop {
        if !wait_for_gate(&timer, params) {
            report.stop_early(params);
            break;
        }
        let page = candidate_page(read, after.as_deref(), CANDIDATE_PAGE, cutoff)?;
        report.sessions_examined += u64::try_from(page.examined).unwrap_or(0);
        report.unreadable_timestamps += u64::try_from(page.unreadable_timestamp).unwrap_or(0);
        for candidate in &page.candidates {
            if params.cancelled() || timer.elapsed() >= params.limits.wall_budget {
                report.stop_early(params);
                break 'walk;
            }
            let session_id = candidate.session_id.as_str();
            let mut outcome = pass.stage_one(candidate);
            // A move needs this pass's own read-back (GM condition 1): a session
            // an earlier pass verified is read back again before it may move.
            let mut proven: Option<ColdSessionRow> = None;
            if let Some(mover) = mover.as_deref_mut() {
                let unchanged = matches!(outcome, Ok(SessionOutcome::Unchanged));
                let staged = matches!(outcome, Ok(SessionOutcome::Staged { .. }));
                if unchanged {
                    match pass.reverify(session_id, params.settings) {
                        Ok(row) => {
                            if row.is_some() {
                                report.moved.sessions_reverified += 1;
                            }
                            proven = row;
                        }
                        Err(error) => outcome = Err(error),
                    }
                } else if staged {
                    proven = access
                        .with_store(|store| store.cold_session_row(session_id))?
                        .filter(|row| {
                            row.state == "staged"
                                && row.verified_at.is_some()
                                && row.last_error_code.is_none()
                        });
                }
                if let Some(row) = &proven {
                    mover.push(row);
                }
            }
            match outcome {
                Ok(SessionOutcome::Held(reason)) => report.hold(reason),
                Ok(SessionOutcome::Skipped(reason)) => report.hold(reason),
                Ok(SessionOutcome::Unchanged) => {
                    report.sessions_unchanged += 1;
                    // A session read back again for a move is real work: its
                    // chunks were decompressed and hashed (#2384 review), so it
                    // counts in the batch like freshly staged work, and the batch
                    // sleep and the host-load gate apply to a pass that only
                    // re-verifies.
                    if let Some(row) = &proven {
                        throttle.note(u64::try_from(row.raw_bytes).unwrap_or(0));
                    }
                }
                Ok(SessionOutcome::Empty) => report.sessions_empty += 1,
                Ok(SessionOutcome::Staged {
                    events,
                    provenance,
                    raw_bytes,
                    compressed_bytes,
                    reused,
                    chunks_written,
                }) => {
                    consecutive_failures = 0;
                    throttle.note(raw_bytes);
                    report.sessions_staged += 1;
                    report.sessions_resumed += u64::from(reused);
                    report.events_staged += events;
                    report.provenance_staged += provenance;
                    report.raw_bytes += raw_bytes;
                    report.compressed_bytes += compressed_bytes;
                    report.chunks_written += chunks_written;
                }
                Err(error) if is_injected(&error) => return Err(error),
                Err(error) => {
                    consecutive_failures += 1;
                    throttle.note(0);
                    report.fail(&candidate.session_id, error.code);
                    if error.code.starts_with("cold_archive_verify") {
                        report.verify_errors += 1;
                    } else {
                        // Not a verify failure: the session's own step did not
                        // run to completion, so remember why on its row.
                        let at = now_string();
                        let code = error.code;
                        let _ = access.with_store(|store| {
                            store.cold_record_session_error(session_id, code, &at)
                        });
                    }
                    tracing::warn!(
                        session_id = %candidate.session_id,
                        error = %error,
                        "cold archive: session step failed"
                    );
                    last_error = Some(error);
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        break 'walk;
                    }
                }
            }
            if let Some(mover) = mover.as_deref_mut()
                && mover.full(&params.limits)
            {
                match mover.flush(access, read, params, &timer, &mut report)? {
                    FlushEnd::Done => {}
                    FlushEnd::Deferred => {
                        report.stop_early(params);
                        break 'walk;
                    }
                    FlushEnd::Refused => break 'walk,
                }
            }
            if !throttle.yield_if_full(&timer, params, &mut report.batches) {
                report.stop_early(params);
                break 'walk;
            }
        }
        match page.next_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES
        && let Some(error) = last_error
    {
        return Err(ColdError::new(
            error.code,
            format!(
                "{MAX_CONSECUTIVE_FAILURES} sessions in a row failed; last: {}",
                error.detail
            ),
        ));
    }
    // The proofs still waiting for a batch get their turn like any other batch:
    // admitted by the gate, the budget and a qualifying backup, else they stay
    // `staged` for the next pass (a closed gate or a spent budget does not end
    // with a last unthrottled delete).
    if let Some(mover) = mover.as_deref_mut() {
        let ended = mover.flush(access, read, params, &timer, &mut report)?;
        if matches!(ended, FlushEnd::Deferred) {
            report.stop_early(params);
        }
        settle_refusal(mover.refusal.take(), &mut report)?;
    }
    throttle.finish(&mut report.batches);
    Ok(report)
}

/// A pass that stopped deleting for want of a qualifying backup: with nothing
/// moved yet the refusal is the pass's error, as it always was (nothing was
/// deleted, and the caller records the failed run); with sessions already moved
/// the pass keeps their accounting and carries the refusal in its report.
fn settle_refusal(refusal: Option<ColdError>, report: &mut StageReport) -> ColdResult<()> {
    let Some(error) = refusal else {
        return Ok(());
    };
    if report.moved.sessions_moved == 0 {
        return Err(error);
    }
    report.move_refused = Some(MoveRefusal {
        code: error.code.to_string(),
        detail: error.detail,
    });
    Ok(())
}

/// Re-reads staged and archived chunks and checks each against its manifest:
/// the least recently verified first, within the wall budget. Records the
/// result on every session and file it touches and counts orphan chunks.
#[allow(clippy::too_many_lines)]
pub fn run_verify_pass<A: ColdStoreAccess>(
    access: &A,
    params: &PassParams<'_>,
) -> ColdResult<VerifyReport> {
    let timer = PassTimer::new(&no_skew);
    access.with_store(|store| store.cold_fail_stale_runs(params.run_id).map(|_| ()))?;
    let mut report = VerifyReport::default();
    // The verify pass reads the same files the stage pass writes, so it waits on
    // the same host-load gate before it starts and between batches (#2437).
    if !wait_for_gate(&timer, params) {
        report.cancelled = params.cancelled();
        report.budget_exhausted = true;
        return Ok(report);
    }
    let rows = access.with_store(|store| store.cold_sessions_for_verify(1_000_000))?;
    let files = access.with_store(|store| store.cold_archive_files())?;
    let (events_columns, provenance_columns) = access.with_store(|store| {
        Ok((
            columns_json(&table_columns(&store.conn, EVENTS_TABLE)?)?,
            columns_json(&table_columns(&store.conn, PROVENANCE_TABLE)?)?,
        ))
    })?;
    let mut throttle = BatchThrottle::default();
    let mut readers: HashMap<i64, Option<Connection>> = HashMap::new();
    let mut file_errors: HashMap<i64, Vec<String>> = HashMap::new();
    let mut touched: Vec<i64> = Vec::new();

    for row in &rows {
        if params.cancelled() {
            report.cancelled = true;
            report.budget_exhausted = true;
            break;
        }
        if timer.elapsed() >= params.limits.wall_budget {
            report.budget_exhausted = true;
            break;
        }
        let Some(file) = files.iter().find(|file| file.file_id == row.file_id) else {
            continue;
        };
        if !touched.contains(&file.file_id) {
            touched.push(file.file_id);
        }
        if !readers.contains_key(&file.file_id) {
            let refusal = recorded_path_refusal(Path::new(&file.path), params.settings);
            let opened = if refusal.is_some() {
                None
            } else if Path::new(&file.path).is_file() {
                open_archive_readonly(Path::new(&file.path)).ok()
            } else {
                None
            };
            if opened.is_none() {
                report.files_missing += 1;
                file_errors.entry(file.file_id).or_default().push(
                    refusal
                        .map_or(
                            "cold_archive_file_missing",
                            |_| "cold_archive_dir_forbidden",
                        )
                        .into(),
                );
            }
            readers.insert(file.file_id, opened);
        }
        let session_id = row.session_id.as_str();
        report.sessions_checked += 1;
        let result = match readers.get(&file.file_id).and_then(Option::as_ref) {
            None => Err(ColdError::new(
                "cold_archive_file_missing",
                format!("{} cannot be opened", file.path),
            )),
            // A staged session's rows are still live, so its chunks must carry
            // today's columns (a pass that finds otherwise re-stages it).
            Some(ro) => verify_indexed_session(
                ro,
                row,
                (row.state == "staged").then_some(LiveColumns {
                    events: &events_columns,
                    provenance: &provenance_columns,
                }),
            ),
        };
        let checked_raw = result
            .as_ref()
            .map_or(0, |(raw, _)| u64::try_from(*raw).unwrap_or(0));
        let at = now_string();
        match result {
            Ok((raw, compressed)) => {
                report.sessions_ok += 1;
                report.raw_bytes += u64::try_from(raw).unwrap_or(0);
                report.compressed_bytes += u64::try_from(compressed).unwrap_or(0);
                access.with_store(|store| store.cold_record_verified(session_id, &at))?;
            }
            Err(error) => {
                report.sessions_failed += 1;
                if report.errors.len() < MAX_LISTED_ERRORS {
                    report.errors.push(SessionError {
                        session_id: session_id.to_string(),
                        code: error.code.to_string(),
                    });
                }
                file_errors
                    .entry(file.file_id)
                    .or_default()
                    .push(error.code.to_string());
                let code = error.code;
                access
                    .with_store(|store| store.cold_record_session_error(session_id, code, &at))?;
            }
        }
        throttle.note(checked_raw);
        if !throttle.yield_if_full(&timer, params, &mut report.batches) {
            report.cancelled = params.cancelled();
            report.budget_exhausted = true;
            break;
        }
    }
    throttle.finish(&mut report.batches);

    for file_id in touched {
        let Some(file) = files.iter().find(|file| file.file_id == file_id) else {
            continue;
        };
        report.files_checked += 1;
        let at = now_string();
        let errors = file_errors.remove(&file_id).unwrap_or_default();
        let summary = (!errors.is_empty()).then(|| {
            let mut distinct: Vec<String> = errors;
            distinct.sort();
            distinct.dedup();
            distinct.join(", ")
        });
        if let Some(Some(ro)) = readers.get(&file_id) {
            if let Ok(chunks) = list_chunk_ids(ro) {
                let referenced: HashSet<i64> = access
                    .with_store(|store| store.cold_referenced_chunk_ids(file_id))
                    .unwrap_or_default();
                for (chunk_id, bytes) in &chunks {
                    if !referenced.contains(chunk_id) {
                        report.orphan_chunks += 1;
                        report.orphan_bytes += u64::try_from(*bytes).unwrap_or(0);
                    }
                }
                let size = std::fs::metadata(&file.path)
                    .map(|meta| i64::try_from(meta.len()).unwrap_or(i64::MAX))
                    .unwrap_or(file.bytes);
                let count = i64::try_from(chunks.len()).unwrap_or(i64::MAX);
                access
                    .with_store(|store| store.cold_update_file_size(file_id, size, count, &at))?;
            }
        }
        access
            .with_store(|store| store.cold_mark_file_verified(file_id, &at, summary.as_deref()))?;
    }
    Ok(report)
}

/// Why a recorded archive file may not be read: its directory now resolves
/// into a forbidden root (the files are only ever written outside them, so this
/// means a link or a setting moved under the index, #2435).
fn recorded_path_refusal(path: &Path, settings: &ColdSettings) -> Option<String> {
    let parent = path.parent()?;
    check_archive_dir(parent, &settings.forbidden_roots).err()
}

/// The `columns_json` the live tables have now: what the chunks of a session
/// whose rows are still live must record.
#[derive(Clone, Copy)]
struct LiveColumns<'a> {
    events: &'a str,
    provenance: &'a str,
}

/// Verifies the chunks an index row names, from the row's own manifest. With
/// `columns`, each chunk must also record exactly the live tables' ordered
/// `(name, type)` lists (`cold_archive_verify_columns` otherwise); `None` is for
/// a row whose live rows are already gone, where the archive is the only copy
/// and a column list that differs from today's is a read concern, not a proof.
fn verify_indexed_session(
    ro: &Connection,
    row: &ColdSessionRow,
    columns: Option<LiveColumns<'_>>,
) -> ColdResult<(i64, i64)> {
    let session_id = row.session_id.as_str();
    let (raw, compressed) = verify_chunk(
        ro,
        &ChunkExpectation {
            chunk_id: row.events_chunk_id,
            session_id,
            generation: row.generation,
            table: EVENTS_TABLE,
            roles: EVENT_ROLES,
            columns_json: columns.map(|live| live.events),
            sha256: &row.events_sha256,
            row_count: row.archived_event_count,
            content_len: row.archived_content_len,
            min_id: None,
            max_id: None,
            min_sequence: row.archived_min_sequence,
            max_sequence: row.archived_max_sequence,
        },
    )?;
    match (row.provenance_chunk_id, row.provenance_sha256.as_deref()) {
        (Some(chunk_id), Some(sha256)) => {
            let (more_raw, more_compressed) = verify_chunk(
                ro,
                &ChunkExpectation {
                    chunk_id,
                    session_id,
                    generation: row.generation,
                    table: PROVENANCE_TABLE,
                    roles: PROVENANCE_ROLES,
                    columns_json: columns.map(|live| live.provenance),
                    sha256,
                    row_count: row.archived_provenance_count,
                    content_len: 0,
                    min_id: None,
                    max_id: None,
                    min_sequence: None,
                    max_sequence: None,
                },
            )?;
            Ok((raw + more_raw, compressed + more_compressed))
        }
        (None, None) if row.archived_provenance_count == 0 => Ok((raw, compressed)),
        _ => Err(ColdError::new(
            "cold_archive_verify_identity",
            "the index row's provenance chunk, digest and count disagree",
        )),
    }
}
