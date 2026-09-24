//! Durable watchdog evidence operations. V128 installs the table before use.

use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::rpc::DaemonRestartRecordV1;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::watchdog::RestartRecord;

const COLUMNS: &str = "id, version, observed_at, last_healthy_at, failed_probes_json";
// RSI-RELEASED-MIGRATION-BEGIN: v128-watchdog-restart-catalog
const TABLE_SQL: &str = "CREATE TABLE daemon_restart_records (
            id TEXT PRIMARY KEY,
            version INTEGER NOT NULL CHECK (version = 1),
            observed_at TEXT NOT NULL,
            last_healthy_at TEXT NOT NULL,
            failed_probes_json TEXT NOT NULL CHECK (json_valid(failed_probes_json))
        )";
const LATEST_INDEX_SQL: &str = "CREATE INDEX daemon_restart_records_latest
            ON daemon_restart_records(observed_at DESC, id DESC)";
const NO_UPDATE_SQL: &str = "CREATE TRIGGER daemon_restart_records_no_update
            BEFORE UPDATE ON daemon_restart_records
            BEGIN SELECT RAISE(ABORT, 'watchdog restart evidence is immutable'); END";
const NO_DELETE_SQL: &str = "CREATE TRIGGER daemon_restart_records_no_delete
            BEFORE DELETE ON daemon_restart_records
            BEGIN SELECT RAISE(ABORT, 'watchdog restart evidence is immutable'); END";

pub(crate) const CATALOG_OBJECTS: [(&str, &str, &str); 4] = [
    ("table", "daemon_restart_records", TABLE_SQL),
    ("index", "daemon_restart_records_latest", LATEST_INDEX_SQL),
    ("trigger", "daemon_restart_records_no_update", NO_UPDATE_SQL),
    ("trigger", "daemon_restart_records_no_delete", NO_DELETE_SQL),
];
// RSI-RELEASED-MIGRATION-END: v128-watchdog-restart-catalog

impl Store {
    /// The caller may unlink a watchdog sidecar only after this returns.
    pub fn persist_daemon_restart_record(&self, record: &RestartRecord) -> Result<()> {
        insert(&self.conn, record)
    }

    pub fn latest_daemon_restart_record(&self) -> Result<Option<DaemonRestartRecordV1>> {
        latest(&self.conn)
    }
}

// RSI-RELEASED-MIGRATION-BEGIN: v128-watchdog-restart-migration
/// Install the restart catalog transactionally from the exact V127 source.
pub(crate) fn apply_v128_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 127 {
        return Err(DaemonError::Store(format!(
            "V128 watchdog restart migration requires V127 source, found V{version}"
        )));
    }
    migrate_v128(&tx)?;
    tx.execute("PRAGMA user_version = 128", [])?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn migrate_v128(conn: &Connection) -> Result<()> {
    for (_, _, ddl) in CATALOG_OBJECTS {
        conn.execute_batch(ddl)?;
    }
    validate_v128_catalog(conn)
}
// RSI-RELEASED-MIGRATION-END: v128-watchdog-restart-migration

/// Refuse changed or missing restart-evidence objects on every Store reopen.
pub(crate) fn validate_v128_catalog(conn: &Connection) -> Result<()> {
    for (kind, name, expected_sql) in CATALOG_OBJECTS {
        let actual: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
                params![kind, name],
                |row| row.get(0),
            )
            .map_err(|_| DaemonError::Store(format!("V128 {name} is missing")))?;
        if actual != expected_sql {
            return Err(DaemonError::Store(format!(
                "V128 {name} definition mismatch"
            )));
        }
    }
    Ok(())
}

/// Commit one record before the sidecar importer removes its file. Replaying an
/// identical ID is safe; divergent evidence under the same ID is an error.
pub(crate) fn insert(conn: &Connection, record: &RestartRecord) -> Result<()> {
    if record.version != 1 {
        return Err(DaemonError::Store(
            "unsupported watchdog restart record version".to_owned(),
        ));
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO daemon_restart_records
         (id, version, observed_at, last_healthy_at, failed_probes_json)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO NOTHING",
        params![
            record.id.to_string(),
            record.version,
            record
                .observed_at
                .to_rfc3339_opts(SecondsFormat::Nanos, true),
            record
                .last_healthy_at
                .to_rfc3339_opts(SecondsFormat::Nanos, true),
            serde_json::to_string(&record.failed_probes)?,
        ],
    )?;
    let existing = read_by_id(&tx, record.id)?
        .ok_or_else(|| DaemonError::Store("watchdog restart insert disappeared".to_owned()))?;
    if existing != *record {
        return Err(DaemonError::Store(
            "watchdog restart ID has divergent evidence".to_owned(),
        ));
    }
    tx.commit()?;
    Ok(())
}

/// Return the most recent committed restart for health and manager reads.
pub(crate) fn latest(conn: &Connection) -> Result<Option<DaemonRestartRecordV1>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM daemon_restart_records ORDER BY observed_at DESC, id DESC LIMIT 1"
    );
    let row = conn.query_row(&sql, [], read_row).optional()?;
    row.map(|record| record.map(public_record)).transpose()
}

fn read_by_id(conn: &Connection, id: Uuid) -> Result<Option<RestartRecord>> {
    let sql = format!("SELECT {COLUMNS} FROM daemon_restart_records WHERE id=?1");
    conn.query_row(&sql, [id.to_string()], read_row)
        .optional()?
        .transpose()
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<RestartRecord>> {
    let id: String = row.get(0)?;
    let version: u8 = row.get(1)?;
    let observed_at: String = row.get(2)?;
    let last_healthy_at: String = row.get(3)?;
    let failed_probes: String = row.get(4)?;
    Ok((|| {
        Ok(RestartRecord {
            version,
            id: Uuid::parse_str(&id).map_err(|error| DaemonError::Store(error.to_string()))?,
            observed_at: parse_time(&observed_at)?,
            last_healthy_at: parse_time(&last_healthy_at)?,
            failed_probes: serde_json::from_str(&failed_probes)?,
        })
    })())
}

fn parse_time(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| DaemonError::Store(error.to_string()))
}

fn public_record(record: RestartRecord) -> DaemonRestartRecordV1 {
    DaemonRestartRecordV1 {
        id: record.id,
        observed_at: record.observed_at,
        last_healthy_at: record.last_healthy_at,
        failed_probes: record.failed_probes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watchdog::{import_pending_restart_records, sidecar_path, write_restart_record};

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate_v128(&conn).unwrap();
        conn
    }

    fn record(id: Uuid, observed_at: &str) -> RestartRecord {
        RestartRecord {
            version: 1,
            id,
            observed_at: parse_time(observed_at).unwrap(),
            last_healthy_at: parse_time("2026-09-23T10:00:00.000000001Z").unwrap(),
            failed_probes: vec!["store_probe_timeout".to_owned()],
        }
    }

    #[test]
    fn identical_replay_commits_once_but_divergent_id_is_rejected() {
        let conn = fixture();
        let first = record(Uuid::new_v4(), "2026-09-23T10:01:00.000000001Z");
        insert(&conn, &first).unwrap();
        insert(&conn, &first).unwrap();
        let mut conflicting = first.clone();
        conflicting.failed_probes = vec!["rpc_timeout".to_owned()];
        assert!(insert(&conn, &conflicting).is_err());
        let count: i64 = conn
            .query_row("SELECT count(*) FROM daemon_restart_records", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            latest(&conn).unwrap().unwrap().failed_probes,
            first.failed_probes
        );
    }

    #[test]
    fn latest_uses_observation_time_and_preserves_nanoseconds() {
        let conn = fixture();
        assert!(latest(&conn).unwrap().is_none());
        let newer = record(Uuid::new_v4(), "2026-09-23T10:02:00.123456789Z");
        let older = record(Uuid::new_v4(), "2026-09-23T10:01:00.000000001Z");
        insert(&conn, &newer).unwrap();
        insert(&conn, &older).unwrap();
        let observed = latest(&conn).unwrap().unwrap();
        assert_eq!(observed.id, newer.id);
        assert_eq!(observed.observed_at, newer.observed_at);
        assert_eq!(observed.last_healthy_at, newer.last_healthy_at);
    }

    #[test]
    fn sidecar_replay_commits_then_unlinks_and_duplicate_replay_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let conn = fixture();
        let restart = record(Uuid::new_v4(), "2026-09-23T10:02:00.123456789Z");
        let path = sidecar_path(directory.path(), restart.id);
        write_restart_record(&path, &restart).unwrap();

        let replay = || {
            import_pending_restart_records(directory.path(), |item| {
                insert(&conn, item).map_err(std::io::Error::other)
            })
        };
        assert_eq!(replay().unwrap(), vec![restart.clone()]);
        assert!(!path.exists());
        assert_eq!(latest(&conn).unwrap().unwrap().id, restart.id);

        // A crash after commit but before unlink can present the same sidecar
        // again on the next boot. The row remains single and unchanged.
        write_restart_record(&path, &restart).unwrap();
        assert_eq!(replay().unwrap(), vec![restart]);
        let count: i64 = conn
            .query_row("SELECT count(*) FROM daemon_restart_records", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn migrated_evidence_is_immutable() {
        let conn = fixture();
        let restart = record(Uuid::new_v4(), "2026-09-23T10:02:00.123456789Z");
        insert(&conn, &restart).unwrap();
        assert!(
            conn.execute(
                "UPDATE daemon_restart_records SET failed_probes_json='[]' WHERE id=?1",
                [restart.id.to_string()],
            )
            .is_err()
        );
        assert!(
            conn.execute(
                "DELETE FROM daemon_restart_records WHERE id=?1",
                [restart.id.to_string()],
            )
            .is_err()
        );
        assert_eq!(
            latest(&conn).unwrap().unwrap().failed_probes,
            restart.failed_probes
        );
    }

    #[test]
    fn catalog_validation_detects_changed_restart_index() {
        let conn = fixture();
        conn.execute_batch(
            "DROP INDEX daemon_restart_records_latest;
             CREATE INDEX daemon_restart_records_latest ON daemon_restart_records(id);",
        )
        .unwrap();
        assert!(
            validate_v128_catalog(&conn)
                .unwrap_err()
                .to_string()
                .contains("V128 daemon_restart_records_latest definition mismatch")
        );
    }

    #[test]
    fn store_api_reads_committed_restart_evidence() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.latest_daemon_restart_record().unwrap().is_none());
        let restart = record(Uuid::new_v4(), "2026-09-23T10:02:00.123456789Z");
        store.persist_daemon_restart_record(&restart).unwrap();
        let observed = store.latest_daemon_restart_record().unwrap().unwrap();
        assert_eq!(observed.id, restart.id);
        assert_eq!(observed.failed_probes, restart.failed_probes);
    }

    #[test]
    fn store_replays_v128_from_exact_v127_and_rejects_catalog_drift_on_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("rsi.db");
        let store = Store::open(&database).unwrap();
        // V130 (#670) and V129 (#634) sit above V128; rewind them first so V128
        // is the head.
        crate::store::agent_child_relaunch_intents::rewind_v130_fixture_to_v129(&store.conn);
        crate::store::topology_v129::rewind_v129_fixture_to_v128(&store.conn);
        store
            .conn
            .execute_batch(
                "DROP TRIGGER daemon_restart_records_no_update;
                 DROP TRIGGER daemon_restart_records_no_delete;
                 DROP INDEX daemon_restart_records_latest;
                 DROP TABLE daemon_restart_records;
                 PRAGMA user_version = 127;",
            )
            .unwrap();
        drop(store);

        let reopened = Store::open(&database).unwrap();
        assert_eq!(
            reopened
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .unwrap(),
            crate::store::LATEST_SCHEMA_VERSION
        );
        validate_v128_catalog(&reopened.conn).unwrap();
        reopened
            .conn
            .execute_batch(
                "DROP INDEX daemon_restart_records_latest;
                 CREATE INDEX daemon_restart_records_latest ON daemon_restart_records(id);",
            )
            .unwrap();
        drop(reopened);
        assert!(Store::open(&database).is_err());
    }
}
