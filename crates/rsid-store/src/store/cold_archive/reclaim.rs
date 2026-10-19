//! Space reclaim after a move (#2384, slice S4; plan §5).
//!
//! * **Incremental steps.** After a move pass, `PRAGMA incremental_vacuum(4096)`
//!   on the main connection, one step per store-mutex hold, until the freelist
//!   is at most 1% of the file or [`INCREMENTAL_BUDGET`] passes. It needs no
//!   temporary database and only works once the file is in incremental
//!   auto-vacuum mode.
//! * **One-time compaction** (`auto_vacuum=INCREMENTAL; VACUUM;
//!   wal_checkpoint(TRUNCATE)`) switches the file to that mode and shrinks it.
//!   It never runs in the live daemon: `VACUUM` needs a temporary database whose
//!   directory is process-global `SQLite` state, which a multithreaded daemon
//!   cannot change safely (review #2398). The deploy loop records a
//!   `cold_archive_runs` row (`mode='compact'`, `running`) at a supervised
//!   restart's quiet point; after the graceful shutdown the exiting daemon runs
//!   the isolated helper process (`rsid cold-archive-compact`, which calls
//!   [`run_compaction`]) with its temporary directory in the environment and a
//!   hard deadline, then exits 75 whatever happened. The next daemon settles the
//!   row from the helper's result file ([`Store::cold_reconcile_compactions`]),
//!   after `request_ready`.

use super::{ColdError, ColdResult, ColdStoreAccess};
use crate::store::{LATEST_SCHEMA_VERSION, Store};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Pages freed per incremental step (16 MiB at 4 KiB pages).
pub const INCREMENTAL_STEP_PAGES: i64 = 4096;
/// Wall-clock budget of the incremental reclaim after one pass.
pub const INCREMENTAL_BUDGET: Duration = Duration::from_secs(30);
/// A compaction is due when the file is not in incremental mode and at least
/// this much (or 20% of the file) sits on the freelist.
pub const COMPACTION_MIN_FREE_BYTES: u64 = 512 * 1024 * 1024;
/// How long the exiting daemon waits for the compaction helper.
pub const COMPACT_DEADLINE: Duration = Duration::from_secs(180);
/// Free space the helper needs, as a multiple of the database's used bytes
/// (the rebuilt copy plus the WAL growth).
pub const COMPACT_FREE_SPACE_FACTOR: f64 = 2.5;
/// The hidden argv mode of the compaction helper.
pub const COMPACT_HELPER_ARG: &str = "cold-archive-compact";
/// The `error_code` of a compaction an operator asked for, waiting for the next
/// supervised restart (its row is `skipped` with no `finished_at` until then).
pub const COMPACT_SCHEDULED_CODE: &str = "compact_scheduled";
/// A compaction whose helper left no result (killed, crashed, or the daemon
/// died first). The database is intact: `VACUUM` is atomic.
pub const COMPACT_INTERRUPTED_CODE: &str = "compact_interrupted";
/// The `outcome` of a result file the exiting daemon wrote instead of starting
/// the helper, because a cold-archive pass thread had not settled within the
/// shutdown wait (#2384 review, #205). Not a failure: reconcile puts the request
/// back for the next supervised restart.
pub const COMPACT_DEFERRED_OUTCOME: &str = "deferred";

/// What the incremental reclaim did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReclaimReport {
    /// False when the file is not in incremental auto-vacuum mode (it needs the
    /// one-time compaction first).
    pub incremental: bool,
    pub steps: u64,
    pub pages_reclaimed: i64,
    pub freelist_after: i64,
}

fn pragma_i64(conn: &Connection, sql: &str) -> ColdResult<i64> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

/// Whether the one-time compaction is due for these page counts.
#[must_use]
pub fn compaction_due(
    auto_vacuum: i64,
    freelist_count: i64,
    page_count: i64,
    page_size: i64,
) -> bool {
    let page_bytes = u64::try_from(page_size).unwrap_or(0);
    let free_bytes = u64::try_from(freelist_count)
        .unwrap_or(0)
        .saturating_mul(page_bytes);
    let file_bytes = u64::try_from(page_count)
        .unwrap_or(0)
        .saturating_mul(page_bytes);
    auto_vacuum != 2 && free_bytes >= COMPACTION_MIN_FREE_BYTES.max(file_bytes / 5)
}

/// Returns freed pages to the file system in small steps, yielding the store
/// between steps, until the freelist is at most 1% of the file or `budget`
/// passes. Does nothing unless the file is in incremental auto-vacuum mode.
///
/// # Errors
/// A persistence error.
pub fn reclaim_incremental<A: ColdStoreAccess>(
    access: &A,
    budget: Duration,
) -> ColdResult<ReclaimReport> {
    reclaim_incremental_until(access, budget, &|| false)
}

/// [`reclaim_incremental`] that also stops, between two steps, when `cancelled`
/// says so (a supervised restart is draining and the database must go quiet).
///
/// # Errors
/// A persistence error.
pub fn reclaim_incremental_until<A: ColdStoreAccess>(
    access: &A,
    budget: Duration,
    cancelled: &dyn Fn() -> bool,
) -> ColdResult<ReclaimReport> {
    let started = Instant::now();
    let mut report = ReclaimReport::default();
    loop {
        let (auto_vacuum, freelist, pages) = access.with_store(|store| {
            Ok((
                pragma_i64(&store.conn, "PRAGMA auto_vacuum")?,
                pragma_i64(&store.conn, "PRAGMA freelist_count")?,
                pragma_i64(&store.conn, "PRAGMA page_count")?,
            ))
        })?;
        report.freelist_after = freelist;
        if auto_vacuum != 2 {
            return Ok(report);
        }
        report.incremental = true;
        if freelist == 0
            || freelist.saturating_mul(100) <= pages
            || started.elapsed() >= budget
            || cancelled()
        {
            return Ok(report);
        }
        let freed = access.with_store(|store| {
            store
                .conn
                .execute_batch("PRAGMA incremental_vacuum(4096);")?;
            let after = pragma_i64(&store.conn, "PRAGMA freelist_count")?;
            Ok(freelist - after)
        })?;
        report.steps += 1;
        report.pages_reclaimed += freed.max(0);
        if freed <= 0 {
            // Nothing moved (another writer holds the pages): stop rather than spin.
            return Ok(report);
        }
    }
}

/// What the helper writes to its result file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactResult {
    pub run_id: String,
    /// `completed` or `failed`.
    pub outcome: String,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub compact_ms: i64,
    pub bytes_before: i64,
    pub bytes_after: i64,
    pub auto_vacuum: i64,
}

/// Where the helper of `run_id` keeps its temporary database and its result:
/// `<data_dir>/tmp/compact-<run_id>` and `<data_dir>/tmp/compact-<run_id>.json`.
/// Both are on the database's own file system.
#[must_use]
pub fn compact_paths(database_path: &Path, run_id: Uuid) -> (PathBuf, PathBuf) {
    let tmp = database_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("tmp");
    (
        tmp.join(format!("compact-{run_id}")),
        tmp.join(format!("compact-{run_id}.json")),
    )
}

fn file_len(path: &Path) -> i64 {
    std::fs::metadata(path)
        .map(|meta| i64::try_from(meta.len()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn database_bytes(path: &Path) -> i64 {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    file_len(path) + file_len(Path::new(&wal))
}

/// Free bytes on the file system holding `path`; `None` where it cannot be read
/// (the check is then skipped and `VACUUM` reports a full disk itself).
fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let existing = path.ancestors().find(|candidate| candidate.exists())?;
        let stats = nix::sys::statvfs::statvfs(existing).ok()?;
        Some(u64::from(stats.blocks_available()).saturating_mul(u64::from(stats.fragment_size())))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// The compaction itself, as the isolated helper process runs it (never call
/// it from the daemon). Opens the database with a raw connection (no
/// migration, no startup work), refuses a schema newer than this build knows,
/// checks free space, then switches the file to incremental auto-vacuum and
/// rebuilds it. `VACUUM` is atomic: a failure or a kill leaves the old file.
/// The caller's environment decides where `SQLite` puts the temporary database.
#[must_use]
pub fn run_compaction(database_path: &Path, run_id: Uuid) -> CompactResult {
    let started = Instant::now();
    let bytes_before = database_bytes(database_path);
    let mut result = CompactResult {
        run_id: run_id.to_string(),
        outcome: "failed".into(),
        error_code: None,
        error_detail: None,
        compact_ms: 0,
        bytes_before,
        bytes_after: bytes_before,
        auto_vacuum: -1,
    };
    let outcome = (|| -> ColdResult<i64> {
        let conn = Connection::open(database_path)?;
        Store::register_sql_functions(&conn)
            .map_err(|error| ColdError::new("compact_open", error.to_string()))?;
        let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > LATEST_SCHEMA_VERSION {
            return Err(ColdError::new(
                "compact_schema_newer",
                format!("schema {version} is newer than this build's {LATEST_SCHEMA_VERSION}"),
            ));
        }
        let page_size = pragma_i64(&conn, "PRAGMA page_size")?;
        let used = (pragma_i64(&conn, "PRAGMA page_count")?
            - pragma_i64(&conn, "PRAGMA freelist_count")?)
        .saturating_mul(page_size);
        if let Some(free) = free_bytes(database_path) {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let needed = (used.max(0) as f64 * COMPACT_FREE_SPACE_FACTOR) as u64;
            if free < needed {
                return Err(ColdError::new(
                    "compact_disk_space",
                    format!("{free} bytes free, {needed} needed"),
                ));
            }
        }
        conn.busy_timeout(Duration::from_secs(30))?;
        conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL; VACUUM;")?;
        // The checkpoint copies the rebuilt pages back and truncates the WAL.
        let _: (i64, i64, i64) = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(pragma_i64(&conn, "PRAGMA auto_vacuum")?)
    })();
    result.compact_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    result.bytes_after = database_bytes(database_path);
    match outcome {
        Ok(auto_vacuum) => {
            result.outcome = "completed".into();
            result.auto_vacuum = auto_vacuum;
        }
        Err(error) => {
            result.error_code = Some(error.code.to_string());
            result.error_detail = Some(error.detail);
        }
    }
    result
}

/// Writes the helper's result atomically (temporary file, fsync, rename).
///
/// # Errors
/// An I/O error.
pub fn write_compact_result(path: &Path, result: &CompactResult) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec(result)?)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    super::writer::sync_dir(dir)
}

/// How a compaction row was settled by [`Store::cold_reconcile_compactions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionSettled {
    pub run_id: String,
    pub outcome: String,
    pub error_code: Option<String>,
}

/// The newest compaction for the status read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LastCompaction {
    pub pending: bool,
    pub last_at: Option<String>,
    pub last_outcome: Option<String>,
    pub last_ms: Option<i64>,
    pub bytes_before: Option<i64>,
    pub bytes_after: Option<i64>,
}

fn now_string() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

impl Store {
    /// The live file's compaction state: `(due, auto_vacuum)`.
    ///
    /// # Errors
    /// A persistence error.
    pub fn cold_compaction_due(&self) -> ColdResult<bool> {
        Ok(compaction_due(
            pragma_i64(&self.conn, "PRAGMA auto_vacuum")?,
            pragma_i64(&self.conn, "PRAGMA freelist_count")?,
            pragma_i64(&self.conn, "PRAGMA page_count")?,
            pragma_i64(&self.conn, "PRAGMA page_size")?,
        ))
    }

    /// `RunColdArchiveNow {mode: "compact"}`: records the operator's request.
    /// Idempotent: a request already waiting is returned.
    ///
    /// # Errors
    /// A persistence error.
    pub fn cold_request_compaction(&self) -> ColdResult<Uuid> {
        if let Some(run_id) = self.cold_pending_compaction()? {
            return Ok(run_id);
        }
        let run_id = Uuid::new_v4();
        self.conn.execute(
            "INSERT INTO cold_archive_runs(run_id, trigger, mode, started_at, outcome, error_code,
                                           error_detail, live_bytes_before)
             VALUES (?1, 'run_now', 'compact', ?2, 'skipped', ?3,
                     'runs at the next supervised restart', ?4)",
            params![
                run_id.to_string(),
                now_string(),
                COMPACT_SCHEDULED_CODE,
                self.cold_live_db_bytes().ok()
            ],
        )?;
        Ok(run_id)
    }

    /// The operator's compaction request still waiting for a restart.
    ///
    /// # Errors
    /// A persistence error.
    pub fn cold_pending_compaction(&self) -> ColdResult<Option<Uuid>> {
        let run_id: Option<String> = self
            .conn
            .query_row(
                "SELECT run_id FROM cold_archive_runs
                 WHERE mode = 'compact' AND outcome = 'skipped' AND finished_at IS NULL
                   AND error_code = ?1
                 ORDER BY started_at DESC LIMIT 1",
                [COMPACT_SCHEDULED_CODE],
                |row| row.get(0),
            )
            .optional()?;
        Ok(run_id.and_then(|text| Uuid::parse_str(&text).ok()))
    }

    /// The decision at a supervised restart's quiet point: the run id the
    /// helper will carry when an operator asked for a compaction, or when
    /// `auto` is on and one is due; `None` otherwise. The row becomes
    /// `running` (it is settled from the helper's result by the next daemon).
    ///
    /// # Errors
    /// A persistence error.
    pub fn cold_claim_compaction(&self, auto: bool) -> ColdResult<Option<Uuid>> {
        let now = now_string();
        if let Some(run_id) = self.cold_pending_compaction()? {
            self.conn.execute(
                "UPDATE cold_archive_runs
                 SET outcome = 'running', started_at = ?2, error_code = NULL, error_detail = NULL,
                     live_bytes_before = ?3
                 WHERE run_id = ?1",
                params![run_id.to_string(), now, self.cold_live_db_bytes().ok()],
            )?;
            return Ok(Some(run_id));
        }
        if !auto || !self.cold_compaction_due()? {
            return Ok(None);
        }
        let run_id = Uuid::new_v4();
        self.conn.execute(
            "INSERT INTO cold_archive_runs(run_id, trigger, mode, started_at, outcome, live_bytes_before)
             VALUES (?1, 'scheduled', 'compact', ?2, 'running', ?3)",
            params![run_id.to_string(), now, self.cold_live_db_bytes().ok()],
        )?;
        Ok(Some(run_id))
    }

    /// The restart did not happen after all (the deploy failed after the quiet
    /// point): an operator's request waits for the next restart again, a
    /// scheduled one is dropped (it is decided again next time).
    ///
    /// # Errors
    /// A persistence error.
    pub fn cold_abandon_compaction(&self, run_id: Uuid) -> ColdResult<()> {
        self.conn.execute(
            "UPDATE cold_archive_runs
             SET outcome = 'skipped',
                 finished_at = CASE trigger WHEN 'run_now' THEN NULL ELSE ?2 END,
                 error_code = CASE trigger WHEN 'run_now' THEN ?3 ELSE 'compact_restart_abandoned' END,
                 error_detail = 'the supervised restart did not happen'
             WHERE run_id = ?1 AND mode = 'compact' AND outcome = 'running'",
            params![run_id.to_string(), now_string(), COMPACT_SCHEDULED_CODE],
        )?;
        Ok(())
    }

    /// Settles every `running` compaction row except `except` (the one this
    /// process scheduled for its own exit) from its helper's result file next to
    /// `database_path`: `completed`/`failed` from the file, `failed` with
    /// [`COMPACT_INTERRUPTED_CODE`] when there is none. Removes the result file.
    /// Runs in the cold-archive loop, after `request_ready`, never on startup.
    ///
    /// # Errors
    /// A persistence error.
    pub fn cold_reconcile_compactions(
        &self,
        database_path: &Path,
        except: Option<Uuid>,
    ) -> ColdResult<Vec<CompactionSettled>> {
        let running: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT run_id FROM cold_archive_runs WHERE mode = 'compact' AND outcome = 'running'",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut settled = Vec::new();
        for run_id in running {
            let Ok(uuid) = Uuid::parse_str(&run_id) else {
                continue;
            };
            if Some(uuid) == except {
                continue;
            }
            let (tmp_dir, result_path) = compact_paths(database_path, uuid);
            let result: Option<CompactResult> = std::fs::read(&result_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .filter(|result: &CompactResult| result.run_id == run_id);
            if result
                .as_ref()
                .is_some_and(|result| result.outcome == COMPACT_DEFERRED_OUTCOME)
            {
                settled.push(self.cold_settle_deferred(
                    uuid,
                    result.as_ref(),
                    &result_path,
                    &tmp_dir,
                )?);
                continue;
            }
            let (outcome, error_code, error_detail, compact_ms, after) = match &result {
                Some(result) if result.outcome == "completed" => (
                    "completed",
                    None,
                    None,
                    Some(result.compact_ms),
                    Some(result.bytes_after),
                ),
                Some(result) => (
                    "failed",
                    Some(
                        result
                            .error_code
                            .clone()
                            .unwrap_or_else(|| "compact_failed".into()),
                    ),
                    result.error_detail.clone(),
                    Some(result.compact_ms),
                    Some(result.bytes_after),
                ),
                None => (
                    "failed",
                    Some(COMPACT_INTERRUPTED_CODE.to_string()),
                    Some(
                        "the compaction helper left no result; the database is intact".to_string(),
                    ),
                    None,
                    None,
                ),
            };
            self.conn.execute(
                "UPDATE cold_archive_runs
                 SET outcome = ?2, finished_at = ?3, error_code = ?4, error_detail = ?5,
                     compact_ms = ?6, live_bytes_after = COALESCE(?7, live_bytes_after),
                     live_bytes_before = COALESCE(?8, live_bytes_before)
                 WHERE run_id = ?1 AND outcome = 'running'",
                params![
                    run_id,
                    outcome,
                    now_string(),
                    error_code,
                    error_detail,
                    compact_ms,
                    after,
                    result.as_ref().map(|result| result.bytes_before),
                ],
            )?;
            let _ = std::fs::remove_file(&result_path);
            let _ = std::fs::remove_dir_all(&tmp_dir);
            settled.push(CompactionSettled {
                run_id,
                outcome: outcome.to_string(),
                error_code,
            });
        }
        Ok(settled)
    }

    /// A compaction the exiting daemon deferred (it wrote a `deferred` result
    /// instead of starting the helper): the database was not touched, so the
    /// claim goes back like an abandoned restart's, an operator's request waits
    /// for the next restart, and the reason stays on the run row.
    fn cold_settle_deferred(
        &self,
        run_id: Uuid,
        result: Option<&CompactResult>,
        result_path: &Path,
        tmp_dir: &Path,
    ) -> ColdResult<CompactionSettled> {
        self.cold_abandon_compaction(run_id)?;
        let detail = result.and_then(|result| result.error_detail.clone());
        // Best effort: the detail only explains the put-back to the operator.
        let _ = self.conn.execute(
            "UPDATE cold_archive_runs SET error_detail = COALESCE(?2, error_detail)
             WHERE run_id = ?1 AND mode = 'compact' AND outcome = 'skipped'",
            params![run_id.to_string(), detail],
        );
        let _ = std::fs::remove_file(result_path);
        let _ = std::fs::remove_dir_all(tmp_dir);
        Ok(CompactionSettled {
            run_id: run_id.to_string(),
            outcome: COMPACT_DEFERRED_OUTCOME.to_string(),
            error_code: None,
        })
    }

    /// The pending request and the newest settled compaction.
    ///
    /// # Errors
    /// A persistence error.
    pub fn cold_last_compaction(&self) -> ColdResult<LastCompaction> {
        let mut last = LastCompaction {
            pending: self.cold_pending_compaction()?.is_some(),
            ..LastCompaction::default()
        };
        let row: Option<(
            Option<String>,
            String,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        )> = self
            .conn
            .query_row(
                "SELECT finished_at, outcome, compact_ms, live_bytes_before, live_bytes_after
                 FROM cold_archive_runs
                 WHERE mode = 'compact' AND outcome IN ('completed', 'failed')
                 ORDER BY finished_at DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        if let Some((finished_at, outcome, ms, before, after)) = row {
            last.last_at = finished_at;
            last.last_outcome = Some(outcome);
            last.last_ms = ms;
            last.bytes_before = before;
            last.bytes_after = after;
        }
        Ok(last)
    }
}
