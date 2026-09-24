//! chat.db polling and inbound message extraction.
//!
//! Opens macOS `~/Library/Messages/chat.db` read-only and polls for new
//! messages using `ROWID > last_seen`. Skips `is_from_me` messages.

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// An inbound message extracted from chat.db.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InboundMessage {
    pub rowid: i64,
    pub text: String,
    pub sender: String,
    pub chat_id: Option<i64>,
    pub chat_identifier: Option<String>,
    pub is_group: bool,
    pub group_name: Option<String>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChatDbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("Failed to open chat.db: {0}. Ensure Full Disk Access is granted to your terminal.")]
    AccessDenied(String),
}

/// Read-only handle to macOS chat.db.
pub struct ChatDb {
    conn: Connection,
}

impl ChatDb {
    /// Open chat.db in read-only mode.
    pub fn open(path: &Path) -> Result<Self, ChatDbError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| ChatDbError::AccessDenied(e.to_string()))?;

        // Enable WAL mode for non-blocking reads
        conn.pragma_update(None, "journal_mode", "wal")
            .unwrap_or_default();

        Ok(Self { conn })
    }

    /// Poll for new messages with ROWID > last_seen.
    /// Only returns messages NOT from the current user (is_from_me = 0).
    pub fn poll(&self, last_seen_rowid: i64) -> Result<Vec<InboundMessage>, ChatDbError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.ROWID, m.text, m.is_from_me, m.date, m.handle_id, \
             h.id as sender, c.ROWID as chat_id, c.chat_identifier, \
             c.display_name, c.group_id \
             FROM message m \
             LEFT JOIN handle h ON m.handle_id = h.ROWID \
             LEFT JOIN chat_message_join cmj ON m.ROWID = cmj.message_id \
             LEFT JOIN chat c ON cmj.chat_id = c.ROWID \
             WHERE m.ROWID > ?1 \
             ORDER BY m.ROWID ASC",
        )?;

        let rows = stmt.query_map([last_seen_rowid], |row| {
            let is_from_me: i32 = row.get(2)?;
            let text: Option<String> = row.get(1)?;
            let sender: Option<String> = row.get(5)?;
            let chat_id: Option<i64> = row.get(6)?;
            let chat_identifier: Option<String> = row.get(7)?;
            let display_name: Option<String> = row.get(8)?;
            let group_id: Option<String> = row.get(9)?;
            let date: Option<i64> = row.get(3)?;

            Ok(RawRow {
                rowid: row.get(0)?,
                text,
                is_from_me,
                sender,
                chat_id,
                chat_identifier,
                display_name,
                group_id,
                date,
            })
        })?;

        let mut messages = Vec::new();
        for row_result in rows {
            let row = row_result?;

            // Skip own messages
            if row.is_from_me == 1 {
                continue;
            }

            // Must have text content
            let text = match row.text {
                Some(t) if !t.is_empty() => t,
                _ => continue,
            };

            let sender = row.sender.unwrap_or_else(|| "unknown".to_string());
            let is_group = row.group_id.is_some();
            let timestamp = apple_date_to_utc(row.date.unwrap_or(0));

            messages.push(InboundMessage {
                rowid: row.rowid,
                text,
                sender,
                chat_id: row.chat_id,
                chat_identifier: row.chat_identifier,
                is_group,
                group_name: row.display_name,
                timestamp,
            });
        }

        Ok(messages)
    }

    /// Get the current maximum ROWID in the message table.
    /// Used for initial state to avoid processing historical messages.
    #[allow(dead_code)]
    pub fn current_max_rowid(&self) -> Result<i64, ChatDbError> {
        let rowid: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(ROWID), 0) FROM message", [], |row| {
                    row.get(0)
                })?;
        Ok(rowid)
    }
}

/// Intermediate struct for the raw query row.
struct RawRow {
    rowid: i64,
    text: Option<String>,
    is_from_me: i32,
    sender: Option<String>,
    chat_id: Option<i64>,
    chat_identifier: Option<String>,
    display_name: Option<String>,
    group_id: Option<String>,
    date: Option<i64>,
}

/// Convert Apple's `date` column (nanoseconds since 2001-01-01) to UTC DateTime.
/// macOS Messages.app uses "Apple Cocoa epoch" — 2001-01-01 00:00:00 UTC.
fn apple_date_to_utc(apple_ns: i64) -> DateTime<Utc> {
    // Apple epoch offset: seconds between Unix epoch and 2001-01-01
    const APPLE_EPOCH_OFFSET: i64 = 978_307_200;

    // date column is in nanoseconds since 2001-01-01
    let secs = apple_ns / 1_000_000_000;
    let nanos = (apple_ns % 1_000_000_000) as u32;

    Utc.timestamp_opt(secs + APPLE_EPOCH_OFFSET, nanos)
        .single()
        .unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    #[test]
    fn test_apple_date_to_utc() {
        // 2024-01-01 00:00:00 UTC in Apple nanoseconds
        // = (2024-01-01 - 2001-01-01) in seconds * 1_000_000_000
        let secs_since_apple_epoch: i64 = 725_846_400; // 23 years in seconds (approx)
        let apple_ns = secs_since_apple_epoch * 1_000_000_000;
        let dt = apple_date_to_utc(apple_ns);
        assert_eq!(dt.year(), 2024);
    }

    #[test]
    fn test_apple_date_zero() {
        // Zero date should map to 2001-01-01 00:00:00 UTC
        let dt = apple_date_to_utc(0);
        assert_eq!(dt.year(), 2001);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 1);
    }

    #[test]
    fn test_inbound_message_fields() {
        let msg = InboundMessage {
            rowid: 42,
            text: "Hello world".to_string(),
            sender: "+15551234567".to_string(),
            chat_id: Some(1),
            chat_identifier: Some("iMessage;-;+15551234567".to_string()),
            is_group: false,
            group_name: None,
            timestamp: Utc::now(),
        };
        assert_eq!(msg.rowid, 42);
        assert_eq!(msg.text, "Hello world");
        assert!(!msg.is_group);
    }
}
