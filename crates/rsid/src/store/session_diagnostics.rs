//! Bounded persistence for session-attributed daemon diagnostics.

use super::Store;
use crate::error::{DaemonError, Result};
use crate::store::parse_timestamp;
use chrono::SecondsFormat;
use rsi_common::types::{NewSessionDiagnosticV1, SessionDiagnosticLevelV1, SessionDiagnosticV1};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

pub const MAX_SESSION_DIAGNOSTIC_MESSAGE_BYTES: usize = 4 * 1024;
pub const MAX_SESSION_DIAGNOSTIC_FIELDS_BYTES: usize = 8 * 1024;
pub const MAX_SESSION_DIAGNOSTIC_PAGE_SIZE: u32 = 100;
pub const MAX_SESSION_DIAGNOSTICS_PER_SESSION: usize = 256;
pub const MAX_SESSION_DIAGNOSTICS_GLOBAL: usize = 10_000;

// RSI-RELEASED-MIGRATION-BEGIN: v127-session-diagnostics-schema
const SESSION_DIAGNOSTICS_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS session_diagnostics (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id  TEXT NOT NULL REFERENCES sessions(id),
        timestamp   TEXT NOT NULL,
        last_seen   TEXT NOT NULL,
        occurrence_count INTEGER NOT NULL DEFAULT 1 CHECK (occurrence_count > 0),
        level       TEXT NOT NULL CHECK (level IN ('WARN', 'ERROR')),
        message     TEXT NOT NULL,
        fields_json TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_session_diagnostics_session_id_id
        ON session_diagnostics(session_id, id);
    CREATE UNIQUE INDEX IF NOT EXISTS idx_session_diagnostics_coalesce
        ON session_diagnostics(session_id, level, message, ifnull(fields_json, ''));
    CREATE INDEX IF NOT EXISTS idx_session_diagnostics_last_seen
        ON session_diagnostics(last_seen, id);
";
// RSI-RELEASED-MIGRATION-END: v127-session-diagnostics-schema

// RSI-RELEASED-MIGRATION-BEGIN: v127-session-diagnostics-migration
pub(crate) fn apply_v127_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let active_version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if active_version != 126 {
        return Err(DaemonError::Store(format!(
            "V127 requires exact V126 source, found V{active_version}"
        )));
    }

    let existing_objects: i64 = tx.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name IN (
             'session_diagnostics',
             'idx_session_diagnostics_session_id_id',
             'idx_session_diagnostics_coalesce',
             'idx_session_diagnostics_last_seen'
         )",
        [],
        |row| row.get(0),
    )?;
    if existing_objects != 0 {
        return Err(DaemonError::Store(format!(
            "V127 expects diagnostics objects to be absent, found {existing_objects}"
        )));
    }

    tx.execute_batch(SESSION_DIAGNOSTICS_SCHEMA)?;
    tx.execute("PRAGMA user_version = 127", [])?;
    tx.commit()?;
    tracing::info!("V127 migration complete: session diagnostics storage");
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v127-session-diagnostics-migration

// RSI-RELEASED-MIGRATION-BEGIN: v127-session-diagnostics-catalog
pub(crate) const V127_CATALOG_OBJECTS: [(&str, &str); 4] = [
    ("table", "session_diagnostics"),
    ("index", "idx_session_diagnostics_session_id_id"),
    ("index", "idx_session_diagnostics_coalesce"),
    ("index", "idx_session_diagnostics_last_seen"),
];
// RSI-RELEASED-MIGRATION-END: v127-session-diagnostics-catalog

/// Install the schema in isolated tests.
pub(crate) fn install_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SESSION_DIAGNOSTICS_SCHEMA)?;
    Ok(())
}

#[cfg(test)]
impl Store {
    pub(crate) fn install_session_diagnostics_schema_for_test(&self) -> Result<()> {
        install_schema(&self.conn)
    }
}

impl Store {
    /// Append one diagnostic. Messages and optional fields are validated here
    /// so every producer receives the same persistence bounds.
    pub fn insert_session_diagnostic(&self, diagnostic: &NewSessionDiagnosticV1) -> Result<i64> {
        validate_diagnostic(diagnostic)?;
        let timestamp = diagnostic
            .timestamp
            .to_rfc3339_opts(SecondsFormat::Nanos, true);
        let level = level_to_str(diagnostic.level);
        let fields = diagnostic
            .fields
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(DaemonError::from)?;
        tx.execute(
            "INSERT INTO session_diagnostics
                 (session_id, timestamp, last_seen, occurrence_count, level, message, fields_json)
             VALUES (?1, ?2, ?2, 1, ?3, ?4, ?5)
             ON CONFLICT(session_id, level, message, ifnull(fields_json, ''))
             DO UPDATE SET occurrence_count = CASE
                               WHEN occurrence_count < 9223372036854775807
                               THEN occurrence_count + 1 ELSE occurrence_count END,
                           last_seen = max(last_seen, excluded.last_seen)",
            params![
                diagnostic.session_id.to_string(),
                timestamp,
                level,
                diagnostic.message,
                fields,
            ],
        )?;
        let id: i64 = tx.query_row(
            "SELECT id FROM session_diagnostics
             WHERE session_id = ?1 AND level = ?2 AND message = ?3
               AND ifnull(fields_json, '') = ifnull(?4, '')",
            params![
                diagnostic.session_id.to_string(),
                level,
                diagnostic.message,
                fields,
            ],
            |row| row.get(0),
        )?;
        tx.execute(
            "DELETE FROM session_diagnostics
             WHERE session_id = ?1 AND id NOT IN (
                 SELECT id FROM session_diagnostics WHERE session_id = ?1
                 ORDER BY last_seen DESC, id DESC LIMIT ?2
             )",
            params![
                diagnostic.session_id.to_string(),
                MAX_SESSION_DIAGNOSTICS_PER_SESSION
            ],
        )?;
        tx.execute(
            "DELETE FROM session_diagnostics WHERE id NOT IN (
                 SELECT id FROM session_diagnostics ORDER BY last_seen DESC, id DESC LIMIT ?1
             )",
            [MAX_SESSION_DIAGNOSTICS_GLOBAL],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Read a stable ascending page for one session. The caller must provide a
    /// page size in `1..=MAX_SESSION_DIAGNOSTIC_PAGE_SIZE`.
    pub fn list_session_diagnostics(
        &self,
        session_id: Uuid,
        after_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<SessionDiagnosticV1>> {
        if !(1..=MAX_SESSION_DIAGNOSTIC_PAGE_SIZE).contains(&limit) {
            return Err(DaemonError::InvalidParam(format!(
                "session diagnostic page limit must be between 1 and {MAX_SESSION_DIAGNOSTIC_PAGE_SIZE}"
            )));
        }
        if after_id.is_some_and(|id| id <= 0) {
            return Err(DaemonError::InvalidParam(
                "session diagnostic cursor must be positive".into(),
            ));
        }

        let exists = self
            .conn
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                params![session_id.to_string()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(DaemonError::SessionNotFound(session_id));
        }

        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, timestamp, last_seen, occurrence_count, level, message, fields_json
             FROM session_diagnostics
             WHERE session_id = ?1 AND (?2 IS NULL OR id > ?2)
             ORDER BY id ASC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![session_id.to_string(), after_id, i64::from(limit)],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            },
        )?;

        rows.map(|row| {
            let (
                id,
                session_id,
                timestamp,
                last_seen,
                occurrence_count,
                level,
                message,
                fields_json,
            ) = row?;
            let session_id = Uuid::parse_str(&session_id).map_err(|error| {
                DaemonError::Store(format!("invalid diagnostic session UUID: {error}"))
            })?;
            let timestamp = parse_timestamp(&timestamp).map_err(DaemonError::Store)?;
            let last_seen = parse_timestamp(&last_seen).map_err(DaemonError::Store)?;
            let level = str_to_level(&level)?;
            let fields = fields_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?;
            Ok(SessionDiagnosticV1 {
                id,
                session_id,
                timestamp,
                last_seen,
                occurrence_count,
                level,
                message,
                fields,
            })
        })
        .collect()
    }
}

fn validate_diagnostic(diagnostic: &NewSessionDiagnosticV1) -> Result<()> {
    if diagnostic.message.trim().is_empty()
        || diagnostic.message.len() > MAX_SESSION_DIAGNOSTIC_MESSAGE_BYTES
        || diagnostic.message.contains('\0')
    {
        return Err(DaemonError::InvalidParam(format!(
            "session diagnostic message must be nonempty, NUL-free, and at most {MAX_SESSION_DIAGNOSTIC_MESSAGE_BYTES} bytes"
        )));
    }
    if let Some(fields) = &diagnostic.fields {
        if !fields.is_object() {
            return Err(DaemonError::InvalidParam(
                "session diagnostic fields must be a JSON object".into(),
            ));
        }
        let encoded = serde_json::to_vec(fields)?;
        if encoded.len() > MAX_SESSION_DIAGNOSTIC_FIELDS_BYTES {
            return Err(DaemonError::InvalidParam(format!(
                "session diagnostic fields exceed {MAX_SESSION_DIAGNOSTIC_FIELDS_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

fn level_to_str(level: SessionDiagnosticLevelV1) -> &'static str {
    match level {
        SessionDiagnosticLevelV1::Warn => "WARN",
        SessionDiagnosticLevelV1::Error => "ERROR",
    }
}

fn str_to_level(level: &str) -> Result<SessionDiagnosticLevelV1> {
    match level {
        "WARN" => Ok(SessionDiagnosticLevelV1::Warn),
        "ERROR" => Ok(SessionDiagnosticLevelV1::Error),
        other => Err(DaemonError::Store(format!(
            "invalid session diagnostic level: {other}"
        ))),
    }
}
