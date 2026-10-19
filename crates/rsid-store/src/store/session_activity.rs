//! Dream's activity maximum without polling the historical sessions table (#2938).

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(super) struct SessionActivityCache {
    // Keep SQLite's TEXT ordering, including its handling of NULL. Parsing all
    // timestamps and comparing instants would change MAX(updated_at) semantics.
    rows: HashMap<i64, String>,
    ordered: BTreeSet<(String, i64)>,
    dirty: Arc<Mutex<HashSet<i64>>>,
    data_version: Option<i64>,
}

impl SessionActivityCache {
    fn install(conn: &Connection) -> Self {
        let cache = Self::default();
        let dirty = Arc::clone(&cache.dirty);
        // Capture all session writers, including archive/status writes and
        // trigger writes, rather than just stamped_session_write. Hooks also
        // fire for rolled-back writes: refresh the final row, never the event.
        conn.update_hook(Some(move |_, database: &str, table: &str, rowid| {
            if database == "main" && table == "sessions" {
                dirty
                    .lock()
                    .expect("session activity dirty set")
                    .insert(rowid);
            }
        }));
        cache
    }

    fn refresh(&mut self, conn: &Connection) -> Result<Option<String>> {
        let data_version = conn.query_row("PRAGMA data_version", [], |row| row.get(0))?;
        let mut dirty = self.dirty.lock().expect("session activity dirty set");
        if self.data_version != Some(data_version) {
            // Once on first use/restart. Another connection bypasses our hook;
            // conservatively rebuild after an external commit as well.
            let mut rows = HashMap::new();
            let mut ordered = BTreeSet::new();
            let mut statement = conn.prepare("SELECT rowid, updated_at FROM sessions")?;
            let mut cursor = statement.query([])?;
            while let Some(row) = cursor.next()? {
                let id: i64 = row.get(0)?;
                if let Some(value) = row.get::<_, Option<String>>(1)? {
                    ordered.insert((value.clone(), id));
                    rows.insert(id, value);
                }
            }
            self.rows = rows;
            self.ordered = ordered;
            self.data_version = Some(data_version);
        } else if !dirty.is_empty() {
            let mut statement =
                conn.prepare_cached("SELECT updated_at FROM sessions WHERE rowid = ?1")?;
            // Only clear the dirty set after every read succeeds. A partial
            // refresh is safe to retry, including a failed transaction commit.
            for &id in dirty.iter() {
                let value = statement
                    .query_row([id], |row| row.get::<_, Option<String>>(0))
                    .optional()?
                    .flatten();
                if let Some(previous) = self.rows.remove(&id) {
                    self.ordered.remove(&(previous, id));
                }
                if let Some(value) = value {
                    self.ordered.insert((value.clone(), id));
                    self.rows.insert(id, value);
                }
            }
        }
        dirty.clear();
        Ok(self.ordered.last().map(|(value, _)| value.clone()))
    }
}

impl Store {
    /// Most recent persisted session activity, including archived/deleted rows.
    ///
    /// Seed lazily once, then read only session rowids changed on this Store's
    /// connection. Unchanged polls perform constant SQLite work, and unrelated
    /// writes on this connection do not invalidate it. External commits require
    /// a conservative rebuild. No persistent schema or index is added.
    pub fn latest_session_activity_at(&self) -> Result<Option<DateTime<Utc>>> {
        let raw = if self.conn.is_autocommit() {
            let mut cache = self.session_activity.borrow_mut();
            let cache = cache.get_or_insert_with(|| SessionActivityCache::install(&self.conn));
            // Version and rows must describe one read snapshot, even if an
            // external connection commits while we are refreshing.
            let tx = self.conn.unchecked_transaction()?;
            let raw = cache.refresh(&tx)?;
            tx.commit()?;
            raw
        } else {
            // A caller inside a transaction may observe provisional activity.
            // Never publish it into the committed cache (rollback/savepoints).
            self.conn
                .query_row("SELECT MAX(updated_at) FROM sessions", [], |row| row.get(0))?
        };
        raw.map(|value| {
            DateTime::parse_from_rfc3339(&value)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| {
                    DaemonError::Store(format!(
                        "invalid session updated_at timestamp '{value}': {e}"
                    ))
                })
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::SessionStatus;
    use uuid::Uuid;

    fn insert(conn: &Connection, id: Uuid, at: Option<&str>) {
        conn.execute(
            "INSERT INTO sessions (id, query, working_dir, status, created_at, updated_at)
             VALUES (?1, 'activity test', '/tmp', 'Completed', ?2, ?2)",
            rusqlite::params![id.to_string(), at],
        )
        .unwrap();
    }

    fn assert_matches_sql(store: &Store) {
        let raw: Option<String> = store
            .conn
            .query_row("SELECT MAX(updated_at) FROM sessions", [], |row| row.get(0))
            .unwrap();
        let expected = raw.map(|raw| {
            DateTime::parse_from_rfc3339(&raw)
                .unwrap()
                .with_timezone(&Utc)
        });
        assert_eq!(store.latest_session_activity_at().unwrap(), expected);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn activity_tracks_all_session_writes_and_transaction_outcomes() {
        let store = Store::open_in_memory().unwrap();
        assert_matches_sql(&store);
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        insert(&store.conn, first, Some("2000-01-01T00:00:00Z"));
        insert(&store.conn, second, Some("2001-01-01T00:00:00Z"));
        assert_matches_sql(&store);
        // Real stamped metadata writer, then real unstamped status writers.
        store.update_session_title(first, "activity").unwrap();
        assert_matches_sql(&store);
        for status in [
            SessionStatus::Running,
            SessionStatus::Archived,
            SessionStatus::Deleted,
        ] {
            store.update_session_status(second, status).unwrap();
            assert_matches_sql(&store);
        }
        // Read inside a transaction and nested savepoint, then roll both back.
        let tx = store.conn.unchecked_transaction().unwrap();
        tx.execute(
            "UPDATE sessions SET updated_at='2099-01-01T00:00:00Z' WHERE id=?1",
            [first.to_string()],
        )
        .unwrap();
        assert_matches_sql(&store);
        tx.execute_batch(
            "SAVEPOINT activity;
            UPDATE sessions SET updated_at='2100-01-01T00:00:00Z';
            ROLLBACK TO activity; RELEASE activity;",
        )
        .unwrap();
        tx.rollback().unwrap();
        assert_matches_sql(&store);
        // Backward timestamps and removal of the maximum must reveal the next
        // persisted value, rather than retain a monotonic watermark.
        store
            .conn
            .execute(
                "UPDATE sessions SET updated_at='1999-01-01T00:00:00Z' WHERE id=?1",
                [second.to_string()],
            )
            .unwrap();
        assert_matches_sql(&store);
        store.delete_session(first).unwrap();
        assert_matches_sql(&store);
        store.delete_session(second).unwrap();
        assert_matches_sql(&store);
        // A rolled-back insert must not create activity on an empty store.
        let tx = store.conn.unchecked_transaction().unwrap();
        insert(&tx, Uuid::new_v4(), Some("2100-01-01T00:00:00Z"));
        tx.rollback().unwrap();
        assert_matches_sql(&store);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn activity_restart_external_commits_and_text_max_match_sql() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("activity.db");
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        {
            let store = Store::open_test_fixture(&path).unwrap();
            insert(&store.conn, first, Some("2000-01-01T00:00:00Z"));
            // The existing contract compares text, not normalized instants.
            insert(&store.conn, second, Some("2000-01-01T01:00:00+02:00"));
            assert_matches_sql(&store);
        }
        let store = Store::open_test_fixture(&path).unwrap();
        assert_matches_sql(&store);
        let external = Store::open_test_fixture(&path).unwrap();
        external
            .conn
            .execute(
                "UPDATE sessions SET updated_at='2002-01-01T00:00:00Z' WHERE id=?1",
                [first.to_string()],
            )
            .unwrap();
        assert_matches_sql(&store);
        // Exercise a real external purge, including its retained execution
        // projection; raw DELETE is correctly refused by the custody trigger.
        external.delete_session(first).unwrap();
        assert_matches_sql(&store);
        store
            .conn
            .execute(
                "UPDATE sessions SET updated_at='invalid' WHERE id=?1",
                [second.to_string()],
            )
            .unwrap();
        assert!(store.latest_session_activity_at().is_err());
        store
            .conn
            .execute(
                "UPDATE sessions SET updated_at='2003-01-01T00:00:00Z' WHERE id=?1",
                [second.to_string()],
            )
            .unwrap();
        assert_matches_sql(&store);
    }
}
