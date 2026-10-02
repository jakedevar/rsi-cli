use super::{BoundedText, ReadError, Result, bounded_column, require_session_project};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SessionDetailSource {
    pub own_title: BoundedText,
    pub query: Option<BoundedText>,
    pub model: Option<BoundedText>,
    /// Latest saved event key. Runtime sequence observations are separate.
    pub saved_history_head: Option<(i32, i64)>,
}

/// Read only selected saved detail fields. Large TEXT columns are fetched via
/// bounded incremental BLOB reads inside the caller's read transaction.
pub fn session_detail_source(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
) -> Result<SessionDetailSource> {
    require_session_project(conn, project, session)?;
    let (rowid, has_title, has_query, has_model): (i64, bool, bool, bool) = conn
        .query_row(
            "SELECT rowid,title IS NOT NULL,query IS NOT NULL,model IS NOT NULL
             FROM sessions WHERE id=?1 AND project_id=?2",
            params![session.to_string(), project.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?
        .ok_or(ReadError::NotFound)?;
    let query = has_query
        .then(|| bounded_column(conn, "sessions", "query", rowid, 4096))
        .transpose()?;
    let model = has_model
        .then(|| bounded_column(conn, "sessions", "model", rowid, 4096))
        .transpose()?;
    let title = has_title
        .then(|| bounded_column(conn, "sessions", "title", rowid, 4096))
        .transpose()?;
    let own_title = title
        .filter(|title| !title.text.is_empty())
        .or_else(|| {
            query
                .as_ref()
                .filter(|query| !query.text.is_empty())
                .cloned()
        })
        .unwrap_or(BoundedText {
            text: "Untitled session".into(),
            observed_bytes: 16,
            truncated: false,
        });
    let saved_history_head = conn
        .query_row(
            "SELECT sequence,id FROM conversation_events WHERE session_id=?1
             ORDER BY sequence DESC,id DESC LIMIT 1",
            [session.to_string()],
            |row| Ok((row.get::<_, i32>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    if saved_history_head.is_some_and(|(_, id)| id <= 0) {
        return Err(ReadError::InvalidSource);
    }
    Ok(SessionDetailSource {
        own_title,
        query,
        model,
        saved_history_head,
    })
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;

    fn fixture() -> (Connection, Uuid, Uuid) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY,project_id TEXT,status TEXT,is_eval INTEGER,
                title TEXT,query TEXT,model TEXT);
             CREATE TABLE conversation_events(id INTEGER PRIMARY KEY,session_id TEXT,sequence INTEGER);
             CREATE INDEX idx_events_session_sequence ON conversation_events(session_id,sequence);",
        )
        .unwrap();
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        conn.execute(
            "INSERT INTO sessions(id,project_id,status,is_eval,title,query,model)
             VALUES(?1,?2,'Running',0,'','🙂','gpt-6-sol')",
            params![session.to_string(), project.to_string()],
        )
        .unwrap();
        (conn, project, session)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn detail_reads_bounded_fields_and_latest_saved_key() {
        let (conn, project, session) = fixture();
        let long_query = "🙂".repeat(8192);
        conn.execute(
            "UPDATE sessions SET query=?1 WHERE id=?2",
            params![long_query, session.to_string()],
        )
        .unwrap();
        for (id, sequence) in [(4, 7), (5, 7), (6, 3)] {
            conn.execute(
                "INSERT INTO conversation_events(id,session_id,sequence) VALUES(?1,?2,?3)",
                params![id, session.to_string(), sequence],
            )
            .unwrap();
        }
        let detail = session_detail_source(&conn, project, session).unwrap();
        assert_eq!(detail.own_title.text, detail.query.as_ref().unwrap().text);
        assert_eq!(detail.query.as_ref().unwrap().text.len(), 4096);
        assert!(detail.query.as_ref().unwrap().truncated);
        assert_eq!(detail.model.as_ref().unwrap().text, "gpt-6-sol");
        assert_eq!(detail.saved_history_head, Some((7, 5)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn detail_checks_project_and_preserves_missing_model() {
        let (conn, project, session) = fixture();
        conn.execute(
            "UPDATE sessions SET title='',query='',model=NULL WHERE id=?1",
            [session.to_string()],
        )
        .unwrap();
        let detail = session_detail_source(&conn, project, session).unwrap();
        assert_eq!(detail.own_title.text, "Untitled session");
        assert!(detail.model.is_none());
        assert!(detail.saved_history_head.is_none());
        assert!(matches!(
            session_detail_source(&conn, Uuid::new_v4(), session),
            Err(ReadError::NotFound)
        ));
    }
}
