//! Cold-storage archive of old session data (#2384).
//!
//! The daemon moves the bulky, non-`Message` rows of old `Archived`/`Deleted`
//! sessions (`conversation_events` and their `conversation_event_provenance`)
//! into append-only `SQLite` archive files of per-session zstd chunks under the
//! cold-archive directory. The live database keeps the session row, every
//! `Message` row, the newest rows and any row another table points at, plus an
//! index of what was archived (`cold_archive_files`, `cold_archive_sessions`,
//! `cold_archive_runs`, migration V176).
//!
//! **Slice S1: stage and verify.** A pass selects the cold set (`candidates`),
//! encodes each session's movable rows (`format`), writes them to the archive
//! with an fsync (`writer`) and reads them back to verify (`pass`). Writing is
//! stage -> fsync -> read back and verify, and every step is recorded durably, so
//! a crash at any point loses nothing and the next pass picks up where it
//! stopped.
//!
//! **Slice S2: cold reads and restore.** [`reader`] merges a cold session's
//! archived rows with its live tail behind one store function
//! (`Store::merge_cold_events`), so every transcript reader returns the same
//! transcript it returned before the move; a missing or corrupt archive is the
//! typed `cold_archive_unavailable`, never a short transcript. [`restore`] puts
//! the rows back under a compare-and-set fence on the index row. The move itself
//! (P6) existed only as a test seam in that slice (`move_seam`).
//!
//! **Slice S4: move, reclaim, notices.** A `move` pass ([`mover`]) deletes a
//! session's archived rows from the live tables only after its chunks were read
//! back and checked against their digests in the same pass, only while a fresh
//! backup of the database exists ([`backup`]), and only after the live rows
//! still hash to what was archived. Freed pages are reclaimed in small
//! incremental steps; the one-time file compaction runs in an isolated helper
//! process at a supervised restart ([`reclaim`]). Every failure raises an
//! operator notice ([`notices`]).
//!
//! Plan: `thoughts/shared/plans/2026-10-14-cold-storage-archive.md`.

pub mod backup;
mod candidates;
mod containment;
pub mod failpoint;
mod format;
mod index;
#[cfg(any(test, feature = "test-seam"))]
pub mod live_connections;
#[cfg(any(test, feature = "test-seam"))]
mod move_seam;
pub mod mover;
pub mod notices;
mod pass;
mod reader;
pub mod reclaim;
mod restore;
mod status;
mod writer;

#[cfg(test)]
#[path = "move_tests.rs"]
mod move_tests;
#[cfg(test)]
#[path = "read_classes.rs"]
mod read_classes;
#[cfg(test)]
#[path = "read_tests.rs"]
mod read_tests;
#[cfg(test)]
#[path = "tests.rs"]
mod tests;

pub use candidates::{EVENTS_TABLE, HOLD_CODES};
pub use containment::{
    check_archive_dir, default_forbidden_roots, forbidden_roots_for, resolve_real_path,
    validate_archive_dir_setting, validate_archive_dir_setting_with,
};
pub use format::{ROW_CODEC, ZSTD_LEVEL};
pub use index::{ColdFileRow, ColdRunRow, ColdSessionRow, RunSummary, StageIntent};
#[cfg(any(test, feature = "test-seam"))]
pub use move_seam::{MoveOutcome, MoveReceipt};
pub use pass::{
    ColdStoreAccess, DirectStoreAccess, MoveRefusal, PassLimits, PassParams, SessionError,
    StageReport, VerifyReport, lower_current_thread_priority, open_read_connection, run_move_pass,
    run_stage_pass, run_verify_pass,
};
pub use reader::{
    ColdRef, ColdSessionStorage, UNAVAILABLE_CODE, is_unavailable_error, unavailable_error,
};
pub use restore::{
    ColdRestoreCommit, DecodedColdSession, RESTORE_ATTEMPTS, RestoreReceipt, decode_cold_session,
};
pub use status::{ColdArchiveStatus, ColdFileStatus, LiveDbStatus};
pub use writer::{
    ARCHIVE_APPLICATION_ID, ARCHIVE_FORMAT_VERSION, archive_file_name, archive_month,
};

use rsid_core::error::DaemonError;
use std::fmt;
use std::path::{Path, PathBuf};

/// The archived share of a session's event count (`state = 'cold'` only): added
/// to the live `COUNT(*)` it gives the logical count, which does not change when
/// the session moves to the archive or is restored (plan §4.1, class *logical
/// aggregate*). Every production statement that reads the logical count repeats
/// this text verbatim (the SQL at a sink must be statically known to the
/// protected-DML audit); `logical_aggregate_statements_share_one_expression`
/// compares them with this constant.
pub const LOGICAL_ARCHIVED_COUNT_SQL: &str = "COALESCE((SELECT archived_event_count FROM cold_archive_sessions WHERE session_id = ?1 AND state = 'cold'), 0)";
/// The archived share of a session's `SUM(LENGTH(content))`.
pub const LOGICAL_ARCHIVED_CONTENT_LEN_SQL: &str = "COALESCE((SELECT archived_content_len FROM cold_archive_sessions WHERE session_id = ?1 AND state = 'cold'), 0)";
/// The logical event count of session `?1`: live rows plus the rows archived for
/// a cold session, in one statement (plan `LOGICAL_EVENT_COUNT_SQL`).
pub const LOGICAL_EVENT_COUNT_SQL: &str = "SELECT (SELECT COUNT(*) FROM conversation_events WHERE session_id = ?1) + COALESCE((SELECT archived_event_count FROM cold_archive_sessions WHERE session_id = ?1 AND state = 'cold'), 0)";
/// The archived share of the database-wide `COUNT(*)` of `conversation_events`
/// (every `cold` session). Added to the live count it is the logical total the
/// portable-bundle manifest records for the table (plan §4.1, class *logical
/// aggregate*, slice S3).
pub const LOGICAL_ARCHIVED_TOTAL_COUNT_SQL: &str =
    "SELECT COALESCE(SUM(archived_event_count), 0) FROM cold_archive_sessions WHERE state = 'cold'";

/// Default of `cold_archive_enabled` (slice S5, plan §6.1): on. A store with no
/// persisted row for the setting runs the archive; an operator who turned it off
/// has a persisted `false`, which a restart keeps.
pub const COLD_ARCHIVE_ENABLED_DEFAULT: bool = true;

/// Minimum, default and maximum of `cold_archive_after_days`.
pub const COLD_AFTER_DAYS_MIN: u32 = 3;
pub const COLD_AFTER_DAYS_DEFAULT: u32 = 14;
pub const COLD_AFTER_DAYS_MAX: u32 = 3650;

/// A failed cold-archive step: a stable machine-readable `code` (recorded on the
/// session or run row and used as the friction signature `cold_archive:<code>`)
/// and a human `detail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColdError {
    pub code: &'static str,
    pub detail: String,
}

impl ColdError {
    #[must_use]
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for ColdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for ColdError {}

impl From<rusqlite::Error> for ColdError {
    fn from(error: rusqlite::Error) -> Self {
        Self::new("cold_archive_sqlite", error.to_string())
    }
}

impl From<std::io::Error> for ColdError {
    fn from(error: std::io::Error) -> Self {
        Self::new("cold_archive_io", error.to_string())
    }
}

impl From<serde_json::Error> for ColdError {
    fn from(error: serde_json::Error) -> Self {
        Self::new("cold_archive_json", error.to_string())
    }
}

impl From<ColdError> for DaemonError {
    fn from(error: ColdError) -> Self {
        Self::Store(error.to_string())
    }
}

pub type ColdResult<T> = Result<T, ColdError>;

/// The operator's cold-archive settings at the start of a pass.
#[derive(Debug, Clone)]
pub struct ColdSettings {
    pub enabled: bool,
    pub after_days: u32,
    /// The resolved archive directory (never empty).
    pub dir: PathBuf,
    /// Directories the archive must never resolve into (the sandbox base, #2435):
    /// a pass resolves `dir` (symlinks and `..` followed) and refuses one that
    /// lands inside any of them before it creates a file.
    pub forbidden_roots: Vec<PathBuf>,
}

/// The archive directory for the `cold_archive_dir` setting: the configured
/// absolute path, or `<directory of the database>/archive` when it is empty.
#[must_use]
pub fn resolve_archive_dir(configured: &str, database_path: Option<&Path>) -> PathBuf {
    let configured = configured.trim();
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    database_path
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from("archive"), |parent| parent.join("archive"))
}

/// What a pass does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdMode {
    /// Write and verify archive chunks for the cold set; delete nothing.
    Stage,
    /// Re-read every staged chunk and check it against its manifest.
    Verify,
    /// Stage and verify, then delete the verified rows from the live database
    /// (P6, slice S4) and reclaim their pages.
    Move,
    /// Shrink the live database file. Never runs in the live daemon: a request
    /// is recorded and an isolated helper process runs it at the next
    /// supervised restart (plan §5, review #2398).
    Compact,
}

impl ColdMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stage => "stage",
            Self::Verify => "verify",
            Self::Move => "move",
            Self::Compact => "compact",
        }
    }

    /// Parses a `RunColdArchiveNow` mode.
    #[must_use]
    pub fn parse(mode: &str) -> Option<Self> {
        match mode {
            "stage" => Some(Self::Stage),
            "verify" => Some(Self::Verify),
            "move" => Some(Self::Move),
            "compact" => Some(Self::Compact),
            _ => None,
        }
    }
}
