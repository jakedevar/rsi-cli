//! Durable operator requests to change a session's model and effort at its
//! next provider turn boundary.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rsi_common::types::{SessionKind, SessionProvider, SessionStatus};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use uuid::Uuid;

// RSI-RELEASED-MIGRATION-BEGIN: v132-session-model-updates-schema
const SESSION_MODEL_UPDATES_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS session_model_updates (
        id TEXT PRIMARY KEY CHECK (length(id) = 36),
        session_id TEXT NOT NULL REFERENCES sessions(id),
        idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
        expected_model_invocation_id TEXT NOT NULL CHECK (length(expected_model_invocation_id) = 36),
        new_model TEXT NOT NULL CHECK (length(new_model) BETWEEN 1 AND 256),
        new_effort TEXT,
        state TEXT NOT NULL CHECK (state IN ('queued', 'applied', 'superseded', 'stale')),
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        applied_at TEXT,
        UNIQUE (session_id, idempotency_key),
        CHECK ((state = 'applied' AND applied_at IS NOT NULL)
            OR (state != 'applied' AND applied_at IS NULL))
    );
    CREATE UNIQUE INDEX IF NOT EXISTS idx_session_model_updates_one_queued
        ON session_model_updates(session_id) WHERE state = 'queued';
    CREATE INDEX IF NOT EXISTS idx_session_model_updates_session_created
        ON session_model_updates(session_id, created_at, id);
";
// RSI-RELEASED-MIGRATION-END: v132-session-model-updates-schema

// RSI-RELEASED-MIGRATION-BEGIN: v132-session-model-updates-migration
pub(crate) fn apply_v132_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if active_version != 131 {
        return Err(DaemonError::Store(format!(
            "V132 requires exact V131 source, found V{active_version}"
        )));
    }

    let existing_objects: i64 = tx.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name IN (
             'session_model_updates',
             'idx_session_model_updates_one_queued',
             'idx_session_model_updates_session_created'
         )",
        [],
        |row| row.get(0),
    )?;
    if existing_objects != 0 {
        return Err(DaemonError::Store(format!(
            "V132 expects model update objects to be absent, found {existing_objects}"
        )));
    }

    tx.execute_batch(SESSION_MODEL_UPDATES_SCHEMA)?;
    tx.execute("PRAGMA user_version = 132", [])?;
    tx.commit()?;
    tracing::info!("V132 migration complete: queued session model updates");
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v132-session-model-updates-migration

// RSI-RELEASED-MIGRATION-BEGIN: v132-session-model-updates-catalog
pub(crate) const V132_CATALOG_OBJECTS: [(&str, &str); 3] = [
    ("index", "idx_session_model_updates_one_queued"),
    ("index", "idx_session_model_updates_session_created"),
    ("table", "session_model_updates"),
];
// RSI-RELEASED-MIGRATION-END: v132-session-model-updates-catalog

pub(crate) fn install_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SESSION_MODEL_UPDATES_SCHEMA)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SessionModelUpdateReceipt {
    pub update_id: Uuid,
    pub session_id: Uuid,
    pub expected_model_invocation_id: Uuid,
    pub new_model: String,
    pub new_effort: Option<String>,
    pub state: String,
    pub created_at: String,
    pub applied_at: Option<String>,
    pub deduplicated: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct AppliedSessionModelUpdate {
    pub model: String,
    pub effort: Option<String>,
}

fn validate_target(provider: SessionProvider, model: &str, effort: Option<&str>) -> Result<()> {
    if model.trim().is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
        return Err(DaemonError::InvalidParam(
            "model must be a non-empty provider model identifier".into(),
        ));
    }

    let valid_effort = |allowed: &[&str]| {
        effort.is_none_or(|value| {
            allowed.contains(&value)
                && rsi_common::model_utils::known_effort_ladder(model)
                    .is_none_or(|ladder| ladder.contains(&value))
        })
    };
    let supported = match provider {
        SessionProvider::Claude => valid_effort(&["low", "medium", "high", "xhigh", "max"]),
        SessionProvider::Codex | SessionProvider::Pioneer => {
            valid_effort(&["low", "medium", "high", "xhigh", "max", "ultra"])
        }
        SessionProvider::Antigravity => valid_effort(&["low", "medium", "high"]),
        _ => {
            return Err(DaemonError::InvalidParam(
                "model/effort switching is not supported for this provider".into(),
            ));
        }
    };
    if !supported {
        return Err(DaemonError::InvalidParam(format!(
            "effort is not supported by {provider:?} model '{model}'"
        )));
    }
    Ok(())
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn receipt_from_row(
    row: &rusqlite::Row<'_>,
    deduplicated: bool,
) -> rusqlite::Result<SessionModelUpdateReceipt> {
    let update_id = row.get::<_, String>(0)?;
    let session_id = row.get::<_, String>(1)?;
    let expected_model_invocation_id = row.get::<_, String>(2)?;
    Ok(SessionModelUpdateReceipt {
        update_id: Uuid::parse_str(&update_id).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        session_id: Uuid::parse_str(&session_id).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        expected_model_invocation_id: Uuid::parse_str(&expected_model_invocation_id).map_err(
            |error| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            },
        )?,
        new_model: row.get(3)?,
        new_effort: row.get(4)?,
        state: row.get(5)?,
        created_at: row.get(6)?,
        applied_at: row.get(7)?,
        deduplicated,
    })
}

const RECEIPT_SELECT: &str = "SELECT id, session_id, expected_model_invocation_id,
    new_model, new_effort, state, created_at, applied_at
    FROM session_model_updates";

impl Store {
    /// Durably queue one model/effort tuple for the next turn. Retries with the
    /// same key and identical content return their original receipt.
    pub(crate) fn queue_session_model_update(
        &mut self,
        session_id: Uuid,
        expected_model_invocation_id: Uuid,
        new_model: &str,
        new_effort: Option<&str>,
        idempotency_key: &str,
    ) -> Result<SessionModelUpdateReceipt> {
        let model = new_model.trim();
        let idempotency_key = idempotency_key.trim();
        if idempotency_key.is_empty() || idempotency_key.len() > 128 {
            return Err(DaemonError::InvalidParam(
                "idempotency_key must contain 1 to 128 bytes".into(),
            ));
        }

        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

        let existing = tx
            .query_row(
                &format!("{RECEIPT_SELECT} WHERE session_id = ?1 AND idempotency_key = ?2"),
                params![session_id.to_string(), idempotency_key],
                |row| receipt_from_row(row, true),
            )
            .optional()?;
        if let Some(receipt) = existing {
            if receipt.expected_model_invocation_id != expected_model_invocation_id
                || receipt.new_model != model
                || receipt.new_effort.as_deref() != new_effort
            {
                return Err(DaemonError::InvalidParam(
                    "idempotency_key was reused with different model update content".into(),
                ));
            }
            tx.commit()?;
            return Ok(receipt);
        }

        let (provider, kind, status, current_model, current_effort, invocation): (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = tx
            .query_row(
                "SELECT provider, session_kind, status, model, effort, model_invocation_id
                   FROM sessions WHERE id = ?1",
                params![session_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or(DaemonError::SessionNotFound(session_id))?;
        let provider = serde_json::from_str::<SessionProvider>(&serde_json::to_string(&provider)?)
            .map_err(|error| DaemonError::Store(format!("invalid stored provider: {error}")))?;
        let kind = serde_json::from_str::<SessionKind>(&serde_json::to_string(&kind)?)
            .map_err(|error| DaemonError::Store(format!("invalid stored session kind: {error}")))?;
        let status = serde_json::from_str::<SessionStatus>(&serde_json::to_string(&status)?)
            .map_err(|error| {
                DaemonError::Store(format!("invalid stored session status: {error}"))
            })?;
        if !rsi_common::is_leaf_kind(kind) {
            return Err(DaemonError::InvalidParam(
                "model/effort switching requires a spawnable leaf session".into(),
            ));
        }
        if !matches!(
            status,
            SessionStatus::Running
                | SessionStatus::Completed
                | SessionStatus::Interrupted
                | SessionStatus::Failed
        ) {
            return Err(DaemonError::InvalidParam(
                "model/effort switching requires a running or terminal session".into(),
            ));
        }
        let invocation = invocation
            .and_then(|raw| Uuid::parse_str(&raw).ok())
            .ok_or_else(|| {
                DaemonError::InvalidParam(
                    "session has no model invocation to fence this update".into(),
                )
            })?;
        if invocation != expected_model_invocation_id {
            return Err(DaemonError::InvalidParam(
                "session model invocation changed; refresh and retry".into(),
            ));
        }
        validate_target(provider, model, new_effort)?;

        let created_at = now();
        let update_id = Uuid::new_v4();
        let same_tuple =
            current_model.as_deref() == Some(model) && current_effort.as_deref() == new_effort;
        let state = if same_tuple { "applied" } else { "queued" };
        let applied_at = same_tuple.then_some(created_at.as_str());

        // A newer operator choice explicitly supersedes an older pending one;
        // historical receipts remain durable for exact retry and diagnosis.
        tx.execute(
            "UPDATE session_model_updates
                SET state = 'superseded', updated_at = ?1
              WHERE session_id = ?2 AND state = 'queued'",
            params![created_at, session_id.to_string()],
        )?;
        tx.execute(
            "INSERT INTO session_model_updates
                (id, session_id, idempotency_key, expected_model_invocation_id,
                 new_model, new_effort, state, created_at, updated_at, applied_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)",
            params![
                update_id.to_string(),
                session_id.to_string(),
                idempotency_key,
                expected_model_invocation_id.to_string(),
                model,
                new_effort,
                state,
                created_at,
                applied_at,
            ],
        )?;
        let receipt = tx.query_row(
            &format!("{RECEIPT_SELECT} WHERE id = ?1"),
            params![update_id.to_string()],
            |row| receipt_from_row(row, false),
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Apply the pending tuple atomically with the model-segment boundary.
    /// A changed invocation marks the request stale and leaves session metadata
    /// untouched.
    pub(crate) fn apply_pending_session_model_update(
        &mut self,
        session_id: Uuid,
        expected_model_invocation_id: Uuid,
        from_sequence: i32,
    ) -> Result<Option<AppliedSessionModelUpdate>> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let pending: Option<(String, String, String, Option<String>)> = tx
            .query_row(
                "SELECT id, expected_model_invocation_id, new_model, new_effort
                   FROM session_model_updates
                  WHERE session_id = ?1 AND state = 'queued'",
                params![session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((update_id, queued_invocation, new_model, new_effort)) = pending else {
            tx.commit()?;
            return Ok(None);
        };

        let current_invocation: Option<String> = tx.query_row(
            "SELECT model_invocation_id FROM sessions WHERE id = ?1",
            params![session_id.to_string()],
            |row| row.get(0),
        )?;
        let current_invocation = current_invocation
            .and_then(|raw| Uuid::parse_str(&raw).ok())
            .ok_or_else(|| DaemonError::Store("session model invocation is malformed".into()))?;
        if current_invocation != expected_model_invocation_id
            || queued_invocation != expected_model_invocation_id.to_string()
        {
            tx.execute(
                "UPDATE session_model_updates
                    SET state = 'stale', updated_at = ?1
                  WHERE id = ?2 AND state = 'queued'",
                params![now(), update_id],
            )?;
            tx.commit()?;
            return Ok(None);
        }

        let current_model: Option<String> = tx.query_row(
            "SELECT model FROM sessions WHERE id = ?1",
            params![session_id.to_string()],
            |row| row.get(0),
        )?;
        let applied_at = now();
        tx.execute(
            "UPDATE sessions SET model = ?1, effort = ?2, updated_at = ?3
              WHERE id = ?4 AND model_invocation_id = ?5",
            params![
                new_model,
                new_effort,
                applied_at,
                session_id.to_string(),
                expected_model_invocation_id.to_string(),
            ],
        )?;
        if current_model.as_deref() != Some(new_model.as_str()) {
            tx.execute(
                "UPDATE model_segments
                    SET to_sequence = ?1
                  WHERE session_id = ?2 AND to_sequence IS NULL",
                params![from_sequence.saturating_sub(1), session_id.to_string()],
            )?;
            tx.execute(
                "INSERT INTO model_segments
                    (session_id, model_id, from_sequence, to_sequence, created_at)
                 VALUES (?1, ?2, ?3, NULL, ?4)",
                params![session_id.to_string(), new_model, from_sequence, applied_at],
            )?;
        }
        tx.execute(
            "UPDATE session_model_updates
                SET state = 'applied', updated_at = ?1, applied_at = ?1
              WHERE id = ?2 AND state = 'queued'",
            params![applied_at, update_id],
        )?;
        tx.commit()?;
        Ok(Some(AppliedSessionModelUpdate {
            model: new_model,
            effort: new_effort,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::make_test_session;

    fn store_with_running_session() -> (tempfile::TempDir, Store, Uuid, Uuid) {
        store_with_running_provider(SessionProvider::Codex)
    }

    fn store_with_running_provider(
        provider: SessionProvider,
    ) -> (tempfile::TempDir, Store, Uuid, Uuid) {
        let dir = tempfile::tempdir().expect("create temp directory");
        let store = Store::open(&dir.path().join("model-updates.db")).expect("open test store");
        let invocation_id = Uuid::new_v4();
        let mut session = make_test_session();
        session.status = SessionStatus::Running;
        session.model = Some(
            match provider {
                SessionProvider::Antigravity => "gemini-3.6-flash",
                _ => "claude-sonnet-4-5",
            }
            .to_string(),
        );
        session.provider = provider;
        session.project_id = None;
        store.insert_session(&session).expect("insert test session");
        store
            .set_session_model_invocation(session.id, Some(invocation_id))
            .expect("set invocation fence");
        (dir, store, session.id, invocation_id)
    }

    #[test]
    fn queued_update_is_idempotent_and_applies_at_the_next_sequence() {
        let (_dir, mut store, session_id, invocation_id) = store_with_running_session();
        let first = store
            .queue_session_model_update(
                session_id,
                invocation_id,
                "claude-opus-4-1",
                Some("high"),
                "switch-1",
            )
            .expect("queue model update");
        let retry = store
            .queue_session_model_update(
                session_id,
                invocation_id,
                "claude-opus-4-1",
                Some("high"),
                "switch-1",
            )
            .expect("retry model update");

        assert_eq!(first.update_id, retry.update_id);
        assert!(retry.deduplicated);
        assert_eq!(retry.state, "queued");

        let applied = store
            .apply_pending_session_model_update(session_id, invocation_id, 7)
            .expect("apply queued update")
            .expect("pending update exists");
        assert_eq!(applied.model, "claude-opus-4-1");
        assert_eq!(applied.effort.as_deref(), Some("high"));

        let session = store
            .get_session(session_id)
            .expect("read updated session")
            .expect("session exists");
        assert_eq!(session.model.as_deref(), Some("claude-opus-4-1"));
        assert_eq!(session.effort.as_deref(), Some("high"));
        let segment_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM model_segments WHERE session_id = ?1",
                params![session_id.to_string()],
                |row| row.get(0),
            )
            .expect("count model segments");
        assert_eq!(segment_count, 1);
    }

    #[test]
    fn antigravity_model_switch_accepts_supported_effort_and_applies_it() {
        let (_dir, mut store, session_id, invocation_id) =
            store_with_running_provider(SessionProvider::Antigravity);

        let rejected = store.queue_session_model_update(
            session_id,
            invocation_id,
            "gemini-3.6-flash",
            Some("ultra"),
            "antigravity-effort-rejected",
        );
        assert!(matches!(rejected, Err(DaemonError::InvalidParam(_))));

        let accepted = store
            .queue_session_model_update(
                session_id,
                invocation_id,
                "gemini-3.6-pro",
                Some("high"),
                "antigravity-model-effort",
            )
            .expect("model and effort update is supported by Antigravity");
        assert_eq!(accepted.state, "queued");
        let applied = store
            .apply_pending_session_model_update(session_id, invocation_id, 2)
            .expect("apply queued Antigravity update")
            .expect("pending update exists");
        assert_eq!(applied.model, "gemini-3.6-pro");
        assert_eq!(applied.effort.as_deref(), Some("high"));
        let session = store
            .get_session(session_id)
            .expect("read updated Antigravity session")
            .expect("session exists");
        assert_eq!(session.model.as_deref(), Some("gemini-3.6-pro"));
        assert_eq!(session.effort.as_deref(), Some("high"));
    }

    #[test]
    fn stale_invocation_marks_the_pending_update_stale_without_applying_it() {
        let (_dir, mut store, session_id, invocation_id) = store_with_running_session();
        let receipt = store
            .queue_session_model_update(
                session_id,
                invocation_id,
                "claude-opus-4-1",
                Some("high"),
                "switch-stale",
            )
            .expect("queue model update");
        let later_invocation = Uuid::new_v4();
        store
            .set_session_model_invocation(session_id, Some(later_invocation))
            .expect("advance invocation fence");

        assert!(
            store
                .apply_pending_session_model_update(session_id, later_invocation, 3)
                .expect("stale update is settled")
                .is_none()
        );
        let state: String = store
            .conn
            .query_row(
                "SELECT state FROM session_model_updates WHERE id = ?1",
                params![receipt.update_id.to_string()],
                |row| row.get(0),
            )
            .expect("read stale state");
        assert_eq!(state, "stale");
        let session = store
            .get_session(session_id)
            .expect("read unchanged session")
            .expect("session exists");
        assert_eq!(session.model.as_deref(), Some("claude-sonnet-4-5"));
    }

    #[test]
    fn target_validation_respects_provider_effort_ladders() {
        assert!(validate_target(SessionProvider::Antigravity, "gemini-3.6-flash", None).is_ok());
        for effort in ["low", "medium", "high"] {
            assert!(
                validate_target(
                    SessionProvider::Antigravity,
                    "gemini-3.6-flash",
                    Some(effort)
                )
                .is_ok()
            );
        }
        assert!(
            validate_target(
                SessionProvider::Antigravity,
                "gemini-3.6-flash",
                Some("ultra")
            )
            .is_err()
        );
        assert!(
            validate_target(SessionProvider::Claude, "claude-opus-4-1", Some("ultra")).is_err()
        );
    }

    #[test]
    fn v132_migration_recreates_the_pinned_catalog() {
        let (_dir, store, _session_id, _invocation_id) = store_with_running_session();
        store
            .conn
            .execute_batch(
                "DROP INDEX idx_session_model_updates_one_queued;
                 DROP INDEX idx_session_model_updates_session_created;
                 DROP TABLE session_model_updates;
                 PRAGMA user_version = 131;",
            )
            .expect("rewind V132 fixture");

        apply_v132_migration(&store).expect("replay V132 migration");
        for (kind, name) in V132_CATALOG_OBJECTS {
            let count: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
                    params![kind, name],
                    |row| row.get(0),
                )
                .expect("query V132 catalog object");
            assert_eq!(count, 1, "V132 installs {kind} {name}");
        }
        let version: i32 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read schema version");
        assert_eq!(version, 132);
    }
}
