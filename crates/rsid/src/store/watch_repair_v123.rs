//! V123 index for bounded terminal-watch history lookups during restart repair.

use super::Store;
use crate::error::{DaemonError, Result};
use rusqlite::{Connection, Transaction, TransactionBehavior};

// RSI-RELEASED-MIGRATION-BEGIN: v123-watch-repair-catalog
const V123_WATCH_HISTORY_INDEX_NAME: &str = "idx_scheduled_jobs_agent_watch_history_natural";
const V123_WATCH_HISTORY_INDEX_SQL: &str = "CREATE INDEX idx_scheduled_jobs_agent_watch_history_natural ON scheduled_jobs(wake_session_id,wake_mode)";
const V123_WATCH_WITNESS_SQL: &str = "CREATE TABLE agent_child_watch_witness (
    owner_session_id TEXT NOT NULL,
    child_session_id TEXT NOT NULL,
    job_id TEXT,
    state TEXT NOT NULL CHECK(state IN ('pending','armed','consumed','abandoned','disabled','deleted')),
    updated_at TEXT NOT NULL,
    PRIMARY KEY(owner_session_id,child_session_id)
)";

pub(in crate::store) fn validate_v123_catalog(connection: &Connection) -> Result<()> {
    let actual: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name=?1 AND tbl_name='scheduled_jobs'",
            [V123_WATCH_HISTORY_INDEX_NAME],
            |row| row.get(0),
        )
        .map_err(|_| DaemonError::Store("V123 watch-history index is missing".into()))?;
    if actual != V123_WATCH_HISTORY_INDEX_SQL {
        return Err(DaemonError::Store(format!(
            "V123 watch-history index definition mismatch: {actual}"
        )));
    }
    let witness_sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='agent_child_watch_witness'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| DaemonError::Store("V123 watch witness table is missing".into()))?;
    if witness_sql != V123_WATCH_WITNESS_SQL {
        return Err(DaemonError::Store(format!(
            "V123 watch witness table definition mismatch: {witness_sql}"
        )));
    }
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v123-watch-repair-catalog

// RSI-RELEASED-MIGRATION-BEGIN: v123-watch-repair-migration
pub(in crate::store) fn apply_v123_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 122 {
        return Err(DaemonError::Store(format!(
            "V123 requires exact V122 source, found V{version}"
        )));
    }

    tx.execute_batch(V123_WATCH_HISTORY_INDEX_SQL)?;
    tx.execute_batch(V123_WATCH_WITNESS_SQL)?;
    validate_v123_catalog(&tx)?;

    let integrity: String = tx.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(DaemonError::Store(format!(
            "V123 watch repair requires integrity_check=ok, got {integrity}"
        )));
    }
    let foreign_key_errors: i64 =
        tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_errors != 0 {
        return Err(DaemonError::Store(format!(
            "V123 watch repair found {foreign_key_errors} foreign-key violations"
        )));
    }

    tx.pragma_update(None, "user_version", 123)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v123-watch-repair-migration

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn v123_refuses_v121_without_changing_catalog_or_version() {
        let store = Store::open_in_memory().expect("current store");
        crate::store::tests::rewind_post_v121_tail_to(&store.conn, 121);
        let error = apply_v123_migration(&store).expect_err("V122 is required");
        assert!(
            error
                .to_string()
                .contains("V123 requires exact V122 source")
        );
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .expect("schema version"),
            121
        );
        assert!(
            store
                .conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE name=?1",
                    [V123_WATCH_HISTORY_INDEX_NAME],
                    |row| row.get::<_, i32>(0)
                )
                .is_err(),
            "the refused migration must not create its index"
        );
    }

    #[test]
    fn v123_index_keys_historical_watch_lookup() {
        let store = Store::open_in_memory().expect("current store");
        validate_v123_catalog(&store.conn).expect("pinned index definition");
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .expect("schema version"),
            crate::store::LATEST_SCHEMA_VERSION
        );

        let mut statement = store
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT 1 FROM scheduled_jobs AS historical_watch \
                 WHERE historical_watch.wake_session_id=?1 \
                   AND historical_watch.wake_mode='on_terminal:' || ?2",
            )
            .expect("prepare watch lookup plan");
        let details = statement
            .query_map(["owner", "child"], |row| row.get::<_, String>(3))
            .expect("explain watch lookup")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect watch plan");
        assert!(
            details.iter().any(|detail| {
                detail.contains("SEARCH historical_watch USING COVERING INDEX")
                    && detail.contains(V123_WATCH_HISTORY_INDEX_NAME)
            }),
            "historical lookup must use the V123 natural-key index: {details:?}"
        );
    }

    #[test]
    fn v121_upgrade_reaches_v123_and_reopen_rejects_catalog_drift() {
        let directory = tempfile::tempdir().expect("test directory");
        let database = directory.path().join("v121-to-v123.sqlite");
        {
            let store = Store::open(&database).expect("create current database");
            crate::store::tests::rewind_post_v121_tail_to(&store.conn, 121);
        }

        let store = Store::open(&database).expect("upgrade V121 through V122 and V123 to head");
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .expect("schema version"),
            crate::store::LATEST_SCHEMA_VERSION
        );
        validate_v123_catalog(&store.conn).expect("V123 catalog after upgrade");
        drop(store);

        let store = Store::open(&database).expect("reopen V123 database");
        validate_v123_catalog(&store.conn).expect("V123 catalog after reopen");
        store
            .conn
            .execute_batch(&format!(
                "DROP INDEX {V123_WATCH_HISTORY_INDEX_NAME}; \
                 CREATE INDEX {V123_WATCH_HISTORY_INDEX_NAME} \
                 ON scheduled_jobs(wake_mode,wake_session_id);"
            ))
            .expect("tamper with the V123 catalog");
        drop(store);

        let error = Store::open(&database)
            .err()
            .expect("catalog drift must refuse reopen");
        assert!(
            error
                .to_string()
                .contains("V123 watch-history index definition mismatch")
        );
    }

    #[test]
    fn v123_reopen_rejects_witness_catalog_drift() {
        let directory = tempfile::tempdir().expect("test directory");
        let database = directory.path().join("v123-witness-drift.sqlite");
        let store = Store::open(&database).expect("create V123 database");
        store
            .conn
            .execute_batch("ALTER TABLE agent_child_watch_witness ADD COLUMN stray TEXT")
            .expect("tamper witness table");
        drop(store);
        let error = Store::open(&database)
            .err()
            .expect("witness catalog drift must refuse reopen");
        assert!(
            error
                .to_string()
                .contains("V123 watch witness table definition mismatch")
        );
    }
}
