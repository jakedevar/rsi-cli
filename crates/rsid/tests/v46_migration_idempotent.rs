//! V46 schema artifacts survive reopening a current Store. The exact V45 to
//! V46 replay is covered by `store::tests::migration_v46_adds_chain_iterations_table_and_indexes`,
//! whose fixture rewinds both the schema and user_version before reopening.

use rsid::store::{LATEST_SCHEMA_VERSION, Store};
use rusqlite::Connection;

fn read_user_version(path: &std::path::Path) -> i32 {
    let conn = Connection::open(path).unwrap();
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn table_exists(path: &std::path::Path, table: &str) -> bool {
    let conn = Connection::open(path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        )
        .unwrap();
    count > 0
}

#[test]
fn v46_schema_artifacts_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");

    {
        let _store = Store::open(&db_path).unwrap();
    }
    assert_eq!(read_user_version(&db_path), LATEST_SCHEMA_VERSION);
    assert!(table_exists(&db_path, "chain_iterations"));

    // Reopening the current schema must preserve the released V46 table.
    {
        let _store = Store::open(&db_path).unwrap();
    }
    assert_eq!(read_user_version(&db_path), LATEST_SCHEMA_VERSION);
    assert!(table_exists(&db_path, "chain_iterations"));
}
