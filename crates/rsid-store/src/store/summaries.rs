//! Session summary persistence operations.

use super::Store;
use super::row_mappers::parse_timestamp;
use crate::error::{DaemonError, Result};
use rsi_common::types::{SessionSummary, SummaryKind};
use rusqlite::params;
use uuid::Uuid;

fn summary_kind_to_str(kind: SummaryKind) -> &'static str {
    match kind {
        SummaryKind::Short => "Short",
        SummaryKind::Long => "Long",
        _ => "Short",
    }
}

fn str_to_summary_kind(s: &str) -> Result<SummaryKind> {
    match s {
        "Short" => Ok(SummaryKind::Short),
        "Long" => Ok(SummaryKind::Long),
        _ => Err(DaemonError::Store(format!("Unknown SummaryKind: {}", s))),
    }
}

fn map_summary_row(row: &rusqlite::Row) -> rusqlite::Result<SummaryRow> {
    Ok(SummaryRow {
        id: row.get(0)?,
        session_id_str: row.get(1)?,
        kind_str: row.get(2)?,
        content: row.get(3)?,
        covers_through_sequence: row.get(4)?,
        token_count: row.get(5)?,
        created_at_str: row.get(6)?,
    })
}

struct SummaryRow {
    id: i64,
    session_id_str: String,
    kind_str: String,
    content: String,
    covers_through_sequence: i32,
    token_count: i32,
    created_at_str: String,
}

impl SummaryRow {
    fn into_summary(self) -> Result<SessionSummary> {
        let session_id = Uuid::parse_str(&self.session_id_str)
            .map_err(|e| DaemonError::Store(format!("Invalid summary session UUID: {}", e)))?;
        let kind = str_to_summary_kind(&self.kind_str)?;
        let created_at = parse_timestamp(&self.created_at_str).map_err(DaemonError::Store)?;

        Ok(SessionSummary {
            id: self.id,
            session_id,
            kind,
            content: self.content,
            covers_through_sequence: self.covers_through_sequence,
            token_count: self.token_count as u32,
            created_at,
        })
    }
}

impl Store {
    /// Insert a new session summary. Returns the auto-increment row ID.
    pub fn insert_session_summary(&self, summary: &SessionSummary) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO session_summaries (session_id, kind, content, covers_through_sequence, token_count, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                summary.session_id.to_string(),
                summary_kind_to_str(summary.kind),
                summary.content,
                summary.covers_through_sequence,
                summary.token_count as i32,
                summary.created_at.to_rfc3339(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get the latest summary of a given kind for a session.
    pub fn get_latest_summary(
        &self,
        session_id: Uuid,
        kind: SummaryKind,
    ) -> Result<Option<SessionSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, kind, content, covers_through_sequence, token_count, created_at
             FROM session_summaries
             WHERE session_id = ?1 AND kind = ?2
             ORDER BY created_at DESC
             LIMIT 1",
        )?;

        let mut rows = stmt.query_map(
            params![session_id.to_string(), summary_kind_to_str(kind)],
            map_summary_row,
        )?;

        match rows.next() {
            Some(Ok(row)) => Ok(Some(row.into_summary()?)),
            Some(Err(e)) => Err(DaemonError::Database(e)),
            None => Ok(None),
        }
    }

    /// Get both latest short and long summaries for a session.
    pub fn get_latest_summaries(
        &self,
        session_id: Uuid,
    ) -> Result<(Option<SessionSummary>, Option<SessionSummary>)> {
        let short = self.get_latest_summary(session_id, SummaryKind::Short)?;
        let long = self.get_latest_summary(session_id, SummaryKind::Long)?;
        Ok((short, long))
    }

    /// Get the latest short summary content for a session.
    /// Used for `Session.short_summary` denormalization at query time.
    pub fn get_short_summary_content(&self, session_id: Uuid) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT content FROM session_summaries
             WHERE session_id = ?1 AND kind = 'Short'
             ORDER BY created_at DESC
             LIMIT 1",
        )?;

        let mut rows = stmt.query_map(params![session_id.to_string()], |row| {
            row.get::<_, String>(0)
        })?;

        match rows.next() {
            Some(Ok(content)) => Ok(Some(content)),
            Some(Err(e)) => Err(DaemonError::Database(e)),
            None => Ok(None),
        }
    }

    /// Batch-load short summary content for multiple sessions.
    /// Returns a map of session_id -> short summary content.
    /// Uses a single query with IN clause for efficiency (avoids N+1).
    pub fn batch_get_short_summaries(
        &self,
        session_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, String>> {
        if session_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // SQLite doesn't support array parameters, so we use a subquery pattern.
        // For each session, get the latest short summary via a correlated subquery.
        let placeholders: Vec<String> = session_ids.iter().map(|_| "?".to_string()).collect();
        let sql = format!(
            "SELECT ss.session_id, ss.content
             FROM session_summaries ss
             INNER JOIN (
                 SELECT session_id, MAX(created_at) as max_created
                 FROM session_summaries
                 WHERE kind = 'Short' AND session_id IN ({})
                 GROUP BY session_id
             ) latest ON ss.session_id = latest.session_id AND ss.created_at = latest.max_created
             WHERE ss.kind = 'Short'",
            placeholders.join(",")
        );

        let mut stmt = self.conn.prepare(&sql)?;

        let params: Vec<String> = session_ids.iter().map(|id| id.to_string()).collect();
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();

        let mut result = std::collections::HashMap::new();
        let mut rows = stmt.query(param_refs.as_slice())?;
        while let Some(row) = rows.next()? {
            let session_id_str: String = row.get(0)?;
            let content: String = row.get(1)?;
            if let Ok(session_id) = Uuid::parse_str(&session_id_str) {
                result.insert(session_id, content);
            }
        }

        Ok(result)
    }

    /// Count assistant message events for a session.
    /// Used to initialize assistant_message_count for continued sessions.
    pub fn count_assistant_messages(&self, session_id: Uuid) -> Result<i32> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM conversation_events
             WHERE session_id = ?1 AND event_type = 'Message' AND role = 'Assistant'",
            params![session_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(count as i32)
    }
}
