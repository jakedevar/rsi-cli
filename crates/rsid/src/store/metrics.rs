//! Turn metrics, context snapshots, and model segment persistence operations.

use super::Store;
use super::row_mappers::{ModelSegmentRow, TurnMetricRow};
use crate::error::{DaemonError, Result};
use rsi_common::types::{ModelSegment, TurnMetric};
use rusqlite::params;
use uuid::Uuid;

impl Store {
    /// Insert a turn metric. Returns the assigned auto-increment ID.
    pub fn insert_turn_metric(&self, metric: &TurnMetric) -> Result<i64> {
        let tools_json = metric
            .tools_used
            .as_ref()
            .map(|t| serde_json::to_string(t))
            .transpose()
            .map_err(|e| DaemonError::Store(format!("failed to serialize tools_used: {}", e)))?;

        self.conn.execute(
            "INSERT INTO turn_metrics (session_id, turn_number, input_tokens,
             cache_creation_tokens, cache_read_tokens, output_tokens,
             stop_reason, tools_used, tool_count, created_at, model,
             thinking_tokens, cache_creation_1h_tokens, cache_creation_5m_tokens, service_tier)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                metric.session_id.to_string(),
                metric.turn_number,
                metric.input_tokens as i64,
                metric.cache_creation_tokens as i64,
                metric.cache_read_tokens as i64,
                metric.output_tokens as i64,
                metric.stop_reason,
                tools_json,
                metric.tool_count as i32,
                metric.created_at.to_rfc3339(),
                metric.model,
                metric.thinking_tokens as i64,
                metric.cache_creation_1h_tokens as i64,
                metric.cache_creation_5m_tokens as i64,
                metric.service_tier,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Load all turn metrics for a session, ordered by turn number.
    pub fn load_turn_metrics(&self, session_id: Uuid) -> Result<Vec<TurnMetric>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, turn_number, input_tokens, cache_creation_tokens,
                    cache_read_tokens, output_tokens, stop_reason, tools_used, tool_count, created_at, model,
                    thinking_tokens, cache_creation_1h_tokens, cache_creation_5m_tokens, service_tier
             FROM turn_metrics WHERE session_id = ?1 ORDER BY turn_number ASC",
        )?;

        let rows = stmt
            .query_map(params![session_id.to_string()], |row| {
                Ok(TurnMetricRow {
                    id: row.get(0)?,
                    session_id_str: row.get(1)?,
                    turn_number: row.get(2)?,
                    input_tokens: row.get(3)?,
                    cache_creation_tokens: row.get(4)?,
                    cache_read_tokens: row.get(5)?,
                    output_tokens: row.get(6)?,
                    stop_reason: row.get(7)?,
                    tools_used_json: row.get(8)?,
                    tool_count: row.get(9)?,
                    created_at_str: row.get(10)?,
                    model: row.get(11)?,
                    thinking_tokens: row.get(12)?,
                    cache_creation_1h_tokens: row.get(13)?,
                    cache_creation_5m_tokens: row.get(14)?,
                    service_tier: row.get(15)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter().map(|row| row.into_turn_metric()).collect()
    }

    /// Insert a context usage snapshot for crash recovery.
    pub fn insert_context_snapshot(&self, session_id: Uuid, tokens_used: u64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO context_snapshots (session_id, tokens_used, created_at)
             VALUES (?1, ?2, ?3)",
            params![
                session_id.to_string(),
                tokens_used as i64,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Load the latest context snapshot for a session.
    /// Returns (tokens_used,) or None if no snapshots exist.
    pub fn load_latest_context_snapshot(&self, session_id: Uuid) -> Result<Option<u64>> {
        let result = self.conn.query_row(
            "SELECT tokens_used FROM context_snapshots
             WHERE session_id = ?1 ORDER BY created_at DESC LIMIT 1",
            params![session_id.to_string()],
            |row| row.get::<_, i64>(0),
        );
        match result {
            Ok(tokens) => Ok(Some(tokens as u64)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Load all model segments for a session, ordered by from_sequence ASC.
    pub fn load_model_segments(&self, session_id: Uuid) -> Result<Vec<ModelSegment>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, model_id, from_sequence, to_sequence, created_at
             FROM model_segments
             WHERE session_id = ?1
             ORDER BY from_sequence ASC",
        )?;

        let rows = stmt
            .query_map(params![session_id.to_string()], |row| {
                Ok(ModelSegmentRow {
                    id: row.get(0)?,
                    session_id_str: row.get(1)?,
                    model_id: row.get(2)?,
                    from_sequence: row.get(3)?,
                    to_sequence: row.get(4)?,
                    created_at_str: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|row| row.into_model_segment())
            .collect()
    }

    /// Get the model for a specific event sequence number.
    /// Returns None if no segment covers that sequence.
    pub fn get_model_at_sequence(&self, session_id: Uuid, sequence: i32) -> Result<Option<String>> {
        let result = self.conn.query_row(
            "SELECT model_id FROM model_segments
             WHERE session_id = ?1
               AND from_sequence <= ?2
               AND (to_sequence IS NULL OR to_sequence >= ?2)
             LIMIT 1",
            params![session_id.to_string(), sequence],
            |row| row.get(0),
        );

        match result {
            Ok(model) => Ok(Some(model)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Record a new model segment, closing the previous active segment.
    /// Returns the new segment ID.
    pub fn create_model_segment(
        &self,
        session_id: Uuid,
        model_id: &str,
        from_sequence: i32,
    ) -> Result<i64> {
        let tx = self.conn.unchecked_transaction()?;

        // Close previous active segment by setting to_sequence = from_sequence - 1
        tx.execute(
            "UPDATE model_segments
             SET to_sequence = ?1
             WHERE session_id = ?2 AND to_sequence IS NULL",
            params![from_sequence - 1, session_id.to_string()],
        )?;

        // Insert new active segment
        let now = chrono::Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO model_segments (session_id, model_id, from_sequence, to_sequence, created_at)
             VALUES (?1, ?2, ?3, NULL, ?4)",
            params![session_id.to_string(), model_id, from_sequence, now],
        )?;

        let new_id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(new_id)
    }
}
