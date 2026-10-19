//! `GetColdArchiveStatus`: what the operator sees (plan §6.2).

use super::index::{ColdFileRow, ColdRunRow};
use super::{ColdResult, ColdSettings};
use crate::store::Store;
use serde::Serialize;

/// The live database file.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LiveDbStatus {
    pub file_bytes: Option<u64>,
    pub wal_bytes: Option<u64>,
    pub page_count: i64,
    pub freelist_count: i64,
    pub page_size: i64,
    /// `PRAGMA auto_vacuum`: 0 none, 1 full, 2 incremental.
    pub auto_vacuum: i64,
}

/// One archive file.
#[derive(Debug, Clone, Serialize)]
pub struct ColdFileStatus {
    pub path: String,
    pub month: String,
    pub bytes: i64,
    pub chunk_count: i64,
    pub last_verified_at: Option<String>,
    pub last_verify_error: Option<String>,
}

impl From<ColdFileRow> for ColdFileStatus {
    fn from(row: ColdFileRow) -> Self {
        Self {
            path: row.path,
            month: row.month,
            bytes: row.bytes,
            chunk_count: row.chunk_count,
            last_verified_at: row.last_verified_at,
            last_verify_error: row.last_verify_error,
        }
    }
}

/// Counts over the index.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ColdTotals {
    /// Sessions whose chunk is written (state `staged`, still live).
    pub staged_sessions: i64,
    /// Of those, the ones whose chunk read back equal to the manifest.
    pub verified_sessions: i64,
    /// Sessions whose rows moved out of the live database (S4).
    pub cold_sessions: i64,
    pub restored_sessions: i64,
    /// Sessions whose last step failed.
    pub sessions_with_error: i64,
    pub archived_events: i64,
    pub archived_provenance: i64,
    pub raw_bytes: i64,
    pub compressed_bytes: i64,
}

/// Space reclaim: the one-time compaction runs in an isolated helper at a
/// supervised restart (slice S4).
#[derive(Debug, Clone, Default, Serialize)]
pub struct CompactionStatus {
    pub available: bool,
    pub due: bool,
    /// An operator asked for it; it runs at the next supervised restart.
    pub pending: bool,
    pub last_at: Option<String>,
    pub last_outcome: Option<String>,
    pub last_ms: Option<i64>,
    pub bytes_before: Option<i64>,
    pub bytes_after: Option<i64>,
}

/// The operator-facing settings, resolved.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    pub enabled: bool,
    pub after_days: u32,
    pub dir: String,
}

/// Everything `GetColdArchiveStatus` returns.
#[derive(Debug, Clone, Serialize)]
pub struct ColdArchiveStatus {
    pub settings: SettingsView,
    pub live_db: LiveDbStatus,
    pub archives: Vec<ColdFileStatus>,
    pub totals: ColdTotals,
    pub last_run: Option<ColdRunRow>,
    pub running: bool,
    pub compaction: CompactionStatus,
    /// Open problems, newest information first: the last run's failure, a file
    /// that failed verification, sessions whose last step failed.
    pub errors: Vec<String>,
}

impl Store {
    /// Builds the status report.
    pub fn cold_archive_status(
        &self,
        settings: &ColdSettings,
        running: bool,
    ) -> ColdResult<ColdArchiveStatus> {
        let mut live_db = LiveDbStatus {
            page_count: self
                .conn
                .query_row("PRAGMA page_count", [], |row| row.get(0))?,
            freelist_count: self
                .conn
                .query_row("PRAGMA freelist_count", [], |row| row.get(0))?,
            page_size: self
                .conn
                .query_row("PRAGMA page_size", [], |row| row.get(0))?,
            auto_vacuum: self
                .conn
                .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?,
            ..LiveDbStatus::default()
        };
        if let Some(path) = self.database_path() {
            live_db.file_bytes = std::fs::metadata(path).ok().map(|meta| meta.len());
            live_db.wal_bytes = std::fs::metadata(format!("{path}-wal"))
                .ok()
                .map(|meta| meta.len());
        }
        let archives: Vec<ColdFileStatus> = self
            .cold_archive_files()?
            .into_iter()
            .map(ColdFileStatus::from)
            .collect();
        let totals = self.conn.query_row(
            "SELECT
                COALESCE(SUM(state = 'staged'), 0),
                COALESCE(SUM(state = 'staged' AND verified_at IS NOT NULL), 0),
                COALESCE(SUM(state = 'cold'), 0),
                COALESCE(SUM(state = 'restored'), 0),
                COALESCE(SUM(last_error_code IS NOT NULL), 0),
                COALESCE(SUM(CASE WHEN state IN ('staged','cold') THEN archived_event_count END), 0),
                COALESCE(SUM(CASE WHEN state IN ('staged','cold') THEN archived_provenance_count END), 0),
                COALESCE(SUM(CASE WHEN state IN ('staged','cold') THEN raw_bytes END), 0),
                COALESCE(SUM(CASE WHEN state IN ('staged','cold') THEN compressed_bytes END), 0)
             FROM cold_archive_sessions",
            [],
            |row| {
                Ok(ColdTotals {
                    staged_sessions: row.get(0)?,
                    verified_sessions: row.get(1)?,
                    cold_sessions: row.get(2)?,
                    restored_sessions: row.get(3)?,
                    sessions_with_error: row.get(4)?,
                    archived_events: row.get(5)?,
                    archived_provenance: row.get(6)?,
                    raw_bytes: row.get(7)?,
                    compressed_bytes: row.get(8)?,
                })
            },
        )?;
        let last_run = self.cold_last_run()?;
        let mut errors = Vec::new();
        if let Some(run) = &last_run
            && let Some(code) = &run.error_code
            // A compaction waiting for the next restart is not a problem.
            && code != super::reclaim::COMPACT_SCHEDULED_CODE
        {
            errors.push(format!(
                "last run {} ({}): {code}{}",
                run.run_id,
                run.outcome,
                run.error_detail
                    .as_deref()
                    .map(|detail| format!(": {detail}"))
                    .unwrap_or_default()
            ));
        }
        for file in &archives {
            if let Some(error) = &file.last_verify_error {
                errors.push(format!("archive {}: {error}", file.path));
            }
        }
        if totals.sessions_with_error > 0 {
            errors.push(format!(
                "{} session(s) failed their last archive step",
                totals.sessions_with_error
            ));
        }
        let due = super::reclaim::compaction_due(
            live_db.auto_vacuum,
            live_db.freelist_count,
            live_db.page_count,
            live_db.page_size,
        );
        let last = self.cold_last_compaction()?;
        Ok(ColdArchiveStatus {
            settings: SettingsView {
                enabled: settings.enabled,
                after_days: settings.after_days,
                dir: settings.dir.display().to_string(),
            },
            live_db,
            archives,
            totals,
            last_run,
            running,
            compaction: CompactionStatus {
                available: true,
                due,
                pending: last.pending,
                last_at: last.last_at,
                last_outcome: last.last_outcome,
                last_ms: last.last_ms,
                bytes_before: last.bytes_before,
                bytes_after: last.bytes_after,
            },
            errors,
        })
    }
}
