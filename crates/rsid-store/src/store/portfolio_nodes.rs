//! Fractal manager hierarchy S2 (#1236, plan §2.1, §3, §5 M1): portfolio
//! node identity.
//!
//! A manager above project level is a `manager_portfolio_nodes` row. Its
//! grants stay in `global_manager_grants` (extended, not replaced, so the
//! message, appointment and transfer FKs keep their meaning), keyed by
//! `node_id`; one grant per node is active. `manager_portfolio_coverage` is
//! the projection of every active grant's projects, rebuilt in the grant
//! transaction; its `PRIMARY KEY(project_id, depth)` makes sibling and root
//! coverage disjoint by construction.
//!
//! The authority epoch is the `grant_version` of the operator grant that
//! opened it: it bumps on every operator edit and never on a context-cap seat
//! transfer, so a successor keeps its predecessor's V2 ledger principal
//! `(project, seat root, epoch)`. The seat root is the seat of that grant.
//! Authority code never reads `tier_label`; only the `*GlobalManager`
//! compatibility shims do, to find the single root labelled `global`.

use rsi_common::global_manager::GlobalManagerGrantV1;
use rsi_common::grant_narrowing::{
    GrantBoundsV1, allowance_overflow, allowance_worsened, grant_narrows, handed_policy_narrows,
    portfolio_bounds, portfolio_grant_bounds,
};
use rsi_common::harness_manager_v2::ManagerPolicyV2;
use rsi_common::portfolio_nodes::{
    ConfigurePortfolioNodeRequestV1, GLOBAL_MANAGER_AMBIGUOUS, GLOBAL_MANAGER_SEAT_UNAVAILABLE,
    GLOBAL_TIER_LABEL, MANAGER_ALLOWANCE_EXCEEDED, MANAGER_ANCESTOR_REVOKED,
    MANAGER_NODE_ROOT_OPERATOR_ONLY, MANAGER_NODE_STALE, MANAGER_SCOPE_OVERLAP,
    PORTFOLIO_ADOPT_NOT_ROOT, PORTFOLIO_COVERAGE_NOT_SUPERSET, PORTFOLIO_IDEMPOTENCY_CONFLICT,
    PORTFOLIO_NODE_NOT_FOUND, PORTFOLIO_PARENT_IMMUTABLE, PORTFOLIO_TIER_LABEL_IMMUTABLE,
    PortfolioCoverageV1, PortfolioNodeV1, RevokePortfolioNodeRequestV1,
};
use rsi_common::types::SessionStatus;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

use super::Store;
use super::global_manager::{GrantRow, grant_from_row, read_grant_row};
use crate::error::{DaemonError, Result};

// The provisional number is assigned at landing.
// RSI-RELEASED-MIGRATION-BEGIN: portfolio-node-migration
/// Provisional schema version of portfolio node identity (M1).
pub(crate) const PORTFOLIO_NODE_SCHEMA_VERSION: i32 = 154;

/// New tables and the additive grant columns. `node_id` stays NULL on
/// revoked pre-M1 grant rows (audit history).
const CATALOG_TABLES: &str = "CREATE TABLE manager_portfolio_nodes (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    tier_label TEXT NOT NULL CHECK(length(tier_label) BETWEEN 1 AND 32 AND tier_label=trim(tier_label)),
    state TEXT NOT NULL CHECK(state IN ('active','revoked')),
    authority_epoch INTEGER NOT NULL CHECK(authority_epoch>0),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at))
);
CREATE TRIGGER manager_portfolio_nodes_no_delete BEFORE DELETE ON manager_portfolio_nodes
 BEGIN SELECT RAISE(ABORT,'portfolio nodes are retained'); END;
CREATE TRIGGER manager_portfolio_nodes_identity_immutable BEFORE UPDATE ON manager_portfolio_nodes
 WHEN NEW.id IS NOT OLD.id OR NEW.tier_label IS NOT OLD.tier_label OR NEW.created_at IS NOT OLD.created_at
 BEGIN SELECT RAISE(ABORT,'only state, authority_epoch and updated_at of a portfolio node change'); END;
CREATE TRIGGER manager_portfolio_nodes_revoked_final BEFORE UPDATE ON manager_portfolio_nodes
 WHEN OLD.state='revoked' AND NEW.state<>'revoked'
 BEGIN SELECT RAISE(ABORT,'a revoked portfolio node stays revoked'); END;
CREATE TRIGGER manager_portfolio_nodes_epoch_forward BEFORE UPDATE ON manager_portfolio_nodes
 WHEN NEW.authority_epoch<OLD.authority_epoch
 BEGIN SELECT RAISE(ABORT,'a portfolio node authority epoch never moves back'); END;
CREATE TABLE manager_portfolio_coverage (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(project_id)),
    depth INTEGER NOT NULL CHECK(depth>=0),
    node_id TEXT NOT NULL REFERENCES manager_portfolio_nodes(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(node_id)),
    grant_version INTEGER NOT NULL CHECK(grant_version>0),
    PRIMARY KEY(project_id, depth)
);
CREATE INDEX manager_portfolio_coverage_by_node ON manager_portfolio_coverage(node_id);
CREATE TRIGGER manager_portfolio_coverage_no_update BEFORE UPDATE ON manager_portfolio_coverage
 BEGIN SELECT RAISE(ABORT,'portfolio coverage rows are replaced, never edited'); END;
CREATE TRIGGER manager_portfolio_coverage_live_retained BEFORE DELETE ON manager_portfolio_coverage
 WHEN EXISTS(SELECT 1 FROM global_manager_grants g WHERE g.node_id=OLD.node_id AND g.state='active' AND g.grant_version=OLD.grant_version)
 BEGIN SELECT RAISE(ABORT,'live portfolio coverage is retained'); END;
ALTER TABLE global_manager_grants ADD COLUMN node_id TEXT REFERENCES manager_portfolio_nodes(id) ON DELETE RESTRICT
    CHECK(node_id IS NULL OR rsi_uuid_is_canonical(node_id));
ALTER TABLE global_manager_grants ADD COLUMN parent_node_id TEXT REFERENCES manager_portfolio_nodes(id) ON DELETE RESTRICT
    CHECK(parent_node_id IS NULL OR rsi_uuid_is_canonical(parent_node_id));
ALTER TABLE global_manager_grants ADD COLUMN grantor TEXT NOT NULL DEFAULT 'operator'
    CHECK(grantor='operator' OR (length(grantor)=41 AND substr(grantor,1,5)='node:' AND rsi_uuid_is_canonical(substr(grantor,6))));
ALTER TABLE global_manager_grants ADD COLUMN child_policy_json TEXT
    CHECK(child_policy_json IS NULL OR (json_valid(child_policy_json) AND json_type(child_policy_json)='object'));
ALTER TABLE global_manager_grants ADD COLUMN max_direct_reports INTEGER NOT NULL DEFAULT 5
    CHECK(max_direct_reports BETWEEN 0 AND 64);";

/// Coverage rows name a live node grant; written after the backfill.
const CATALOG_COVERAGE_GUARD: &str = "CREATE TRIGGER manager_portfolio_coverage_live_grant BEFORE INSERT ON manager_portfolio_coverage
 WHEN NOT EXISTS(SELECT 1 FROM global_manager_grants g JOIN manager_portfolio_nodes n ON n.id=g.node_id
   WHERE g.node_id=NEW.node_id AND g.state='active' AND g.grant_version=NEW.grant_version AND n.state='active')
 BEGIN SELECT RAISE(ABORT,'portfolio coverage names a live node grant'); END;";

/// The grant guards that replace V150's single-active index and identity
/// trigger. The per-node index keeps V150's index name, so V150's catalog
/// projection still names an existing object.
const CATALOG_GRANT_GUARDS: &str = "DROP INDEX global_manager_grants_one_active;
CREATE UNIQUE INDEX global_manager_grants_one_active ON global_manager_grants(node_id) WHERE state='active';
CREATE UNIQUE INDEX global_manager_grants_one_active_seat ON global_manager_grants(seat_session_id) WHERE state='active';
DROP TRIGGER global_manager_grants_identity_immutable;
CREATE TRIGGER global_manager_grants_identity_immutable BEFORE UPDATE ON global_manager_grants
 WHEN NEW.id IS NOT OLD.id OR NEW.grant_version IS NOT OLD.grant_version
   OR NEW.seat_session_id IS NOT OLD.seat_session_id OR NEW.project_ids_json IS NOT OLD.project_ids_json
   OR NEW.allowed_launches_json IS NOT OLD.allowed_launches_json OR NEW.project_policy_json IS NOT OLD.project_policy_json
   OR NEW.operator_origin IS NOT OLD.operator_origin OR NEW.idempotency_key IS NOT OLD.idempotency_key
   OR NEW.created_at IS NOT OLD.created_at OR NEW.node_id IS NOT OLD.node_id
   OR NEW.parent_node_id IS NOT OLD.parent_node_id OR NEW.grantor IS NOT OLD.grantor
   OR NEW.child_policy_json IS NOT OLD.child_policy_json OR NEW.max_direct_reports IS NOT OLD.max_direct_reports
 BEGIN SELECT RAISE(ABORT,'only state and updated_at of a global manager grant change'); END;
CREATE TRIGGER global_manager_grants_active_node BEFORE INSERT ON global_manager_grants
 WHEN NEW.state='active' AND (NEW.node_id IS NULL
   OR NOT EXISTS(SELECT 1 FROM manager_portfolio_nodes WHERE id=NEW.node_id AND state='active'))
 BEGIN SELECT RAISE(ABORT,'an active global manager grant names a live portfolio node'); END;";

/// M1 catalog objects, for presence assertions.
#[cfg(test)]
pub(crate) const CATALOG_OBJECTS: [(&str, &str); 14] = [
    ("table", "manager_portfolio_nodes"),
    ("trigger", "manager_portfolio_nodes_no_delete"),
    ("trigger", "manager_portfolio_nodes_identity_immutable"),
    ("trigger", "manager_portfolio_nodes_revoked_final"),
    ("trigger", "manager_portfolio_nodes_epoch_forward"),
    ("table", "manager_portfolio_coverage"),
    ("index", "manager_portfolio_coverage_by_node"),
    ("trigger", "manager_portfolio_coverage_no_update"),
    ("trigger", "manager_portfolio_coverage_live_retained"),
    ("trigger", "manager_portfolio_coverage_live_grant"),
    ("index", "global_manager_grants_one_active"),
    ("index", "global_manager_grants_one_active_seat"),
    ("trigger", "global_manager_grants_identity_immutable"),
    ("trigger", "global_manager_grants_active_node"),
];

/// One active v0 grant becomes one root node labelled `global`: the epoch is
/// S1's chain head (the newest operator grant at or before the active one),
/// the node id is set on the active row only and its existing projects are
/// covered at depth 0.
fn backfill_v0_grant(tx: &Connection, now: &str) -> Result<()> {
    let active: Option<(String, i64, String)> = tx
        .query_row(
            "SELECT id,grant_version,project_ids_json FROM global_manager_grants WHERE state='active'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((grant_id, version, projects)) = active else {
        return Ok(());
    };
    let head: Option<(i64, String)> = tx
        .query_row(
            "SELECT grant_version,created_at FROM global_manager_grants
             WHERE grant_version<=?1 AND idempotency_key NOT LIKE 'context-cap:%'
             ORDER BY grant_version DESC LIMIT 1",
            [version],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((epoch, started)) = head else {
        return Err(DaemonError::Store(
            "portfolio backfill: the active global grant has no operator grant at its chain head"
                .into(),
        ));
    };
    let node = Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO manager_portfolio_nodes(id,tier_label,state,authority_epoch,created_at,updated_at)
         VALUES(?1,'global','active',?2,?3,?4)",
        params![node, epoch, started, now],
    )?;
    tx.execute(
        "UPDATE global_manager_grants SET node_id=?2 WHERE id=?1 AND state='active'",
        params![grant_id, node],
    )?;
    tx.execute(
        "INSERT INTO manager_portfolio_coverage(project_id,depth,node_id,grant_version)
         SELECT p.id,0,?1,?2 FROM json_each(?3) j JOIN projects p ON p.id=j.value",
        params![node, version, projects],
    )?;
    Ok(())
}

/// The backfill's counts must agree before the version is bumped.
fn validate_backfill(tx: &Connection) -> Result<()> {
    let (active_grants, unbound, active_nodes, expected_coverage, coverage, ahead): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = tx.query_row(
        "SELECT
            (SELECT count(*) FROM global_manager_grants WHERE state='active'),
            (SELECT count(*) FROM global_manager_grants WHERE state='active' AND node_id IS NULL),
            (SELECT count(*) FROM manager_portfolio_nodes WHERE state='active'),
            (SELECT count(*) FROM global_manager_grants g, json_each(g.project_ids_json) j
               JOIN projects p ON p.id=j.value WHERE g.state='active'),
            (SELECT count(*) FROM manager_portfolio_coverage),
            (SELECT count(*) FROM manager_portfolio_nodes n JOIN global_manager_grants g
               ON g.node_id=n.id AND g.state='active' WHERE n.authority_epoch>g.grant_version)",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    if active_grants != active_nodes || unbound != 0 || expected_coverage != coverage || ahead != 0
    {
        return Err(DaemonError::Store(format!(
            "portfolio backfill counts disagree: grants={active_grants} unbound={unbound} nodes={active_nodes} coverage={coverage}/{expected_coverage} epoch_ahead={ahead}"
        )));
    }
    Ok(())
}

pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != PORTFOLIO_NODE_SCHEMA_VERSION || prior != version - 1 {
        return Err(DaemonError::Store(format!(
            "portfolio node identity requires V{}, found V{prior}",
            version - 1
        )));
    }
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    tx.execute_batch(CATALOG_TABLES)?;
    backfill_v0_grant(&tx, &now)?;
    tx.execute_batch(CATALOG_COVERAGE_GUARD)?;
    tx.execute_batch(CATALOG_GRANT_GUARDS)?;
    validate_backfill(&tx)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: portfolio-node-migration

/// V150's exact `global_manager_grants` DDL, for the V154-to-V153 rewind
/// fixture (the released V150 catalog is private to its module).
#[cfg(test)]
const V153_GRANTS_TABLE: &str = "CREATE TABLE global_manager_grants (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    grant_version INTEGER NOT NULL UNIQUE CHECK(grant_version>0),
    seat_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(seat_session_id)),
    state TEXT NOT NULL CHECK(state IN ('active','revoked')),
    project_ids_json TEXT NOT NULL CHECK(json_valid(project_ids_json) AND json_type(project_ids_json)='array' AND json_array_length(project_ids_json)>0),
    allowed_launches_json TEXT NOT NULL CHECK(json_valid(allowed_launches_json) AND json_type(allowed_launches_json)='array' AND json_array_length(allowed_launches_json)>0),
    project_policy_json TEXT NOT NULL CHECK(json_valid(project_policy_json) AND json_type(project_policy_json)='object'),
    operator_origin TEXT NOT NULL CHECK(length(operator_origin)>0),
    idempotency_key TEXT NOT NULL UNIQUE CHECK(length(idempotency_key) BETWEEN 1 AND 128),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at))
);";

/// V150's exact grant index and triggers, restored by the rewind fixture.
#[cfg(test)]
const V153_GRANT_GUARDS: &str = "CREATE UNIQUE INDEX global_manager_grants_one_active ON global_manager_grants(state) WHERE state='active';
CREATE TRIGGER global_manager_grants_no_delete BEFORE DELETE ON global_manager_grants BEGIN SELECT RAISE(ABORT,'global manager grants are retained'); END;
CREATE TRIGGER global_manager_grants_identity_immutable BEFORE UPDATE ON global_manager_grants
 WHEN NEW.id IS NOT OLD.id OR NEW.grant_version IS NOT OLD.grant_version
   OR NEW.seat_session_id IS NOT OLD.seat_session_id OR NEW.project_ids_json IS NOT OLD.project_ids_json
   OR NEW.allowed_launches_json IS NOT OLD.allowed_launches_json OR NEW.project_policy_json IS NOT OLD.project_policy_json
   OR NEW.operator_origin IS NOT OLD.operator_origin OR NEW.idempotency_key IS NOT OLD.idempotency_key
   OR NEW.created_at IS NOT OLD.created_at
 BEGIN SELECT RAISE(ABORT,'only state and updated_at of a global manager grant change'); END;
CREATE TRIGGER global_manager_grants_revoked_final BEFORE UPDATE OF state ON global_manager_grants
 WHEN OLD.state='revoked' AND NEW.state<>'revoked'
 BEGIN SELECT RAISE(ABORT,'a revoked global manager grant stays revoked'); END;";

/// Test fixture: undo M1 exactly (V154 to V153). The grants table is rebuilt
/// under V150's exact DDL text (SQLite cannot drop a foreign-key column) and
/// keeps its rows; the new tables are dropped. The old table is renamed in
/// legacy mode so the message and appointment foreign keys keep naming
/// `global_manager_grants`, which then resolves to the rebuilt table.
#[cfg(test)]
pub(crate) fn rewind_to_v153(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON;")?;
    // sql-dynamic-ok: test fixture built from static V150 literals.
    let result = connection.execute_batch(&format!( // sql-dynamic-ok
        "BEGIN IMMEDIATE;
         DROP TABLE manager_portfolio_coverage;
         ALTER TABLE global_manager_grants RENAME TO global_manager_grants_v154;
         {V153_GRANTS_TABLE}
         INSERT INTO global_manager_grants SELECT id,grant_version,seat_session_id,state,project_ids_json,allowed_launches_json,project_policy_json,operator_origin,idempotency_key,created_at,updated_at FROM global_manager_grants_v154;
         DROP TABLE global_manager_grants_v154;
         DROP TABLE manager_portfolio_nodes;
         {V153_GRANT_GUARDS}
         PRAGMA user_version=153;
         COMMIT;"
    ));
    if result.is_err() {
        let _ = connection.execute_batch("ROLLBACK;");
    }
    connection.execute_batch("PRAGMA legacy_alter_table=OFF; PRAGMA foreign_keys=ON;")?;
    result
}

fn refused(code: &str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

/// A coverage primary-key clash is a sibling overlap; anything else is an
/// error.
fn overlap_or(error: rusqlite::Error) -> DaemonError {
    match &error {
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY =>
        {
            refused(MANAGER_SCOPE_OVERLAP)
        }
        _ => error.into(),
    }
}

fn stamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|_| DaemonError::Store("invalid portfolio node identity".into()))
}

fn parse_time(text: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    super::parse_timestamp(text)
        .map_err(|_| DaemonError::Store("invalid portfolio node timestamp".into()))
}

/// One grant row with its portfolio columns.
#[derive(Debug, Clone)]
pub(crate) struct GrantRecord {
    pub grant: GlobalManagerGrantV1,
    pub node_id: Option<Uuid>,
    pub parent_node_id: Option<Uuid>,
    pub grantor: String,
    pub child_policy: Option<ManagerPolicyV2>,
    pub max_direct_reports: u16,
}

const RECORD_SELECT: &str = "SELECT id,grant_version,seat_session_id,state,project_ids_json,allowed_launches_json,project_policy_json,operator_origin,created_at,updated_at,node_id,parent_node_id,grantor,child_policy_json,max_direct_reports FROM global_manager_grants";

type RecordRow = (
    GrantRow,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    i64,
);

fn read_record_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecordRow> {
    Ok((
        read_grant_row(row)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
    ))
}

fn record_from(raw: RecordRow) -> Result<GrantRecord> {
    let (grant, node, parent, grantor, child, reports) = raw;
    Ok(GrantRecord {
        grant: grant_from_row(grant)?,
        node_id: node.as_deref().map(parse_uuid).transpose()?,
        parent_node_id: parent.as_deref().map(parse_uuid).transpose()?,
        grantor,
        child_policy: child.map(|json| serde_json::from_str(&json)).transpose()?,
        max_direct_reports: u16::try_from(reports)
            .map_err(|_| DaemonError::Store("invalid portfolio max_direct_reports".into()))?,
    })
}

/// `clause` is always a static literal from this module.
fn record_where(
    conn: &Connection,
    clause: &'static str,
    args: impl rusqlite::Params,
) -> Result<Option<GrantRecord>> {
    let sql = format!("{RECORD_SELECT} {clause}"); // sql-dynamic-ok: static clause
    conn.query_row(&sql, args, read_record_row)
        .optional()?
        .map(record_from)
        .transpose()
}

/// The active grant whose seat is exactly `seat` (any node).
pub(crate) fn seat_grant_on(conn: &Connection, seat: Uuid) -> Result<Option<GrantRecord>> {
    record_where(
        conn,
        "WHERE state='active' AND seat_session_id=?1",
        [seat.to_string()],
    )
}

/// The active grant of `node`.
pub(crate) fn node_grant_on(conn: &Connection, node: Uuid) -> Result<Option<GrantRecord>> {
    record_where(
        conn,
        "WHERE state='active' AND node_id=?1",
        [node.to_string()],
    )
}

/// The newest grant of `node` (active, else its last revoked one).
pub(crate) fn latest_node_grant_on(conn: &Connection, node: Uuid) -> Result<Option<GrantRecord>> {
    record_where(
        conn,
        "WHERE node_id=?1 ORDER BY grant_version DESC LIMIT 1",
        [node.to_string()],
    )
}

/// The grant stored under an idempotency key.
pub(crate) fn grant_by_key_on(conn: &Connection, key: &str) -> Result<Option<GrantRecord>> {
    record_where(conn, "WHERE idempotency_key=?1", [key])
}

/// The grant at exactly `version`.
pub(crate) fn grant_by_version_on(conn: &Connection, version: i64) -> Result<Option<GrantRecord>> {
    record_where(conn, "WHERE grant_version=?1", [version])
}

/// Every active grant whose parent is `node`, oldest first.
pub(crate) fn active_children_on(conn: &Connection, node: Uuid) -> Result<Vec<GrantRecord>> {
    let mut statement = conn.prepare(
        "SELECT id,grant_version,seat_session_id,state,project_ids_json,allowed_launches_json,project_policy_json,operator_origin,created_at,updated_at,node_id,parent_node_id,grantor,child_policy_json,max_direct_reports
         FROM global_manager_grants WHERE state='active' AND parent_node_id=?1 ORDER BY grant_version",
    )?;
    let rows = statement
        .query_map([node.to_string()], read_record_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(record_from).collect()
}

/// Bound on every parent-chain and subtree walk: deeper trees (or a cycle)
/// grant nothing.
pub(crate) const PORTFOLIO_DEPTH_LIMIT: usize = 64;

/// `node`'s depth: the length of its active parent chain (a root is 0).
pub(crate) fn node_depth_on(conn: &Connection, node: Uuid) -> Result<u16> {
    let mut depth = 0u16;
    let mut cursor = node_grant_on(conn, node)?.and_then(|record| record.parent_node_id);
    while let Some(parent) = cursor {
        depth += 1;
        if usize::from(depth) > PORTFOLIO_DEPTH_LIMIT {
            return Err(DaemonError::Store(
                "portfolio parent chain is too deep or cyclic".into(),
            ));
        }
        cursor = node_grant_on(conn, parent)?
            .ok_or_else(|| refused(MANAGER_ANCESTOR_REVOKED))?
            .parent_node_id;
    }
    Ok(depth)
}

/// The active grants of `node` and of every active descendant, parents
/// before children.
pub(crate) fn active_subtree_on(conn: &Connection, node: Uuid) -> Result<Vec<GrantRecord>> {
    let Some(top) = node_grant_on(conn, node)? else {
        return Ok(Vec::new());
    };
    let mut subtree = vec![top];
    let mut next = 0;
    while next < subtree.len() {
        if subtree.len() > PORTFOLIO_DEPTH_LIMIT * 64 {
            return Err(DaemonError::Store("portfolio subtree is too large".into()));
        }
        let Some(id) = subtree[next].node_id else {
            break;
        };
        subtree.extend(active_children_on(conn, id)?);
        next += 1;
    }
    Ok(subtree)
}

/// What one grantor-scoped revoke did (#1237).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortfolioRevokeOutcome {
    /// The revoked node and every node whose authority died with it.
    pub revoked: Vec<Uuid>,
    /// Surviving descendants moved up one level (direct children the revoked
    /// node did not grant now sit under its parent, or are roots).
    pub reparented: Vec<Uuid>,
    /// #1305 (plan §9 Q2): operator-granted children always re-parent, so a
    /// revoke never fails on its parent's caps. The surviving parent this
    /// left over its `max_direct_reports` or a sibling allowance aggregate
    /// is named here; it may appoint nothing new, and no edit may add to the
    /// overflow, until it is back under (`portfolio_narrowing`).
    pub over_capacity: Vec<Uuid>,
}

/// A node row.
#[derive(Debug, Clone)]
pub(crate) struct NodeRow {
    pub id: Uuid,
    pub tier_label: String,
    pub active: bool,
    pub authority_epoch: i64,
    pub created_at: String,
    pub updated_at: String,
}

pub(crate) fn node_row_on(conn: &Connection, node: Uuid) -> Result<Option<NodeRow>> {
    let row: Option<(String, String, String, i64, String, String)> = conn
        .query_row(
            "SELECT id,tier_label,state,authority_epoch,created_at,updated_at FROM manager_portfolio_nodes WHERE id=?1",
            [node.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(id, tier_label, state, authority_epoch, created_at, updated_at)| {
            Ok(NodeRow {
                id: parse_uuid(&id)?,
                tier_label,
                active: state == "active",
                authority_epoch,
                created_at,
                updated_at,
            })
        },
    )
    .transpose()
}

/// `(epoch, seat root, epoch created_at)` of an active node: the seat root is
/// the seat of the operator grant that opened the epoch.
pub(crate) fn portfolio_head_on(conn: &Connection, node: Uuid) -> Result<(i64, Uuid, String)> {
    let (epoch, seat, started): (i64, String, String) = conn.query_row(
        "SELECT n.authority_epoch,g.seat_session_id,g.created_at FROM manager_portfolio_nodes n
         JOIN global_manager_grants g ON g.grant_version=n.authority_epoch WHERE n.id=?1",
        [node.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok((epoch, parse_uuid(&seat)?, started))
}

/// Whether `caller` is a retired seat of an active node's current transfer
/// chain (a grant of that node at or after its epoch, before its active
/// grant). Pre-M1 rows of the backfilled chain carry no node id; they are
/// older than every post-M1 epoch, so only the backfilled node can match them.
pub(crate) fn retired_chain_seat_on(conn: &Connection, caller: Uuid) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM manager_portfolio_nodes n
           JOIN global_manager_grants cur ON cur.node_id=n.id AND cur.state='active'
           JOIN global_manager_grants old ON old.seat_session_id=?1
             AND (old.node_id=n.id OR old.node_id IS NULL)
             AND old.grant_version>=n.authority_epoch AND old.grant_version<cur.grant_version
         WHERE n.state='active')",
        [caller.to_string()],
        |row| row.get(0),
    )?)
}

/// The deepest active node covering `project`.
pub(crate) fn covering_node_on(conn: &Connection, project: Uuid) -> Result<Option<Uuid>> {
    conn.query_row(
        "SELECT node_id FROM manager_portfolio_coverage WHERE project_id=?1 ORDER BY depth DESC LIMIT 1",
        [project.to_string()],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .as_deref()
    .map(parse_uuid)
    .transpose()
}

/// The active node whose transfer chain (since its epoch) seats `session`:
/// its active seat or a retired one. Pre-M1 rows carry no node id and are
/// older than every post-M1 epoch, so only the backfilled node can match them.
pub(crate) fn chain_seat_node_on(conn: &Connection, session: Uuid) -> Result<Option<Uuid>> {
    conn.query_row(
        "SELECT n.id FROM manager_portfolio_nodes n
           JOIN global_manager_grants g ON g.seat_session_id=?1
             AND (g.node_id=n.id OR g.node_id IS NULL) AND g.grant_version>=n.authority_epoch
         WHERE n.state='active' ORDER BY g.grant_version DESC LIMIT 1",
        [session.to_string()],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .as_deref()
    .map(parse_uuid)
    .transpose()
}

/// Bound on the rotation-lineage walk of [`Store::portfolio_seat_node`].
const SEAT_LINEAGE_LIMIT: usize = 32;

/// The `*GlobalManager` shims' node: the single active root labelled
/// `global`. Refused `global_manager_ambiguous` when more than one exists.
pub(crate) fn global_shim_node_on(conn: &Connection) -> Result<Option<Uuid>> {
    let mut statement = conn.prepare(
        "SELECT n.id FROM manager_portfolio_nodes n
         JOIN global_manager_grants g ON g.node_id=n.id AND g.state='active'
         WHERE n.state='active' AND n.tier_label=?1 AND g.parent_node_id IS NULL
         ORDER BY n.created_at,n.id LIMIT 2",
    )?;
    let ids = statement
        .query_map([GLOBAL_TIER_LABEL], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    match ids.as_slice() {
        [] => Ok(None),
        [one] => parse_uuid(one).map(Some),
        _ => Err(refused(GLOBAL_MANAGER_AMBIGUOUS)),
    }
}

/// What a configure's checks resolved, for its writes.
struct PortfolioConfigureChecked {
    node_id: Uuid,
    previous: Option<GrantRecord>,
    adopted: Vec<GrantRecord>,
}

/// Who writes a portfolio grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortfolioGrantor {
    Operator,
    /// A node acting on its own subtree (S5); never creates a root.
    Node(Uuid),
}

impl PortfolioGrantor {
    pub(crate) fn as_column(self) -> String {
        match self {
            Self::Operator => "operator".into(),
            Self::Node(node) => format!("node:{node}"),
        }
    }
}

impl Store {
    fn portfolio_view(&self, node: &NodeRow, record: GrantRecord) -> Result<PortfolioNodeV1> {
        let seat_root: String = self
            .conn
            .query_row(
                "SELECT seat_session_id FROM global_manager_grants WHERE grant_version=?1",
                [node.authority_epoch],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_else(|| record.grant.seat_session_id.to_string());
        Ok(PortfolioNodeV1 {
            node_id: node.id,
            tier_label: node.tier_label.clone(),
            state: if node.active { "active" } else { "revoked" }.into(),
            authority_epoch: node.authority_epoch,
            parent_node_id: record.parent_node_id,
            grantor: record.grantor,
            seat_root_session_id: parse_uuid(&seat_root)?,
            max_direct_reports: record.max_direct_reports,
            child_policy: record.child_policy,
            grant: record.grant,
            created_at: parse_time(&node.created_at)?,
            updated_at: parse_time(&node.updated_at)?,
        })
    }

    /// Operator-only `GetPortfolioNode`.
    pub fn get_portfolio_node(&self, node: Uuid) -> Result<Option<PortfolioNodeV1>> {
        let Some(row) = node_row_on(&self.conn, node)? else {
            return Ok(None);
        };
        let Some(record) = latest_node_grant_on(&self.conn, node)? else {
            return Ok(None);
        };
        self.portfolio_view(&row, record).map(Some)
    }

    /// Operator-only `ListPortfolioNodes`, ordered by creation.
    pub fn list_portfolio_nodes(&self, include_revoked: bool) -> Result<Vec<PortfolioNodeV1>> {
        let ids: Vec<String> = {
            let mut statement = self.conn.prepare(
                "SELECT id FROM manager_portfolio_nodes WHERE ?1 OR state='active'
                 ORDER BY created_at,id LIMIT 256",
            )?;
            statement
                .query_map([include_revoked], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut nodes = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(node) = self.get_portfolio_node(parse_uuid(&id)?)? {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    /// The node whose active seat is exactly `caller`.
    pub fn portfolio_node_for_seat(&self, caller: Uuid) -> Result<Option<PortfolioNodeV1>> {
        let Some(record) = seat_grant_on(&self.conn, caller)? else {
            return Ok(None);
        };
        let Some(node) = record.node_id else {
            return Ok(None);
        };
        let Some(row) = node_row_on(&self.conn, node)? else {
            return Ok(None);
        };
        self.portfolio_view(&row, record).map(Some)
    }

    /// #1236: the portfolio node whose seat `session` is, through rotation
    /// lineage: the session itself or any `continued_from` predecessor is a
    /// seat of that node's current transfer chain. `None` for every other
    /// session. Mutation, read, watch and mail reach use it so no node ever
    /// reaches another node's seat (plan §2.4: no sibling reach).
    pub(crate) fn portfolio_seat_node(&self, session: Uuid) -> Result<Option<Uuid>> {
        let mut cursor = Some(session);
        for _ in 0..SEAT_LINEAGE_LIMIT {
            let Some(member) = cursor else { break };
            if let Some(node) = chain_seat_node_on(&self.conn, member)? {
                return Ok(Some(node));
            }
            cursor = self.get_session(member)?.and_then(|row| row.continued_from);
        }
        Ok(None)
    }

    /// The active grant whose seat is exactly `caller`, if any.
    pub fn portfolio_seat_grant(&self, caller: Uuid) -> Result<Option<GlobalManagerGrantV1>> {
        Ok(seat_grant_on(&self.conn, caller)?.map(|record| record.grant))
    }

    /// The nodes covering `project`, root first (coverage ordered by depth).
    pub fn portfolio_chain_for_project(&self, project: Uuid) -> Result<Vec<PortfolioCoverageV1>> {
        let mut statement = self.conn.prepare(
            "SELECT depth,node_id,grant_version FROM manager_portfolio_coverage
             WHERE project_id=?1 ORDER BY depth",
        )?;
        let rows = statement
            .query_map([project.to_string()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(depth, node, grant_version)| {
                Ok(PortfolioCoverageV1 {
                    project_id: project,
                    depth: u16::try_from(depth)
                        .map_err(|_| DaemonError::Store("invalid portfolio depth".into()))?,
                    node_id: parse_uuid(&node)?,
                    grant_version,
                })
            })
            .collect()
    }

    /// Effective configured resource ceilings for a live project manager.
    pub fn project_effective_resource_caps(
        &self,
        project: Uuid,
    ) -> Result<Option<rsi_common::portfolio_nodes::ManagerResourceCapsV1>> {
        self.project_resource_caps_without_node(project, None)
    }

    fn project_resource_caps_without_node(
        &self,
        project: Uuid,
        omitted_node: Option<Uuid>,
    ) -> Result<Option<rsi_common::portfolio_nodes::ManagerResourceCapsV1>> {
        use rsi_common::portfolio_nodes::ManagerResourceCapsV1;
        if self
            .get_harness_manager(project)?
            .is_none_or(|config| config.is_revoked())
        {
            return Ok(None);
        }
        let Some(policy) = self
            .get_harness_manager_policy(project)?
            .filter(|policy| !policy.revoked)
        else {
            return Ok(None);
        };
        let mut caps = ManagerResourceCapsV1::from_policy(&policy.policy);
        for coverage in self.portfolio_chain_for_project(project)? {
            if Some(coverage.node_id) != omitted_node
                && let Some(node) = self
                    .get_portfolio_node(coverage.node_id)?
                    .filter(|node| node.state == "active")
            {
                caps.intersect(&node.grant.project_policy);
            }
        }
        Ok(Some(caps))
    }

    /// Called inside the configure transaction, after replay and topology checks.
    /// Compare the existing chain with the proposed grant, omitting the old
    /// version on edits. Adoption retains the other nodes' resource policies.
    fn confirm_project_cap_reductions(
        &self,
        request: &ConfigurePortfolioNodeRequestV1,
        confirmed: bool,
    ) -> Result<()> {
        if confirmed {
            return Ok(());
        }
        let mut impacts = Vec::new();
        for project in &request.project_ids {
            let Some(before) = self.project_effective_resource_caps(*project)? else {
                continue;
            };
            let Some(mut after) =
                self.project_resource_caps_without_node(*project, request.node_id)?
            else {
                continue;
            };
            after.intersect(&request.policy);
            let changes = after.reductions_from(&before);
            if !changes.is_empty() {
                let name = self
                    .get_project(*project)?
                    .map_or_else(|| project.to_string(), |project| project.name);
                impacts.push(format!("{name} ({project}): {}", changes.join(", ")));
            }
        }
        if impacts.is_empty() {
            return Ok(());
        }
        Err(DaemonError::InvalidParam(format!(
            "{}: {}. Confirm with confirm_cap_reductions:true after reviewing these changes.",
            rsi_common::portfolio_nodes::PORTFOLIO_CAP_REDUCTION_CONFIRMATION_REQUIRED,
            impacts.join("; "),
        )))
    }

    /// The active grant of the node covering `project` (deepest first).
    pub(crate) fn covering_portfolio_grant(&self, project: Uuid) -> Result<Option<GrantRecord>> {
        let Some(node) = covering_node_on(&self.conn, project)? else {
            return Ok(None);
        };
        node_grant_on(&self.conn, node)
    }

    /// Rebuild the coverage of `nodes` from their active grants: every row
    /// of a listed node goes (its grant was replaced or revoked, so the
    /// retention trigger lets it go), then each active listed node covers
    /// its grant's projects at its current depth. All deletes run before any
    /// insert, so a subtree may shift depth inside one transaction.
    fn rebuild_coverage_of(&self, nodes: &[Uuid]) -> Result<()> {
        for node in nodes {
            self.conn.execute(
                "DELETE FROM manager_portfolio_coverage WHERE node_id=?1
                 AND grant_version NOT IN (SELECT grant_version FROM global_manager_grants WHERE node_id=?1 AND state='active')",
                [node.to_string()],
            )?;
        }
        for node in nodes {
            let Some(record) = node_grant_on(&self.conn, *node)? else {
                continue;
            };
            let current: i64 = self.conn.query_row(
                "SELECT count(*) FROM manager_portfolio_coverage WHERE node_id=?1",
                [node.to_string()],
                |row| row.get(0),
            )?;
            if current > 0 {
                continue;
            }
            let depth = node_depth_on(&self.conn, *node)?;
            for project in &record.grant.project_ids {
                self.conn.execute(
                    "INSERT INTO manager_portfolio_coverage(project_id,depth,node_id,grant_version)
                     VALUES(?1,?2,?3,?4)",
                    params![
                        project.to_string(),
                        i64::from(depth),
                        node.to_string(),
                        record.grant.grant_version
                    ],
                )
                .map_err(|error| overlap_or(error))?;
            }
        }
        Ok(())
    }

    /// #1237: a structural move (adoption, or re-parenting when an ancestor
    /// is revoked). The node gets a new grant version that copies every
    /// column but its parent; its node id, grantor, seat and authority epoch
    /// stay, so its V2 ledger principal `(project, seat root, epoch)`, its
    /// workers and its seat survive the move. Queued mail of the replaced
    /// grant is retired, as on a seat transfer. Coverage is rebuilt by the
    /// caller.
    fn move_portfolio_grant(
        &self,
        record: &GrantRecord,
        parent: Option<Uuid>,
        now: &str,
    ) -> Result<()> {
        let node = record
            .node_id
            .ok_or_else(|| DaemonError::Store("an active portfolio grant has no node".into()))?;
        self.conn.execute(
            "UPDATE global_manager_grants SET state='revoked',updated_at=?2 WHERE id=?1 AND state='active'",
            params![record.grant.grant_id.to_string(), now],
        )?;
        self.retire_global_grant_messages(record.grant.grant_id, now)?;
        let next = self.portfolio_next_version()?;
        let id = Uuid::new_v4();
        self.conn.execute(
            "INSERT INTO global_manager_grants(id,grant_version,seat_session_id,state,project_ids_json,allowed_launches_json,project_policy_json,operator_origin,idempotency_key,created_at,updated_at,node_id,parent_node_id,grantor,child_policy_json,max_direct_reports)
             SELECT ?1,?2,seat_session_id,'active',project_ids_json,allowed_launches_json,project_policy_json,operator_origin,?3,?4,?4,node_id,?5,grantor,child_policy_json,max_direct_reports
             FROM global_manager_grants WHERE id=?6",
            params![
                id.to_string(),
                next,
                format!("move:{id}"),
                now,
                parent.map(|parent| parent.to_string()),
                record.grant.grant_id.to_string(),
            ],
        )?;
        self.conn.execute(
            "UPDATE manager_portfolio_nodes SET updated_at=?2 WHERE id=?1",
            params![node.to_string(), now],
        )?;
        Ok(())
    }

    fn portfolio_next_version(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(grant_version),0)+1 FROM global_manager_grants",
            [],
            |row| row.get(0),
        )?)
    }

    fn portfolio_view_of(&self, node: Uuid) -> Result<PortfolioNodeV1> {
        self.get_portfolio_node(node)?
            .ok_or_else(|| DaemonError::Store("portfolio node vanished".into()))
    }

    /// The nodes the configure that wrote `existing` adopted (#1303): the
    /// roots its transaction moved under it. Each move grant carries the
    /// transaction's timestamp, a `move:` key, a later version and this node
    /// as its parent; the grant rows are retained, so the set never drifts.
    fn portfolio_adopted_by(&self, existing: &GrantRecord) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT m.node_id FROM global_manager_grants m JOIN global_manager_grants g ON g.id=?1
             WHERE m.parent_node_id=g.node_id AND m.created_at=g.created_at
               AND m.grant_version>g.grant_version AND substr(m.idempotency_key,1,5)='move:'",
        )?;
        let mut adopted = statement
            .query_map([existing.grant.grant_id.to_string()], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .map(|id| parse_uuid(id))
            .collect::<Result<Vec<_>>>()?;
        adopted.sort_unstable();
        Ok(adopted)
    }

    /// Whether a replayed configure request names the stored grant exactly,
    /// adoption set included (#1303): a same-key request that adopts other
    /// nodes is a conflict, never a silent replay that adopts nothing.
    fn portfolio_replay_matches(
        &self,
        existing: &GrantRecord,
        request: &ConfigurePortfolioNodeRequestV1,
        grantor: PortfolioGrantor,
    ) -> Result<bool> {
        let Some(node) = existing.node_id else {
            return Ok(false);
        };
        let label = node_row_on(&self.conn, node)?.map(|row| row.tier_label);
        let mut adopt = request.adopt_node_ids.clone();
        adopt.sort_unstable();
        Ok(request.node_id.is_none_or(|id| id == node)
            && self.portfolio_adopted_by(existing)? == adopt
            && label.as_deref() == Some(request.tier_label.as_str())
            && existing.parent_node_id == request.parent_node_id
            && existing.grantor == grantor.as_column()
            && existing.grant.seat_session_id == request.seat_session_id
            && existing.grant.project_ids == request.project_ids
            && existing.grant.allowed_launches == request.allowed_launches
            && existing.grant.project_policy == request.policy
            && existing.child_policy == request.child_policy
            && existing.max_direct_reports == request.max_direct_reports)
    }

    /// Create a node or write a new grant version of one (plan §3). One
    /// IMMEDIATE transaction revokes the previous grant, retires its queued
    /// mail, inserts the new grant, bumps the epoch, moves every adopted node
    /// under this one and rebuilds the coverage. A replay under the same
    /// idempotency key returns the node.
    ///
    /// # Errors
    /// `portfolio_invalid_request`, `manager_node_root_operator_only`,
    /// `portfolio_idempotency_conflict`, `portfolio_node_not_found`,
    /// `manager_node_stale`, `manager_ancestor_revoked`,
    /// `portfolio_parent_immutable`, `portfolio_tier_label_immutable`,
    /// `global_manager_seat_unavailable`, `global_manager_unknown_project`,
    /// `portfolio_adopt_not_root`, `portfolio_coverage_not_superset`,
    /// `manager_scope_widened`, `manager_capability_widened`,
    /// `manager_allowance_exceeded`, `manager_scope_overlap`.
    pub fn configure_portfolio_node(
        &self,
        request: &ConfigurePortfolioNodeRequestV1,
        grantor: PortfolioGrantor,
        operator_origin: &str,
    ) -> Result<PortfolioNodeV1> {
        self.configure_portfolio_node_confirmed(request, grantor, operator_origin, false)
    }

    /// Operator acknowledgment of the reduction preview; no grant is written
    /// until the same transaction has validated topology and project impacts.
    pub fn configure_portfolio_node_confirmed(
        &self,
        request: &ConfigurePortfolioNodeRequestV1,
        grantor: PortfolioGrantor,
        operator_origin: &str,
        confirm_cap_reductions: bool,
    ) -> Result<PortfolioNodeV1> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let view = self.configure_portfolio_node_as_in_tx_confirmed(
            request,
            grantor,
            operator_origin,
            None,
            confirm_cap_reductions,
        )?;
        tx.commit()?;
        Ok(view)
    }

    /// #1239: configure inside the caller's transaction, creating the node
    /// under `new_node_id` when given (a delegated appointment reserves the
    /// child's id before it launches the seat).
    pub(crate) fn configure_portfolio_node_as_in_tx(
        &self,
        request: &ConfigurePortfolioNodeRequestV1,
        grantor: PortfolioGrantor,
        operator_origin: &str,
        new_node_id: Option<Uuid>,
    ) -> Result<PortfolioNodeV1> {
        self.configure_portfolio_node_as_in_tx_confirmed(
            request,
            grantor,
            operator_origin,
            new_node_id,
            false,
        )
    }

    pub(crate) fn configure_portfolio_node_as_in_tx_confirmed(
        &self,
        request: &ConfigurePortfolioNodeRequestV1,
        grantor: PortfolioGrantor,
        operator_origin: &str,
        new_node_id: Option<Uuid>,
        confirm_cap_reductions: bool,
    ) -> Result<PortfolioNodeV1> {
        Self::portfolio_grantor_may_configure(request, grantor)?;
        if let Some(existing) = grant_by_key_on(&self.conn, &request.idempotency_key)? {
            if !self.portfolio_replay_matches(&existing, request, grantor)? {
                return Err(refused(PORTFOLIO_IDEMPOTENCY_CONFLICT));
            }
            let node = existing
                .node_id
                .ok_or_else(|| refused(PORTFOLIO_IDEMPOTENCY_CONFLICT))?;
            return self.portfolio_view_of(node);
        }
        let checked = self.portfolio_configure_checks(request, true, new_node_id)?;
        if grantor == PortfolioGrantor::Operator {
            self.confirm_project_cap_reductions(request, confirm_cap_reductions)?;
        }
        self.portfolio_configure_write(request, grantor, operator_origin, checked)
    }

    /// Validation and the I1 rule: only the operator creates a root, adopts
    /// or re-parents.
    fn portfolio_grantor_may_configure(
        request: &ConfigurePortfolioNodeRequestV1,
        grantor: PortfolioGrantor,
    ) -> Result<()> {
        request.validate().map_err(refused)?;
        if grantor != PortfolioGrantor::Operator
            && (request.parent_node_id.is_none() || !request.adopt_node_ids.is_empty())
        {
            return Err(refused(MANAGER_NODE_ROOT_OPERATOR_ONLY));
        }
        Ok(())
    }

    /// #1239: every check of a configure except the seat's (the seat of a
    /// delegated appointment does not exist before its launch), so an agent
    /// appointment is refused before any session is created. Writes nothing.
    pub(crate) fn portfolio_configure_preflight(
        &self,
        request: &ConfigurePortfolioNodeRequestV1,
        grantor: PortfolioGrantor,
    ) -> Result<()> {
        Self::portfolio_grantor_may_configure(request, grantor)?;
        self.portfolio_configure_checks(request, false, None)
            .map(drop)
    }

    /// The checks of a configure, before the first write.
    fn portfolio_configure_checks(
        &self,
        request: &ConfigurePortfolioNodeRequestV1,
        check_seat: bool,
        new_node_id: Option<Uuid>,
    ) -> Result<PortfolioConfigureChecked> {
        let (node_id, previous) = match request.node_id {
            None => (new_node_id.unwrap_or_else(Uuid::new_v4), None),
            Some(node_id) => {
                let row = node_row_on(&self.conn, node_id)?
                    .ok_or_else(|| refused(PORTFOLIO_NODE_NOT_FOUND))?;
                let active = node_grant_on(&self.conn, node_id)?
                    .filter(|_| row.active)
                    .ok_or_else(|| refused(MANAGER_NODE_STALE))?;
                if active.grant.grant_version != request.expected_node_grant_version
                    || row.authority_epoch != request.expected_authority_epoch
                {
                    return Err(refused(MANAGER_NODE_STALE));
                }
                if row.tier_label != request.tier_label {
                    return Err(refused(PORTFOLIO_TIER_LABEL_IMMUTABLE));
                }
                if active.parent_node_id != request.parent_node_id {
                    return Err(refused(PORTFOLIO_PARENT_IMMUTABLE));
                }
                (node_id, Some(active))
            }
        };
        let parent = match request.parent_node_id {
            None => None,
            Some(parent_id) => {
                let row = node_row_on(&self.conn, parent_id)?
                    .ok_or_else(|| refused(PORTFOLIO_NODE_NOT_FOUND))?;
                let grant = node_grant_on(&self.conn, parent_id)?
                    .filter(|_| row.active)
                    .ok_or_else(|| refused(MANAGER_ANCESTOR_REVOKED))?;
                // Every ancestor must still be live.
                node_depth_on(&self.conn, parent_id)?;
                if request
                    .expected_parent_grant_version
                    .is_some_and(|version| version != grant.grant.grant_version)
                {
                    return Err(refused(MANAGER_NODE_STALE));
                }
                Some(grant)
            }
        };
        if check_seat {
            self.portfolio_seat_available(request.seat_session_id, node_id)?;
        }
        for project in &request.project_ids {
            if self.get_project(*project)?.is_none() {
                return Err(refused("global_manager_unknown_project"));
            }
        }
        let adopted = self.portfolio_adoption(request)?;
        self.portfolio_narrowing(
            request,
            node_id,
            parent.as_ref(),
            previous.as_ref(),
            &adopted,
        )?;
        // Disjointness at this node's depth: only this node or an adopted
        // node (which moves one level down) may hold a requested project.
        let depth = match request.parent_node_id {
            Some(parent_id) => node_depth_on(&self.conn, parent_id)? + 1,
            None => 0,
        };
        for project in &request.project_ids {
            let holder: Option<String> = self
                .conn
                .query_row(
                    "SELECT node_id FROM manager_portfolio_coverage WHERE project_id=?1 AND depth=?2",
                    params![project.to_string(), i64::from(depth)],
                    |row| row.get(0),
                )
                .optional()?;
            let holder = holder.as_deref().map(parse_uuid).transpose()?;
            if holder.is_some_and(|holder| {
                holder != node_id && !adopted.iter().any(|record| record.node_id == Some(holder))
            }) {
                return Err(refused(MANAGER_SCOPE_OVERLAP));
            }
        }
        Ok(PortfolioConfigureChecked {
            node_id,
            previous,
            adopted,
        })
    }

    /// A seat is an existing, unarchived leaf session that seats no other
    /// active node.
    pub(crate) fn portfolio_seat_available(&self, seat: Uuid, node_id: Uuid) -> Result<()> {
        let session = self
            .get_session(seat)?
            .ok_or_else(|| refused(GLOBAL_MANAGER_SEAT_UNAVAILABLE))?;
        if !rsi_common::is_leaf_kind(session.session_kind)
            || matches!(
                session.status,
                SessionStatus::Archived | SessionStatus::Deleted
            )
        {
            return Err(refused(GLOBAL_MANAGER_SEAT_UNAVAILABLE));
        }
        if seat_grant_on(&self.conn, seat)?.is_some_and(|held| held.node_id != Some(node_id)) {
            return Err(refused(GLOBAL_MANAGER_SEAT_UNAVAILABLE));
        }
        Ok(())
    }

    /// The writes of a checked configure.
    fn portfolio_configure_write(
        &self,
        request: &ConfigurePortfolioNodeRequestV1,
        grantor: PortfolioGrantor,
        operator_origin: &str,
        checked: PortfolioConfigureChecked,
    ) -> Result<PortfolioNodeV1> {
        let PortfolioConfigureChecked {
            node_id,
            previous,
            adopted,
        } = checked;
        let now = stamp();
        let next = self.portfolio_next_version()?;
        match &previous {
            Some(previous) => {
                self.conn.execute(
                    "UPDATE global_manager_grants SET state='revoked',updated_at=?2 WHERE id=?1 AND state='active'",
                    params![previous.grant.grant_id.to_string(), now],
                )?;
                self.retire_global_grant_messages(previous.grant.grant_id, &now)?;
                self.conn.execute(
                    "UPDATE manager_portfolio_nodes SET authority_epoch=?2,updated_at=?3 WHERE id=?1",
                    params![node_id.to_string(), next, now],
                )?;
            }
            None => {
                self.conn.execute(
                    "INSERT INTO manager_portfolio_nodes(id,tier_label,state,authority_epoch,created_at,updated_at)
                     VALUES(?1,?2,'active',?3,?4,?4)",
                    params![node_id.to_string(), request.tier_label, next, now],
                )?;
            }
        }
        self.conn.execute(
            "INSERT INTO global_manager_grants(id,grant_version,seat_session_id,state,project_ids_json,allowed_launches_json,project_policy_json,operator_origin,idempotency_key,created_at,updated_at,node_id,parent_node_id,grantor,child_policy_json,max_direct_reports)
             VALUES(?1,?2,?3,'active',?4,?5,?6,?7,?8,?9,?9,?10,?11,?12,?13,?14)",
            params![
                Uuid::new_v4().to_string(),
                next,
                request.seat_session_id.to_string(),
                serde_json::to_string(&request.project_ids)?,
                serde_json::to_string(&request.allowed_launches)?,
                serde_json::to_string(&request.policy)?,
                operator_origin,
                request.idempotency_key,
                now,
                node_id.to_string(),
                request.parent_node_id.map(|parent| parent.to_string()),
                grantor.as_column(),
                request
                    .child_policy
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                i64::from(request.max_direct_reports),
            ],
        )?;
        // Adopted subtrees move one level down: the adopted roots under this
        // node, their descendants under their unchanged parents (a new grant
        // version each, so their coverage can shift depth).
        let mut moved = vec![node_id];
        for record in &adopted {
            let Some(adopted_id) = record.node_id else {
                continue;
            };
            let subtree = active_subtree_on(&self.conn, adopted_id)?;
            for member in &subtree {
                let parent = if member.node_id == Some(adopted_id) {
                    Some(node_id)
                } else {
                    member.parent_node_id
                };
                self.move_portfolio_grant(member, parent, &now)?;
                moved.extend(member.node_id);
            }
        }
        self.rebuild_coverage_of(&moved)?;
        self.portfolio_view_of(node_id)
    }

    /// The active grants of the nodes a request adopts: each must be a
    /// current child of the request's parent (a root, for a root request),
    /// and the request must cover all of its projects.
    fn portfolio_adoption(
        &self,
        request: &ConfigurePortfolioNodeRequestV1,
    ) -> Result<Vec<GrantRecord>> {
        let mut adopted = Vec::with_capacity(request.adopt_node_ids.len());
        for id in &request.adopt_node_ids {
            let row =
                node_row_on(&self.conn, *id)?.ok_or_else(|| refused(PORTFOLIO_NODE_NOT_FOUND))?;
            let record = node_grant_on(&self.conn, *id)?
                .filter(|_| row.active)
                .ok_or_else(|| refused(PORTFOLIO_ADOPT_NOT_ROOT))?;
            if record.parent_node_id != request.parent_node_id {
                return Err(refused(PORTFOLIO_ADOPT_NOT_ROOT));
            }
            if record
                .grant
                .project_ids
                .iter()
                .any(|project| !request.project_ids.contains(project))
            {
                return Err(refused(PORTFOLIO_COVERAGE_NOT_SUPERSET));
            }
            adopted.push(record);
        }
        Ok(adopted)
    }

    /// I2 on every edge the request touches: the node narrows its parent,
    /// every child it will have (current ones on an edit, plus the adopted)
    /// narrows it, and neither the node nor its parent exceeds its
    /// direct-report limit. A parent narrowing that would leave a child
    /// over-granted is refused here, before any write.
    ///
    /// #1302, plan §2.2: the active children of each parent together leave
    /// it at least one unit of every finite allowance (spend may reach the
    /// cap), on both edges: the request among its siblings under its parent,
    /// and the request's children (current plus adopted) under the request.
    /// A parent a revoke left over its caps (#1305) may still be edited, but
    /// no write may add to an overflow (`allowance_worsened`).
    fn portfolio_narrowing(
        &self,
        request: &ConfigurePortfolioNodeRequestV1,
        node_id: Uuid,
        parent: Option<&GrantRecord>,
        previous: Option<&GrantRecord>,
        adopted: &[GrantRecord],
    ) -> Result<()> {
        // #1239: the policy the node hands to the PMs it appoints narrows
        // the node's own in every dimension, launches included.
        if let Some(child) = &request.child_policy {
            handed_policy_narrows(child, &request.allowed_launches, &request.policy)
                .map_err(refused)?;
        }
        let bounds = portfolio_bounds(
            &request.project_ids,
            &request.allowed_launches,
            &request.policy,
            request.max_direct_reports,
        );
        let bounds_of =
            |record: &GrantRecord| portfolio_grant_bounds(&record.grant, record.max_direct_reports);
        if let Some(parent) = parent {
            let parent_bounds = bounds_of(parent);
            grant_narrows(&bounds, &parent_bounds).map_err(refused)?;
            let siblings = active_children_on(&self.conn, parent.node_id.unwrap_or(node_id))?;
            let staying: Vec<GrantRecord> = siblings
                .iter()
                .filter(|sibling| {
                    sibling.node_id != Some(node_id)
                        && !adopted
                            .iter()
                            .any(|record| record.node_id == sibling.node_id)
                })
                .cloned()
                .collect();
            // Over its cap (a revoke re-parented operator grants under it),
            // the parent appoints nothing new until it is back under.
            if request.node_id.is_none()
                && staying.len() + 1 > usize::from(parent.max_direct_reports)
            {
                return Err(refused(MANAGER_ALLOWANCE_EXCEEDED));
            }
            let before: Vec<GrantBoundsV1> = siblings.iter().map(bounds_of).collect();
            let mut after: Vec<GrantBoundsV1> = staying.iter().map(bounds_of).collect();
            after.push(bounds.clone());
            if allowance_worsened(
                Some(&allowance_overflow(
                    &before.iter().collect::<Vec<_>>(),
                    &parent_bounds,
                )),
                &allowance_overflow(&after.iter().collect::<Vec<_>>(), &parent_bounds),
            ) {
                return Err(refused(MANAGER_ALLOWANCE_EXCEEDED));
            }
        }
        let current = match request.node_id {
            Some(node) => active_children_on(&self.conn, node)?,
            None => Vec::new(),
        };
        let mut children = current.clone();
        children.extend(adopted.iter().cloned());
        let reports_over = |count: usize, cap: u16| count as i64 - i64::from(cap);
        let over = reports_over(children.len(), request.max_direct_reports);
        if over > 0
            && previous.is_none_or(|previous| {
                over > reports_over(current.len(), previous.max_direct_reports)
            })
        {
            return Err(refused(MANAGER_ALLOWANCE_EXCEEDED));
        }
        let child_bounds: Vec<GrantBoundsV1> = children.iter().map(bounds_of).collect();
        for child in &child_bounds {
            grant_narrows(child, &bounds).map_err(refused)?;
        }
        let before = previous.map(|previous| {
            let current: Vec<GrantBoundsV1> = current.iter().map(bounds_of).collect();
            allowance_overflow(&current.iter().collect::<Vec<_>>(), &bounds_of(previous))
        });
        if allowance_worsened(
            before.as_deref(),
            &allowance_overflow(&child_bounds.iter().collect::<Vec<_>>(), &bounds),
        ) {
            return Err(refused(MANAGER_ALLOWANCE_EXCEEDED));
        }
        Ok(())
    }

    /// The nodes a revoke left over a direct-report cap or a sibling
    /// allowance aggregate (#1305): the surviving parent of re-parented
    /// operator grants, when it is now over.
    fn portfolio_over_capacity(&self, parent: Option<Uuid>) -> Result<Vec<Uuid>> {
        let Some(parent) = parent else {
            return Ok(Vec::new());
        };
        let Some(record) = node_grant_on(&self.conn, parent)? else {
            return Ok(Vec::new());
        };
        let children = active_children_on(&self.conn, parent)?;
        let bounds: Vec<GrantBoundsV1> = children
            .iter()
            .map(|child| portfolio_grant_bounds(&child.grant, child.max_direct_reports))
            .collect();
        let over = children.len() > usize::from(record.max_direct_reports)
            || allowance_overflow(
                &bounds.iter().collect::<Vec<_>>(),
                &portfolio_grant_bounds(&record.grant, record.max_direct_reports),
            )
            .iter()
            .any(|(_, overflow)| *overflow > 0);
        Ok(if over { vec![parent] } else { Vec::new() })
    }

    /// Revoke one node, grantor-scoped (#1237, plan §3, operator decision
    /// 2026-10-05): authority dies with its grantor, never wholesale.
    /// - The node's grant, coverage and queued mail go; it is revoked.
    /// - A descendant whose grantor is a revoked node (`node:<id>`) is
    ///   revoked with its whole subtree.
    /// - Every other descendant survives, one level up: the revoked node's
    ///   direct children move under its parent (or become roots) and keep
    ///   their grantor, seat and authority epoch, so their ledgers and
    ///   workers stay intact.
    ///
    /// Queued actions of a revoked node are refused at effect time (every
    /// effect re-resolves the principal). A replay against the already
    /// revoked node at the same versions returns it.
    ///
    /// # Errors
    /// `portfolio_invalid_request`, `portfolio_node_not_found`,
    /// `manager_node_stale`.
    pub fn revoke_portfolio_node(
        &self,
        request: &RevokePortfolioNodeRequestV1,
    ) -> Result<PortfolioNodeV1> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (view, _) = self.revoke_portfolio_node_in_tx(request)?;
        tx.commit()?;
        Ok(view)
    }

    /// [`Self::revoke_portfolio_node`] with what it did.
    pub fn revoke_portfolio_node_outcome(
        &self,
        request: &RevokePortfolioNodeRequestV1,
    ) -> Result<(PortfolioNodeV1, PortfolioRevokeOutcome)> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let result = self.revoke_portfolio_node_in_tx(request)?;
        tx.commit()?;
        Ok(result)
    }

    /// [`Self::revoke_portfolio_node`] inside the caller's IMMEDIATE
    /// transaction (the `RevokeGlobalManager` shim).
    pub(crate) fn revoke_portfolio_node_in_tx(
        &self,
        request: &RevokePortfolioNodeRequestV1,
    ) -> Result<(PortfolioNodeV1, PortfolioRevokeOutcome)> {
        request.validate().map_err(refused)?;
        let row = node_row_on(&self.conn, request.node_id)?
            .ok_or_else(|| refused(PORTFOLIO_NODE_NOT_FOUND))?;
        let latest = latest_node_grant_on(&self.conn, request.node_id)?
            .ok_or_else(|| refused(PORTFOLIO_NODE_NOT_FOUND))?;
        if latest.grant.grant_version != request.expected_grant_version
            || row.authority_epoch != request.expected_authority_epoch
        {
            return Err(refused(MANAGER_NODE_STALE));
        }
        let mut outcome = PortfolioRevokeOutcome::default();
        if row.active {
            let now = stamp();
            let subtree = active_subtree_on(&self.conn, request.node_id)?;
            // Parents precede children, so each member's fate is known
            // before its children's.
            let mut revoked: Vec<Uuid> = vec![request.node_id];
            let mut survivors: Vec<(&GrantRecord, Option<Uuid>)> = Vec::new();
            for member in subtree.iter().skip(1) {
                let Some(id) = member.node_id else { continue };
                let parent = member.parent_node_id;
                let parent_dies_whole = parent
                    .is_some_and(|parent| parent != request.node_id && revoked.contains(&parent));
                let grantor_died = revoked
                    .iter()
                    .any(|dead| member.grantor == PortfolioGrantor::Node(*dead).as_column());
                if parent_dies_whole || grantor_died {
                    revoked.push(id);
                } else if parent == Some(request.node_id) {
                    survivors.push((member, latest.parent_node_id));
                } else {
                    survivors.push((member, parent));
                }
            }
            for (index, id) in revoked.iter().enumerate() {
                let grant = if index == 0 {
                    latest.clone()
                } else {
                    node_grant_on(&self.conn, *id)?.ok_or_else(|| {
                        DaemonError::Store("a revoked portfolio subtree lost a grant".into())
                    })?
                };
                self.conn.execute(
                    "UPDATE global_manager_grants SET state='revoked',updated_at=?2 WHERE id=?1 AND state='active'",
                    params![grant.grant.grant_id.to_string(), now],
                )?;
                self.retire_global_grant_messages(grant.grant.grant_id, &now)?;
                self.conn.execute(
                    "DELETE FROM manager_portfolio_coverage WHERE node_id=?1",
                    [id.to_string()],
                )?;
                self.conn.execute(
                    "UPDATE manager_portfolio_nodes SET state='revoked',updated_at=?2 WHERE id=?1",
                    params![id.to_string(), now],
                )?;
            }
            let mut moved = Vec::with_capacity(survivors.len());
            for (member, parent) in &survivors {
                self.move_portfolio_grant(member, *parent, &now)?;
                moved.extend(member.node_id);
            }
            self.rebuild_coverage_of(&moved)?;
            outcome = PortfolioRevokeOutcome {
                revoked,
                reparented: moved,
                over_capacity: self.portfolio_over_capacity(latest.parent_node_id)?,
            };
        }
        Ok((self.portfolio_view_of(request.node_id)?, outcome))
    }

    /// The #1005 context-cap seat move for one node, inside the caller's
    /// IMMEDIATE transaction: the successor gets a new grant version that
    /// copies every grant column, the node id and epoch are kept (so the
    /// successor keeps the ledger), queued mail of the old grant is retired
    /// and the coverage follows the new version. `Ok(false)` when `predecessor`
    /// is not the node's active seat.
    pub(crate) fn transfer_portfolio_seat_in_tx(
        &self,
        node: Uuid,
        predecessor: Uuid,
        successor: Uuid,
    ) -> Result<bool> {
        let Some(active) = node_grant_on(&self.conn, node)? else {
            return Ok(false);
        };
        if active.grant.seat_session_id != predecessor {
            return Ok(false);
        }
        let lineage: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT continued_from FROM sessions WHERE id=?1 AND status NOT IN ('Archived','Deleted')",
                [successor.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if lineage.flatten() != Some(predecessor.to_string()) {
            return Err(refused("global_manager_successor_unrelated"));
        }
        let now = stamp();
        self.conn.execute(
            "UPDATE global_manager_grants SET state='revoked',updated_at=?2 WHERE id=?1 AND state='active'",
            params![active.grant.grant_id.to_string(), now],
        )?;
        self.retire_global_grant_messages(active.grant.grant_id, &now)?;
        let next = self.portfolio_next_version()?;
        self.conn.execute(
            "INSERT INTO global_manager_grants(id,grant_version,seat_session_id,state,project_ids_json,allowed_launches_json,project_policy_json,operator_origin,idempotency_key,created_at,updated_at,node_id,parent_node_id,grantor,child_policy_json,max_direct_reports)
             SELECT ?1,?2,?3,'active',project_ids_json,allowed_launches_json,project_policy_json,operator_origin,?4,?5,?5,node_id,parent_node_id,grantor,child_policy_json,max_direct_reports
             FROM global_manager_grants WHERE id=?6",
            params![
                Uuid::new_v4().to_string(),
                next,
                successor.to_string(),
                format!("context-cap:{successor}"),
                now,
                active.grant.grant_id.to_string(),
            ],
        )?;
        // The node keeps its depth (#1237: it may be nested).
        self.rebuild_coverage_of(&[node])?;
        self.conn.execute(
            "UPDATE manager_portfolio_nodes SET updated_at=?2 WHERE id=?1",
            params![node.to_string(), now],
        )?;
        super::agent_jobs::transfer_manager_jobs_on(&self.conn, predecessor, successor)?;
        Ok(true)
    }

    /// #1239: a grantor node replaces its child's seat. As with a
    /// context-cap transfer, the child keeps its node id, grant content and
    /// authority epoch, so the new seat acts under the child's ledger
    /// principal `(project, seat root, epoch)` and controls the workers its
    /// predecessor launched; the predecessor seat loses authority in the same
    /// commit (`manager_node_custody_changed`). Unlike a transfer, the new
    /// seat need not continue the old one. `key` is the new grant's replay
    /// key. Queued mail of the old grant is retired.
    pub(crate) fn replace_portfolio_seat_in_tx(
        &self,
        node: Uuid,
        seat: Uuid,
        key: &str,
    ) -> Result<GrantRecord> {
        let active = node_grant_on(&self.conn, node)?.ok_or_else(|| refused(MANAGER_NODE_STALE))?;
        self.portfolio_seat_available(seat, node)?;
        let now = stamp();
        self.conn.execute(
            "UPDATE global_manager_grants SET state='revoked',updated_at=?2 WHERE id=?1 AND state='active'",
            params![active.grant.grant_id.to_string(), now],
        )?;
        self.retire_global_grant_messages(active.grant.grant_id, &now)?;
        let next = self.portfolio_next_version()?;
        self.conn.execute(
            "INSERT INTO global_manager_grants(id,grant_version,seat_session_id,state,project_ids_json,allowed_launches_json,project_policy_json,operator_origin,idempotency_key,created_at,updated_at,node_id,parent_node_id,grantor,child_policy_json,max_direct_reports)
             SELECT ?1,?2,?3,'active',project_ids_json,allowed_launches_json,project_policy_json,operator_origin,?4,?5,?5,node_id,parent_node_id,grantor,child_policy_json,max_direct_reports
             FROM global_manager_grants WHERE id=?6",
            params![
                Uuid::new_v4().to_string(),
                next,
                seat.to_string(),
                key,
                now,
                active.grant.grant_id.to_string(),
            ],
        )?;
        self.rebuild_coverage_of(&[node])?;
        self.conn.execute(
            "UPDATE manager_portfolio_nodes SET updated_at=?2 WHERE id=?1",
            params![node.to_string(), now],
        )?;
        super::agent_jobs::transfer_manager_jobs_on(
            &self.conn,
            active.grant.seat_session_id,
            seat,
        )?;
        node_grant_on(&self.conn, node)?
            .ok_or_else(|| DaemonError::Store("a replaced portfolio seat lost its grant".into()))
    }

    /// The node whose active seat is `predecessor`, then its transfer.
    pub(crate) fn transfer_seat_node_in_tx(
        &self,
        predecessor: Uuid,
        successor: Uuid,
    ) -> Result<bool> {
        let Some(node) = seat_grant_on(&self.conn, predecessor)?.and_then(|record| record.node_id)
        else {
            return Ok(false);
        };
        self.transfer_portfolio_seat_in_tx(node, predecessor, successor)
    }
}

#[cfg(test)]
#[path = "portfolio_nodes_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "portfolio_levels_tests.rs"]
mod level_tests;

// #1239 (hierarchy S5): delegation at every portfolio level, part of the
// portfolio node feature.
#[path = "portfolio_delegation.rs"]
pub mod delegation;

// #1239: delegation tests share this module's fixtures.
#[cfg(test)]
#[path = "portfolio_delegation_tests.rs"]
mod delegation_tests;
