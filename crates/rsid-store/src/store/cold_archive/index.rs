//! The live-database side of the cold archive: the index tables of migration
//! V176 (`cold_archive_files`, `cold_archive_sessions`, `cold_archive_runs`) and
//! the small, short transactions that move a session through its states.
//!
//! The index is authoritative for what is archived where; each archive file is
//! also self-describing (`archive_meta`, `chunks`, `session_snapshots`).

use super::writer::WrittenChunks;
use super::{ColdError, ColdMode, ColdResult};
use crate::store::Store;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use std::collections::HashSet;
use uuid::Uuid;

/// A row of `cold_archive_sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ColdSessionRow {
    pub session_id: String,
    pub state: String,
    pub generation: i64,
    pub file_id: i64,
    pub events_chunk_id: i64,
    pub provenance_chunk_id: Option<i64>,
    pub pre_event_count: i64,
    pub pre_provenance_count: i64,
    pub archived_event_count: i64,
    pub archived_provenance_count: i64,
    pub archived_content_len: i64,
    pub archived_min_sequence: Option<i64>,
    pub archived_max_sequence: Option<i64>,
    pub events_sha256: String,
    pub provenance_sha256: Option<String>,
    pub raw_bytes: i64,
    pub compressed_bytes: i64,
    pub staged_at: String,
    pub archived_at: Option<String>,
    pub restored_at: Option<String>,
    pub updated_at: String,
    pub last_error_code: Option<String>,
    pub verified_at: Option<String>,
}

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ColdSessionRow> {
    Ok(ColdSessionRow {
        session_id: row.get(0)?,
        state: row.get(1)?,
        generation: row.get(2)?,
        file_id: row.get(3)?,
        events_chunk_id: row.get(4)?,
        provenance_chunk_id: row.get(5)?,
        pre_event_count: row.get(6)?,
        pre_provenance_count: row.get(7)?,
        archived_event_count: row.get(8)?,
        archived_provenance_count: row.get(9)?,
        archived_content_len: row.get(10)?,
        archived_min_sequence: row.get(11)?,
        archived_max_sequence: row.get(12)?,
        events_sha256: row.get(13)?,
        provenance_sha256: row.get(14)?,
        raw_bytes: row.get(15)?,
        compressed_bytes: row.get(16)?,
        staged_at: row.get(17)?,
        archived_at: row.get(18)?,
        restored_at: row.get(19)?,
        updated_at: row.get(20)?,
        last_error_code: row.get(21)?,
        verified_at: row.get(22)?,
    })
}

/// A row of `cold_archive_files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ColdFileRow {
    pub file_id: i64,
    pub path: String,
    pub month: String,
    pub format_version: i64,
    pub bytes: i64,
    pub chunk_count: i64,
    pub last_verified_at: Option<String>,
    pub last_verify_error: Option<String>,
}

/// A row of `cold_archive_runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ColdRunRow {
    pub run_id: String,
    pub trigger: String,
    pub mode: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub outcome: String,
    pub sessions_staged: i64,
    pub sessions_moved: i64,
    pub events_moved: i64,
    pub provenance_moved: i64,
    pub raw_bytes: i64,
    pub compressed_bytes: i64,
    pub holds: serde_json::Value,
    pub live_bytes_before: Option<i64>,
    pub live_bytes_after: Option<i64>,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
}

/// Everything P2 records about a session before its chunk is written.
#[derive(Debug, Clone)]
pub struct StageIntent<'a> {
    pub session_id: &'a str,
    pub generation: i64,
    pub file_id: i64,
    pub pre_event_count: i64,
    pub pre_provenance_count: i64,
    pub archived_event_count: i64,
    pub archived_provenance_count: i64,
    pub archived_content_len: i64,
    pub archived_min_sequence: Option<i64>,
    pub archived_max_sequence: Option<i64>,
    pub events_sha256: &'a str,
    pub provenance_sha256: Option<&'a str>,
    pub raw_bytes: i64,
    pub now: &'a str,
}

/// How a run ended.
#[derive(Debug, Clone, Default)]
pub struct RunSummary {
    pub outcome: &'static str,
    pub sessions_staged: i64,
    pub sessions_moved: i64,
    pub events_moved: i64,
    pub provenance_moved: i64,
    /// Pages the incremental reclaim after a move returned to the file system.
    pub pages_reclaimed: i64,
    pub raw_bytes: i64,
    pub compressed_bytes: i64,
    pub holds_json: String,
    pub live_bytes_after: Option<i64>,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
}

fn now_string() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn parse_started(started: Option<String>) -> Option<chrono::DateTime<chrono::Utc>> {
    started
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(&text).ok())
        .map(|time| time.with_timezone(&chrono::Utc))
}

impl Store {
    /// The index row of a session, if it was ever staged.
    pub fn cold_session_row(&self, session_id: &str) -> ColdResult<Option<ColdSessionRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT session_id, state, generation, file_id, events_chunk_id,
                    provenance_chunk_id, pre_event_count, pre_provenance_count,
                    archived_event_count, archived_provenance_count, archived_content_len,
                    archived_min_sequence, archived_max_sequence, events_sha256,
                    provenance_sha256, raw_bytes, compressed_bytes, staged_at, archived_at,
                    restored_at, updated_at, last_error_code, verified_at
                 FROM cold_archive_sessions WHERE session_id = ?1",
                [session_id],
                map_session_row,
            )
            .optional()?)
    }

    /// Staged and archived sessions whose chunks a verify pass should re-read,
    /// the least recently verified first.
    pub fn cold_sessions_for_verify(&self, limit: usize) -> ColdResult<Vec<ColdSessionRow>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT session_id, state, generation, file_id, events_chunk_id,
                    provenance_chunk_id, pre_event_count, pre_provenance_count,
                    archived_event_count, archived_provenance_count, archived_content_len,
                    archived_min_sequence, archived_max_sequence, events_sha256,
                    provenance_sha256, raw_bytes, compressed_bytes, staged_at, archived_at,
                    restored_at, updated_at, last_error_code, verified_at
             FROM cold_archive_sessions
             WHERE state IN ('staged','cold') AND events_chunk_id > 0
             ORDER BY verified_at IS NOT NULL, verified_at, session_id LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], map_session_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every chunk id the index refers to in one archive file.
    pub fn cold_referenced_chunk_ids(&self, file_id: i64) -> ColdResult<HashSet<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT events_chunk_id, provenance_chunk_id FROM cold_archive_sessions
             WHERE file_id = ?1",
        )?;
        let rows = stmt.query_map([file_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        let mut ids = HashSet::new();
        for row in rows {
            let (events, provenance) = row?;
            ids.insert(events);
            ids.extend(provenance);
        }
        Ok(ids)
    }

    /// The `cold_archive_files` row for `path`, created when absent (idempotent).
    pub fn cold_register_archive_file(
        &self,
        path: &str,
        month: &str,
        format_version: i64,
    ) -> ColdResult<i64> {
        let now = now_string();
        self.conn.execute(
            "INSERT OR IGNORE INTO cold_archive_files(path, month, format_version, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![path, month, format_version, now],
        )?;
        Ok(self.conn.query_row(
            "SELECT file_id FROM cold_archive_files WHERE path = ?1",
            [path],
            |row| row.get(0),
        )?)
    }

    /// Every archive file with its recorded size and last verification.
    pub fn cold_archive_files(&self) -> ColdResult<Vec<ColdFileRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_id, path, month, format_version, bytes, chunk_count,
                    last_verified_at, last_verify_error
             FROM cold_archive_files ORDER BY month, file_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ColdFileRow {
                file_id: row.get(0)?,
                path: row.get(1)?,
                month: row.get(2)?,
                format_version: row.get(3)?,
                bytes: row.get(4)?,
                chunk_count: row.get(5)?,
                last_verified_at: row.get(6)?,
                last_verify_error: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// P2: records that `session_id` is staged at `generation`, with the counts
    /// and digests of the rows about to be written. Idempotent: a `staged` row
    /// is overwritten (its chunk ids reset until P4 records them), a `restored`
    /// row returns to `staged`. A session in any other state is left alone and
    /// reported as a conflict.
    pub fn cold_stage_session(&self, intent: &StageIntent<'_>) -> ColdResult<()> {
        let changed = self.conn.execute(
            "INSERT INTO cold_archive_sessions(
                session_id, state, generation, file_id, events_chunk_id, provenance_chunk_id,
                pre_event_count, pre_provenance_count, archived_event_count,
                archived_provenance_count, archived_content_len, archived_min_sequence,
                archived_max_sequence, events_sha256, provenance_sha256, raw_bytes,
                compressed_bytes, staged_at, updated_at)
             VALUES (?1, 'staged', ?2, ?3, 0, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     0, ?14, ?14)
             ON CONFLICT(session_id) DO UPDATE SET
                state = 'staged', generation = excluded.generation, file_id = excluded.file_id,
                events_chunk_id = 0, provenance_chunk_id = NULL,
                pre_event_count = excluded.pre_event_count,
                pre_provenance_count = excluded.pre_provenance_count,
                archived_event_count = excluded.archived_event_count,
                archived_provenance_count = excluded.archived_provenance_count,
                archived_content_len = excluded.archived_content_len,
                archived_min_sequence = excluded.archived_min_sequence,
                archived_max_sequence = excluded.archived_max_sequence,
                events_sha256 = excluded.events_sha256,
                provenance_sha256 = excluded.provenance_sha256,
                raw_bytes = excluded.raw_bytes, compressed_bytes = 0,
                archived_at = NULL, updated_at = excluded.updated_at,
                last_error_code = NULL, verified_at = NULL
             WHERE cold_archive_sessions.state IN ('staged', 'restored')",
            params![
                intent.session_id,
                intent.generation,
                intent.file_id,
                intent.pre_event_count,
                intent.pre_provenance_count,
                intent.archived_event_count,
                intent.archived_provenance_count,
                intent.archived_content_len,
                intent.archived_min_sequence,
                intent.archived_max_sequence,
                intent.events_sha256,
                intent.provenance_sha256,
                intent.raw_bytes,
                intent.now,
            ],
        )?;
        if changed == 0 {
            return Err(ColdError::new(
                "cold_archive_state_conflict",
                format!("{} is not in a stageable state", intent.session_id),
            ));
        }
        Ok(())
    }

    /// P4 done: the chunks are in the archive. Records their ids (and the
    /// generation, which moves when the writer found other data at the planned
    /// one).
    pub fn cold_record_written(
        &self,
        session_id: &str,
        written: &WrittenChunks,
        now: &str,
    ) -> ColdResult<()> {
        let changed = self.conn.execute(
            "UPDATE cold_archive_sessions
             SET generation = ?2, events_chunk_id = ?3, provenance_chunk_id = ?4,
                 compressed_bytes = ?5, updated_at = ?6, last_error_code = NULL
             WHERE session_id = ?1 AND state = 'staged'",
            params![
                session_id,
                written.generation,
                written.events_chunk_id,
                written.provenance_chunk_id,
                written.compressed_bytes,
                now,
            ],
        )?;
        if changed == 0 {
            return Err(ColdError::new(
                "cold_archive_state_conflict",
                format!("{session_id} left the staged state while its chunk was written"),
            ));
        }
        Ok(())
    }

    /// P5 done: the chunks read back equal to the manifest.
    pub fn cold_record_verified(&self, session_id: &str, now: &str) -> ColdResult<()> {
        self.conn.execute(
            "UPDATE cold_archive_sessions
             SET verified_at = ?2, updated_at = ?2, last_error_code = NULL
             WHERE session_id = ?1 AND state IN ('staged', 'cold')",
            params![session_id, now],
        )?;
        Ok(())
    }

    /// A step failed for this session: remember why, and require a fresh
    /// verification before it is trusted again.
    pub fn cold_record_session_error(
        &self,
        session_id: &str,
        code: &str,
        now: &str,
    ) -> ColdResult<()> {
        self.conn.execute(
            "UPDATE cold_archive_sessions
             SET last_error_code = ?2, verified_at = NULL, updated_at = ?3
             WHERE session_id = ?1 AND state IN ('staged', 'restored')",
            params![session_id, code, now],
        )?;
        Ok(())
    }

    /// Records the size of an archive file after a write.
    pub fn cold_update_file_size(
        &self,
        file_id: i64,
        bytes: i64,
        chunk_count: i64,
        now: &str,
    ) -> ColdResult<()> {
        self.conn.execute(
            "UPDATE cold_archive_files SET bytes = ?2, chunk_count = ?3, updated_at = ?4
             WHERE file_id = ?1",
            params![file_id, bytes, chunk_count, now],
        )?;
        Ok(())
    }

    /// Records the outcome of verifying an archive file: `error` is `None` when
    /// every chunk checked read back equal.
    pub fn cold_mark_file_verified(
        &self,
        file_id: i64,
        now: &str,
        error: Option<&str>,
    ) -> ColdResult<()> {
        self.conn.execute(
            "UPDATE cold_archive_files
             SET last_verified_at = ?2, last_verify_error = ?3, updated_at = ?2
             WHERE file_id = ?1",
            params![file_id, now, error],
        )?;
        Ok(())
    }

    /// Opens a run row (`running`) and returns its id.
    pub fn cold_begin_run(&self, trigger: &str, mode: ColdMode) -> ColdResult<Uuid> {
        let run_id = Uuid::new_v4();
        let live_bytes = self.cold_live_db_bytes().ok();
        self.conn.execute(
            "INSERT INTO cold_archive_runs(run_id, trigger, mode, started_at, outcome, live_bytes_before)
             VALUES (?1, ?2, ?3, ?4, 'running', ?5)",
            params![run_id.to_string(), trigger, mode.as_str(), now_string(), live_bytes],
        )?;
        Ok(run_id)
    }

    /// Settles a run row.
    pub fn cold_finish_run(&self, run_id: Uuid, summary: &RunSummary) -> ColdResult<()> {
        self.conn.execute(
            "UPDATE cold_archive_runs
             SET finished_at = ?2, outcome = ?3, sessions_staged = ?4, raw_bytes = ?5,
                 compressed_bytes = ?6, holds_json = ?7, live_bytes_after = ?8,
                 error_code = ?9, error_detail = ?10, sessions_moved = ?11,
                 events_moved = ?12, provenance_moved = ?13, pages_reclaimed = ?14
             WHERE run_id = ?1",
            params![
                run_id.to_string(),
                now_string(),
                summary.outcome,
                summary.sessions_staged,
                summary.raw_bytes,
                summary.compressed_bytes,
                summary.holds_json,
                summary.live_bytes_after,
                summary.error_code,
                summary.error_detail,
                summary.sessions_moved,
                summary.events_moved,
                summary.provenance_moved,
                summary.pages_reclaimed,
            ],
        )?;
        Ok(())
    }

    /// A run row left `running` by a previous daemon incarnation is settled as
    /// interrupted; `except` is the run that is starting now. A compaction row
    /// is not a pass: it is settled from its helper's result file
    /// (`Store::cold_reconcile_compactions`).
    pub fn cold_fail_stale_runs(&self, except: Uuid) -> ColdResult<usize> {
        Ok(self.conn.execute(
            "UPDATE cold_archive_runs
             SET outcome = 'failed', finished_at = ?1, error_code = 'cold_archive_interrupted',
                 error_detail = 'the daemon stopped before the run finished'
             WHERE outcome = 'running' AND run_id <> ?2 AND mode <> 'compact'",
            params![now_string(), except.to_string()],
        )?)
    }

    /// When the newest stage or move run started (any trigger; a run that
    /// failed or was interrupted counts, it did start). The scheduled loop reads
    /// it so its interval survives a daemon restart (#2436). `None` when no such
    /// run exists or its start time does not parse. "Newest" is the last row
    /// inserted (`rowid`, durable execution order), not the greatest
    /// `started_at`: a row stamped by a clock that later moved back must not
    /// outrank the passes that started after the correction (#2449).
    pub fn cold_last_stage_started_at(&self) -> ColdResult<Option<chrono::DateTime<chrono::Utc>>> {
        let started: Option<String> = self
            .conn
            .query_row(
                "SELECT started_at FROM cold_archive_runs WHERE mode IN ('stage', 'move')
                 ORDER BY rowid DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(parse_started(started))
    }

    /// When the newest verify run started (the weekly verify pass, slice S4).
    pub fn cold_last_verify_started_at(&self) -> ColdResult<Option<chrono::DateTime<chrono::Utc>>> {
        let started: Option<String> = self
            .conn
            .query_row(
                "SELECT started_at FROM cold_archive_runs WHERE mode = 'verify'
                 ORDER BY rowid DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(parse_started(started))
    }

    /// The oldest successful read-back of an archived chunk (staged or cold
    /// sessions; a session whose last step failed is the stage pass's business).
    /// The weekly verify pass is due when it is more than a week old.
    pub fn cold_oldest_verified_at(&self) -> ColdResult<Option<chrono::DateTime<chrono::Utc>>> {
        let oldest: Option<String> = self.conn.query_row(
            "SELECT MIN(verified_at) FROM cold_archive_sessions
             WHERE state IN ('staged', 'cold') AND events_chunk_id > 0
               AND verified_at IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(parse_started(oldest))
    }

    /// The newest run, if any: the last row inserted, not the greatest
    /// `started_at` (a row stamped by a clock that moved back must not hide later
    /// runs, #2449).
    pub fn cold_last_run(&self) -> ColdResult<Option<ColdRunRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT run_id, trigger, mode, started_at, finished_at, outcome, sessions_staged,
                        sessions_moved, events_moved, provenance_moved, raw_bytes,
                        compressed_bytes, holds_json, live_bytes_before, live_bytes_after,
                        error_code, error_detail
                 FROM cold_archive_runs ORDER BY rowid DESC LIMIT 1",
                [],
                |row| {
                    let holds: String = row.get(12)?;
                    Ok(ColdRunRow {
                        run_id: row.get(0)?,
                        trigger: row.get(1)?,
                        mode: row.get(2)?,
                        started_at: row.get(3)?,
                        finished_at: row.get(4)?,
                        outcome: row.get(5)?,
                        sessions_staged: row.get(6)?,
                        sessions_moved: row.get(7)?,
                        events_moved: row.get(8)?,
                        provenance_moved: row.get(9)?,
                        raw_bytes: row.get(10)?,
                        compressed_bytes: row.get(11)?,
                        holds: serde_json::from_str(&holds)
                            .unwrap_or(serde_json::Value::String(holds)),
                        live_bytes_before: row.get(13)?,
                        live_bytes_after: row.get(14)?,
                        error_code: row.get(15)?,
                        error_detail: row.get(16)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// The live database's page count times its page size.
    pub fn cold_live_db_bytes(&self) -> ColdResult<i64> {
        let pages: i64 = self
            .conn
            .query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let size: i64 = self
            .conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))?;
        Ok(pages.saturating_mul(size))
    }
}
