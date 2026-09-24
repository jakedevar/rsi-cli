//! V129 durable topology executor tables (#634, plan §2.1).
//!
//! Additive only: three new tables plus nullable/defaulted `topologies`
//! columns. No existing row is rewritten.

use super::Store;
use crate::error::{DaemonError, Result};
use rusqlite::{Connection, Transaction, TransactionBehavior};

// RSI-RELEASED-MIGRATION-BEGIN: v129-topology-executor-catalog
pub(in crate::store) const V129_TOPOLOGY_EXECUTIONS_SQL: &str = "CREATE TABLE topology_executions (
    id TEXT PRIMARY KEY NOT NULL,
    topology_id TEXT REFERENCES topologies(id) ON DELETE SET NULL,
    topology_revision INTEGER,
    topology_name_snapshot TEXT NOT NULL,
    workflow_id TEXT NOT NULL,
    definition_json TEXT NOT NULL CHECK(json_valid(definition_json)),
    definition_digest TEXT NOT NULL,
    project_id TEXT,
    epic_id TEXT,
    parent_session_id TEXT,
    requested_by_kind TEXT NOT NULL CHECK(requested_by_kind IN ('operator','manager','epic_lead','schedule')),
    requested_by_session_id TEXT,
    scope_version INTEGER,
    policy_digest TEXT,
    idempotency_key TEXT,
    request_fingerprint TEXT,
    repo_root TEXT NOT NULL,
    base_ref TEXT,
    base_commit TEXT NOT NULL,
    custody_plan_json TEXT NOT NULL CHECK(json_valid(custody_plan_json)),
    status TEXT NOT NULL CHECK(status IN ('accepted','running','cancelling','succeeded','failed','cancelled','blocked')),
    blocked_reason_json TEXT CHECK(blocked_reason_json IS NULL OR json_valid(blocked_reason_json)),
    input_json TEXT CHECK(input_json IS NULL OR json_valid(input_json)),
    output_json TEXT CHECK(output_json IS NULL OR json_valid(output_json)),
    error TEXT,
    lease_owner TEXT,
    lease_expires_at TEXT,
    row_version INTEGER NOT NULL DEFAULT 1 CHECK(row_version > 0),
    max_node_attempts INTEGER NOT NULL DEFAULT 64 CHECK(max_node_attempts BETWEEN 1 AND 64),
    deadline_at TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL,
    UNIQUE(requested_by_session_id,idempotency_key)
)";
pub(in crate::store) const V129_TOPOLOGY_NODE_ATTEMPTS_SQL: &str = "CREATE TABLE topology_node_attempts (
    id TEXT PRIMARY KEY NOT NULL,
    execution_id TEXT NOT NULL REFERENCES topology_executions(id),
    node_id TEXT NOT NULL,
    iteration INTEGER NOT NULL CHECK(iteration >= 0),
    attempt_no INTEGER NOT NULL CHECK(attempt_no > 0),
    node_kind TEXT NOT NULL CHECK(node_kind IN ('session','command','review','land','gate')),
    status TEXT NOT NULL CHECK(status IN ('reserved','launching','running','waiting','succeeded','failed','blocked','skipped','cancelled','interrupted','lost')),
    dedup_key TEXT NOT NULL UNIQUE,
    session_id TEXT,
    catalog_op TEXT,
    effect_class TEXT CHECK(effect_class IS NULL OR effect_class IN ('check','publish')),
    process_group_id INTEGER,
    boot_id TEXT,
    exit_code INTEGER,
    review_assignment_id TEXT,
    integrate_action_id TEXT,
    custody_id TEXT,
    sandbox_root TEXT,
    sandbox_branch TEXT,
    base_commit TEXT,
    pre_head TEXT,
    result_commit TEXT,
    pin_ref TEXT,
    input_json TEXT NOT NULL CHECK(json_valid(input_json)),
    output_json TEXT CHECK(output_json IS NULL OR json_valid(output_json)),
    output_digest TEXT,
    failure_class TEXT,
    error TEXT,
    resolution TEXT CHECK(resolution IS NULL OR resolution IN ('accepted','discarded','retried')),
    resolved_by_kind TEXT,
    resolved_by_session_id TEXT,
    resolved_at TEXT,
    preserved_ref TEXT,
    preserved_commit TEXT,
    preserved_paths_digest TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL,
    UNIQUE(execution_id,node_id,iteration,attempt_no)
)";
pub(in crate::store) const V129_TOPOLOGY_EVENTS_SQL: &str = "CREATE TABLE topology_events (
    id INTEGER PRIMARY KEY,
    execution_id TEXT NOT NULL REFERENCES topology_executions(id),
    execution_seq INTEGER NOT NULL CHECK(execution_seq > 0),
    topology_id TEXT,
    node_id TEXT,
    attempt_id TEXT,
    kind TEXT NOT NULL,
    actor_kind TEXT NOT NULL,
    actor_session_id TEXT,
    payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
    created_at TEXT NOT NULL,
    UNIQUE(execution_id,execution_seq)
)";
const V129_INDEXES: [(&str, &str); 2] = [
    (
        "idx_topology_executions_active",
        "CREATE INDEX idx_topology_executions_active ON topology_executions(status,created_at) WHERE status IN ('accepted','running','cancelling')",
    ),
    (
        "idx_topology_node_attempts_session",
        "CREATE INDEX idx_topology_node_attempts_session ON topology_node_attempts(session_id) WHERE session_id IS NOT NULL",
    ),
];
const V129_TOPOLOGIES_COLUMNS: [(&str, &str); 8] = [
    (
        "owner_kind",
        "TEXT NOT NULL DEFAULT 'operator' CHECK(owner_kind IN ('operator','manager','epic'))",
    ),
    ("owner_session_id", "TEXT"),
    ("project_id", "TEXT"),
    ("epic_id", "TEXT"),
    ("revision", "INTEGER NOT NULL DEFAULT 1 CHECK(revision > 0)"),
    ("definition_digest", "TEXT"),
    (
        "shared",
        "INTEGER NOT NULL DEFAULT 0 CHECK(shared IN (0,1))",
    ),
    ("archived_at", "TEXT"),
];

pub(in crate::store) fn validate_v129_catalog(connection: &Connection) -> Result<()> {
    for (name, expected) in [
        ("topology_executions", V129_TOPOLOGY_EXECUTIONS_SQL),
        ("topology_node_attempts", V129_TOPOLOGY_NODE_ATTEMPTS_SQL),
        ("topology_events", V129_TOPOLOGY_EVENTS_SQL),
    ] {
        let actual: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |row| row.get(0),
            )
            .map_err(|_| DaemonError::Store(format!("V129 table {name} is missing")))?;
        if actual != expected {
            return Err(DaemonError::Store(format!(
                "V129 table {name} definition mismatch"
            )));
        }
    }
    for (name, expected) in V129_INDEXES {
        let actual: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name=?1",
                [name],
                |row| row.get(0),
            )
            .map_err(|_| DaemonError::Store(format!("V129 index {name} is missing")))?;
        if actual != expected {
            return Err(DaemonError::Store(format!(
                "V129 index {name} definition mismatch"
            )));
        }
    }
    let mut statement = connection.prepare("SELECT name FROM pragma_table_info('topologies')")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (column, _) in V129_TOPOLOGIES_COLUMNS {
        if !columns.iter().any(|name| name == column) {
            return Err(DaemonError::Store(format!(
                "V129 topologies.{column} is missing"
            )));
        }
    }
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v129-topology-executor-catalog

// RSI-RELEASED-MIGRATION-BEGIN: v129-topology-executor-migration
pub(in crate::store) fn apply_v129_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 128 {
        return Err(DaemonError::Store(format!(
            "V129 requires exact V128 source, found V{version}"
        )));
    }

    for (column, definition) in V129_TOPOLOGIES_COLUMNS {
        tx.execute_batch(&format!(
            "ALTER TABLE topologies ADD COLUMN {column} {definition}"
        ))?;
    }
    tx.execute_batch(V129_TOPOLOGY_EXECUTIONS_SQL)?;
    tx.execute_batch(V129_TOPOLOGY_NODE_ATTEMPTS_SQL)?;
    tx.execute_batch(V129_TOPOLOGY_EVENTS_SQL)?;
    for (_, sql) in V129_INDEXES {
        tx.execute_batch(sql)?;
    }
    validate_v129_catalog(&tx)?;

    let foreign_key_errors: i64 =
        tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_errors != 0 {
        return Err(DaemonError::Store(format!(
            "V129 topology executor found {foreign_key_errors} foreign-key violations"
        )));
    }

    tx.pragma_update(None, "user_version", 129)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v129-topology-executor-migration

/// Test teardown: rewind a head database to the exact V128 catalog.
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) fn rewind_v129_fixture_to_v128(connection: &Connection) {
    let version: i32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("schema version");
    if version == 128 {
        return;
    }
    assert_eq!(version, 129, "V129-to-V128 rewind requires V129");
    let mut sql = String::from(
        "DROP INDEX idx_topology_executions_active; \
         DROP INDEX idx_topology_node_attempts_session; \
         DROP TABLE topology_events; DROP TABLE topology_node_attempts; \
         DROP TABLE topology_executions;",
    );
    for (column, _) in V129_TOPOLOGIES_COLUMNS {
        sql.push_str(&format!(" ALTER TABLE topologies DROP COLUMN {column};"));
    }
    sql.push_str(" PRAGMA user_version=128;");
    connection.execute_batch(&sql).expect("rewind V129");
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn user_version(store: &Store) -> i32 {
        store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("schema version")
    }

    /// T2-A8: the V129 catalog is pinned, additive over existing topology
    /// rows, refuses a non-V128 source, and drift refuses reopen.
    #[test]
    fn t2_a8_v129_is_additive_pinned_and_rejects_drift() {
        let directory = tempfile::tempdir().expect("test directory");
        let database = directory.path().join("v128-to-v129.sqlite");
        {
            let store = Store::open(&database).expect("current database");
            // V130 (#670) sits above V129; rewind it first so V129 is the head.
            crate::store::agent_child_relaunch_intents::rewind_v130_fixture_to_v129(&store.conn);
            assert_eq!(user_version(&store), 129);
            rewind_v129_fixture_to_v128(&store.conn);
            store
                .conn
                .execute(
                    "INSERT INTO topologies (id,name,definition_json,created_at,updated_at) \
                     VALUES ('t-1','legacy','{\"nodes\":[],\"edges\":[]}','2026-01-01T00:00:00.000000000Z','2026-01-01T00:00:00.000000000Z')",
                    [],
                )
                .expect("legacy topology row");
        }

        let store = Store::open(&database).expect("upgrade V128 to V129");
        assert_eq!(user_version(&store), crate::store::LATEST_SCHEMA_VERSION);
        validate_v129_catalog(&store.conn).expect("pinned catalog");
        let (owner_kind, revision, shared, name): (String, i64, i64, String) = store
            .conn
            .query_row(
                "SELECT owner_kind,revision,shared,name FROM topologies WHERE id='t-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("legacy row survives");
        assert_eq!(
            (owner_kind.as_str(), revision, shared, name.as_str()),
            ("operator", 1, 0, "legacy")
        );

        // Wrong source version is refused without touching the catalog.
        store
            .conn
            .execute_batch("PRAGMA user_version=127;")
            .expect("forge older source");
        let error = apply_v129_migration(&store).expect_err("V128 is required");
        assert!(
            error
                .to_string()
                .contains("V129 requires exact V128 source")
        );
        store
            .conn
            .execute_batch(&format!(
                "PRAGMA user_version={}; DROP INDEX idx_topology_node_attempts_session; \
                 CREATE INDEX idx_topology_node_attempts_session ON topology_node_attempts(execution_id);",
                crate::store::LATEST_SCHEMA_VERSION
            ))
            .expect("tamper with the V129 catalog");
        drop(store);
        let error = Store::open(&database)
            .err()
            .expect("catalog drift must refuse reopen");
        assert!(error.to_string().contains("V129 index"));
    }
}
