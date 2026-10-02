use super::{
    BoundedText, ReadError, Result, SourcePage, bounded_column, bounded_text,
    require_session_project,
};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

pub type EventKey = (i32, i64);

#[derive(Debug, Clone, Copy)]
pub enum HistoryRange {
    Latest,
    Older {
        anchor: EventKey,
    },
    Newer {
        anchor: EventKey,
        through: Option<EventKey>,
    },
    Interval {
        lower_exclusive: EventKey,
        upper_inclusive: EventKey,
    },
}

#[derive(Debug, Clone)]
pub struct HistoryRow {
    pub id: i64,
    pub sequence: i32,
    pub event_type: String,
    pub role: Option<String>,
    pub created_at: String,
    pub content: BoundedText,
    pub tool_name: Option<BoundedText>,
    /// Exact only when the whole source ID fits. Prefixes never become keys.
    pub tool_pair_key: Option<String>,
    pub tool_id_display: Option<BoundedText>,
    pub offloaded: bool,
}

/// One bounded page plus the visible interval observed in the caller's Store
/// read transaction. The interval contains only this page's keys, while head
/// is the separately observed latest saved key. Neither is a snapshot lease.
pub struct HistoryObservedPage {
    pub page: SourcePage<HistoryRow, EventKey>,
    pub head: Option<EventKey>,
    pub lower_exclusive: Option<EventKey>,
    pub upper_inclusive: Option<EventKey>,
}

/// Check one complete tool ID against at most 65 exact-key index entries in
/// the caller's history transaction. The released Store index is on the full
/// `tool_use_id`; a 256-byte display prefix never enters this lookup. If the
/// capped seek cannot prove uniqueness, refuse the exact-pairing claim.
pub fn tool_pair_ambiguous(
    conn: &Connection,
    session: Uuid,
    event_id: i64,
    full_key: &str,
) -> Result<bool> {
    if conn.is_autocommit() || event_id <= 0 || full_key.is_empty() || full_key.len() > 256 {
        return Err(ReadError::InvalidSource);
    }
    let mut statement = conn.prepare_cached(
        "SELECT id,session_id=?2 FROM conversation_events INDEXED BY idx_events_tool_use_id
         WHERE tool_use_id=?1 ORDER BY id LIMIT 65",
    )?;
    let rows = statement
        .query_map(params![full_key, session.to_string()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let target_seen = rows
        .iter()
        .any(|(id, same_session)| *id == event_id && *same_session);
    let duplicate_seen = rows
        .iter()
        .any(|(id, same_session)| *id != event_id && *same_session);
    if !target_seen {
        return Err(if rows.len() == 65 {
            ReadError::SourceUnavailable
        } else {
            ReadError::InvalidSource
        });
    }
    if duplicate_seen {
        return Ok(true);
    }
    if rows.len() == 65 {
        return Err(ReadError::SourceUnavailable);
    }
    Ok(false)
}

pub fn observed_history_page(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    range: HistoryRange,
    limit: usize,
) -> Result<HistoryObservedPage> {
    if !(1..=50).contains(&limit) {
        return Err(ReadError::ResourceLimit);
    }
    if conn.is_autocommit() {
        return Err(ReadError::InvalidSource);
    }
    require_session_project(conn, project, session)?;
    let head: Option<EventKey> = conn
        .query_row(
            "SELECT sequence,id FROM conversation_events WHERE session_id=?1
             ORDER BY sequence DESC,id DESC LIMIT 1",
            [session.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if head.is_some_and(|(_, id)| id <= 0) {
        return Err(ReadError::InvalidSource);
    }
    let page = history_page(conn, project, session, range, limit)?;
    let upper_inclusive = page.items.last().map(|row| (row.sequence, row.id));
    if upper_inclusive.is_some_and(|upper| head.is_none_or(|head| upper > head)) {
        return Err(ReadError::InvalidSource);
    }
    let lower_exclusive = if let Some(first) = page.items.first() {
        match range {
            HistoryRange::Latest | HistoryRange::Older { .. } if page.has_more => conn
                .query_row(
                    "SELECT sequence,id FROM conversation_events WHERE session_id=?1
                     AND (sequence,id)<(?2,?3)
                     ORDER BY sequence DESC,id DESC LIMIT 1",
                    params![session.to_string(), first.sequence, first.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or(ReadError::InvalidSource)
                .map(Some)?,
            HistoryRange::Latest | HistoryRange::Older { .. } => None,
            HistoryRange::Newer { anchor, .. } => Some(anchor),
            HistoryRange::Interval {
                lower_exclusive, ..
            } => Some(lower_exclusive),
        }
    } else {
        None
    };
    if lower_exclusive
        .is_some_and(|lower| lower.1 <= 0 || upper_inclusive.is_none_or(|upper| lower >= upper))
    {
        return Err(ReadError::InvalidSource);
    }
    Ok(HistoryObservedPage {
        page,
        head,
        lower_exclusive,
        upper_inclusive,
    })
}

/// Keyset window over saved history. `next` is the edge from which another
/// query proceeds in the same direction. The caller supplies a read snapshot
/// and turns rows into wire DTOs with coverage and a signed cursor.
pub fn history_page(
    conn: &Connection,
    project: Uuid,
    session: Uuid,
    range: HistoryRange,
    limit: usize,
) -> Result<SourcePage<HistoryRow, EventKey>> {
    if !(1..=50).contains(&limit) {
        return Err(ReadError::ResourceLimit);
    }
    require_session_project(conn, project, session)?;
    let (sql, lower, upper, descending) = match range {
        HistoryRange::Latest => (
            "SELECT id,sequence FROM conversation_events WHERE session_id=?1 ORDER BY sequence DESC,id DESC LIMIT ?6",
            None,
            None,
            true,
        ),
        HistoryRange::Older { anchor } => (
            "SELECT id,sequence FROM conversation_events WHERE session_id=?1 AND (sequence,id)<(?2,?3) ORDER BY sequence DESC,id DESC LIMIT ?6",
            Some(anchor),
            None,
            true,
        ),
        HistoryRange::Newer { anchor, through } => (
            "SELECT id,sequence FROM conversation_events WHERE session_id=?1 AND (sequence,id)>(?2,?3) AND (?4 IS NULL OR (sequence,id)<=(?4,?5)) ORDER BY sequence,id LIMIT ?6",
            Some(anchor),
            through,
            false,
        ),
        HistoryRange::Interval {
            lower_exclusive,
            upper_inclusive,
        } => {
            if lower_exclusive >= upper_inclusive {
                return Err(ReadError::InvalidSource);
            }
            (
                "SELECT id,sequence FROM conversation_events WHERE session_id=?1 AND (sequence,id)>(?2,?3) AND (sequence,id)<=(?4,?5) ORDER BY sequence,id LIMIT ?6",
                Some(lower_exclusive),
                Some(upper_inclusive),
                false,
            )
        }
    };
    // Every query is a fixed keyset seek. None for the newer ceiling means
    // unbounded above, never a fabricated immutable head.
    let mut statement = conn.prepare_cached(sql)?;
    let candidates = statement
        .query_map(
            params![
                session.to_string(),
                lower.map(|k| k.0),
                lower.map(|k| k.1),
                upper.map(|k| k.0),
                upper.map(|k| k.1),
                (limit + 1) as i64
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i32>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_more = candidates.len() > limit;
    let mut items = Vec::with_capacity(limit.min(candidates.len()));
    for (id, sequence) in candidates.into_iter().take(limit) {
        if id <= 0 {
            return Err(ReadError::InvalidSource);
        }
        let raw = conn.query_row(
            "SELECT event_type,role,created_at,content IS NOT NULL,
                    tool_name IS NOT NULL,tool_use_id IS NOT NULL,
                    EXISTS(SELECT 1 FROM offloaded_content o WHERE o.session_id=conversation_events.session_id
                           AND o.event_sequence=conversation_events.sequence)
             FROM conversation_events WHERE id=?1 AND session_id=?2",
            params![id, session.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?,
                      row.get::<_, String>(2)?, row.get::<_, bool>(3)?,
                      row.get::<_, bool>(4)?, row.get::<_, bool>(5)?,
                      row.get::<_, bool>(6)?)),
        )?;
        let content = if raw.3 {
            bounded_column(conn, "conversation_events", "content", id, 8192)?
        } else {
            bounded_text(String::new(), 0, 8192)?
        };
        let tool_name = raw
            .4
            .then(|| bounded_column(conn, "conversation_events", "tool_name", id, 512))
            .transpose()?;
        let tool_id_display = raw
            .5
            .then(|| bounded_column(conn, "conversation_events", "tool_use_id", id, 256))
            .transpose()?;
        let tool_pair_key = tool_id_display.as_ref().and_then(|v| {
            if !v.truncated && !v.text.is_empty() {
                Some(v.text.clone())
            } else {
                None
            }
        });
        items.push(HistoryRow {
            id,
            sequence,
            event_type: raw.0,
            role: raw.1,
            created_at: raw.2,
            content,
            tool_name,
            tool_pair_key,
            tool_id_display,
            offloaded: raw.6,
        });
    }
    let next = if has_more {
        items.last().map(|row| (row.sequence, row.id))
    } else {
        None
    };
    if descending {
        items.reverse();
    }
    Ok(SourcePage {
        items,
        next,
        has_more,
    })
}

#[cfg(all(
    test,
    any(not(feature = "test-shard-mode"), feature = "test-shard-store-04")
))]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn observed_interval_contains_only_bounded_tied_key_page() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY,project_id TEXT,status TEXT,is_eval INTEGER);
             CREATE TABLE conversation_events(id INTEGER PRIMARY KEY,session_id TEXT,sequence INTEGER,
               event_type TEXT,role TEXT,created_at TEXT,content TEXT,tool_name TEXT,tool_use_id TEXT);
             CREATE INDEX idx_events_session_sequence ON conversation_events(session_id,sequence);
             CREATE INDEX idx_events_tool_use_id ON conversation_events(tool_use_id);
             CREATE TABLE offloaded_content(session_id TEXT,event_sequence INTEGER);",
        )
        .unwrap();
        let project = Uuid::new_v4();
        let session = Uuid::new_v4();
        conn.execute(
            "INSERT INTO sessions(id,project_id,status,is_eval) VALUES(?1,?2,'Running',0)",
            params![session.to_string(), project.to_string()],
        )
        .unwrap();
        let base = 9_007_199_254_740_992_i64;
        for offset in 1..=5_i64 {
            conn.execute(
                "INSERT INTO conversation_events(id,session_id,sequence,event_type,created_at,content)
                 VALUES(?1,?2,7,'message','2026-09-27T00:00:00.000000000Z','ok')",
                params![base + offset, session.to_string()],
            )
            .unwrap();
        }
        assert!(matches!(
            observed_history_page(&conn, project, session, HistoryRange::Latest, 2),
            Err(ReadError::InvalidSource)
        ));
        conn.execute_batch("BEGIN").unwrap();
        let latest =
            observed_history_page(&conn, project, session, HistoryRange::Latest, 2).unwrap();
        assert_eq!(latest.head, Some((7, base + 5)));
        assert_eq!(
            latest
                .page
                .items
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![base + 4, base + 5]
        );
        assert_eq!(latest.lower_exclusive, Some((7, base + 3)));
        assert_eq!(latest.upper_inclusive, Some((7, base + 5)));
        assert_eq!(latest.page.next, Some((7, base + 4)));

        let newer = observed_history_page(
            &conn,
            project,
            session,
            HistoryRange::Newer {
                anchor: (7, base + 1),
                through: Some((7, base + 4)),
            },
            2,
        )
        .unwrap();
        assert_eq!(newer.lower_exclusive, Some((7, base + 1)));
        assert_eq!(newer.upper_inclusive, Some((7, base + 3)));
        assert_eq!(newer.page.next, Some((7, base + 3)));
        assert!(newer.page.has_more);

        let empty = observed_history_page(
            &conn,
            project,
            session,
            HistoryRange::Newer {
                anchor: (7, base + 5),
                through: None,
            },
            2,
        )
        .unwrap();
        assert_eq!(empty.head, latest.head);
        assert!(empty.page.items.is_empty());
        assert_eq!(empty.lower_exclusive, None);
        assert_eq!(empty.upper_inclusive, None);
        conn.execute_batch("ROLLBACK").unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn tool_pair_check_uses_complete_key_and_refuses_capped_uncertainty() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE conversation_events(id INTEGER PRIMARY KEY,session_id TEXT,tool_use_id TEXT);
             CREATE INDEX idx_events_tool_use_id ON conversation_events(tool_use_id);",
        )
        .unwrap();
        let session = Uuid::new_v4();
        let foreign = Uuid::new_v4();
        let insert = |id: i64, owner: Uuid, key: &str| {
            conn.execute(
                "INSERT INTO conversation_events(id,session_id,tool_use_id) VALUES(?1,?2,?3)",
                params![id, owner.to_string(), key],
            )
            .unwrap();
        };
        insert(1, session, "short");
        insert(2, foreign, "short");
        let prefix = "x".repeat(255);
        insert(4, session, &format!("{prefix}a"));
        insert(5, session, &format!("{prefix}b"));
        for id in 100..164 {
            insert(id, foreign, "crowd");
        }
        insert(164, session, "crowd");
        assert!(matches!(
            tool_pair_ambiguous(&conn, session, 1, "short"),
            Err(ReadError::InvalidSource)
        ));
        conn.execute_batch("BEGIN").unwrap();
        assert!(!tool_pair_ambiguous(&conn, session, 1, "short").unwrap());
        assert!(!tool_pair_ambiguous(&conn, session, 4, &format!("{prefix}a")).unwrap());
        assert!(matches!(
            tool_pair_ambiguous(&conn, session, 164, "crowd"),
            Err(ReadError::SourceUnavailable)
        ));
        assert!(matches!(
            tool_pair_ambiguous(&conn, session, 1, "missing"),
            Err(ReadError::InvalidSource)
        ));
        conn.execute(
            "INSERT INTO conversation_events(id,session_id,tool_use_id) VALUES(3,?1,'short')",
            [session.to_string()],
        )
        .unwrap();
        assert!(tool_pair_ambiguous(&conn, session, 1, "short").unwrap());
        conn.execute_batch("ROLLBACK").unwrap();
    }
}
