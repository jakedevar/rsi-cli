//! Session-owned preamble delivery state (#426).
//!
//! The daemon owns the JSON format; Store owns every table access. A consumed
//! entry clears its payload without deleting a row. Session deletion cascades.

use super::Store;
use crate::error::Result;
use chrono::{SecondsFormat, Utc};
use rusqlite::OptionalExtension;
use uuid::Uuid;

impl Store {
    pub fn get_session_start_preamble(&self, session_id: Uuid) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT state_json FROM session_start_preambles WHERE session_id=?1",
                [session_id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Commit before the caller acknowledges deferring, serving or consuming
    /// context. Clearing a missing entry creates no row.
    pub fn set_session_start_preamble(&self, session_id: Uuid, state: Option<&str>) -> Result<()> {
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        if let Some(state) = state {
            self.conn.execute(
                "INSERT INTO session_start_preambles(session_id,state_json,updated_at)
                 VALUES (?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET
                    state_json=excluded.state_json,updated_at=excluded.updated_at",
                rusqlite::params![session_id.to_string(), state, now],
            )?;
        } else {
            self.conn.execute(
                "UPDATE session_start_preambles SET state_json=NULL,updated_at=?2 WHERE session_id=?1",
                rusqlite::params![session_id.to_string(), now],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn state_requires_a_session_clears_without_deletion_and_cascades_with_its_owner() {
        let store = Store::open_in_memory().unwrap();
        let session = Uuid::new_v4();
        assert!(
            store
                .set_session_start_preamble(session, Some("{}"))
                .is_err()
        );
        store
            .insert_session(&crate::test_support::test_session(
                session,
                std::env::temp_dir(),
            ))
            .unwrap();
        store
            .set_session_start_preamble(session, Some("{\"batch\":1}"))
            .unwrap();
        assert_eq!(
            store
                .get_session_start_preamble(session)
                .unwrap()
                .as_deref(),
            Some("{\"batch\":1}")
        );
        store.set_session_start_preamble(session, None).unwrap();
        assert_eq!(store.get_session_start_preamble(session).unwrap(), None);
        let rows: i64 = store
            .conn
            .query_row("SELECT count(*) FROM session_start_preambles", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            rows, 1,
            "consumption retains only a session-owned empty row"
        );
        store
            .set_session_start_preamble(session, Some("{}"))
            .unwrap();
        // The canonical purge prepares the retained execution projection
        // before deleting this synthetic session and cascading to its debt.
        store.purge_session(session).unwrap();
        let rows: i64 = store
            .conn
            .query_row("SELECT count(*) FROM session_start_preambles", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "session deletion reclaims its preamble payload");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn migration_replays_from_previous_head_without_changing_existing_rows() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_daemon_setting("operator-test", "preserved")
            .unwrap();
        let before = store.list_daemon_settings_with_prefix("").unwrap();
        let session = Uuid::new_v4();
        store
            .insert_session(&crate::test_support::test_session(
                session,
                std::env::temp_dir(),
            ))
            .unwrap();
        super::super::tests::rewind_store_to_schema_version(
            &store.conn,
            super::super::tests::SESSION_START_PREAMBLE_SCHEMA_VERSION - 1,
        );
        let prior_catalog = super::super::catalog_sql_by_name(&store.conn);
        store.init_schema().unwrap();
        super::super::assert_catalog_preserved_across_replay(
            &prior_catalog,
            &store.conn,
            super::super::tests::SESSION_START_PREAMBLE_SCHEMA_VERSION - 1,
        );
        assert_eq!(store.get_session(session).unwrap().unwrap().id, session);
        assert_eq!(store.list_daemon_settings_with_prefix("").unwrap(), before);
        store
            .set_session_start_preamble(session, Some("{}"))
            .unwrap();
        assert_eq!(
            store
                .get_session_start_preamble(session)
                .unwrap()
                .as_deref(),
            Some("{}")
        );
    }
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn failed_write_keeps_prior_durable_debt() {
        let store = Store::open_in_memory().unwrap();
        let session = Uuid::new_v4();
        store
            .insert_session(&crate::test_support::test_session(
                session,
                std::env::temp_dir(),
            ))
            .unwrap();
        store
            .set_session_start_preamble(session, Some("{\"owed\":true}"))
            .unwrap();
        store.conn.execute_batch("PRAGMA query_only=ON").unwrap();
        assert!(
            store
                .set_session_start_preamble(session, Some("{}"))
                .is_err()
        );
        assert!(store.set_session_start_preamble(session, None).is_err());
        assert_eq!(
            store
                .get_session_start_preamble(session)
                .unwrap()
                .as_deref(),
            Some("{\"owed\":true}")
        );
    }
}
