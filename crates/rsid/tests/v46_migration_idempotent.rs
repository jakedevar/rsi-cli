//! V46 migration idempotency: opening a Store at version 45 must apply V46,
//! and re-opening must be a no-op (PRAGMA user_version stays at LATEST_SCHEMA_VERSION).
//! (Originally written as V45 in PR #10; renumbered to V46 during 2026-05-15
//! merge because PR #18 took V45 for capability_class.)

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
fn v46_migration_fires_from_v45_then_idempotent_on_reopen() {
    // First open: applies all migrations to LATEST_SCHEMA_VERSION.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");

    {
        let _store = Store::open(&db_path).unwrap();
    }
    assert_eq!(read_user_version(&db_path), LATEST_SCHEMA_VERSION);
    assert!(table_exists(&db_path, "chain_iterations"));

    // Force version back to V45 to simulate a freshly-upgraded daemon binary
    // attaching to a V45 database.
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "user_version", 45).unwrap();
        // Drop chain_iterations so the V46 block has work to do.
        conn.execute("DROP TABLE IF EXISTS chain_iterations", [])
            .unwrap();
        conn.execute("DROP INDEX IF EXISTS idx_chain_iterations_child_exec", [])
            .unwrap();
        conn.execute("DROP INDEX IF EXISTS idx_chain_iterations_chain_active", [])
            .unwrap();
    }
    assert_eq!(read_user_version(&db_path), 45);
    assert!(!table_exists(&db_path, "chain_iterations"));

    // Re-open: V46 block should fire and recreate the table.
    {
        let _store = Store::open(&db_path).unwrap();
    }
    assert_eq!(read_user_version(&db_path), LATEST_SCHEMA_VERSION);
    assert!(table_exists(&db_path, "chain_iterations"));

    // Third open: idempotent — version already at head, no-op.
    {
        let _store = Store::open(&db_path).unwrap();
    }
    assert_eq!(read_user_version(&db_path), LATEST_SCHEMA_VERSION);
    assert!(table_exists(&db_path, "chain_iterations"));
}
