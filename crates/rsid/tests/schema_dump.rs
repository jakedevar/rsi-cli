//! Schema-equivalence probe for migration-layout refactors.
//!
//! Migrates an empty database to the latest version and dumps the complete
//! `sqlite_master` catalog plus `PRAGMA user_version`. When
//! `RSI_SCHEMA_DUMP_PATH` is set the dump is written there, so the output of
//! two source layouts can be compared byte for byte. Without it, the test
//! asserts the empty database reaches `LATEST_SCHEMA_VERSION`.

use rsid::store::{LATEST_SCHEMA_VERSION, Store};
use rusqlite::Connection;

fn dump(path: &std::path::Path) -> (i32, String) {
    let conn = Connection::open(path).unwrap();
    let user_version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    let mut statement = conn
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
        .unwrap();
    let mut text = format!("user_version={user_version}\n");
    let rows = statement
        .query_map([], |row| {
            Ok(format!(
                "{}|{}|{}|{}\n",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?.unwrap_or_default()
            ))
        })
        .unwrap();
    for row in rows {
        text.push_str(&row.unwrap());
    }
    (user_version, text)
}

#[test]
fn empty_database_migrates_to_the_latest_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("schema.db");
    drop(Store::open(&path).unwrap());
    let (user_version, text) = dump(&path);
    assert_eq!(user_version, LATEST_SCHEMA_VERSION);
    if let Some(target) = std::env::var_os("RSI_SCHEMA_DUMP_PATH") {
        std::fs::write(target, &text).unwrap();
    }
}
