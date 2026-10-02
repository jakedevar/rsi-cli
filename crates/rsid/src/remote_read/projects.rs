use super::{BoundedText, ReadError, Result, SourcePage, bounded_column};
use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ProjectRow {
    pub id: Uuid,
    pub name: BoundedText,
}

/// Discover only the operator-configured project IDs. The fixed 32-ID cap
/// bounds source work, and the cursor advances over absent configured IDs.
pub fn list_projects(
    conn: &Connection,
    configured: &[Uuid],
    after: Option<Uuid>,
    limit: usize,
) -> Result<SourcePage<ProjectRow, Uuid>> {
    if configured.len() > 32 || !(1..=50).contains(&limit) {
        return Err(ReadError::ResourceLimit);
    }
    let ids: BTreeSet<_> = configured.iter().copied().collect();
    if ids.len() != configured.len() {
        return Err(ReadError::InvalidSource);
    }
    let candidates: Vec<_> = ids
        .into_iter()
        .filter(|id| after.is_none_or(|after| *id > after))
        .take(limit + 1)
        .collect();
    let has_more = candidates.len() > limit;
    let examined: Vec<_> = candidates.into_iter().take(limit).collect();
    let next = has_more.then(|| *examined.last().expect("limit is positive"));
    let mut items = Vec::with_capacity(examined.len());
    let mut statement = conn.prepare_cached("SELECT rowid FROM projects WHERE id=?1")?;
    for id in examined {
        let rowid: Option<i64> = statement
            .query_row([id.to_string()], |row| row.get(0))
            .optional()?;
        if let Some(rowid) = rowid {
            items.push(ProjectRow {
                id,
                name: bounded_column(conn, "projects", "name", rowid, 512)?,
            });
        }
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
    use rusqlite::params;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn configured_discovery_continues_across_missing_project() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE projects(id TEXT PRIMARY KEY,name TEXT NOT NULL)")
            .unwrap();
        let ids = [
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        ];
        for id in [ids[0], ids[2]] {
            conn.execute(
                "INSERT INTO projects(id,name) VALUES(?1,?2)",
                params![id.to_string(), "é".repeat(300)],
            )
            .unwrap();
        }
        let first = list_projects(&conn, &ids, None, 1).unwrap();
        assert_eq!(first.items[0].id, ids[0]);
        assert_eq!(first.items[0].name.text.len(), 512);
        assert!(first.items[0].name.truncated);
        let empty = list_projects(&conn, &ids, first.next, 1).unwrap();
        assert!(empty.items.is_empty());
        assert!(empty.has_more);
        assert_eq!(empty.next, Some(ids[1]));
        let last = list_projects(&conn, &ids, empty.next, 1).unwrap();
        assert_eq!(last.items[0].id, ids[2]);
        assert!(!last.has_more);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn project_discovery_rejects_duplicate_or_unbounded_config() {
        let conn = Connection::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        assert!(matches!(
            list_projects(&conn, &[id, id], None, 1),
            Err(ReadError::InvalidSource)
        ));
        assert!(matches!(
            list_projects(&conn, &[id; 33], None, 1),
            Err(ReadError::ResourceLimit)
        ));
    }
}
