//! Global manager v0 (#872 Slice B): the operator grant, global-manager
//! messages and PM appointments.
//!
//! The grant is isolated from `manager_nodes`: a project-less node there would
//! be seen by every node query and by the legacy-mail rule. Rows are retained
//! and only `state`/`updated_at` change.
//!
//! Since #1236 (S2) every active grant belongs to a portfolio node
//! (`portfolio_nodes.rs`), one active grant per node and per seat. The seat
//! verbs resolve the caller's own node; the operator `*GlobalManager` calls
//! are shims over the single active root labelled `global` and refuse
//! `global_manager_ambiguous` when there are several.

use chrono::{SecondsFormat, Utc};
use rsi_common::global_manager::{
    ConfigureGlobalManagerRequestV1, GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT,
    GLOBAL_MANAGER_INVALID_REQUEST, GLOBAL_MANAGER_MAILBOX_FULL, GLOBAL_MANAGER_NOT_CONFIGURED,
    GLOBAL_MANAGER_NOT_SEAT, GLOBAL_MANAGER_STALE, GLOBAL_PROJECT_HAS_NO_MANAGER,
    GLOBAL_PROJECT_NOT_IN_GRANT, GLOBAL_REPORT_NOT_AUTHORIZED, GlobalIssueCountsV1,
    GlobalManagerGrantV1, GlobalManagerMessageReceiptV1, GlobalPmPolicyV1,
    RevokeGlobalManagerRequestV1,
};
use rsi_common::harness_manager_v2::{
    ConfigureHarnessManagerPolicyRequestV2, ManagerLaunchChoiceV2,
};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::Store;
use super::portfolio_nodes;
use crate::error::{DaemonError, Result};
use rsi_common::portfolio_nodes::{
    ConfigurePortfolioNodeRequestV1, GLOBAL_TIER_LABEL, MANAGER_NODE_STALE,
    PORTFOLIO_IDEMPOTENCY_CONFLICT, PORTFOLIO_INVALID_REQUEST, RevokePortfolioNodeRequestV1,
};

// The provisional number is assigned at landing.
// RSI-RELEASED-MIGRATION-BEGIN: global-manager-migration
/// Provisional schema version of the global manager tables.
pub(crate) const GLOBAL_MANAGER_SCHEMA_VERSION: i32 = 150;

const CATALOG: &str = "CREATE TABLE global_manager_grants (
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
);
CREATE UNIQUE INDEX global_manager_grants_one_active ON global_manager_grants(state) WHERE state='active';
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
 BEGIN SELECT RAISE(ABORT,'a revoked global manager grant stays revoked'); END;
CREATE TABLE global_manager_messages (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    grant_id TEXT NOT NULL REFERENCES global_manager_grants(id) ON DELETE RESTRICT,
    direction TEXT NOT NULL CHECK(direction IN ('to_manager','to_global')),
    project_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(project_id)),
    sender_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(sender_session_id)),
    target_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(target_session_id)),
    idempotency_key TEXT NOT NULL CHECK(length(idempotency_key) BETWEEN 1 AND 128),
    request_digest TEXT NOT NULL CHECK(length(request_digest)=64),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    UNIQUE(sender_session_id, idempotency_key)
);
CREATE TRIGGER global_manager_messages_no_update BEFORE UPDATE ON global_manager_messages BEGIN SELECT RAISE(ABORT,'global manager messages are immutable'); END;
CREATE TRIGGER global_manager_messages_no_delete BEFORE DELETE ON global_manager_messages BEGIN SELECT RAISE(ABORT,'global manager messages are retained'); END;
CREATE TABLE global_manager_appointments (
    id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
    grant_id TEXT NOT NULL REFERENCES global_manager_grants(id) ON DELETE RESTRICT,
    project_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(project_id)),
    caller_session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(caller_session_id)),
    idempotency_key TEXT NOT NULL CHECK(length(idempotency_key) BETWEEN 1 AND 128),
    request_digest TEXT NOT NULL CHECK(length(request_digest)=64),
    session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(session_id)),
    state TEXT NOT NULL CHECK(state IN ('launched','appointed')),
    scope_version INTEGER,
    policy_version INTEGER,
    policy_request_json TEXT CHECK(policy_request_json IS NULL OR json_valid(policy_request_json)),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
    UNIQUE(grant_id, idempotency_key),
    CHECK((state='launched' AND scope_version IS NULL AND policy_version IS NULL)
       OR (state='appointed' AND scope_version>0 AND policy_version>0))
);
CREATE TRIGGER global_manager_appointments_no_delete BEFORE DELETE ON global_manager_appointments BEGIN SELECT RAISE(ABORT,'global manager appointments are retained'); END;
CREATE TRIGGER global_manager_appointments_identity_immutable BEFORE UPDATE ON global_manager_appointments
 WHEN NEW.id IS NOT OLD.id OR NEW.grant_id IS NOT OLD.grant_id OR NEW.project_id IS NOT OLD.project_id
   OR NEW.caller_session_id IS NOT OLD.caller_session_id OR NEW.idempotency_key IS NOT OLD.idempotency_key
   OR NEW.request_digest IS NOT OLD.request_digest OR NEW.session_id IS NOT OLD.session_id
   OR NEW.created_at IS NOT OLD.created_at OR OLD.state='appointed'
   OR (OLD.policy_request_json IS NOT NULL AND NEW.policy_request_json IS NOT OLD.policy_request_json)
 BEGIN SELECT RAISE(ABORT,'a global manager appointment changes only launched to appointed'); END;";

/// Catalog objects, for the fixture rewind and presence assertions.
#[cfg(test)]
pub(crate) const CATALOG_OBJECTS: [(&str, &str); 11] = [
    ("table", "global_manager_grants"),
    ("index", "global_manager_grants_one_active"),
    ("trigger", "global_manager_grants_no_delete"),
    ("trigger", "global_manager_grants_identity_immutable"),
    ("trigger", "global_manager_grants_revoked_final"),
    ("table", "global_manager_messages"),
    ("trigger", "global_manager_messages_no_update"),
    ("trigger", "global_manager_messages_no_delete"),
    ("table", "global_manager_appointments"),
    ("trigger", "global_manager_appointments_no_delete"),
    ("trigger", "global_manager_appointments_identity_immutable"),
];

/// Teardown for the fixture rewind, newest object first.
#[cfg(test)]
pub(crate) const REWIND_SQL: &str = "DROP TRIGGER global_manager_appointments_identity_immutable;
DROP TRIGGER global_manager_appointments_no_delete;
DROP TABLE global_manager_appointments;
DROP TRIGGER global_manager_messages_no_delete;
DROP TRIGGER global_manager_messages_no_update;
DROP TABLE global_manager_messages;
DROP TRIGGER global_manager_grants_revoked_final;
DROP TRIGGER global_manager_grants_identity_immutable;
DROP TRIGGER global_manager_grants_no_delete;
DROP INDEX global_manager_grants_one_active;
DROP TABLE global_manager_grants;";

pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != GLOBAL_MANAGER_SCHEMA_VERSION || prior != version - 1 {
        return Err(DaemonError::Store(format!(
            "the global manager tables require V{}, found V{prior}",
            version - 1
        )));
    }
    tx.execute_batch(CATALOG)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: global-manager-migration

/// Most undelivered global-manager messages one grant may hold, across all
/// recipients. Queued mail follows a PM's rotation at delivery, so a
/// per-recipient count could be exceeded by an old and a new seat together;
/// one cap per grant cannot.
pub(crate) const MAX_PENDING_PER_GRANT: i64 = 64;

fn refused(code: &'static str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

fn stamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn digest(value: &impl serde::Serialize) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(value).map_err(|error| {
                DaemonError::Store(format!("global manager digest: {error}"))
            })?
        )
    ))
}

fn parse_uuid(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|_| DaemonError::Store("invalid global manager identity".into()))
}

pub(super) type GrantRow = (
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

const GRANT_COLUMNS_ACTIVE_BY_ID: &str = "SELECT id,grant_version,seat_session_id,state,project_ids_json,allowed_launches_json,project_policy_json,operator_origin,created_at,updated_at FROM global_manager_grants WHERE id=?1 AND state='active'";

/// The newest grant of a node labelled `global`, or a pre-#1236 grant (whose
/// revoked rows carry no node): what the operator workspace shows when no
/// `global` root is active.
const GRANT_COLUMNS_LATEST_GLOBAL: &str = "SELECT id,grant_version,seat_session_id,state,project_ids_json,allowed_launches_json,project_policy_json,operator_origin,created_at,updated_at FROM global_manager_grants
 WHERE node_id IS NULL OR node_id IN (SELECT id FROM manager_portfolio_nodes WHERE tier_label='global')
 ORDER BY grant_version DESC LIMIT 1";

pub(super) fn read_grant_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GrantRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

pub(super) fn grant_from_row(row: GrantRow) -> Result<GlobalManagerGrantV1> {
    let (id, version, seat, state, projects, launches, policy, origin, created, updated) = row;
    let timestamp = |text: &str| {
        super::parse_timestamp(text)
            .map_err(|_| DaemonError::Store("invalid global manager timestamp".into()))
    };
    Ok(GlobalManagerGrantV1 {
        grant_id: parse_uuid(&id)?,
        grant_version: version,
        seat_session_id: parse_uuid(&seat)?,
        state,
        project_ids: serde_json::from_str(&projects)?,
        allowed_launches: serde_json::from_str(&launches)?,
        project_policy: serde_json::from_str(&policy)?,
        operator_origin: origin,
        created_at: timestamp(&created)?,
        updated_at: timestamp(&updated)?,
    })
}

/// Who a global-manager message goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalMessageDirection {
    /// The global seat to a project's current PM.
    ToManager,
    /// A PM to the global seat.
    ToGlobal,
}

impl GlobalMessageDirection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ToManager => "to_manager",
            Self::ToGlobal => "to_global",
        }
    }
}

/// One message to queue as a durable one-shot resume wake.
pub struct GlobalMessage<'a> {
    pub grant: &'a GlobalManagerGrantV1,
    pub direction: GlobalMessageDirection,
    pub project_id: Uuid,
    pub sender: Uuid,
    pub target: Uuid,
    pub idempotency_key: &'a str,
    /// The caller's request, for the replay digest.
    pub request: serde_json::Value,
    /// The text the recipient is resumed with.
    pub delivery: String,
}

/// A recorded PM appointment.
#[derive(Debug, Clone)]
pub struct GlobalAppointment {
    pub id: Uuid,
    pub session_id: Uuid,
    pub request_digest: String,
    /// `(scope_version, policy_version)` once appointed.
    pub appointed: Option<(i64, i64)>,
    /// The exact policy request recorded before it was sent.
    pub policy_request: Option<ConfigureHarnessManagerPolicyRequestV2>,
}

/// Raw per-project rows of the overview; the session layer adds the PM seat.
pub struct GlobalProjectRow {
    pub project_id: Uuid,
    pub name: String,
    pub path: Option<String>,
    pub manager_session_id: Option<Uuid>,
    pub scope_version: Option<i64>,
    /// The project's manager scope was revoked.
    pub scope_revoked: bool,
    pub policy: Option<GlobalPmPolicyV1>,
    pub issues: GlobalIssueCountsV1,
    pub running_sessions: i64,
    pub waiting_approval_sessions: i64,
    pub pending_questions: i64,
    pub pending_approvals: i64,
}

/// The `global` shim's active grant read through `conn`: the active grant
/// of the single active root labelled `global` (`global_manager_ambiguous`
/// when several exist).
pub(crate) fn active_global_grant_on(
    conn: &rusqlite::Connection,
) -> Result<Option<GlobalManagerGrantV1>> {
    let Some(node) = portfolio_nodes::global_shim_node_on(conn)? else {
        return Ok(None);
    };
    Ok(portfolio_nodes::node_grant_on(conn, node)?.map(|record| record.grant))
}

/// Map the portfolio refusals the v0 shim surfaces to its own codes.
fn shim_code(error: DaemonError) -> DaemonError {
    match error {
        DaemonError::InvalidParam(code) if code == MANAGER_NODE_STALE => {
            refused(GLOBAL_MANAGER_STALE)
        }
        DaemonError::InvalidParam(code) if code == PORTFOLIO_IDEMPOTENCY_CONFLICT => {
            refused(GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT)
        }
        other => other,
    }
}

impl Store {
    /// Compatibility shim (#1236): the active grant of the single active root
    /// labelled `global`, if any; `global_manager_ambiguous` when several.
    pub fn active_global_grant(&self) -> Result<Option<GlobalManagerGrantV1>> {
        active_global_grant_on(&self.conn)
    }

    /// The shim's active grant, else the most recent (revoked) grant of a
    /// `global` node or of the pre-#1236 history: the operator workspace shows
    /// a revoked seat rather than an unappointed one.
    pub fn latest_global_grant(&self) -> Result<Option<GlobalManagerGrantV1>> {
        if let Some(grant) = self.active_global_grant()? {
            return Ok(Some(grant));
        }
        self.conn
            .query_row(GRANT_COLUMNS_LATEST_GLOBAL, [], read_grant_row)
            .optional()?
            .map(grant_from_row)
            .transpose()
    }

    /// Operator-only shim: appoint or replace the seat of the single root
    /// labelled `global` (created on first use). The previous active grant is
    /// revoked and the new one inserted in one transaction; the node keeps its
    /// id and the edit opens a new authority epoch.
    pub fn configure_global_manager(
        &self,
        request: &ConfigureGlobalManagerRequestV1,
        operator_origin: &str,
    ) -> Result<GlobalManagerGrantV1> {
        self.configure_global_manager_confirmed(request, operator_origin, false)
    }

    /// The global shim carries the operator's explicit reduction acknowledgment.
    pub fn configure_global_manager_confirmed(
        &self,
        request: &ConfigureGlobalManagerRequestV1,
        operator_origin: &str,
        confirm_cap_reductions: bool,
    ) -> Result<GlobalManagerGrantV1> {
        request.validate().map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) =
            portfolio_nodes::grant_by_key_on(&self.conn, &request.idempotency_key)?
        {
            // #1236: a replay binds to the shim's semantics: a grant of a root
            // labelled `global` (or pre-#1236 history), written by an operator
            // configure, never a context-cap transfer or another tier's key.
            let global_node = match existing.node_id {
                None => true,
                Some(node) => {
                    existing.parent_node_id.is_none()
                        && portfolio_nodes::node_row_on(&self.conn, node)?
                            .is_some_and(|row| row.tier_label == GLOBAL_TIER_LABEL)
                }
            };
            let same = global_node
                && existing.grantor == "operator"
                && !request.idempotency_key.starts_with("context-cap:")
                && existing.grant.seat_session_id == request.session_id
                && existing.grant.project_ids == request.project_ids
                && existing.grant.allowed_launches == request.allowed_launches
                && existing.grant.project_policy == request.project_policy;
            if !same {
                return Err(refused(GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT));
            }
            tx.commit()?;
            return Ok(existing.grant);
        }
        let node = portfolio_nodes::global_shim_node_on(&self.conn)?;
        let active = match node {
            Some(node) => portfolio_nodes::node_grant_on(&self.conn, node)?,
            None => None,
        };
        let actual = active
            .as_ref()
            .map_or(0, |record| record.grant.grant_version);
        if actual != request.expected_grant_version {
            return Err(refused(GLOBAL_MANAGER_STALE));
        }
        let (expected_authority_epoch, child_policy, max_direct_reports) = match (node, &active) {
            (Some(node), Some(record)) => (
                portfolio_nodes::node_row_on(&self.conn, node)?
                    .map_or(0, |row| row.authority_epoch),
                record.child_policy.clone(),
                record.max_direct_reports,
            ),
            _ => (
                0,
                None,
                rsi_common::portfolio_nodes::PORTFOLIO_DEFAULT_MAX_DIRECT_REPORTS,
            ),
        };
        let saved = self
            .configure_portfolio_node_as_in_tx_confirmed(
                &ConfigurePortfolioNodeRequestV1 {
                    node_id: node,
                    parent_node_id: None,
                    adopt_node_ids: Vec::new(),
                    expected_parent_grant_version: None,
                    tier_label: GLOBAL_TIER_LABEL.into(),
                    seat_session_id: request.session_id,
                    project_ids: request.project_ids.clone(),
                    allowed_launches: request.allowed_launches.clone(),
                    policy: request.project_policy.clone(),
                    child_policy,
                    max_direct_reports,
                    expected_node_grant_version: actual,
                    expected_authority_epoch,
                    idempotency_key: request.idempotency_key.clone(),
                },
                portfolio_nodes::PortfolioGrantor::Operator,
                operator_origin,
                None,
                confirm_cap_reductions,
            )
            .map_err(|error| match error {
                DaemonError::InvalidParam(code) if code == PORTFOLIO_INVALID_REQUEST => {
                    refused(GLOBAL_MANAGER_INVALID_REQUEST)
                }
                other => shim_code(other),
            })?;
        tx.commit()?;
        Ok(saved.grant)
    }

    /// #1005: the daemon rotated the global seat at its context cap; the
    /// operator's grant moves to the rotation successor unchanged (same
    /// projects, launches, policy and operator origin) under a new version.
    /// As with an operator re-appointment, messages queued under the old
    /// grant are retired. Returns `false` when `predecessor` is not the active
    /// seat (already moved, revoked or replaced).
    pub fn transfer_global_seat(&self, predecessor: Uuid, successor: Uuid) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let moved = self.transfer_seat_node_in_tx(predecessor, successor)?;
        tx.commit()?;
        Ok(moved)
    }

    /// Move the active global grant from `predecessor` to `successor` only
    /// when that exact successor holds a durable publication witness for the
    /// rotation (`published_rotation_successor_of`), checked in the same
    /// `IMMEDIATE` transaction as the move (#1142). The repair path of a cap
    /// settlement uses it: parent archival or a reserved/failed child is not a
    /// witness. The witness is an immutable committed fact and the transaction
    /// serializes with every grant writer, so no spawn guard is needed.
    /// `Ok(false)` when the successor is unwitnessed or the grant is elsewhere.
    pub fn transfer_global_seat_if_published(
        &self,
        predecessor: Uuid,
        successor: Uuid,
        rotation_id: Option<&str>,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if self.published_rotation_successor_of(predecessor, rotation_id)? != Some(successor) {
            return Ok(false);
        }
        let moved = self.transfer_seat_node_in_tx(predecessor, successor)?;
        tx.commit()?;
        Ok(moved)
    }

    /// Operator-only shim: revoke the single active root labelled `global`
    /// (its node, grant and coverage). A replay after the revocation returns
    /// the revoked grant.
    pub fn revoke_global_manager(
        &self,
        request: &RevokeGlobalManagerRequestV1,
    ) -> Result<GlobalManagerGrantV1> {
        request.validate().map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let node = portfolio_nodes::global_shim_node_on(&self.conn)?;
        let active = match node {
            Some(node) => portfolio_nodes::node_grant_on(&self.conn, node)?,
            None => None,
        };
        let (Some(node), Some(active)) = (node, active) else {
            let revoked =
                portfolio_nodes::grant_by_version_on(&self.conn, request.expected_grant_version)?
                    .map(|record| record.grant)
                    .filter(|grant| grant.state == "revoked")
                    .ok_or_else(|| refused(GLOBAL_MANAGER_NOT_CONFIGURED))?;
            tx.commit()?;
            return Ok(revoked);
        };
        if active.grant.grant_version != request.expected_grant_version {
            return Err(refused(GLOBAL_MANAGER_STALE));
        }
        let epoch =
            portfolio_nodes::node_row_on(&self.conn, node)?.map_or(0, |row| row.authority_epoch);
        let (revoked, _) = self
            .revoke_portfolio_node_in_tx(&RevokePortfolioNodeRequestV1 {
                node_id: node,
                expected_grant_version: active.grant.grant_version,
                expected_authority_epoch: epoch,
                idempotency_key: request.idempotency_key.clone(),
            })
            .map_err(shim_code)?;
        tx.commit()?;
        Ok(revoked.grant)
    }

    /// The active grant whose seat is exactly `caller`, on any portfolio
    /// node. Authority does not follow rotation lineage by itself: a
    /// context-cap transfer moves the grant to the successor. Every other
    /// caller, a revoked grant and a replaced seat are refused with
    /// `global_manager_not_seat`.
    pub fn global_seat_grant(&self, caller: Uuid) -> Result<GlobalManagerGrantV1> {
        self.portfolio_seat_grant(caller)?
            .ok_or_else(|| refused(GLOBAL_MANAGER_NOT_SEAT))
    }

    /// The live PM seat of a project: a non-revoked appointment whose current
    /// principal resolves. A cleared or broken appointment has no PM.
    pub(crate) fn global_live_manager(&self, project_id: Uuid) -> Result<Option<Uuid>> {
        Ok(self
            .get_harness_manager(project_id)?
            .filter(|config| !config.is_revoked())
            .and_then(|config| config.current_session_id))
    }

    /// Refuse an appointment while the project's root has active area
    /// delegates, before any launch (the appoint would refuse it later).
    pub fn global_appointment_delegates_free(&self, project_id: Uuid) -> Result<()> {
        let active: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM manager_nodes child JOIN manager_nodes root
               ON child.parent_node_id=root.id
             WHERE root.legacy_project_id=?1 AND child.state='active')",
            [project_id.to_string()],
            |row| row.get(0),
        )?;
        if active {
            return Err(refused(
                "manager_node_root_has_active_delegates: revoke child nodes first",
            ));
        }
        Ok(())
    }

    /// Refuse a project outside the grant.
    pub fn global_grant_covers(grant: &GlobalManagerGrantV1, project_id: Uuid) -> Result<()> {
        if grant.project_ids.contains(&project_id) {
            return Ok(());
        }
        Err(refused(GLOBAL_PROJECT_NOT_IN_GRANT))
    }

    /// The live PM seat of a project, refused when none.
    pub fn global_current_manager(&self, project_id: Uuid) -> Result<Uuid> {
        self.global_live_manager(project_id)?
            .ok_or_else(|| refused(GLOBAL_PROJECT_HAS_NO_MANAGER))
    }

    /// True when `caller` is the active global seat and `target` is the live
    /// PM seat (the non-revoked appointment's current principal) of a granted
    /// project. This authorizes reads and terminal watches only, never a
    /// mutation.
    pub fn global_seat_reads_manager(&self, caller: Uuid, target: Uuid) -> Result<bool> {
        let Ok(grant) = self.global_seat_grant(caller) else {
            return Ok(false);
        };
        let Some(project) = self.get_session(target)?.and_then(|s| s.project_id) else {
            return Ok(false);
        };
        if !grant.project_ids.contains(&project) {
            return Ok(false);
        }
        // #1236: no sibling reach, even to a PM seat that also seats a node.
        let own = portfolio_nodes::seat_grant_on(&self.conn, caller)?.and_then(|r| r.node_id);
        if self
            .portfolio_seat_node(target)?
            .is_some_and(|node| Some(node) != own)
        {
            return Ok(false);
        }
        Ok(self.global_live_manager(project)? == Some(target))
    }

    /// The grant and project of a PM reporting up: `caller` must be the
    /// current PM seat of a project, and the grant is the active grant of the
    /// portfolio node covering that project.
    pub fn global_report_grant(&self, caller: Uuid) -> Result<(GlobalManagerGrantV1, Uuid)> {
        let denied = || refused(GLOBAL_REPORT_NOT_AUTHORIZED);
        let project = self
            .get_session(caller)?
            .and_then(|session| session.project_id)
            .ok_or_else(denied)?;
        let grant = self
            .covering_portfolio_grant(project)?
            .map(|record| record.grant)
            .ok_or_else(denied)?;
        if !grant.project_ids.contains(&project) {
            return Err(denied());
        }
        if self.global_live_manager(project)? != Some(caller) {
            return Err(denied());
        }
        Ok((grant, project))
    }

    /// Whether `caller` may report up (catalog advertisement only).
    pub(crate) fn global_reporter(&self, caller: Uuid) -> Result<bool> {
        match self.global_report_grant(caller) {
            Ok(_) => Ok(true),
            Err(DaemonError::InvalidParam(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// #1235: whether `session` was ever the seat of a global grant (active or
    /// retired). Effect gates use it so a retired seat's request never falls
    /// back to another principal's authority.
    pub fn is_global_seat_ever(&self, session: Uuid) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM global_manager_grants WHERE seat_session_id=?1)",
            [session.to_string()],
            |row| row.get(0),
        )?)
    }

    /// Whether `caller` is the active global seat (catalog advertisement only).
    pub(crate) fn is_global_seat(&self, caller: Uuid) -> Result<bool> {
        match self.global_seat_grant(caller) {
            Ok(_) => Ok(true),
            Err(DaemonError::InvalidParam(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Queue one message as a durable one-shot resume wake on its recipient.
    /// The scheduler delivers it at the recipient's next idle boundary (and
    /// wakes an idle recipient) only while the grant and the recipient seat are
    /// current ([`Self::global_message_deliverable`]); grant replacement,
    /// revocation and PM displacement retire it.
    /// A replay under the same `(sender, idempotency_key)` returns the same
    /// message; a changed request under that key is refused.
    pub fn queue_global_message(
        &self,
        message: &GlobalMessage<'_>,
    ) -> Result<GlobalManagerMessageReceiptV1> {
        let request_digest = digest(&serde_json::json!({
            "direction": message.direction.as_str(),
            "project_id": message.project_id,
            "request": message.request,
        }))?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, String, String)> = self
            .conn
            .query_row(
                "SELECT id,project_id,target_session_id,request_digest FROM global_manager_messages
                 WHERE sender_session_id=?1 AND idempotency_key=?2",
                params![message.sender.to_string(), message.idempotency_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((id, project, target, stored_digest)) = existing {
            if stored_digest != request_digest {
                return Err(refused(GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT));
            }
            tx.commit()?;
            return Ok(GlobalManagerMessageReceiptV1 {
                message_id: parse_uuid(&id)?,
                project_id: parse_uuid(&project)?,
                target_session_id: parse_uuid(&target)?,
                deduplicated: true,
            });
        }
        let pending: i64 = self.conn.query_row(
            "SELECT count(*) FROM global_manager_messages m JOIN scheduled_jobs j ON j.id=m.id
             WHERE m.grant_id=?1 AND j.enabled=1",
            [message.grant.grant_id.to_string()],
            |row| row.get(0),
        )?;
        if pending >= MAX_PENDING_PER_GRANT {
            return Err(refused(GLOBAL_MANAGER_MAILBOX_FULL));
        }
        let id = Uuid::new_v4();
        let now = Utc::now();
        let job = ScheduledJob {
            id,
            name: match message.direction {
                GlobalMessageDirection::ToManager => "Global manager message".into(),
                GlobalMessageDirection::ToGlobal => "Project manager report".into(),
            },
            message: message.delivery.clone(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: now,
            },
            last_fired_at: None,
            next_fire_at: now,
            enabled: true,
            working_dir: None,
            provider: None,
            model: None,
            project_id: Some(message.project_id),
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::Resume,
            wake_session_id: Some(message.target),
        };
        super::scheduled_jobs::insert_scheduled_job_conn(&self.conn, &job)?;
        self.conn.execute(
            "INSERT INTO global_manager_messages(id,grant_id,direction,project_id,sender_session_id,target_session_id,idempotency_key,request_digest,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id.to_string(),
                message.grant.grant_id.to_string(),
                message.direction.as_str(),
                message.project_id.to_string(),
                message.sender.to_string(),
                message.target.to_string(),
                message.idempotency_key,
                request_digest,
                now.to_rfc3339_opts(SecondsFormat::Nanos, true),
            ],
        )?;
        tx.commit()?;
        Ok(GlobalManagerMessageReceiptV1 {
            message_id: id,
            project_id: message.project_id,
            target_session_id: message.target,
            deduplicated: false,
        })
    }

    /// The recorded appointment under `(grant, key)`, if any. A different
    /// request under the same key is refused.
    pub fn global_appointment(
        &self,
        grant: &GlobalManagerGrantV1,
        idempotency_key: &str,
        request: &impl serde::Serialize,
    ) -> Result<Option<GlobalAppointment>> {
        let request_digest = digest(request)?;
        type Row = (
            String,
            String,
            String,
            Option<i64>,
            Option<i64>,
            Option<String>,
        );
        let row: Option<Row> = self
            .conn
            .query_row(
                "SELECT id,session_id,request_digest,scope_version,policy_version,policy_request_json
                 FROM global_manager_appointments WHERE grant_id=?1 AND idempotency_key=?2",
                params![grant.grant_id.to_string(), idempotency_key],
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
        let Some((id, session, stored_digest, scope, policy, policy_request)) = row else {
            return Ok(None);
        };
        if stored_digest != request_digest {
            return Err(refused(GLOBAL_MANAGER_IDEMPOTENCY_CONFLICT));
        }
        Ok(Some(GlobalAppointment {
            id: parse_uuid(&id)?,
            session_id: parse_uuid(&session)?,
            request_digest: stored_digest,
            appointed: scope.zip(policy),
            policy_request: policy_request
                .map(|json| serde_json::from_str(&json))
                .transpose()?,
        }))
    }

    /// Record the launched PM session before it is appointed, so a replay
    /// never launches a second one.
    pub fn record_global_appointment_launch(
        &self,
        grant: &GlobalManagerGrantV1,
        project_id: Uuid,
        caller: Uuid,
        idempotency_key: &str,
        request: &impl serde::Serialize,
        session_id: Uuid,
    ) -> Result<GlobalAppointment> {
        let appointment = GlobalAppointment {
            id: Uuid::new_v4(),
            session_id,
            request_digest: digest(request)?,
            appointed: None,
            policy_request: None,
        };
        let now = stamp();
        self.conn.execute(
            "INSERT INTO global_manager_appointments(id,grant_id,project_id,caller_session_id,idempotency_key,request_digest,session_id,state,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,'launched',?8,?8)",
            params![
                appointment.id.to_string(),
                grant.grant_id.to_string(),
                project_id.to_string(),
                caller.to_string(),
                idempotency_key,
                appointment.request_digest,
                session_id.to_string(),
                now,
            ],
        )?;
        Ok(appointment)
    }

    /// Persist the exact policy request before it is sent, so a retry after a
    /// crash replays it verbatim (same key, same fences) instead of
    /// conflicting with its own receipt.
    pub fn record_global_appointment_policy_request(
        &self,
        appointment_id: Uuid,
        request: &ConfigureHarnessManagerPolicyRequestV2,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE global_manager_appointments SET policy_request_json=?2,updated_at=?3
             WHERE id=?1 AND state='launched' AND policy_request_json IS NULL",
            params![
                appointment_id.to_string(),
                serde_json::to_string(request)?,
                stamp()
            ],
        )?;
        Ok(())
    }

    /// Disable every pending message of a replaced or revoked grant, and
    /// (#1238) retire the queued tier mail and open escalation hops at either
    /// end under it.
    pub(super) fn retire_global_grant_messages(&self, grant_id: Uuid, now: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE enabled=1 AND id IN
             (SELECT id FROM global_manager_messages WHERE grant_id=?1)",
            params![grant_id.to_string(), now],
        )?;
        self.retire_tier_grant(grant_id, now)
    }

    /// Disable every pending global-manager message to a project's PM seat
    /// when that seat is displaced or cleared.
    pub(crate) fn retire_global_project_messages(&self, project_id: Uuid, now: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE enabled=1 AND id IN
             (SELECT id FROM global_manager_messages WHERE project_id=?1 AND direction='to_manager')",
            params![project_id.to_string(), now],
        )?;
        // #1238: queued tier mail to the seat and its own reports.
        self.retire_tier_project_seat(project_id, now)
    }

    /// Delivery-time fence for a scheduled job. Non-global jobs pass. A global
    /// message passes only while its grant is the active one and its recipient
    /// is still current: the project's live PM (its rotation tip) for
    /// `to_manager`, the exact unrotated seat for `to_global`.
    pub fn global_message_deliverable(&self, job_id: Uuid) -> Result<bool> {
        self.global_message_deliverable_to(job_id, None)
    }

    /// Whether `job_id` is a global-manager message (the scheduler binds its
    /// delivery to the exact job row even on a manual trigger).
    /// #1238: tier messages (`manager_tier_messages`) ride the same path.
    pub fn is_global_message(&self, job_id: Uuid) -> Result<bool> {
        let legacy: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM global_manager_messages WHERE id=?1)",
            [job_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(legacy || self.is_tier_message(job_id)?)
    }

    /// Effect-claim half of the delivery fence. Runs inside the continuation
    /// effect claim's IMMEDIATE transaction with `tip`, the session the
    /// continuation actually resumes. Every global message among `job_ids`
    /// must still be enabled, under the active grant, and addressed to that
    /// exact tip; a failing one is retired in the same transaction and the
    /// claim must be refused (`false`). Non-global jobs pass.
    pub(crate) fn claim_global_messages_for_tip(
        &self,
        job_ids: &[Uuid],
        tip: Uuid,
    ) -> Result<bool> {
        let mut deliverable = true;
        let mut claimed = Vec::new();
        for &job_id in job_ids {
            if !self.is_global_message(job_id)? {
                continue;
            }
            let enabled: bool = self
                .conn
                .query_row(
                    "SELECT enabled FROM scheduled_jobs WHERE id=?1",
                    [job_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(false);
            if enabled && self.global_message_deliverable_to(job_id, Some(tip))? {
                claimed.push(job_id);
                continue;
            }
            self.retire_global_message(job_id)?;
            deliverable = false;
        }
        // #1238/#1266: tier messages are claimed only with a claim that
        // succeeds. A refused batch commits only its retirements (the caller
        // commits them), so its current messages stay queued and deliverable.
        // A successful claim leaves them `claimed`: the continuation settles
        // them delivered, failed or uncertain (`settle_tier_messages`).
        if deliverable {
            for job_id in claimed {
                self.claim_tier_message(job_id)?;
            }
        }
        Ok(deliverable)
    }

    /// `delivery_tip`: the session the continuation resumes, when known;
    /// otherwise the recorded target's lineage tip.
    fn global_message_deliverable_to(
        &self,
        job_id: Uuid,
        delivery_tip: Option<Uuid>,
    ) -> Result<bool> {
        if let Some(deliverable) = self.tier_message_deliverable(job_id, delivery_tip)? {
            return Ok(deliverable);
        }
        let row: Option<(String, String, String, String)> = self
            .conn
            .query_row(
                "SELECT grant_id,direction,project_id,target_session_id FROM global_manager_messages WHERE id=?1",
                [job_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((grant_id, direction, project, target)) = row else {
            return Ok(true);
        };
        // The message's own grant must still be its node's active grant.
        let grant = self
            .conn
            .query_row(GRANT_COLUMNS_ACTIVE_BY_ID, [&grant_id], read_grant_row)
            .optional()?
            .map(grant_from_row)
            .transpose()?;
        let Some(grant) = grant else {
            return Ok(false);
        };
        let target = parse_uuid(&target)?;
        let tip = match delivery_tip {
            Some(tip) => tip,
            None => self.session_lineage_tip(target)?,
        };
        if direction == GlobalMessageDirection::ToGlobal.as_str() {
            return Ok(grant.seat_session_id == target && tip == target);
        }
        let project = parse_uuid(&project)?;
        Ok(grant.project_ids.contains(&project) && self.global_live_manager(project)? == Some(tip))
    }

    /// Retire one undeliverable global (or #1238 tier) message without
    /// delivering it.
    pub fn retire_global_message(&self, job_id: Uuid) -> Result<()> {
        self.conn.execute(
            "UPDATE scheduled_jobs SET enabled=0,updated_at=?2 WHERE id=?1 AND enabled=1
             AND id IN (SELECT id FROM global_manager_messages)",
            params![job_id.to_string(), stamp()],
        )?;
        self.retire_tier_message(job_id)
    }

    /// Mark an appointment complete with the scope and policy versions.
    pub fn complete_global_appointment(
        &self,
        appointment_id: Uuid,
        scope_version: i64,
        policy_version: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE global_manager_appointments SET state='appointed',scope_version=?2,policy_version=?3,updated_at=?4
             WHERE id=?1 AND state='launched'",
            params![
                appointment_id.to_string(),
                scope_version,
                policy_version,
                stamp()
            ],
        )?;
        Ok(())
    }

    /// Refuse a launch outside the grant's allowlist. The effort matches when
    /// the allowlist entry names none or names the same one.
    pub fn global_launch_allowed(
        grant: &GlobalManagerGrantV1,
        launch: &ManagerLaunchChoiceV2,
    ) -> Result<()> {
        let allowed = grant.allowed_launches.iter().any(|choice| {
            choice.provider == launch.provider
                && choice.model == launch.model
                && (choice.effort.is_none() || choice.effort == launch.effort)
        });
        if allowed {
            return Ok(());
        }
        Err(refused(
            rsi_common::global_manager::GLOBAL_LAUNCH_NOT_ALLOWED,
        ))
    }

    /// Bounded, static-SQL portfolio read for the overview.
    pub fn global_project_rows(
        &self,
        grant: &GlobalManagerGrantV1,
    ) -> Result<Vec<GlobalProjectRow>> {
        let mut rows = Vec::with_capacity(grant.project_ids.len());
        for project_id in &grant.project_ids {
            let Some(project) = self.get_project(*project_id)? else {
                continue;
            };
            rows.push(self.global_project_row(project)?);
        }
        Ok(rows)
    }

    pub(crate) fn global_project_row(
        &self,
        project: rsi_common::types::Project,
    ) -> Result<GlobalProjectRow> {
        let key = project.id.to_string();
        let config = self.get_harness_manager(project.id)?;
        let effective_caps = self.project_effective_resource_caps(project.id)?;
        let policy = self
            .get_harness_manager_policy(project.id)?
            .map(|policy| GlobalPmPolicyV1 {
                effective_caps,
                policy_version: policy.row_version,
                mode: policy.policy.mode,
                revoked: policy.revoked,
                paused: policy.policy.paused,
                capabilities: policy.policy.capabilities,
            });
        let issues = self.conn.query_row(
            "SELECT
                COALESCE(SUM(status='Open'),0),
                COALESCE(SUM(status='InProgress'),0),
                COALESCE(SUM(status='Open' AND EXISTS(SELECT 1 FROM json_each(issues.labels) WHERE json_each.value='operator-request')),0)
             FROM issues WHERE project_id=?1 AND archived_at IS NULL",
            [&key],
            |row| {
                Ok(GlobalIssueCountsV1 {
                    open: row.get(0)?,
                    in_progress: row.get(1)?,
                    open_operator_requests: row.get(2)?,
                })
            },
        )?;
        let (running, waiting, questions): (i64, i64, i64) = self.conn.query_row(
            "SELECT
                COALESCE(SUM(status='Running'),0),
                COALESCE(SUM(status='WaitingApproval'),0),
                COALESCE(SUM(pending_question_json IS NOT NULL AND status NOT IN ('Archived','Deleted')),0)
             FROM sessions WHERE project_id=?1",
            [&key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let approvals: i64 = self.conn.query_row(
            "SELECT count(*) FROM approvals a JOIN sessions s ON s.id=a.session_id
             WHERE s.project_id=?1 AND a.status='Pending'",
            [&key],
            |row| row.get(0),
        )?;
        Ok(GlobalProjectRow {
            project_id: project.id,
            name: project.name,
            path: project.path.map(|path| path.to_string_lossy().into_owned()),
            manager_session_id: config
                .as_ref()
                .filter(|c| !c.is_revoked())
                .and_then(|c| c.current_session_id),
            scope_version: config.as_ref().map(|c| c.row_version),
            scope_revoked: config.as_ref().is_some_and(|c| c.is_revoked()),
            policy,
            issues,
            running_sessions: running,
            waiting_approval_sessions: waiting,
            pending_questions: questions,
            pending_approvals: approvals,
        })
    }
}

#[cfg(test)]
#[path = "global_manager_tests.rs"]
mod tests;
