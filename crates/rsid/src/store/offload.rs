//! Offloaded (compressed) content storage and retrieval.

use crate::error::Result;
use crate::store::Store;
use chrono::Utc;
use rsi_common::types::OffloadEntry;
use uuid::Uuid;

impl Store {
    /// Insert offloaded content for a session event.
    pub fn insert_offloaded_content(
        &self,
        session_id: Uuid,
        event_sequence: i32,
        content_hash: &str,
        original_content: &str,
    ) -> Result<i64> {
        let byte_size = original_content.len() as i64;
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);

        self.conn.execute(
            "INSERT INTO offloaded_content (session_id, event_sequence, content_hash, original_content, byte_size, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            [
                session_id.to_string(),
                event_sequence.to_string(),
                content_hash.to_string(),
                original_content.to_string(),
                byte_size.to_string(),
                now,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Retrieve offloaded content for a session event.
    pub fn get_offloaded_content(
        &self,
        session_id: Uuid,
        event_sequence: i32,
    ) -> Result<Option<String>> {
        let session_id_str = session_id.to_string();
        let seq_str = event_sequence.to_string();

        let result = self.conn.query_row(
            "SELECT original_content FROM offloaded_content WHERE session_id = ?1 AND event_sequence = ?2",
            [&session_id_str, &seq_str],
            |row| row.get::<_, String>(0),
        );

        match result {
            Ok(content) => Ok(Some(content)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Delete all offloaded content for a session (for cascade cleanup).
    pub fn delete_offloaded_for_session(&self, session_id: Uuid) -> Result<()> {
        self.conn.execute(
            "DELETE FROM offloaded_content WHERE session_id = ?1",
            [session_id.to_string()],
        )?;
        Ok(())
    }

    /// Count total bytes of offloaded content for a session.
    pub fn count_offloaded_bytes(&self, session_id: Uuid) -> Result<u64> {
        let session_id_str = session_id.to_string();
        let total: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(byte_size), 0) FROM offloaded_content WHERE session_id = ?1",
            [&session_id_str],
            |row| row.get(0),
        )?;
        Ok(total as u64)
    }

    /// Get metadata for all offloaded entries in a session.
    pub fn get_offloaded_entries(&self, session_id: Uuid) -> Result<Vec<OffloadEntry>> {
        let session_id_str = session_id.to_string();
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, event_sequence, byte_size, content_hash, created_at
             FROM offloaded_content
             WHERE session_id = ?1
             ORDER BY event_sequence ASC",
        )?;

        let entries = stmt.query_map([&session_id_str], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i32>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;

        let mut result = Vec::new();
        for entry_result in entries {
            let (id, session_id_str, event_sequence, byte_size, content_hash, created_at_str) =
                entry_result?;
            let session_uuid = Uuid::parse_str(&session_id_str)
                .map_err(|e| crate::error::DaemonError::Store(format!("Invalid UUID: {}", e)))?;
            let created_at = crate::store::parse_timestamp(&created_at_str)
                .map_err(|e| crate::error::DaemonError::Store(e))?;
            result.push(OffloadEntry {
                id,
                session_id: session_uuid,
                event_sequence,
                original_byte_size: byte_size as u64,
                content_hash,
                created_at,
            });
        }

        Ok(result)
    }
}
