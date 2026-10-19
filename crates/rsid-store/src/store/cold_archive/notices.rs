//! Cold-archive notices (#2384, slice S4; plan §6.4): never silent.
//!
//! A failed pass, a failed verification, an archive that cannot be read, a
//! failed or interrupted compaction, a compaction due for more than a day, a
//! move refused for want of a fresh backup, an unclassified hold state and a
//! restore conflict each produce:
//!
//! 1. a WARN log line;
//! 2. a friction event `cold_archive:<code>`, so repeats reach the andon sweep;
//! 3. an entry in the status `errors` list (the run row, file or session row
//!    that failed already carries it);
//! 4. an operator to-do keyed `system:cold-archive:<code>:<stamp>`, one open
//!    item per code (a repeat while one is open adds nothing), closed by the
//!    daemon once a later pass of that kind succeeds.
//!
//! #2307's dedicated notice API is not on `rolling`; the daemon-written
//! operator-scope to-do (#2451, `operator_todo_system_notice`) is the
//! operator-visible surface.

use super::ColdResult;
use crate::store::Store;
use rsi_common::friction::{FrictionKind, NewFrictionEventV1};
use std::sync::Mutex;

/// Key prefix of every cold-archive notice.
pub const NOTICE_KEY_PREFIX: &str = "system:cold-archive:";
/// Notices about the one-time compaction (closed by a completed compaction,
/// not by a pass).
pub const COMPACTION_NOTICE_CODES: [&str; 4] = [
    super::reclaim::COMPACT_INTERRUPTED_CODE,
    "compact_failed",
    "compact_due_too_long",
    "compact_disk_space",
];
/// A read of a cold transcript found its archive missing or corrupt.
pub const UNAVAILABLE_NOTICE: &str = "cold_archive_unavailable";
/// Notices about a read or a restore of one session: they stay open until the
/// operator closes them (a later pass does not prove the file is back).
pub const READ_NOTICE_CODES: [&str; 2] = [UNAVAILABLE_NOTICE, "cold_restore_conflict"];

const TODO_PRIORITY: u8 = 2;
const MAX_PENDING: usize = 16;

/// Notices raised where no store is at hand (a read deep in a caller's
/// transaction): queued here and raised by the cold-archive loop.
static PENDING: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Queues a notice for the next [`Store::cold_archive_flush_notices`].
pub fn queue_notice(code: &str, detail: &str) {
    let mut pending = PENDING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if pending.len() < MAX_PENDING && !pending.iter().any(|(queued, _)| queued == code) {
        pending.push((code.to_string(), detail.to_string()));
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn title_for(code: &str) -> String {
    let what = match code {
        "cold_archive_backup_stale" => "moved nothing: no fresh rsi.db backup",
        "cold_archive_unavailable" => "an archive file cannot be read",
        "cold_restore_conflict" => "a restore found its rows already present",
        "cold_archive_hold_schema_unclassified" => "a hold state is not classified",
        "compact_interrupted" => "the database compaction was interrupted",
        "compact_due_too_long" => "the database compaction is overdue",
        code if code.starts_with("compact") => "the database compaction failed",
        code if code.starts_with("cold_archive_verify") => "an archive chunk failed verification",
        _ => "a cold-storage pass failed",
    };
    clip(&format!("Cold storage: {what} ({code})"), 480)
}

fn how_for(code: &str) -> &'static str {
    match code {
        "cold_archive_backup_stale" => {
            "A move pass deletes archived rows only while a backup of rsi.db younger than 24 h \
             sits in ~/.rsi/backups; none qualified and the pass could not take one (disk space \
             or permissions of the backups directory). Nothing was deleted. Free space or fix \
             the directory, then run :cold-archive move."
        }
        "cold_archive_unavailable" => {
            "A cold session's transcript could not be read from its archive file (missing, \
             moved or corrupt). Readers get a typed error, never a short transcript. Restore \
             the file from your backup of the archive directory (see the Cold Storage tab), \
             then run :cold-archive verify and close this item."
        }
        "cold_restore_conflict" => {
            "Restoring a cold session found one of its archived rows already in the live \
             database; the restore rolled back and the session stays cold. Inspect the session \
             and the daemon log, then close this item."
        }
        "compact_due_too_long" => {
            "The live database holds a lot of free space and the compaction that returns it \
             runs only at a supervised restart. Restart the daemon (or deploy) when convenient."
        }
        code if code.starts_with("compact") => {
            "The one-time compaction of rsi.db runs in a helper process at a supervised restart; \
             it did not complete. The database is intact (VACUUM is atomic). It is retried at the \
             next restart; see :cold-archive status and the daemon log."
        }
        _ => {
            "A cold-storage pass reported a failure. See :cold-archive status (last run and \
             errors) and the daemon log. The item closes itself after the next successful pass."
        }
    }
}

impl Store {
    /// Raises one notice (see the module documentation). Never fails the
    /// caller: a notice that cannot be recorded is logged.
    pub fn cold_archive_raise_notice(&self, code: &str, detail: &str) {
        tracing::warn!(code, detail, "cold archive notice");
        let event = NewFrictionEventV1::new(FrictionKind::ColdArchive, &[code]);
        if let Err(error) = self.record_friction_event(&event) {
            tracing::warn!(code, error = %error, "cold archive notice: friction event not recorded");
        }
        let prefix = format!("{NOTICE_KEY_PREFIX}{code}:");
        let open = match self.operator_todo_open_system_keys(&prefix) {
            Ok(open) => open,
            Err(error) => {
                tracing::warn!(code, error = %error, "cold archive notice: to-do lookup failed");
                return;
            }
        };
        if !open.is_empty() {
            return;
        }
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let key = clip(&format!("{prefix}{stamp}"), 128);
        if let Err(error) = self.operator_todo_system_notice(
            &key,
            None,
            &title_for(code),
            how_for(code),
            TODO_PRIORITY,
        ) {
            tracing::warn!(code, error = %error, "cold archive notice: to-do not written");
        }
    }

    /// The notices a finished run leaves (plan §6.4: never silent): a failed or
    /// partial run raises its first error (a move that stopped for want of a
    /// qualifying backup after it had already moved sessions carries
    /// `cold_archive_backup_stale` here), a completed one closes the pass
    /// notices. A run cut short without an error (budget, host load, a
    /// supervised restart) raises nothing.
    pub fn cold_archive_settle_run_notices(&self, summary: &super::index::RunSummary) {
        match (&summary.error_code, summary.outcome) {
            (Some(code), _) => self.cold_archive_raise_notice(
                code,
                summary.error_detail.as_deref().unwrap_or(summary.outcome),
            ),
            (None, "completed") => {
                if let Err(error) = self.cold_archive_resolve_notices(false) {
                    tracing::warn!(error = %error, "cold archive notices not resolved");
                }
            }
            (None, _) => {}
        }
    }

    /// Raises the notices queued by [`queue_notice`].
    pub fn cold_archive_flush_notices(&self) {
        let pending: Vec<(String, String)> = std::mem::take(
            &mut *PENDING
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (code, detail) in pending {
            self.cold_archive_raise_notice(&code, &detail);
        }
    }

    /// Closes the open cold-archive notices a success resolves: after a
    /// completed pass every pass notice, after a completed compaction every
    /// compaction notice. Read and restore notices stay for the operator.
    ///
    /// # Errors
    /// A persistence error.
    pub fn cold_archive_resolve_notices(&self, compaction: bool) -> ColdResult<usize> {
        let keys = self
            .operator_todo_open_system_keys(NOTICE_KEY_PREFIX)
            .map_err(|error| super::ColdError::new("cold_archive_notice", error.to_string()))?;
        let mut closed = 0;
        for key in keys {
            let code = key
                .strip_prefix(NOTICE_KEY_PREFIX)
                .and_then(|rest| rest.rsplit_once(':'))
                .map_or("", |(code, _)| code);
            if READ_NOTICE_CODES.contains(&code) {
                continue;
            }
            let is_compaction =
                COMPACTION_NOTICE_CODES.contains(&code) || code.starts_with("compact");
            if is_compaction != compaction {
                continue;
            }
            let note = if compaction {
                "resolved: a later compaction completed"
            } else {
                "resolved: a later cold-storage pass completed"
            };
            if self.operator_todo_system_resolve(&key, note).is_ok() {
                closed += 1;
            }
        }
        Ok(closed)
    }
}
