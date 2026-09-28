//! V133 append-only ledger of agent topology verb calls (#633).
//!
//! One row per `AgentTopology*` call after token resolution: the durable
//! audit for calls that have no execution (upsert, list, pre-admission
//! refusals) and the idempotency ledger for `AgentTopologyUpsert`. The
//! released V129 `topology_events` table is unchanged; execution-scoped events
//! stay there.
//!
//! `idempotency_key` is bound only on the first non-refused row of a verb
//! whose idempotency this ledger owns (upsert); replays and refusals carry
//! `NULL`, so the partial unique index binds exactly one request per key.

use super::Store;
use crate::error::{DaemonError, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

// RSI-RELEASED-MIGRATION-BEGIN: v133-topology-agent-requests-catalog
pub(in crate::store) const V133_TOPOLOGY_AGENT_REQUESTS_SQL: &str = "CREATE TABLE topology_agent_requests (
    id INTEGER PRIMARY KEY,
    verb TEXT NOT NULL CHECK(verb IN ('upsert','list','execute','get_execution','interrupt','resolve_attempt')),
    caller_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(caller_session_id)),
    caller_kind TEXT CHECK(caller_kind IS NULL OR caller_kind IN ('manager','epic_lead')),
    epic_id TEXT CHECK(epic_id IS NULL OR rsi_uuid_is_canonical(epic_id)),
    topology_id TEXT CHECK(topology_id IS NULL OR rsi_uuid_is_canonical(topology_id)),
    execution_id TEXT CHECK(execution_id IS NULL OR rsi_uuid_is_canonical(execution_id)),
    idempotency_key TEXT CHECK(idempotency_key IS NULL OR length(idempotency_key) BETWEEN 1 AND 128),
    request_digest TEXT NOT NULL CHECK(rsi_sha256_digest_is_canonical(request_digest)),
    outcome TEXT NOT NULL CHECK(outcome IN ('accepted','deduplicated','refused')),
    code TEXT CHECK(code IS NULL OR length(code) BETWEEN 1 AND 64),
    result_json TEXT CHECK(result_json IS NULL OR (json_valid(result_json) AND length(result_json) <= 65536)),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    CHECK((outcome = 'refused') = (code IS NOT NULL)),
    CHECK(idempotency_key IS NULL OR outcome IN ('accepted','deduplicated'))
)";
const V133_INDEXES: [(&str, &str); 1] = [(
    "idx_topology_agent_requests_key",
    "CREATE UNIQUE INDEX idx_topology_agent_requests_key ON topology_agent_requests(caller_session_id,verb,idempotency_key) WHERE idempotency_key IS NOT NULL",
)];
const V133_TRIGGERS: [(&str, &str); 2] = [
    (
        "topology_agent_requests_no_delete",
        "CREATE TRIGGER topology_agent_requests_no_delete BEFORE DELETE ON topology_agent_requests BEGIN SELECT RAISE(ABORT,'topology_agent_requests_append_only'); END",
    ),
    (
        "topology_agent_requests_no_update",
        "CREATE TRIGGER topology_agent_requests_no_update BEFORE UPDATE ON topology_agent_requests BEGIN SELECT RAISE(ABORT,'topology_agent_requests_append_only'); END",
    ),
];

pub(in crate::store) fn validate_v133_catalog(connection: &Connection) -> Result<()> {
    let objects = std::iter::once((
        "table",
        "topology_agent_requests",
        V133_TOPOLOGY_AGENT_REQUESTS_SQL,
    ))
    .chain(
        V133_INDEXES
            .iter()
            .map(|(name, sql)| ("index", *name, *sql)),
    )
    .chain(
        V133_TRIGGERS
            .iter()
            .map(|(name, sql)| ("trigger", *name, *sql)),
    );
    for (kind, name, expected) in objects {
        let actual: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
                [kind, name],
                |row| row.get(0),
            )
            .map_err(|_| DaemonError::Store(format!("V133 {kind} {name} is missing")))?;
        if actual != expected {
            return Err(DaemonError::Store(format!(
                "V133 {kind} {name} definition mismatch"
            )));
        }
    }
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v133-topology-agent-requests-catalog

// RSI-RELEASED-MIGRATION-BEGIN: v133-topology-agent-requests-migration
pub(in crate::store) fn apply_v133_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 132 {
        return Err(DaemonError::Store(format!(
            "V133 requires exact V132 source, found V{version}"
        )));
    }
    tx.execute_batch(V133_TOPOLOGY_AGENT_REQUESTS_SQL)?;
    for (_, sql) in V133_INDEXES {
        tx.execute_batch(sql)?;
    }
    for (_, sql) in V133_TRIGGERS {
        tx.execute_batch(sql)?;
    }
    validate_v133_catalog(&tx)?;
    tx.pragma_update(None, "user_version", 133)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: v133-topology-agent-requests-migration

#[cfg(test)]
pub(crate) const V133_CATALOG_OBJECTS: [(&str, &str); 4] = [
    ("table", "topology_agent_requests"),
    ("index", "idx_topology_agent_requests_key"),
    ("trigger", "topology_agent_requests_no_delete"),
    ("trigger", "topology_agent_requests_no_update"),
];

/// Test teardown: rewind a head database to the exact V132 catalog.
#[cfg(test)]
#[allow(clippy::expect_used)]
pub(crate) fn rewind_v133_fixture_to_v132(connection: &Connection) {
    let version: i32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("schema version");
    assert_eq!(
        version, 133,
        "V133 topology agent ledger rewind requires V133"
    );
    connection
        .execute_batch(
            "DROP TRIGGER topology_agent_requests_no_update;
             DROP TRIGGER topology_agent_requests_no_delete;
             DROP INDEX idx_topology_agent_requests_key;
             DROP TABLE topology_agent_requests;
             PRAGMA user_version=132;",
        )
        .expect("rewind V133");
}

/// Result of one agent topology verb call, as recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentRequestOutcome {
    Accepted,
    Deduplicated,
    Refused,
}

impl AgentRequestOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Deduplicated => "deduplicated",
            Self::Refused => "refused",
        }
    }
}

/// One append-only ledger row.
#[derive(Clone, Debug)]
pub(crate) struct AgentRequestRow<'a> {
    pub(crate) verb: &'a str,
    pub(crate) caller_session_id: Uuid,
    /// `manager`, `epic_lead`, or `None` when the caller holds no authority.
    pub(crate) caller_kind: Option<&'a str>,
    pub(crate) epic_id: Option<Uuid>,
    pub(crate) topology_id: Option<Uuid>,
    pub(crate) execution_id: Option<Uuid>,
    /// Bound only on the first non-refused row of a ledger-owned key.
    pub(crate) idempotency_key: Option<&'a str>,
    pub(crate) request_digest: &'a str,
    pub(crate) outcome: AgentRequestOutcome,
    pub(crate) code: Option<&'a str>,
    pub(crate) result_json: Option<String>,
}

/// The bound receipt of a ledger-owned idempotency key.
#[derive(Clone, Debug)]
pub(crate) struct BoundRequest {
    pub(crate) request_digest: String,
    pub(crate) result_json: Option<String>,
}

/// Largest stored `result_json`; a larger result is recorded as `NULL`.
const RESULT_JSON_MAX: usize = 65_536;

pub(crate) fn insert_agent_request(
    connection: &Connection,
    row: &AgentRequestRow<'_>,
) -> Result<()> {
    let result_json = row
        .result_json
        .as_deref()
        .filter(|json| json.len() <= RESULT_JSON_MAX);
    connection.execute(
        "INSERT INTO topology_agent_requests (verb,caller_session_id,caller_kind,epic_id,topology_id,\
            execution_id,idempotency_key,request_digest,outcome,code,result_json,created_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![
            row.verb,
            row.caller_session_id.to_string(),
            row.caller_kind,
            row.epic_id.map(|id| id.to_string()),
            row.topology_id.map(|id| id.to_string()),
            row.execution_id.map(|id| id.to_string()),
            row.idempotency_key,
            row.request_digest,
            row.outcome.as_str(),
            row.code,
            result_json,
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        ],
    )?;
    Ok(())
}

pub(crate) fn bound_request(
    connection: &Connection,
    caller_session_id: Uuid,
    verb: &str,
    idempotency_key: &str,
) -> Result<Option<BoundRequest>> {
    Ok(connection
        .query_row(
            "SELECT request_digest,result_json FROM topology_agent_requests \
             WHERE caller_session_id=?1 AND verb=?2 AND idempotency_key=?3",
            params![caller_session_id.to_string(), verb, idempotency_key],
            |row| {
                Ok(BoundRequest {
                    request_digest: row.get(0)?,
                    result_json: row.get(1)?,
                })
            },
        )
        .optional()?)
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

    fn row(key: Option<&str>, outcome: AgentRequestOutcome) -> AgentRequestRow<'_> {
        AgentRequestRow {
            verb: "upsert",
            caller_session_id: Uuid::nil(),
            caller_kind: Some("epic_lead"),
            epic_id: None,
            topology_id: None,
            execution_id: None,
            idempotency_key: key,
            request_digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            outcome,
            code: (outcome == AgentRequestOutcome::Refused).then_some("authority_denied"),
            result_json: Some("{\"ok\":true}".into()),
        }
    }

    /// V133 is additive and pinned, refuses a non-V132 source, and drift
    /// refuses reopen.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
    #[test]
    fn t4_v133_is_additive_pinned_and_rejects_drift() {
        let directory = tempfile::tempdir().expect("test directory");
        let database = directory.path().join("v132-to-v133.sqlite");
        {
            let store = Store::open(&database).expect("current database");
            assert_eq!(user_version(&store), super::super::LATEST_SCHEMA_VERSION);
            super::super::tests::rewind_post_v121_tail_to(&store.conn, 132);
        }
        let store = Store::open(&database).expect("upgrade V132 to V133");
        assert_eq!(user_version(&store), super::super::LATEST_SCHEMA_VERSION);
        validate_v133_catalog(&store.conn).expect("pinned catalog");

        store
            .conn
            .execute_batch("PRAGMA user_version=129;")
            .expect("forge older source");
        let error = apply_v133_migration(&store).expect_err("V132 is required");
        assert!(
            error
                .to_string()
                .contains("V133 requires exact V132 source")
        );
        store
            .conn
            .pragma_update(None, "user_version", super::super::LATEST_SCHEMA_VERSION)
            .expect("restore current schema version");
        store
            .conn
            .execute_batch(
                "DROP INDEX idx_topology_agent_requests_key; \
                 CREATE UNIQUE INDEX idx_topology_agent_requests_key ON topology_agent_requests(caller_session_id,idempotency_key) WHERE idempotency_key IS NOT NULL;",
            )
            .expect("tamper with the V133 catalog");
        drop(store);
        let error = Store::open(&database)
            .err()
            .expect("catalog drift must refuse reopen");
        assert!(error.to_string().contains("V133 index"));
    }

    /// Rows are append-only, a key binds once, and refusals never bind.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-02"))]
    #[test]
    fn t4_v133_ledger_is_append_only_and_binds_each_key_once() {
        let store = Store::open_in_memory().expect("store");
        let conn = &store.conn;
        insert_agent_request(conn, &row(Some("k"), AgentRequestOutcome::Accepted)).expect("bind");
        insert_agent_request(conn, &row(None, AgentRequestOutcome::Deduplicated)).expect("replay");
        insert_agent_request(conn, &row(None, AgentRequestOutcome::Refused)).expect("refusal");
        assert!(
            insert_agent_request(conn, &row(Some("k"), AgentRequestOutcome::Accepted)).is_err()
        );
        assert!(insert_agent_request(conn, &row(Some("r"), AgentRequestOutcome::Refused)).is_err());
        let bound = bound_request(conn, Uuid::nil(), "upsert", "k")
            .expect("lookup")
            .expect("bound");
        assert_eq!(bound.result_json.as_deref(), Some("{\"ok\":true}"));
        assert!(
            conn.execute("DELETE FROM topology_agent_requests", [])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE topology_agent_requests SET code='x'", [])
                .is_err()
        );
        let count: i64 = conn
            .query_row("SELECT count(*) FROM topology_agent_requests", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(count, 3);
    }
}
