//! Daemon-owned global settings persisted in SQLite (RSI-026).
//!
//! Key-value store for settings that survive daemon restarts and are
//! authoritative across TUI restarts. Mutations are synchronous against
//! the Store connection (not the store_worker queue) because settings
//! cycle once per user keystroke, not in the session write hot path.
//!
//! Schema: `daemon_settings(key TEXT PRIMARY KEY, value TEXT NOT NULL,
//! updated_at TEXT NOT NULL)`. See migration V48 in `mod.rs`.

use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rsi_common::types::Session;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use std::path::Path;

use super::Store;
use super::row_mappers::{
    capability_class_to_str, sandbox_cleanup_state_to_str, sandbox_kind_to_str,
    session_kind_to_str, session_provider_to_str, session_status_to_str,
};
use super::sessions::persisted_context_budget;

/// Closed results for Store-owned C5 state changes.  Callers must not infer
/// whether a failure was a missing source, a concurrent conflict, or a
/// retryable SQLite failure from an error string.
#[derive(Debug)]
pub enum C5TransitionError {
    MissingSource {
        session_id: uuid::Uuid,
    },
    Conflict {
        operation: &'static str,
        session_id: uuid::Uuid,
    },
    ReclaimPrepared {
        session_id: uuid::Uuid,
    },
    InvalidJournal {
        key: String,
        source: C5JournalError,
    },
    Retryable {
        operation: &'static str,
        source: DaemonError,
    },
}

impl std::fmt::Display for C5TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSource { session_id } => {
                write!(f, "c5 missing source session: {session_id}")
            }
            Self::Conflict {
                operation,
                session_id,
            } => {
                write!(f, "c5 transition conflict during {operation}: {session_id}")
            }
            Self::ReclaimPrepared { session_id } => {
                write!(f, "c5 retry awaits prepared target reclaim: {session_id}")
            }
            Self::InvalidJournal { key, source } => {
                write!(f, "c5 invalid journal at {key}: {source}")
            }
            Self::Retryable { operation, source } => {
                write!(f, "c5 retryable store failure during {operation}: {source}")
            }
        }
    }
}

impl std::error::Error for C5TransitionError {}

impl C5TransitionError {
    /// Store failures and a live reclaim gate leave the retry owner eligible
    /// for guarded rearm. A missing source, stale compare, or malformed witness
    /// is settled/user-intent state and must never resurrect a retry timer.
    pub(crate) fn is_retryable_admission_failure(&self) -> bool {
        matches!(self, Self::Retryable { .. } | Self::ReclaimPrepared { .. })
    }
}

impl From<C5TransitionError> for DaemonError {
    fn from(error: C5TransitionError) -> Self {
        match error {
            C5TransitionError::ReclaimPrepared { session_id } => {
                crate::error::sandbox_custody_error(rsi_common::types::SandboxCustodyErrorV1 {
                    version: 1,
                    code: rsi_common::types::SandboxCustodyErrorCodeV1::ReclaimPrepared,
                    session_id: Some(session_id),
                    transition: rsi_common::types::SandboxCustodyTransitionV1::Retry,
                    retryable: true,
                    recovery: rsi_common::types::SandboxCustodyRecoveryV1::RetryAfterReconcile,
                })
            }
            other => DaemonError::Store(other.to_string()),
        }
    }
}

pub type C5TransitionResult<T> = std::result::Result<T, C5TransitionError>;

/// Closed classification for a persisted C5 journal record.  Replay and Store
/// admission must never recover this information by inspecting serde text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum C5JournalError {
    MalformedJson,
    InvalidShape,
    UnknownField(String),
    InvalidVersion,
    UnknownVersion(u64),
    InvalidSourceSessionId,
    UnknownCause(String),
    InvalidStagedAt,
}

impl std::fmt::Display for C5JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedJson => write!(f, "c5 autofile pending malformed json"),
            Self::InvalidShape => write!(f, "c5 autofile pending invalid shape"),
            Self::UnknownField(field) => write!(f, "c5 autofile pending unknown field: {field}"),
            Self::InvalidVersion => write!(f, "c5 autofile pending invalid version"),
            Self::UnknownVersion(version) => {
                write!(f, "c5 autofile pending unknown version: {version}")
            }
            Self::InvalidSourceSessionId => {
                write!(f, "c5 autofile pending invalid source session id")
            }
            Self::UnknownCause(cause) => write!(f, "c5 autofile pending unknown cause: {cause}"),
            Self::InvalidStagedAt => write!(f, "c5 autofile pending invalid staged_at"),
        }
    }
}

impl std::error::Error for C5JournalError {}

impl From<C5JournalError> for DaemonError {
    fn from(error: C5JournalError) -> Self {
        DaemonError::Store(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C5StageOutcome {
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C5SuppressionOutcome {
    Committed,
    AlreadyResolved,
}

/// Canonical key for the system-prompt-preset setting.
pub const KEY_SYSTEM_PROMPT_PRESET: &str = "system_prompt_preset";
pub const C5_AUTOFILE_ACTIVATION_KEY: &str = "c5.autofile.activation.v1";
pub const C5_AUTOFILE_PENDING_PREFIX: &str = "c5.autofile.pending.v1/";
pub const C5_AUTOFILE_PENDING_BATCH_SIZE: usize = 64;

fn now_nanos() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

// Test-only commit seam: SQLite triggers cannot reach a failure returned by
// `Transaction::commit`. It does not exist in production builds.
#[cfg(test)]
thread_local! {
    static C5_TEST_FAIL_COMMIT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn c5_test_fail_next_commit(operation: &'static str) {
    C5_TEST_FAIL_COMMIT.with(|fault| fault.set(Some(operation)));
}

pub(crate) fn commit_c5_transaction(
    tx: rusqlite::Transaction<'_>,
    operation: &'static str,
) -> C5TransitionResult<()> {
    #[cfg(test)]
    if C5_TEST_FAIL_COMMIT.with(|fault| fault.take()) == Some(operation) {
        return Err(C5TransitionError::Retryable {
            operation,
            source: DaemonError::Store("c5 test injected commit failure".into()),
        });
    }
    tx.commit().map_err(|source| C5TransitionError::Retryable {
        operation,
        source: DaemonError::Database(source),
    })
}

pub fn c5_autofile_pending_key(session_id: uuid::Uuid) -> String {
    format!("{C5_AUTOFILE_PENDING_PREFIX}{session_id}")
}

/// A malformed value still has an authoritative source identity when the
/// journal key is valid. Replay uses this solely for error observation; it
/// never treats a malformed row as admitted work.
pub(crate) fn source_session_id_from_c5_pending_key(key: &str) -> Option<uuid::Uuid> {
    key.strip_prefix(C5_AUTOFILE_PENDING_PREFIX)
        .and_then(|source| uuid::Uuid::parse_str(source).ok())
}

/// Bounded terminal causes written to the C5 durable journal.  This is kept
/// store-private so no agent wire format can manufacture an automatic cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AutofileCause {
    NoMeaningfulOutput,
    NonZeroExit,
    ProcessDied,
    StoreDesync,
    RotationMonitorPanic,
    StallTimeout,
    OtherTerminalFailure,
}

impl AutofileCause {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NoMeaningfulOutput => "no-meaningful-output",
            Self::NonZeroExit => "non-zero-exit",
            Self::ProcessDied => "process-died",
            Self::StoreDesync => "store-desync",
            Self::RotationMonitorPanic => "rotation-monitor-panic",
            Self::StallTimeout => "stall-timeout",
            Self::OtherTerminalFailure => "other-terminal-failure",
        }
    }
}

/// Recovery is decided by the recovery path, never inferred from journal
/// strings during replay.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RecoveryDisposition {
    PolicyDeclined,
    BudgetExhausted,
    RetryLaunchFailed,
    NoRecoverySource,
}

impl RecoveryDisposition {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PolicyDeclined => "policy-declined",
            Self::BudgetExhausted => "budget-exhausted",
            Self::RetryLaunchFailed => "retry-launch-failed",
            Self::NoRecoverySource => "no-recovery-source",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct C5AutofilePending {
    pub(crate) version: u8,
    pub(crate) source_session_id: uuid::Uuid,
    pub(crate) cause: AutofileCause,
    staged_at: String,
    pub(crate) terminal_model_invocation_id: Option<uuid::Uuid>,
}

impl C5AutofilePending {
    const VERSION: u8 = 2;

    fn new(
        source_session_id: uuid::Uuid,
        cause: AutofileCause,
        staged_at: String,
        terminal_model_invocation_id: Option<uuid::Uuid>,
    ) -> Self {
        Self {
            version: Self::VERSION,
            source_session_id,
            cause,
            staged_at,
            terminal_model_invocation_id,
        }
    }

    pub(crate) fn parse(raw: &str) -> std::result::Result<Self, C5JournalError> {
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|_| C5JournalError::MalformedJson)?;
        let object = value.as_object().ok_or(C5JournalError::InvalidShape)?;
        for field in object.keys() {
            if !matches!(
                field.as_str(),
                "version"
                    | "source_session_id"
                    | "cause"
                    | "staged_at"
                    | "terminal_model_invocation_id"
            ) {
                return Err(C5JournalError::UnknownField(field.clone()));
            }
        }
        let version = object
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or(C5JournalError::InvalidVersion)?;
        if !matches!(version, 1 | 2) {
            return Err(C5JournalError::UnknownVersion(version));
        }
        if object.len() != if version == 1 { 4 } else { 5 }
            || (version == 1 && object.contains_key("terminal_model_invocation_id"))
            || (version == 2 && !object.contains_key("terminal_model_invocation_id"))
        {
            return Err(C5JournalError::InvalidShape);
        }
        let source_session_id = object
            .get("source_session_id")
            .and_then(serde_json::Value::as_str)
            .ok_or(C5JournalError::InvalidSourceSessionId)
            .and_then(|value| {
                uuid::Uuid::parse_str(value).map_err(|_| C5JournalError::InvalidSourceSessionId)
            })?;
        let cause = match object
            .get("cause")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| C5JournalError::UnknownCause("<non-string>".into()))?
        {
            "no-meaningful-output" => AutofileCause::NoMeaningfulOutput,
            "non-zero-exit" => AutofileCause::NonZeroExit,
            "process-died" => AutofileCause::ProcessDied,
            "store-desync" => AutofileCause::StoreDesync,
            "rotation-monitor-panic" => AutofileCause::RotationMonitorPanic,
            "stall-timeout" => AutofileCause::StallTimeout,
            "other-terminal-failure" => AutofileCause::OtherTerminalFailure,
            cause => return Err(C5JournalError::UnknownCause(cause.into())),
        };
        let staged_at = object
            .get("staged_at")
            .and_then(serde_json::Value::as_str)
            .ok_or(C5JournalError::InvalidStagedAt)?;
        let fraction = staged_at
            .split_once('.')
            .and_then(|(_, suffix)| suffix.strip_suffix('Z'));
        if !fraction.is_some_and(|fraction| {
            fraction.len() == 9 && fraction.bytes().all(|c| c.is_ascii_digit())
        }) || chrono::DateTime::parse_from_rfc3339(staged_at).is_err()
        {
            return Err(C5JournalError::InvalidStagedAt);
        }
        let terminal_model_invocation_id = if version == 1 {
            None
        } else {
            match object.get("terminal_model_invocation_id") {
                Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(value)) => {
                    Some(uuid::Uuid::parse_str(value).map_err(|_| C5JournalError::InvalidShape)?)
                }
                _ => return Err(C5JournalError::InvalidShape),
            }
        };
        Ok(Self {
            version: version as u8,
            source_session_id,
            cause,
            staged_at: staged_at.to_string(),
            terminal_model_invocation_id,
        })
    }
}

impl Store {
    /// Activation is deliberately established before session restore. Existing
    /// Failed rows have no journal row and are never backfilled.
    pub fn ensure_c5_autofile_activation(&self) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO daemon_settings (key, value, updated_at) VALUES (?1, ?2, ?2)",
            params![C5_AUTOFILE_ACTIVATION_KEY, now_nanos()],
        )?;
        Ok(())
    }

    /// Atomic status transition plus first-write-wins pending journal stage.
    pub(crate) fn update_failed_and_stage_c5_autofile(
        &self,
        session_id: uuid::Uuid,
        cause: AutofileCause,
    ) -> C5TransitionResult<C5StageOutcome> {
        let now = now_nanos();
        let key = c5_autofile_pending_key(session_id);
        let stop_reason = format!("terminal_failure:{}", cause.as_str());
        let tx =
            self.conn
                .unchecked_transaction()
                .map_err(|source| C5TransitionError::Retryable {
                    operation: "stage_begin",
                    source: DaemonError::Database(source),
                })?;
        let changed = tx
            .execute(
                "UPDATE sessions SET status = 'Failed', stop_reason = COALESCE(stop_reason, ?1), updated_at = ?2 WHERE id = ?3",
                params![stop_reason, now, session_id.to_string()],
            )
            .map_err(|source| C5TransitionError::Retryable {
                operation: "stage_failed_status",
                source: DaemonError::Database(source),
            })?;
        if changed != 1 {
            return Err(C5TransitionError::MissingSource { session_id });
        }
        let terminal_model_invocation_id: Option<String> = tx
            .query_row(
                "SELECT model_invocation_id FROM sessions WHERE id=?1",
                params![session_id.to_string()],
                |row| row.get(0),
            )
            .map_err(|source| C5TransitionError::Retryable {
                operation: "stage_terminal_invocation_read",
                source: DaemonError::Database(source),
            })?;
        let terminal_model_invocation_id = terminal_model_invocation_id
            .map(|value| {
                uuid::Uuid::parse_str(&value).map_err(|error| C5TransitionError::Retryable {
                    operation: "stage_terminal_invocation_parse",
                    source: DaemonError::Store(format!(
                        "invalid terminal model invocation UUID: {error}"
                    )),
                })
            })
            .transpose()?;
        let value = serde_json::to_string(&C5AutofilePending::new(
            session_id,
            cause,
            now.clone(),
            terminal_model_invocation_id,
        ))
        .map_err(|source| C5TransitionError::Retryable {
            operation: "stage_pending_encode",
            source: DaemonError::Json(source),
        })?;
        tx.execute(
            "INSERT OR IGNORE INTO daemon_settings (key, value, updated_at) VALUES (?1, ?2, ?3)",
            params![key, value, now_nanos()],
        )
        .map_err(|source| C5TransitionError::Retryable {
            operation: "stage_pending_insert",
            source: DaemonError::Database(source),
        })?;
        commit_c5_transaction(tx, "stage_commit")?;
        Ok(C5StageOutcome::Committed)
    }

    /// Enumerate only C5 journal keys, ordered by their indexed primary key.
    pub fn list_c5_autofile_pending(
        &self,
        after_key: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let upper = format!("{C5_AUTOFILE_PENDING_PREFIX}\u{10ffff}");
        let after = after_key.unwrap_or(C5_AUTOFILE_PENDING_PREFIX);
        let mut stmt = self.conn.prepare(
            "SELECT key, value FROM daemon_settings WHERE key > ?1 AND key < ?2 ORDER BY key ASC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                after,
                upper,
                limit.min(C5_AUTOFILE_PENDING_BATCH_SIZE) as i64
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        rows.map(|row| row.map_err(DaemonError::Database)).collect()
    }

    pub fn resolve_c5_autofile_pending(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM daemon_settings WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Transaction-composable C5 marker suppression for settlement paths that
    /// must archive the Session, tombstone custody, and finalize a journal item
    /// in the same commit.
    pub(crate) fn resolve_c5_autofile_pending_tx(
        tx: &rusqlite::Transaction<'_>,
        session_id: uuid::Uuid,
    ) -> Result<usize> {
        tx.execute(
            "DELETE FROM daemon_settings WHERE key = ?1",
            params![c5_autofile_pending_key(session_id)],
        )
        .map_err(Into::into)
    }

    /// Atomically make archival durable user intent and remove any pending C5
    /// marker. Replay can therefore never observe Archived with a live marker.
    pub(crate) fn archive_and_resolve_c5_autofile_pending(
        &self,
        session_id: uuid::Uuid,
    ) -> C5TransitionResult<C5SuppressionOutcome> {
        let tx =
            self.conn
                .unchecked_transaction()
                .map_err(|source| C5TransitionError::Retryable {
                    operation: "archive_begin",
                    source: DaemonError::Database(source),
                })?;
        let changed = tx.execute(
            "UPDATE sessions SET status = 'Archived', pending_archive = 0, updated_at = ?1 WHERE id = ?2",
            params![now_nanos(), session_id.to_string()],
        ).map_err(|source| C5TransitionError::Retryable {
            operation: "archive_status",
            source: DaemonError::Database(source),
        })?;
        if changed != 1 {
            return Err(C5TransitionError::MissingSource { session_id });
        }
        tx.execute(
            "DELETE FROM daemon_settings WHERE key = ?1",
            params![c5_autofile_pending_key(session_id)],
        )
        .map_err(|source| C5TransitionError::Retryable {
            operation: "archive_marker_delete",
            source: DaemonError::Database(source),
        })?;
        commit_c5_transaction(tx, "archive_commit")?;
        Ok(C5SuppressionOutcome::Committed)
    }

    /// Resolve a pending failure and durably exhaust its retry budget together.
    /// This is the user-intent cancellation primitive; callers mirror the
    /// resulting retry values into memory only after this commits.
    pub fn suppress_c5_autofile_pending_and_exhaust_retry(
        &self,
        session_id: uuid::Uuid,
        max_retries: u8,
    ) -> Result<()> {
        self.acknowledge_c5_autofile_suppression_and_exhaust_retry(session_id, max_retries)
            .map(|_| ())
            .map_err(Into::into)
    }

    /// Typed acknowledgement used by later lifecycle ordering work.  The
    /// compatibility wrapper above preserves the current lifecycle surface.
    pub(crate) fn acknowledge_c5_autofile_suppression_and_exhaust_retry(
        &self,
        session_id: uuid::Uuid,
        max_retries: u8,
    ) -> C5TransitionResult<C5SuppressionOutcome> {
        let tx =
            self.conn
                .unchecked_transaction()
                .map_err(|source| C5TransitionError::Retryable {
                    operation: "suppress_begin",
                    source: DaemonError::Database(source),
                })?;
        let changed = tx.execute(
            "UPDATE sessions SET retry_attempt = ?1, max_retries = ?1, updated_at = ?2 WHERE id = ?3",
            params![max_retries, now_nanos(), session_id.to_string()],
        ).map_err(|source| C5TransitionError::Retryable {
            operation: "suppress_retry_state",
            source: DaemonError::Database(source),
        })?;
        if changed != 1 {
            return Err(C5TransitionError::MissingSource { session_id });
        }
        let marker_deleted = tx
            .execute(
                "DELETE FROM daemon_settings WHERE key = ?1",
                params![c5_autofile_pending_key(session_id)],
            )
            .map_err(|source| C5TransitionError::Retryable {
                operation: "suppress_marker_delete",
                source: DaemonError::Database(source),
            })?;
        commit_c5_transaction(tx, "suppress_commit")?;
        Ok(if marker_deleted == 0 {
            C5SuppressionOutcome::AlreadyResolved
        } else {
            C5SuppressionOutcome::Committed
        })
    }

    /// Admit a retry successor in the one transition that makes it a recovery
    /// owner.  Until this commits there is no child row, no exhausted parent,
    /// and no resolved parent marker; callers must not expose the child.
    pub(crate) fn admit_c5_retry_successor(
        &self,
        parent_session_id: uuid::Uuid,
        child: &Session,
        retry_attempt: u8,
        max_retries: u8,
        model_invocation_id: uuid::Uuid,
        expected_marker: &C5AutofilePending,
    ) -> C5TransitionResult<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate).map_err(
            |source| C5TransitionError::Retryable {
                operation: "retry_admission_begin",
                source: DaemonError::Database(source),
            },
        )?;
        if child.continued_from != Some(parent_session_id) {
            return Err(C5TransitionError::Conflict {
                operation: "retry_admission_lineage",
                session_id: child.id,
            });
        }
        if expected_marker.source_session_id != parent_session_id {
            return Err(C5TransitionError::Conflict {
                operation: "retry_admission_marker_source",
                session_id: parent_session_id,
            });
        }
        let marker_key = c5_autofile_pending_key(parent_session_id);
        let marker: Option<String> = tx
            .query_row(
                "SELECT value FROM daemon_settings WHERE key = ?1",
                params![marker_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| C5TransitionError::Retryable {
                operation: "retry_admission_marker_read",
                source: DaemonError::Database(source),
            })?;
        let Some(marker) = marker else {
            return Err(C5TransitionError::Conflict {
                operation: "retry_admission_marker_missing",
                session_id: parent_session_id,
            });
        };
        let marker = C5AutofilePending::parse(&marker).map_err(|source| {
            C5TransitionError::InvalidJournal {
                key: c5_autofile_pending_key(parent_session_id),
                source,
            }
        })?;
        if marker != *expected_marker {
            return Err(C5TransitionError::Conflict {
                operation: "retry_admission_marker_compare",
                session_id: parent_session_id,
            });
        }
        if let (Some(root), Some(branch)) = (&child.sandbox_root, &child.sandbox_branch) {
            let prepared = super::sandbox_custody::prepared_reclaim_for_successor_on(
                &tx,
                parent_session_id,
                root,
                branch,
            )
            .map_err(|source| C5TransitionError::Retryable {
                operation: "retry_admission_reclaim_gate",
                source,
            })?;
            if prepared {
                return Err(C5TransitionError::ReclaimPrepared {
                    session_id: parent_session_id,
                });
            }
        }
        let parent_changed = tx
            .execute(
                "UPDATE sessions SET retry_attempt = ?1, max_retries = ?2, updated_at = ?3
                 WHERE id = ?4 AND status = 'Failed' AND retry_attempt = ?5 AND max_retries = ?2",
                params![
                    max_retries,
                    max_retries,
                    now_nanos(),
                    parent_session_id.to_string(),
                    retry_attempt,
                ],
            )
            .map_err(|source| C5TransitionError::Retryable {
                operation: "retry_admission_parent_state",
                source: DaemonError::Database(source),
            })?;
        if parent_changed != 1 {
            return Err(C5TransitionError::Conflict {
                operation: "retry_admission_parent_compare",
                session_id: parent_session_id,
            });
        }
        let pending_question_json = child
            .pending_question
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|source| C5TransitionError::Retryable {
                operation: "retry_admission_pending_question",
                source: DaemonError::Store(format!("Invalid pending question JSON: {source}")),
            })?;
        let context_budget =
            persisted_context_budget(child.context_window, child.resolved_context_budget.as_ref())
                .map_err(|source| C5TransitionError::Retryable {
                    operation: "retry_admission_context_budget",
                    source,
                })?;
        tx.execute(
            "INSERT INTO sessions (id, claude_session_id, provider, query, working_dir, status,
             project_id, pinned_at, created_at, updated_at, cost_usd, duration_ms, num_turns, model,
             input_tokens, output_tokens, context_window,
             total_input_tokens, total_output_tokens, total_cache_creation_tokens, total_cache_read_tokens,
             stop_reason, session_kind, continued_from, handoff_filepath, rotation_depth,
             daemon_input_tokens, daemon_output_tokens, title, description, pipeline_artifact, workflow_id,
             git_branch, active_task, group_id, pending_archive, effort, retry_attempt, max_retries,
             issue_identifier, issue_url, issue_tracker_id, scheduled_job_id,
             rating, harness_version_hash, test_passed, clippy_passed, turn_count, retry_count,
             sandbox_kind, sandbox_root, sandbox_branch, sandbox_cleanup_state, parent_id,
             approval_wait_ms, lead_session_id, is_eval, capability_class,
             topology_node_id, topology_iteration, pending_question_json, work_time_ms,
             testing_needed_at, rotation_disabled_at,
             model_invocation_id, context_window_source, context_window_source_version,
             context_window_source_digest, context_window_observed_at,
             context_window_configured_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                     ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32,
                     ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43,
                     ?44, ?45, ?46, ?47, ?48, ?49,
                     ?50, ?51, ?52, ?53, ?54, ?55, ?56, ?57, ?58, ?59, ?60, ?61, ?62, ?63,
                     ?64, ?65, ?66, ?67, ?68, ?69, ?70)",
            params![
                child.id.to_string(), child.claude_session_id,
                session_provider_to_str(child.provider), child.query,
                child.working_dir.to_string_lossy().to_string(), session_status_to_str(child.status),
                child.project_id.map(|id| id.to_string()), child.pinned_at.map(|dt| dt.to_rfc3339()),
                child.created_at.to_rfc3339(), child.updated_at.to_rfc3339(), child.cost_usd,
                child.duration_ms.map(|v| v as i64), child.num_turns.map(|v| v as i32), child.model,
                child.input_tokens.map(|v| v as i64), child.output_tokens.map(|v| v as i64),
                context_budget.context_window, child.total_input_tokens.map(|v| v as i64),
                child.total_output_tokens.map(|v| v as i64), child.total_cache_creation_tokens.map(|v| v as i64),
                child.total_cache_read_tokens.map(|v| v as i64), child.stop_reason.clone(),
                session_kind_to_str(child.session_kind), child.continued_from.map(|id| id.to_string()),
                child.handoff_filepath.clone(), child.rotation_depth as i64,
                child.daemon_input_tokens.map(|v| v as i64), child.daemon_output_tokens.map(|v| v as i64),
                child.title.as_deref(), child.description.as_deref(), child.pipeline_artifact.as_deref(),
                child.workflow_id.map(|id| id.to_string()), child.git_branch.as_deref(), child.active_task.as_deref(),
                child.group_id.map(|id| id.to_string()), child.pending_archive as i64, child.effort.as_deref(),
                retry_attempt, max_retries, child.issue_identifier.as_deref(), child.issue_url.as_deref(),
                child.issue_tracker_id.as_deref(), child.scheduled_job_id.map(|id| id.to_string()), child.rating,
                child.harness_version_hash.as_deref(), child.test_passed.map(i64::from), child.clippy_passed.map(i64::from),
                child.turn_count.map(|v| v as i64), child.retry_count.map(|v| v as i64),
                child.sandbox_kind.map(sandbox_kind_to_str), child.sandbox_root.as_ref().map(|p| p.to_string_lossy().to_string()),
                child.sandbox_branch.as_deref(), child.sandbox_cleanup_state.map(sandbox_cleanup_state_to_str),
                child.parent_id.map(|id| id.to_string()), child.approval_wait_ms.map(|v| v as i64),
                child.lead_session_id.map(|id| id.to_string()), i64::from(child.is_eval),
                child.capability_class.map(capability_class_to_str), child.topology_node_id.as_deref(),
                child.topology_iteration as i64, pending_question_json.as_deref(), child.work_time_ms.map(|v| v as i64),
                child.testing_needed_at.map(|dt| dt.to_rfc3339()),
                child.rotation_disabled_at.map(|dt| dt.to_rfc3339()),
                model_invocation_id.to_string(),
                context_budget.source, context_budget.source_version, context_budget.source_digest,
                context_budget.observed_at,
                context_budget.configured_tokens,
            ],
        )
        .map_err(|source| C5TransitionError::Retryable {
            operation: "retry_admission_child_insert",
            source: DaemonError::Database(source),
        })?;
        let marker_deleted = tx
            .execute(
                "DELETE FROM daemon_settings WHERE key = ?1",
                params![c5_autofile_pending_key(parent_session_id)],
            )
            .map_err(|source| C5TransitionError::Retryable {
                operation: "retry_admission_marker_delete",
                source: DaemonError::Database(source),
            })?;
        if marker_deleted != 1 {
            return Err(C5TransitionError::Conflict {
                operation: "retry_admission_marker_delete",
                session_id: parent_session_id,
            });
        }
        commit_c5_transaction(tx, "retry_admission_commit")
    }

    pub(crate) fn get_c5_autofile_pending(&self, key: &str) -> Result<Option<C5AutofilePending>> {
        self.get_daemon_setting(key)?
            .map(|raw| C5AutofilePending::parse(&raw).map_err(Into::into))
            .transpose()
    }

    /// A live persisted continuation means this failed attempt is superseded.
    /// Retried-child admission rollback leaves a logical tombstone rather than
    /// hard-deleting a session row; such a row must not suppress the parent's
    /// retained pending marker. This remains a bounded indexed lookup, never a
    /// Failed-session scan.
    pub fn has_c5_autofile_successor(&self, session_id: uuid::Uuid) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM sessions WHERE continued_from = ?1 AND status NOT IN ('Deleted', 'Archived') LIMIT 1",
                params![session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }
    /// Read a `daemon_settings` value by key. Returns `None` if the row
    /// does not exist (caller decides the default).
    pub fn get_daemon_setting(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM daemon_settings WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get::<_, String>(0)?))
        } else {
            Ok(None)
        }
    }

    /// UPSERT a `daemon_settings` row. Caller MUST pre-validate `value` —
    /// this is a raw write. Timestamp is `chrono::Utc::now().to_rfc3339()`
    /// per the daemon's timestamp convention (CLAUDE.md "Database Rules").
    pub fn set_daemon_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO daemon_settings (key, value, updated_at) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
                                            updated_at = excluded.updated_at",
            params![key, value, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }
}

fn daemon_setting_text_to_json(field: &str, raw: &str) -> serde_json::Value {
    match field {
        "title_model_base_url"
        | "memory_model_fallback_base_url"
        | "dream_model_base_url"
        | "prompt_compile_model_base_url" => {
            if raw == "null" {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(raw.to_string())
            }
        }
        "title_model_local"
        | "title_model_provider"
        | "title_model_fallback"
        | "memory_model_local"
        | "memory_model_fallback"
        | "memory_model_fallback_provider"
        | "dream_model"
        | "dream_model_provider"
        | "prompt_compile_model_local"
        | "prompt_compile_model_provider"
        | "codex_sandbox_mode"
        | "claude_config_isolation"
        | "system_prompt_preset"
        | "stall_classifier_model"
        // Issue #35. A bare effort name is not valid JSON, so without this arm
        // it would only survive by falling through the `_` arm's error
        // recovery. Listing it keeps the intent explicit.
        | "orchestration_max_child_effort" => serde_json::Value::String(raw.to_string()),
        _ => {
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
        }
    }
}

fn daemon_setting_json_to_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Persist the current canonical value for a durable RuntimeConfig field.
/// Returns `Ok(false)` when the field is runtime-mutable but intentionally
/// not durable.
pub fn persist_runtime_config_field(
    store: &Store,
    runtime_config: &crate::config::RuntimeConfig,
    field: &str,
) -> Result<bool> {
    let Some(value) = runtime_config.persisted_field_value(field) else {
        return Ok(false);
    };
    let text = daemon_setting_json_to_text(&value);
    store.set_daemon_setting(field, &text)?;
    Ok(true)
}

/// Persist an already validated canonical value. This is the durable half of
/// the 69A prepare -> persist -> publish transaction; it deliberately does not
/// read mutable runtime state.
pub fn persist_runtime_config_value(
    store: &Store,
    field: &str,
    value: &serde_json::Value,
) -> Result<bool> {
    if !crate::config::is_persisted_runtime_config_field(field) {
        return Ok(false);
    }
    store.set_daemon_setting(field, &daemon_setting_json_to_text(value))?;
    Ok(true)
}

/// Persist one validated target-cache config update before publishing it.
///
/// The watermarks are a single invariant-bearing value even though the
/// existing settings table stores them in two rows.  A first one-sided RPC
/// update must therefore seed the unchanged counterpart in the same SQLite
/// transaction; otherwise restart would reject the incomplete pair and lose
/// an update that the daemon already acknowledged.
pub fn persist_sandbox_build_cache_config_update(
    store: &Store,
    field: &str,
    config: rsi_common::sandbox_storage::SandboxBuildCacheReclaimConfig,
) -> Result<bool> {
    const HIGH: &str = "sandbox_build_cache_reclaim_high_watermark_pct";
    const LOW: &str = "sandbox_build_cache_reclaim_low_watermark_pct";

    if field != HIGH && field != LOW {
        let value = match field {
            "sandbox_build_cache_reclaim_enabled" => serde_json::json!(config.enabled),
            "sandbox_build_cache_reclaim_ttl_secs" => serde_json::json!(config.ttl_secs),
            "sandbox_build_cache_reclaim_interval_secs" => {
                serde_json::json!(config.interval_secs)
            }
            "sandbox_build_cache_reclaim_max_candidates" => {
                serde_json::json!(config.max_candidates)
            }
            _ => return Ok(false),
        };
        return persist_runtime_config_value(store, field, &value);
    }

    let tx = store.conn.unchecked_transaction()?;
    let updated_at = now_nanos();
    for (key, value) in [
        (HIGH, config.high_watermark_pct.to_string()),
        (LOW, config.low_watermark_pct.to_string()),
    ] {
        tx.execute(
            "INSERT INTO daemon_settings (key, value, updated_at) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
                                            updated_at = excluded.updated_at",
            params![key, value, updated_at],
        )?;
    }
    tx.commit()?;
    Ok(true)
}

/// Re-apply durable RuntimeConfig rows from SQLite to a freshly-created
/// `RuntimeConfig`. Invalid rows are ignored with a warning so a corrupt
/// setting cannot prevent the daemon from booting.
pub fn apply_persisted_runtime_config(
    store: &Store,
    runtime_config: &crate::config::RuntimeConfig,
) -> Result<usize> {
    let mut applied = 0;
    let high_field = "sandbox_build_cache_reclaim_high_watermark_pct";
    let low_field = "sandbox_build_cache_reclaim_low_watermark_pct";
    let high_raw = store.get_daemon_setting(high_field)?;
    let low_raw = store.get_daemon_setting(low_field)?;
    match (high_raw.as_deref(), low_raw.as_deref()) {
        (None, None) => {}
        (Some(high), Some(low)) => {
            let mut snapshot = runtime_config.sandbox_build_cache_reclaim_snapshot();
            let parsed = serde_json::from_str::<u8>(high)
                .ok()
                .zip(serde_json::from_str::<u8>(low).ok())
                .and_then(|(high, low)| {
                    snapshot.high_watermark_pct = high;
                    snapshot.low_watermark_pct = low;
                    snapshot.validate().ok()
                });
            if let Some(snapshot) = parsed {
                runtime_config.publish_sandbox_build_cache_config(snapshot);
                applied += 2;
            } else {
                tracing::warn!(
                    high = %high,
                    low = %low,
                    "Ignoring invalid persisted target-cache watermark pair"
                );
            }
        }
        (high, low) => tracing::warn!(
            high = ?high,
            low = ?low,
            "Ignoring incomplete persisted target-cache watermark pair"
        ),
    }
    for field in crate::config::PERSISTED_RUNTIME_CONFIG_FIELDS {
        if *field == high_field || *field == low_field {
            continue;
        }
        let Some(raw) = store.get_daemon_setting(field)? else {
            continue;
        };
        let value = daemon_setting_text_to_json(field, &raw);
        match runtime_config.update_field(field, &value) {
            Ok(true) => applied += 1,
            Ok(false) => tracing::warn!(
                field = %field,
                "Persisted daemon setting field is no longer recognized"
            ),
            Err(e) => tracing::warn!(
                field = %field,
                raw = %raw,
                error = %e,
                "Ignoring invalid persisted daemon setting"
            ),
        }
    }
    Ok(applied)
}

/// One-time migration: if `daemon_settings` has no row for
/// `system_prompt_preset`, seed it from `~/.rsi/state.json`'s legacy
/// `settings.system_prompt_preset` field. If state.json is absent or the
/// field is missing/Default, seed `"default"`. Idempotent — survives
/// daemon restarts (subsequent calls return early after the row exists).
///
/// Returns the resulting (now-persisted) canonical slug for the seed
/// value, suitable for handing to `RuntimeConfig::from_config`.
pub fn maybe_import_legacy_system_prompt_preset(
    store: &Store,
    state_json_path: &Path,
) -> Result<String> {
    // Idempotency: if the row exists, return it.
    if let Some(existing) = store.get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET)? {
        tracing::info!(
            value = %existing,
            "system_prompt_preset already in daemon_settings (RSI-026)"
        );
        return Ok(existing);
    }

    // Attempt legacy import from state.json. Failures are non-fatal —
    // we always seed at least "default" so the row exists after boot.
    let mut seed: String = "default".into();
    if state_json_path.exists() {
        match std::fs::read_to_string(state_json_path) {
            Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(json) => {
                    if let Some(preset) = json
                        .get("settings")
                        .and_then(|s| s.get("system_prompt_preset"))
                        .and_then(|p| p.as_str())
                    {
                        if let Some(canonical) =
                            crate::config::normalize_system_prompt_preset(preset)
                        {
                            seed = canonical.to_string();
                            tracing::info!(
                                value = %seed,
                                "Imported legacy system_prompt_preset from state.json (RSI-026)"
                            );
                        } else {
                            tracing::warn!(
                                raw = %preset,
                                "Unknown system_prompt_preset in state.json — seeding default"
                            );
                        }
                    }
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    "state.json parse failed during preset import (RSI-026)"
                ),
            },
            Err(e) => tracing::warn!(
                error = %e,
                "state.json read failed during preset import (RSI-026)"
            ),
        }
    } else {
        tracing::debug!("state.json absent during preset import (clean install)");
    }

    store.set_daemon_setting(KEY_SYSTEM_PROMPT_PRESET, &seed)?;
    Ok(seed)
}

/// Import the four former TUI-owned memory settings only where no daemon row
/// exists. A missing or unreadable state file leaves daemon defaults intact.
///
/// # Errors
/// Returns a store error if a settings row cannot be read or inserted.
pub fn maybe_import_legacy_memory_settings(store: &Store, state_json_path: &Path) -> Result<()> {
    const FIELDS: [(&str, &str); 4] = [
        ("memory_enabled", "memory_enabled"),
        ("dream_enabled", "dream_enabled"),
        ("dream_observation_threshold", "observation_threshold"),
        ("dream_cooldown_secs", "dream_cooldown_secs"),
    ];
    if FIELDS
        .iter()
        .map(|(field, _)| store.get_daemon_setting(field))
        .collect::<Result<Vec<_>>>()?
        .iter()
        .all(Option::is_some)
    {
        return Ok(());
    }
    let raw = match std::fs::read_to_string(state_json_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            tracing::warn!(%error, "Could not read state.json for memory settings import");
            return Ok(());
        }
    };
    let json: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(json) => json,
        Err(error) => {
            tracing::warn!(%error, "Could not parse state.json for memory settings import");
            return Ok(());
        }
    };
    let Some(settings) = json.get("settings") else {
        return Ok(());
    };
    let validator = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
    for (field, legacy_field) in FIELDS {
        if store.get_daemon_setting(field)?.is_some() {
            continue;
        }
        let Some(value) = settings.get(legacy_field) else {
            continue;
        };
        if !matches!(validator.update_field(field, value), Ok(true)) {
            tracing::warn!(%field, "Invalid legacy memory setting; keeping daemon default");
            continue;
        }
        store.conn.execute(
            "INSERT OR IGNORE INTO daemon_settings (key, value, updated_at) VALUES (?1, ?2, ?3)",
            params![field, daemon_setting_json_to_text(value), now_nanos()],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn open_store(dir: &std::path::Path) -> Store {
        Store::open(&dir.join("rsi.db")).expect("Store::open")
    }

    #[allow(clippy::unwrap_used)]
    fn legacy_memory_state(path: &Path, memory: bool, dream: bool, threshold: u64, cooldown: u64) {
        std::fs::write(
            path,
            serde_json::json!({"settings": {
                "memory_enabled": memory,
                "dream_enabled": dream,
                "observation_threshold": threshold,
                "dream_cooldown_secs": cooldown
            }})
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn legacy_memory_import_keeps_existing_rows_when_state_json_differs() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let path = tmp.path().join("state.json");
        legacy_memory_state(&path, true, true, 25, 60);
        for (field, value) in [
            ("memory_enabled", "false"),
            ("dream_enabled", "false"),
            ("dream_observation_threshold", "50"),
            ("dream_cooldown_secs", "28800"),
        ] {
            store.set_daemon_setting(field, value).unwrap();
        }
        maybe_import_legacy_memory_settings(&store, &path).unwrap();
        for (field, value) in [
            ("memory_enabled", "false"),
            ("dream_enabled", "false"),
            ("dream_observation_threshold", "50"),
            ("dream_cooldown_secs", "28800"),
        ] {
            assert_eq!(
                store.get_daemon_setting(field).unwrap().as_deref(),
                Some(value)
            );
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn legacy_memory_import_seeds_rows_from_state_json_when_absent() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let path = tmp.path().join("state.json");
        legacy_memory_state(&path, false, true, 25, 60);
        maybe_import_legacy_memory_settings(&store, &path).unwrap();
        for (field, value) in [
            ("memory_enabled", "false"),
            ("dream_enabled", "true"),
            ("dream_observation_threshold", "25"),
            ("dream_cooldown_secs", "60"),
        ] {
            assert_eq!(
                store.get_daemon_setting(field).unwrap().as_deref(),
                Some(value)
            );
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn legacy_memory_import_is_idempotent() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let path = tmp.path().join("state.json");
        legacy_memory_state(&path, false, true, 25, 60);
        maybe_import_legacy_memory_settings(&store, &path).unwrap();
        legacy_memory_state(&path, true, false, 50, 120);
        maybe_import_legacy_memory_settings(&store, &path).unwrap();
        assert_eq!(
            store
                .get_daemon_setting("memory_enabled")
                .unwrap()
                .as_deref(),
            Some("false")
        );
        assert_eq!(
            store
                .get_daemon_setting("dream_enabled")
                .unwrap()
                .as_deref(),
            Some("true")
        );
        assert_eq!(
            store
                .get_daemon_setting("dream_observation_threshold")
                .unwrap()
                .as_deref(),
            Some("25")
        );
        assert_eq!(
            store
                .get_daemon_setting("dream_cooldown_secs")
                .unwrap()
                .as_deref(),
            Some("60")
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn legacy_memory_import_writes_nothing_without_state_json_value() {
        for contents in [
            None,
            Some("invalid{"),
            Some("{\"settings\":{}}"),
            Some("{\"settings\":{\"memory_enabled\":\"wrong\"}}"),
        ] {
            let tmp = tempdir().unwrap();
            let store = open_store(tmp.path());
            let path = tmp.path().join("state.json");
            if let Some(contents) = contents {
                std::fs::write(&path, contents).unwrap();
            }
            maybe_import_legacy_memory_settings(&store, &path).unwrap();
            for field in [
                "memory_enabled",
                "dream_enabled",
                "dream_observation_threshold",
                "dream_cooldown_secs",
            ] {
                assert_eq!(store.get_daemon_setting(field).unwrap(), None);
            }
        }
    }

    #[test]
    fn daemon_settings_get_missing_returns_none() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let v = store
            .get_daemon_setting("nonexistent_key")
            .expect("get_daemon_setting");
        assert!(v.is_none());
    }

    #[test]
    fn daemon_settings_upsert_roundtrip() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());

        // First write.
        store
            .set_daemon_setting("test_key", "first_value")
            .expect("first set");
        let v1 = store.get_daemon_setting("test_key").unwrap();
        assert_eq!(v1.as_deref(), Some("first_value"));

        // Fetch the first updated_at timestamp.
        let ts1: String = store
            .conn
            .query_row(
                "SELECT updated_at FROM daemon_settings WHERE key = ?1",
                params!["test_key"],
                |row| row.get(0),
            )
            .expect("read updated_at");

        // Sleep briefly so the second write produces a strictly later RFC3339
        // timestamp (chrono::Utc::now is nanosecond-resolution but we want to
        // exercise the ON CONFLICT path without depending on sub-ms ticks).
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Second write — UPSERT must overwrite both columns.
        store
            .set_daemon_setting("test_key", "second_value")
            .expect("second set");
        let v2 = store.get_daemon_setting("test_key").unwrap();
        assert_eq!(v2.as_deref(), Some("second_value"));

        let ts2: String = store
            .conn
            .query_row(
                "SELECT updated_at FROM daemon_settings WHERE key = ?1",
                params!["test_key"],
                |row| row.get(0),
            )
            .expect("read updated_at");
        assert!(ts2 > ts1, "updated_at should advance: ts1={ts1}, ts2={ts2}");
    }

    #[test]
    fn runtime_config_field_persistence_roundtrip() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let config = crate::config::Config::from_env();
        let rc = crate::config::RuntimeConfig::from_config(&config);

        rc.update_field(
            "codex_sandbox_mode",
            &serde_json::json!("danger-full-access"),
        )
        .unwrap();
        let persisted = persist_runtime_config_field(&store, &rc, "codex_sandbox_mode")
            .expect("persist_runtime_config_field");
        assert!(persisted);
        assert_eq!(
            store
                .get_daemon_setting("codex_sandbox_mode")
                .unwrap()
                .as_deref(),
            Some("danger-full-access")
        );

        let restarted = crate::config::RuntimeConfig::from_config(&config);
        let applied = apply_persisted_runtime_config(&store, &restarted)
            .expect("apply_persisted_runtime_config");
        assert_eq!(applied, 1);
        assert_eq!(
            restarted
                .to_json()
                .get("codex_sandbox_mode")
                .and_then(|v| v.as_str()),
            Some("danger-full-access")
        );
    }

    #[test]
    fn runtime_config_bool_field_persistence_roundtrip() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let config = crate::config::Config::from_env();
        let rc = crate::config::RuntimeConfig::from_config(&config);

        rc.update_field("queue_enabled", &serde_json::json!(false))
            .unwrap();
        persist_runtime_config_field(&store, &rc, "queue_enabled")
            .expect("persist_runtime_config_field");
        assert_eq!(
            store
                .get_daemon_setting("queue_enabled")
                .unwrap()
                .as_deref(),
            Some("false")
        );

        let restarted = crate::config::RuntimeConfig::from_config(&config);
        apply_persisted_runtime_config(&store, &restarted).expect("apply_persisted_runtime_config");
        assert!(
            !restarted
                .queue_enabled
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[test]
    fn daemon_settings_restarts_target_cache_watermark_pairs_exactly() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let config = crate::config::Config::from_env();

        for (high, low) in [(50, 40), (95, 90)] {
            store
                .set_daemon_setting(
                    "sandbox_build_cache_reclaim_high_watermark_pct",
                    &high.to_string(),
                )
                .unwrap();
            store
                .set_daemon_setting(
                    "sandbox_build_cache_reclaim_low_watermark_pct",
                    &low.to_string(),
                )
                .unwrap();
            let restarted = crate::config::RuntimeConfig::from_config(&config);
            apply_persisted_runtime_config(&store, &restarted).unwrap();
            let snapshot = restarted.sandbox_build_cache_reclaim_snapshot();
            assert_eq!(
                (snapshot.high_watermark_pct, snapshot.low_watermark_pct),
                (high, low)
            );
        }
    }

    #[test]
    fn daemon_settings_rejects_incomplete_or_invalid_watermark_pairs_atomically() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let config = crate::config::Config::from_env();

        store
            .set_daemon_setting("sandbox_build_cache_reclaim_high_watermark_pct", "50")
            .unwrap();
        let restarted = crate::config::RuntimeConfig::from_config(&config);
        apply_persisted_runtime_config(&store, &restarted).unwrap();
        assert_eq!(
            restarted.sandbox_build_cache_reclaim_snapshot(),
            crate::config::RuntimeConfig::from_config(&config)
                .sandbox_build_cache_reclaim_snapshot()
        );

        store
            .set_daemon_setting("sandbox_build_cache_reclaim_low_watermark_pct", "60")
            .unwrap();
        let restarted = crate::config::RuntimeConfig::from_config(&config);
        apply_persisted_runtime_config(&store, &restarted).unwrap();
        let snapshot = restarted.sandbox_build_cache_reclaim_snapshot();
        assert_eq!(
            (snapshot.high_watermark_pct, snapshot.low_watermark_pct),
            (
                crate::config::SANDBOX_BUILD_CACHE_RECLAIM_HIGH_WATERMARK_PCT_DEFAULT,
                crate::config::SANDBOX_BUILD_CACHE_RECLAIM_LOW_WATERMARK_PCT_DEFAULT,
            )
        );
    }

    #[test]
    fn canonical_target_cache_setting_failure_leaves_existing_row_unchanged() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let field = "sandbox_build_cache_reclaim_ttl_secs";
        store.set_daemon_setting(field, "21600").unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER reject_target_cache_setting BEFORE UPDATE ON daemon_settings
                 WHEN NEW.key='sandbox_build_cache_reclaim_ttl_secs'
                 BEGIN SELECT RAISE(FAIL, 'injected durable failure'); END;",
            )
            .unwrap();
        assert!(persist_runtime_config_value(&store, field, &serde_json::json!(3600)).is_err());
        assert_eq!(
            store.get_daemon_setting(field).unwrap().as_deref(),
            Some("21600")
        );
    }

    #[test]
    fn import_legacy_system_prompt_preset_seeds_default_when_state_json_absent() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let missing = tmp.path().join("does-not-exist.json");
        let result = maybe_import_legacy_system_prompt_preset(&store, &missing)
            .expect("import succeeds with missing state.json");
        assert_eq!(result, "default");
        let row = store.get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET).unwrap();
        assert_eq!(row.as_deref(), Some("default"));
    }

    #[test]
    fn import_legacy_system_prompt_preset_imports_concise_from_state_json() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let state_path = tmp.path().join("state.json");
        let mut f = std::fs::File::create(&state_path).unwrap();
        writeln!(f, r#"{{"settings":{{"system_prompt_preset":"Concise"}}}}"#).unwrap();
        let result =
            maybe_import_legacy_system_prompt_preset(&store, &state_path).expect("import succeeds");
        assert_eq!(result, "concise");
        let row = store.get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET).unwrap();
        assert_eq!(row.as_deref(), Some("concise"));
    }

    #[test]
    fn import_legacy_system_prompt_preset_imports_caveman_alias() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let state_path = tmp.path().join("state.json");
        std::fs::write(
            &state_path,
            r#"{"settings":{"system_prompt_preset":"Caveman"}}"#,
        )
        .unwrap();
        let result =
            maybe_import_legacy_system_prompt_preset(&store, &state_path).expect("import succeeds");
        assert_eq!(result, "caveman");
    }

    #[test]
    fn import_legacy_system_prompt_preset_imports_code_only_label_alias() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let state_path = tmp.path().join("state.json");
        std::fs::write(
            &state_path,
            r#"{"settings":{"system_prompt_preset":"CodeOnly"}}"#,
        )
        .unwrap();
        // "CodeOnly" is the legacy serde-default label — normalized to "code-only".
        let result =
            maybe_import_legacy_system_prompt_preset(&store, &state_path).expect("import succeeds");
        assert_eq!(result, "code-only");
    }

    #[test]
    fn import_legacy_system_prompt_preset_is_idempotent() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let state_path = tmp.path().join("state.json");
        std::fs::write(
            &state_path,
            r#"{"settings":{"system_prompt_preset":"Concise"}}"#,
        )
        .unwrap();
        let first =
            maybe_import_legacy_system_prompt_preset(&store, &state_path).expect("first import");
        assert_eq!(first, "concise");

        // Mutate state.json. Second call must NOT re-import — the row exists.
        std::fs::write(
            &state_path,
            r#"{"settings":{"system_prompt_preset":"Caveman"}}"#,
        )
        .unwrap();
        let second =
            maybe_import_legacy_system_prompt_preset(&store, &state_path).expect("second import");
        assert_eq!(
            second, "concise",
            "second import must return existing row, not re-read state.json"
        );
        let row = store.get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET).unwrap();
        assert_eq!(row.as_deref(), Some("concise"));
    }

    #[test]
    fn import_legacy_system_prompt_preset_handles_corrupt_state_json() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let state_path = tmp.path().join("state.json");
        std::fs::write(&state_path, b"not-valid-json{][").unwrap();
        let result = maybe_import_legacy_system_prompt_preset(&store, &state_path)
            .expect("import does not error on corrupt state.json");
        assert_eq!(result, "default");
        let row = store.get_daemon_setting(KEY_SYSTEM_PROMPT_PRESET).unwrap();
        assert_eq!(row.as_deref(), Some("default"));
    }

    #[test]
    fn import_legacy_system_prompt_preset_handles_unknown_variant() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let state_path = tmp.path().join("state.json");
        std::fs::write(
            &state_path,
            r#"{"settings":{"system_prompt_preset":"FutureVariant"}}"#,
        )
        .unwrap();
        let result = maybe_import_legacy_system_prompt_preset(&store, &state_path)
            .expect("import succeeds with unknown variant");
        assert_eq!(result, "default");
    }
}
