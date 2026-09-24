//! Manager v2 grants, authority witnesses and shared durable storage.
//!
//! Callers of the record helpers hold the Store mutex and an immediate
//! transaction spanning authorization, CAS, event, and operation receipt.

use chrono::{SecondsFormat, Utc};
use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::harness_manager_v2::*;
use rsi_common::types::{Session, SessionKind, SessionStatus};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::Store;
use crate::error::{DaemonError, Result};

pub(crate) fn refused(code: &str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}
pub(crate) fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}
pub(crate) fn fingerprint(value: &Value) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > 65536 {
        return Err(refused("manager_v2_payload_limit"));
    }
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

/// Live-row cap for each bookkeeping class. Enforcement and the Inspector's
/// `record_budget` row both read this constant, so the displayed limit cannot
/// drift from the enforced one (Issue #643).
pub(super) const BOOKKEEPING_LIMIT: i64 = 1024;

/// Coordination-class count for one manager scope. The predicate repeats the
/// `harness_manager_v2_coordination_budget` partial index term for term, so
/// archived bookkeeping history never enters the scanned range. The trailing
/// request-marker exclusion (#664) only narrows it: a stricter WHERE still
/// implies the partial-index WHERE, so the released index DDL is unchanged.
pub(super) const COORDINATION_BUDGET_COUNT_SQL: &str = "SELECT count(*) FROM harness_manager_v2_records INDEXED BY harness_manager_v2_coordination_budget WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind NOT IN ('retrieval','resource_spend','resource_launch_origin','lifecycle_context','lifecycle_execution','lifecycle_hold') AND NOT (kind='decision_generation' OR (kind IN ('decision','decision_target') AND substr(record_key,1,9)='approval:') OR (kind='decision_delivery' AND json_extract(payload_json,'$.target.kind') IS 'appserver_approval') OR (kind='decision_retrieval' AND (substr(record_key,1,17)='manager:approval:' OR substr(record_key,1,14)='lead:approval:'))) AND kind NOT IN ('request_released','request_settle','request_rollover','request_unsettled')";

/// Server-owned #664/#656 request markers, each its own bookkeeping class
/// (see [`bookkeeping_class`]) excluded from the coordination count above. A
/// lead-terminal release, the orphan sweep and a rollover therefore never
/// fail on a full coordination budget. Each marker is keyed by one request
/// message id, so a kind holds at most one row per request; a
/// `request_released` row is further paired with that request's `request`
/// coordination record, so it cannot exceed `MANAGER_V2_MAX_RECORDS` and its
/// class limit is structurally unreachable. None is ever retired. The
/// `request_settle`, `request_unsettled` and `request_rollover` classes are
/// unmetered (see [`bookkeeping_class_limit`]).
pub(super) const REQUEST_MARKER_KINDS: [&str; 4] = [
    super::harness_manager::REQUEST_RELEASED_KIND,
    super::harness_manager::REQUEST_SETTLE_KIND,
    super::harness_manager::REQUEST_ROLLOVER_KIND,
    super::harness_manager::REQUEST_UNSETTLED_KIND,
];

/// Live bookkeeping counts `(retrieval, resource, lifecycle)` for one scope,
/// read only from the live-row index.
pub(super) const LIVE_BOOKKEEPING_COUNTS_SQL: &str = "SELECT COALESCE(SUM(kind='retrieval'),0),
        COALESCE(SUM(kind IN ('resource_spend','resource_launch_origin')),0),
        COALESCE(SUM(kind IN ('lifecycle_context','lifecycle_execution','lifecycle_hold')),0)
 FROM harness_manager_v2_records INDEXED BY harness_manager_v2_live_records
 WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND archived=0
   AND kind IN ('retrieval','resource_spend','resource_launch_origin','lifecycle_context','lifecycle_execution','lifecycle_hold')";

/// Live count for one bookkeeping class predicate from [`bookkeeping_class`].
pub(super) fn live_class_count_sql(predicate: &str) -> String {
    format!(
        "SELECT count(*) FROM harness_manager_v2_records INDEXED BY harness_manager_v2_live_records WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND archived=0 AND {predicate}"
    )
}

/// Retires finished bookkeeping rows in one scope. Ranges over live rows of
/// the retirable kinds only, never over archived history.
pub(super) const RETIRE_BOOKKEEPING_SQL: &str = "UPDATE harness_manager_v2_records AS r INDEXED BY harness_manager_v2_live_records
             SET archived=1,updated_at=?4
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND archived=0
               AND kind IN ('retrieval','lifecycle_context','lifecycle_execution','resource_spend','resource_launch_origin')
               AND (kind='retrieval' OR
                    (kind IN ('lifecycle_context','lifecycle_execution') AND EXISTS(
                        SELECT 1 FROM harness_manager_v2_operations o WHERE o.id=r.record_key
                        AND o.project_id=r.project_id AND o.manager_session_id=r.manager_session_id
                        AND o.scope_version=r.scope_version AND o.state NOT IN ('queued','running')))
                    OR (kind IN ('resource_spend','resource_launch_origin') AND EXISTS(
                        SELECT 1 FROM sessions s WHERE s.id=r.record_key
                        AND s.status IN ('Completed','Failed','Archived','Deleted')
                        AND NOT EXISTS(SELECT 1 FROM model_invocations i WHERE i.session_id=s.id
                            AND i.admission_status='admitted' AND i.status IN ('running','cancellation_requested'))))
                    OR (kind IN ('resource_spend','resource_launch_origin')
                        AND NOT EXISTS(SELECT 1 FROM sessions s WHERE s.id=r.record_key)
                        AND EXISTS(SELECT 1 FROM harness_manager_v2_records origin
                            JOIN model_invocations i ON i.id=json_extract(origin.payload_json,'$.invocation_id')
                            WHERE origin.project_id=r.project_id AND origin.manager_session_id=r.manager_session_id
                            AND origin.scope_version=r.scope_version AND origin.kind='resource_launch_origin'
                            AND origin.record_key=r.record_key AND i.admission_status='admitted'
                            AND i.status NOT IN ('running','cancellation_requested'))))";

#[cfg(test)]
pub(super) const LIVE_BOOKKEEPING_INDEX_CATALOG_OBJECTS: [(&str, &str); 2] = [
    ("index", "harness_manager_v2_live_records"),
    ("index", "harness_manager_v2_coordination_budget"),
];

// RSI-RELEASED-MIGRATION-BEGIN: manager-v2-live-bookkeeping-index-migration
/// Schema version of the Issue #643 live-scope indexes (V124, issued by the
/// manager). This is the single source for the migration's version checks; the `if version < N` block in `store/mod.rs` and
/// `LATEST_SCHEMA_VERSION` must name the same literal because the
/// released-migration inventory parses it.
pub(super) const MANAGER_V2_LIVE_BOOKKEEPING_INDEX_VERSION: i32 = 124;

/// Live-scope indexes: bookkeeping sweeps, per-class live counts and live
/// kind reads range over non-archived rows only; the coordination count
/// excludes every bookkeeping kind. Additive; no row changes.
pub(super) fn apply_live_bookkeeping_index_migration(store: &Store) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != MANAGER_V2_LIVE_BOOKKEEPING_INDEX_VERSION - 1 {
        return Err(DaemonError::Store(format!(
            "V{MANAGER_V2_LIVE_BOOKKEEPING_INDEX_VERSION} requires exact V{} source, found V{version}",
            MANAGER_V2_LIVE_BOOKKEEPING_INDEX_VERSION - 1
        )));
    }
    tx.execute_batch(
        "CREATE INDEX harness_manager_v2_live_records
            ON harness_manager_v2_records(project_id,manager_session_id,scope_version,kind,record_key)
            WHERE archived=0;
         CREATE INDEX harness_manager_v2_coordination_budget
            ON harness_manager_v2_records(project_id,manager_session_id,scope_version)
            WHERE kind NOT IN ('retrieval','resource_spend','resource_launch_origin','lifecycle_context','lifecycle_execution','lifecycle_hold') AND NOT (kind='decision_generation' OR (kind IN ('decision','decision_target') AND substr(record_key,1,9)='approval:') OR (kind='decision_delivery' AND json_extract(payload_json,'$.target.kind') IS 'appserver_approval') OR (kind='decision_retrieval' AND (substr(record_key,1,17)='manager:approval:' OR substr(record_key,1,14)='lead:approval:')));",
    )?;
    tx.execute(
        &format!("PRAGMA user_version = {MANAGER_V2_LIVE_BOOKKEEPING_INDEX_VERSION}"),
        [],
    )?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: manager-v2-live-bookkeeping-index-migration

/// Live-row cap enforced (and displayed by the Inspector) for a bookkeeping
/// class, or `None` when the class is unmetered. Each unmetered class is
/// bounded by immutable message history instead of a scope-wide cap, which
/// would only let past requests refuse a later valid write:
/// - `request_rollover` holds one chain record per rolled-over standing
///   request (review 1cdaa85f): each chain is bounded by `LINEAGE_LIMIT`
///   generations and every record implies at least 33 immutable message rows.
/// - `request_settle` is keyed by one manager request id and written at CAS 0
///   at most once per request, so its rows never exceed the scope's
///   manager->lead request messages. A cap here stopped the orphan sweep for
///   good once enough orphans had settled (review 1273507c).
/// - `request_unsettled` is keyed by the same request id and voids that
///   request's settle row, so it is bounded by `request_settle`.
///
/// `request_released` keeps its (structurally unreachable) cap: it is paired
/// with a `request` coordination record bounded by `MANAGER_V2_MAX_RECORDS`.
pub(super) fn bookkeeping_class_limit(kind: &str) -> Option<i64> {
    use super::harness_manager::{
        REQUEST_ROLLOVER_KIND, REQUEST_SETTLE_KIND, REQUEST_UNSETTLED_KIND,
    };
    (![
        REQUEST_ROLLOVER_KIND,
        REQUEST_SETTLE_KIND,
        REQUEST_UNSETTLED_KIND,
    ]
    .contains(&kind))
    .then_some(BOOKKEEPING_LIMIT)
}

pub(super) fn bookkeeping_class(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "retrieval" => Some(("kind='retrieval'", "manager_v2_retrieval_limit")),
        "resource_spend" | "resource_launch_origin" => Some((
            "kind IN ('resource_spend','resource_launch_origin')",
            "manager_v2_resource_record_limit",
        )),
        "lifecycle_context" | "lifecycle_execution" | "lifecycle_hold" => Some((
            "kind IN ('lifecycle_context','lifecycle_execution','lifecycle_hold')",
            "manager_v2_lifecycle_limit",
        )),
        "request_released" => Some((
            "kind='request_released'",
            "manager_v2_request_release_limit",
        )),
        "request_settle" => Some(("kind='request_settle'", "manager_v2_request_settle_limit")),
        "request_unsettled" => Some((
            "kind='request_unsettled'",
            "manager_v2_request_unsettle_limit",
        )),
        "request_rollover" => Some((
            "kind='request_rollover'",
            "manager_v2_request_rollover_record_limit",
        )),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ManagerAuthorityV2 {
    pub config: HarnessManagerConfigV1,
    pub grant: HarnessManagerPolicyConfigV2,
    pub caller: Uuid,
    pub is_manager: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ManagerRecordV2 {
    pub kind: String,
    pub key: String,
    pub epic_id: Option<Uuid>,
    pub row_version: i64,
    pub payload: Value,
    pub archived: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl Store {
    /// Additional authority for the current manager only. Call after the
    /// historical agent scope denies, so existing callers keep their result.
    pub(crate) fn manager_session_control_scope(
        &self,
        caller: Uuid,
        target: Uuid,
        mutation: bool,
    ) -> Result<Option<super::harness_manager::ManagerSessionScope>> {
        let Some(scope) = self.manager_session_scope(caller, target)? else {
            return Ok(None);
        };
        if !mutation {
            return Ok(Some(scope));
        }
        if !rsi_common::is_leaf_kind(scope.target.session_kind) {
            return Err(refused("manager_v2_leaf_required"));
        }
        let grant = self.get_harness_manager_policy(scope.config.project_id)?;
        let Some(grant) = grant.filter(|grant| {
            !grant.revoked
                && grant
                    .policy
                    .capabilities
                    .contains(&ManagerCapabilityV2::SessionControl)
        }) else {
            return Err(refused("manager_v2_capability_denied"));
        };
        if grant.policy.mode != ManagerOperatingModeV2::Execute
            || grant.policy.paused
            || grant.policy.paused_epic_ids.contains(&scope.epic_id)
        {
            return Err(refused("manager_v2_paused"));
        }
        self.manager_session_control_human_gate(target)?;
        Ok(Some(scope))
    }

    fn manager_session_control_human_gate(&self, target: Uuid) -> Result<()> {
        let held: bool = self.conn.query_row(
            "SELECT pending_question_json IS NOT NULL OR status='WaitingApproval'
             OR EXISTS(SELECT 1 FROM approvals a WHERE a.session_id=?1 AND a.status='Pending'
                 AND NOT EXISTS(SELECT 1 FROM appserver_approval_publications p
                   WHERE p.approval_id=a.id AND (p.closure_state='closed' OR p.state='superseded')))
             OR EXISTS(SELECT 1 FROM appserver_approval_publications
                 WHERE session_id=?1 AND closure_state<>'closed' AND state<>'superseded')
             OR EXISTS(SELECT 1 FROM daemon_settings WHERE key=?2 AND value<>'false')
             FROM sessions WHERE id=?1",
            params![
                target.to_string(),
                format!("manager_operator_pause:{target}")
            ],
            |row| row.get(0),
        )?;
        if held {
            return Err(refused("manager_v2_human_or_recovery_owner"));
        }
        Ok(())
    }

    pub(crate) fn audit_manager_session_control(
        &self,
        scope: &super::harness_manager::ManagerSessionScope,
        verb: &str,
        phase: &str,
    ) -> Result<()> {
        self.manager_v2_event(
            &scope.config,
            scope.config.current_session_id,
            "session_control",
            &scope.target.id.to_string(),
            1,
            &serde_json::json!({"verb":verb,"phase":phase,"target_session_id":scope.target.id,"epic_id":scope.epic_id}),
        )?;
        Ok(())
    }

    pub(crate) fn get_harness_manager_policy(
        &self,
        project: Uuid,
    ) -> Result<Option<HarnessManagerPolicyConfigV2>> {
        let raw: Option<(String,i64,i64,String,String)> = self.conn.query_row(
            "SELECT manager_session_id,scope_version,row_version,policy_json,updated_at FROM harness_manager_v2_policies WHERE project_id=?1",
            [project.to_string()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
        ).optional()?;
        let Some((anchor, scope, row_version, json, updated)) = raw else {
            return Ok(None);
        };
        let anchor =
            Uuid::parse_str(&anchor).map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
        let config = self.get_harness_manager_notice_config(project)?;
        let revoked = config
            .as_ref()
            .is_none_or(|c| c.manager_session_id != anchor || c.row_version != scope);
        Ok(Some(HarnessManagerPolicyConfigV2 {
            project_id: project,
            manager_session_id: anchor,
            scope_version: scope,
            row_version,
            policy: serde_json::from_str(&json)?,
            updated_at: super::parse_timestamp(&updated)
                .map_err(|_| refused("manager_v2_invalid_stored_timestamp"))?,
            revoked,
        }))
    }

    pub(crate) fn configure_harness_manager_policy(
        &self,
        request: &ConfigureHarnessManagerPolicyRequestV2,
    ) -> Result<HarnessManagerPolicyConfigV2> {
        request.policy.validate().map_err(refused)?;
        text(&request.idempotency_key, 128).map_err(refused)?;
        if request.project_id.is_nil()
            || request.expected_scope_version <= 0
            || request.expected_policy_version < 0
        {
            return Err(refused("manager_v2_invalid_policy_request"));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let config = self
            .get_harness_manager(request.project_id)?
            .ok_or_else(|| refused("manager_not_configured"))?;
        if config.row_version != request.expected_scope_version {
            return Err(refused("manager_v2_scope_changed"));
        }
        let payload = serde_json::to_value(request)?;
        if let Some(receipt) =
            self.manager_v2_replay(&config, &request.idempotency_key, &payload)?
        {
            let mut result: HarnessManagerPolicyConfigV2 = serde_json::from_value(receipt)?;
            result.revoked = config.row_version != result.scope_version;
            tx.commit()?;
            return Ok(result);
        }
        for group_id in &request.policy.group_ids {
            let group = self
                .get_session(*group_id)?
                .ok_or_else(|| refused("manager_v2_group_out_of_scope"))?;
            if group.project_id != Some(config.project_id)
                || group.parent_id.is_some()
                || group.session_kind != SessionKind::Group
                || matches!(
                    group.status,
                    SessionStatus::Archived | SessionStatus::Deleted
                )
            {
                return Err(refused("manager_v2_group_out_of_scope"));
            }
        }
        if request
            .policy
            .paused_epic_ids
            .iter()
            .any(|id| !config.epic_ids.contains(id))
        {
            return Err(refused("manager_v2_epic_out_of_scope"));
        }
        let previous = self.get_harness_manager_policy(config.project_id)?;
        let actual = previous.as_ref().map_or(0, |p| p.row_version);
        if actual != request.expected_policy_version {
            return Err(refused("manager_v2_policy_changed"));
        }
        let updated_at = Utc::now();
        let next = actual
            .checked_add(1)
            .ok_or_else(|| refused("manager_v2_version_exhausted"))?;
        self.conn.execute("INSERT INTO harness_manager_v2_policies(project_id,manager_session_id,scope_version,row_version,policy_json,updated_at)
            VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(project_id) DO UPDATE SET manager_session_id=excluded.manager_session_id,
            scope_version=excluded.scope_version,row_version=excluded.row_version,policy_json=excluded.policy_json,updated_at=excluded.updated_at",
            params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,next,
                serde_json::to_string(&request.policy)?,updated_at.to_rfc3339_opts(SecondsFormat::Nanos,true)])?;
        let result = HarnessManagerPolicyConfigV2 {
            project_id: config.project_id,
            manager_session_id: config.manager_session_id,
            scope_version: config.row_version,
            row_version: next,
            policy: request.policy.clone(),
            updated_at,
            revoked: false,
        };
        self.manager_v2_event(
            &config,
            None,
            "policy",
            "policy",
            next,
            &serde_json::to_value(&result)?,
        )?;
        self.manager_v2_save_receipt(
            &config,
            None,
            next,
            "configure_policy",
            &request.idempotency_key,
            &payload,
            &serde_json::to_value(&result)?,
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// Current scope/lineage is always resolved before a mutation replay.
    pub(crate) fn manager_v2_authorize(
        &self,
        caller: Uuid,
        fence: &ManagerFenceV2,
        capability: Option<ManagerCapabilityV2>,
    ) -> Result<ManagerAuthorityV2> {
        fence.validate().map_err(refused)?;
        let (config, is_manager) = self.manager_config_for_caller(caller)?;
        if config.row_version != fence.scope_version {
            return Err(refused("manager_v2_scope_changed"));
        }
        let grant = self
            .get_harness_manager_policy(config.project_id)?
            .ok_or_else(|| refused("manager_v2_grant_required"))?;
        if grant.revoked || grant.row_version != fence.policy_version {
            return Err(refused("manager_v2_policy_changed"));
        }
        if capability.is_some_and(|c| !is_manager || !grant.policy.capabilities.contains(&c)) {
            return Err(refused("manager_v2_capability_denied"));
        }
        Ok(ManagerAuthorityV2 {
            config,
            grant,
            caller,
            is_manager,
        })
    }

    pub(crate) fn manager_v2_require_epic(
        &self,
        authority: &ManagerAuthorityV2,
        epic: Uuid,
    ) -> Result<Session> {
        if !authority.config.epic_ids.contains(&epic) {
            return Err(refused("manager_v2_epic_out_of_scope"));
        }
        if !authority.is_manager
            && self.manager_lead(authority.config.project_id, epic)?.id != authority.caller
        {
            return Err(refused("manager_v2_epic_out_of_scope"));
        }
        self.manager_epic(authority.config.project_id, epic)
    }

    pub(crate) fn manager_v2_require_container(
        &self,
        authority: &ManagerAuthorityV2,
        id: Uuid,
        allow_retired: bool,
    ) -> Result<Session> {
        let session = self
            .get_session(id)?
            .ok_or_else(|| refused("manager_v2_container_out_of_scope"))?;
        let owned: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM harness_manager_v2_entities WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND session_id=?4)",
            params![authority.config.project_id.to_string(),authority.config.manager_session_id.to_string(),authority.config.row_version,id.to_string()],|r|r.get(0))?;
        let selected = match session.session_kind {
            SessionKind::Group => {
                authority.config.covers_group(id)
                    || authority.grant.policy.group_ids.contains(&id)
                    || owned
            }
            SessionKind::Epic => authority.config.epic_ids.contains(&id) || owned,
            _ => false,
        };
        if !selected
            || session.project_id != Some(authority.config.project_id)
            || (!allow_retired
                && matches!(
                    session.status,
                    SessionStatus::Archived | SessionStatus::Deleted
                ))
        {
            return Err(refused("manager_v2_container_out_of_scope"));
        }
        let parent = session
            .parent_id
            .map(|p| self.get_session(p))
            .transpose()?
            .flatten();
        if !rsi_common::legal_children(parent.as_ref().map(|p| p.session_kind))
            .contains(&session.session_kind)
            || parent.as_ref().is_some_and(|p| {
                p.project_id != session.project_id
                    || matches!(p.status, SessionStatus::Archived | SessionStatus::Deleted)
            })
        {
            return Err(refused("manager_v2_container_out_of_scope"));
        }
        Ok(session)
    }

    pub(crate) fn manager_v2_record(
        &self,
        config: &HarnessManagerConfigV1,
        kind: &str,
        key: &str,
    ) -> Result<Option<ManagerRecordV2>> {
        // D19: durable facts are identified by the work, never by the seat.
        if super::manager_ledger::is_work_fact(kind) {
            return self.manager_v2_fact(config.project_id, kind, key);
        }
        let raw: Option<(Option<String>,i64,String,bool,String,String)> = self.conn.query_row(
            "SELECT epic_id,row_version,payload_json,archived,created_at,updated_at FROM harness_manager_v2_records
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 AND record_key=?5",
            params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,kind,key],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        raw.map(
            |(epic, row_version, json, archived, created_at, updated_at)| {
                Ok(ManagerRecordV2 {
                    kind: kind.into(),
                    key: key.into(),
                    epic_id: epic
                        .map(|x| Uuid::parse_str(&x))
                        .transpose()
                        .map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
                    row_version,
                    payload: serde_json::from_str(&json)?,
                    archived,
                    created_at,
                    updated_at,
                })
            },
        )
        .transpose()
    }

    pub(crate) fn manager_v2_put_record(
        &self,
        config: &HarnessManagerConfigV1,
        kind: &str,
        key: &str,
        epic: Option<Uuid>,
        expected: i64,
        payload: &Value,
    ) -> Result<ManagerRecordV2> {
        text(key, 256).map_err(refused)?;
        text(kind, 64).map_err(refused)?;
        let json = serde_json::to_string(payload)?;
        if json.len() > 65536 || expected < 0 {
            return Err(refused("manager_v2_invalid_record"));
        }
        if super::manager_ledger::is_work_fact(kind) {
            return self.manager_v2_put_fact(config, kind, key, epic, expected, payload);
        }
        let prior = self.manager_v2_record(config, kind, key)?;
        if prior.as_ref().map_or(0, |r| r.row_version) != expected {
            return Err(refused("manager_v2_record_changed"));
        }
        if prior.is_none() {
            if let Some((predicate, code)) = bookkeeping_class(kind) {
                if kind != "retrieval" && !REQUEST_MARKER_KINDS.contains(&kind) {
                    self.manager_v2_retire_bookkeeping(config)?;
                }
                if let Some(limit) = bookkeeping_class_limit(kind) {
                    let count: i64 = self.conn.query_row(
                        &live_class_count_sql(predicate),
                        params![
                            config.project_id.to_string(),
                            config.manager_session_id.to_string(),
                            config.row_version
                        ],
                        |r| r.get(0),
                    )?;
                    if count >= limit {
                        return Err(refused(code));
                    }
                }
            } else if !super::manager_decision_history::native_history(kind, key, payload) {
                let count: i64 = self.conn.query_row(
                    COORDINATION_BUDGET_COUNT_SQL,
                    params![
                        config.project_id.to_string(),
                        config.manager_session_id.to_string(),
                        config.row_version
                    ],
                    |r| r.get(0),
                )?;
                if count
                    >= i64::try_from(MANAGER_V2_MAX_RECORDS)
                        .map_err(|_| refused("manager_v2_record_limit"))?
                {
                    return Err(refused("manager_v2_record_limit"));
                }
            }
        }
        let next = expected
            .checked_add(1)
            .ok_or_else(|| refused("manager_v2_version_exhausted"))?;
        let stamp = now();
        self.conn.execute("INSERT INTO harness_manager_v2_records(project_id,manager_session_id,scope_version,kind,record_key,epic_id,row_version,payload_json,created_at,updated_at)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9) ON CONFLICT(project_id,manager_session_id,scope_version,kind,record_key)
            DO UPDATE SET epic_id=excluded.epic_id,row_version=excluded.row_version,payload_json=excluded.payload_json,updated_at=excluded.updated_at",
            params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,kind,key,epic.map(|id|id.to_string()),next,json,stamp])?;
        if kind == "decision"
            && prior
                .as_ref()
                .is_none_or(|old| old.epic_id != epic || old.payload != *payload)
        {
            self.manager_v2_touch_decision_generation(config, epic)?;
            if let Some(old) = prior.as_ref().filter(|old| old.epic_id != epic) {
                self.manager_v2_touch_decision_generation(config, old.epic_id)?;
            }
        }
        self.manager_v2_record(config, kind, key)?
            .ok_or_else(|| refused("manager_v2_record_unavailable"))
    }

    /// Historical rows stay addressable by exact key. Retirement changes only
    /// the live bookkeeping quota, never the spend floor or action evidence.
    pub(crate) fn manager_v2_retire_bookkeeping(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<()> {
        let scope = params![
            config.project_id.to_string(),
            config.manager_session_id.to_string(),
            config.row_version,
            now()
        ];
        self.conn.execute(RETIRE_BOOKKEEPING_SQL, scope)?;
        Ok(())
    }

    pub(crate) fn manager_v2_event(
        &self,
        config: &HarnessManagerConfigV1,
        actor: Option<Uuid>,
        kind: &str,
        key: &str,
        version: i64,
        value: &Value,
    ) -> Result<i64> {
        let json = serde_json::to_string(value)?;
        if json.len() > 65536 {
            return Err(refused("manager_v2_payload_limit"));
        }
        self.conn.execute("INSERT INTO harness_manager_v2_events(project_id,manager_session_id,scope_version,actor_session_id,kind,record_key,row_version,payload_json,created_at)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,
                actor.map(|id|id.to_string()),kind,key,version,json,now()])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub(crate) fn manager_v2_replay(
        &self,
        config: &HarnessManagerConfigV1,
        key: &str,
        payload: &Value,
    ) -> Result<Option<Value>> {
        text(key, 128).map_err(refused)?;
        let old:Option<(String,Option<String>)>=self.conn.query_row("SELECT fingerprint,outcome_json FROM harness_manager_v2_operations
            WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND idempotency_key=?4",
            params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((hash, outcome)) = old {
            if hash != fingerprint(payload)? {
                return Err(refused("manager_v2_idempotency_conflict"));
            }
            let mut result: Value = serde_json::from_str(
                &outcome.ok_or_else(|| refused("manager_v2_operation_pending"))?,
            )?;
            if result.get("deduplicated").is_some() {
                result["deduplicated"] = Value::Bool(true);
            }
            return Ok(Some(result));
        }
        Ok(None)
    }

    pub(crate) fn manager_v2_save_receipt(
        &self,
        config: &HarnessManagerConfigV1,
        actor: Option<Uuid>,
        policy_version: i64,
        kind: &str,
        key: &str,
        payload: &Value,
        receipt: &Value,
    ) -> Result<Uuid> {
        text(key, 128).map_err(refused)?;
        let id = Uuid::new_v4();
        let stamp = now();
        self.conn.execute("INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,scope_version,policy_version,actor_session_id,idempotency_key,fingerprint,kind,payload_json,state,outcome_json,not_before,created_at,updated_at)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'succeeded',?11,?12,?12,?12)",
            params![id.to_string(),config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,policy_version,actor.map(|id|id.to_string()),key,
                fingerprint(payload)?,kind,serde_json::to_string(payload)?,serde_json::to_string(receipt)?,stamp])?;
        Ok(id)
    }

    // ── Slice 2a: integrate action journal ───────────────────────────────

    /// Same kind/context/execution constants as the lifecycle action journal.
    /// Reusing them lets the existing claim, recover, and finish infrastructure
    /// process integrate operations without a parallel system.
    const INTEGRATE_ACTION_KIND: &str = "lifecycle_action";
    const INTEGRATE_CONTEXT_KIND: &str = "lifecycle_context";
    const INTEGRATE_EXECUTION_KIND: &str = "lifecycle_execution";

    /// Narrow acceptance seam (D15). Returns the work-record acceptance if the
    /// work identified by `work_key` has accepted `source_commit`; otherwise
    /// `None`. Backed by the manager-ledger acceptance record fields
    /// `source_accepted` (presence of `acceptance`), `acceptance.source_commit`,
    /// and `acceptance.method`.
    pub(crate) fn manager_v2_accepted_work_source(
        &self,
        config: &HarnessManagerConfigV1,
        work_key: &str,
        source_commit: &str,
    ) -> Result<Option<AcceptedSource>> {
        let Some(record) = self.manager_v2_record(config, "work", work_key)? else {
            return Ok(None);
        };
        let work: super::manager_ledger::WorkRecord = super::manager_ledger::decode(&record)?;
        let Some(acceptance) = &work.acceptance else {
            return Ok(None);
        };
        if acceptance.source_commit != source_commit {
            return Ok(None);
        }
        Ok(Some(AcceptedSource {
            work_key: work.key,
            epic_id: work.epic_id,
            source_commit: acceptance.source_commit.clone(),
            method: acceptance.method.clone(),
        }))
    }

    /// Enqueue an integrate action through the existing journal with the
    /// exact-fence `AgentManagerControl` path. Uses `kind='lifecycle_action'`
    /// so the existing claim, recover, and finish infrastructure processes it.
    /// Enforces "one non-terminal integrate per project and target" inside the
    /// IMMEDIATE transaction.
    pub(crate) fn enqueue_integrate_action(
        &self,
        origin: super::manager_actions::ManagerActionOriginV2,
        request: AgentManagerControlRequestV2,
    ) -> Result<ManagerActionReceiptV2> {
        let IntegrateActionData {
            work_key,
            target_ref,
            source_commit,
        } = extract_integrate_data(&request)?;

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority = self.manager_action_authority(&origin, &request)?;
        // Replay check (idempotency) — must precede the one-per-target guard
        // so an identical retry returns the cached receipt instead of being
        // rejected as a conflicting in-progress operation.
        let payload = serde_json::json!({"origin":origin,"request":request});
        if let Some(replay) =
            self.manager_v2_replay(&authority.config, &request.idempotency_key, &payload)?
        {
            let mut receipt: ManagerActionReceiptV2 = serde_json::from_value(replay)?;
            receipt.refresh_action_metadata(&request.operation);
            return Ok(receipt);
        }
        // RME-S2A-005: Bound total pending integrate operations per project,
        // matching the existing manager_action_admission MAX_PENDING gate.
        let pending: i64 = self.conn.query_row(
            "SELECT count(*) FROM harness_manager_v2_operations
             WHERE project_id=?1 AND kind=?2
             AND state IN ('queued','running','uncertain')",
            params![
                authority.config.project_id.to_string(),
                Self::INTEGRATE_ACTION_KIND
            ],
            |r| r.get(0),
        )?;
        if pending >= 64 {
            return Err(refused("manager_v2_action_queue_full"));
        }
        // Verify the source is accepted for this work key.
        let accepted =
            self.manager_v2_accepted_work_source(&authority.config, &work_key, &source_commit)?;
        if accepted.is_none() {
            return Err(refused("manager_v2_source_acceptance_required"));
        }
        // Enforce one non-terminal integrate per project and target.
        let conflicting: i64 = self.conn.query_row(
            "SELECT count(*) FROM harness_manager_v2_operations
             WHERE project_id=?1 AND kind=?2
             AND state IN ('queued','running','uncertain')
             AND json_extract(payload_json,'$.request.operation.action')='integrate'
             AND json_extract(payload_json,'$.request.operation.target_ref')=?3",
            params![
                authority.config.project_id.to_string(),
                Self::INTEGRATE_ACTION_KIND,
                target_ref,
            ],
            |r| r.get(0),
        )?;
        if conflicting > 0 {
            return Err(refused("manager_v2_integrate_in_progress"));
        }
        let receipt = self.persist_integrate_operation(&authority, origin, request, &payload)?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Insert the durable operation, context record, and event for an integrate
    /// action that has passed admission and guards.
    fn persist_integrate_operation(
        &self,
        authority: &ManagerAuthorityV2,
        origin: super::manager_actions::ManagerActionOriginV2,
        request: AgentManagerControlRequestV2,
        payload: &Value,
    ) -> Result<ManagerActionReceiptV2> {
        use super::manager_actions::ManagerActionOriginV2;
        let operation_id = Uuid::new_v4();
        let receipt = ManagerActionReceiptV2 {
            operation_id,
            state: ManagerActionStateV2::Queued,
            action_kind: request.operation.action_kind(),
            target_type: request.operation.target_type(),
            row_version: 1,
            target_session_id: None,
            outcome: None,
            result: None,
            deduplicated: false,
            operator_result: None,
        };
        let stamp = now();
        let actor = match &origin {
            ManagerActionOriginV2::Agent { caller } => Some(caller.to_string()),
            ManagerActionOriginV2::OperatingIntent { .. } => None,
        };
        self.conn.execute(
            "INSERT INTO harness_manager_v2_operations(
                id,project_id,manager_session_id,scope_version,policy_version,
                actor_session_id,idempotency_key,fingerprint,kind,payload_json,
                state,row_version,target_session_id,outcome_json,not_before,
                created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'queued',1,NULL,?11,?12,?13,?13)",
            params![
                operation_id.to_string(),
                authority.config.project_id.to_string(),
                authority.config.manager_session_id.to_string(),
                authority.config.row_version,
                authority.grant.row_version,
                actor,
                request.idempotency_key,
                fingerprint(payload)?,
                Self::INTEGRATE_ACTION_KIND,
                serde_json::to_string(payload)?,
                serde_json::to_string(&receipt)?,
                stamp,
                stamp,
            ],
        )?;
        let context = super::manager_actions::ManagerActionContextV2 {
            origin,
            request,
            target_session_id: None,
            source: None,
            launch: None,
            manager_pause_version: 0,
        };
        self.manager_v2_put_record(
            &authority.config,
            Self::INTEGRATE_CONTEXT_KIND,
            &operation_id.to_string(),
            None,
            0,
            &serde_json::to_value(&context)?,
        )?;
        self.manager_v2_event(
            &authority.config,
            actor.map(|s| s.parse::<Uuid>().unwrap_or_default()),
            "action_queued",
            &operation_id.to_string(),
            1,
            &serde_json::to_value(&receipt)?,
        )?;
        Ok(receipt)
    }

    /// Runtime gate for integrate actions. Avoids `manager_action_target`
    /// (which has `unreachable!()` for non-lead/non-container actions) by
    /// doing its own claim assertion and authority recheck. When `effect` is
    /// true, marks `effect_started` in the execution record so a crash after
    /// this point becomes `Uncertain` rather than being replayed.
    pub(crate) fn manager_v2_integrate_runtime_gate(
        &self,
        claim: &super::manager_actions::ManagerActionClaimV2,
        effect: bool,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let op = self.manager_action_assert_claim(claim)?;
        let authority = self.manager_action_authority(&op.context.origin, &op.context.request)?;
        if authority.config.project_id != op.project_id
            || authority.config.manager_session_id != op.manager_session_id
            || authority.config.row_version != op.scope_version
        {
            return Err(refused("manager_v2_scope_changed"));
        }
        if authority.grant.revoked
            || authority.grant.row_version != op.context.request.fence.policy_version
        {
            return Err(refused("manager_v2_policy_changed"));
        }
        // Extract work_key, target_ref, expected_tip, source_commit for gates.
        let (work_key, target_ref, expected_tip, source_commit) =
            match &op.context.request.operation {
                ManagerActionV2::Integrate {
                    work_key,
                    target_ref,
                    expected_tip,
                    source_commit,
                } => (
                    work_key.clone(),
                    target_ref.clone(),
                    expected_tip.clone(),
                    source_commit.clone(),
                ),
                _ => return Err(refused("manager_v2_not_integrate_action")),
            };
        // Recheck acceptance (D20): queue-time acceptance alone is insufficient.
        let accepted =
            self.manager_v2_accepted_work_source(&authority.config, &work_key, &source_commit)?;
        let epic = accepted.as_ref().map(|a| a.epic_id);
        if accepted.is_none() {
            if effect {
                return Err(refused("manager_v2_source_acceptance_required"));
            }
        } else {
            // RME-S2A-001: Reproduce the shared stop gates that
            // manager_action_runtime_gate_on applies for lead/container
            // actions. The integrate action has no epic in its variant, so
            // resolve it from the accepted source's work record.
            if let Some(epic) = epic {
                self.manager_v2_decision_gate(&authority.config, epic)?;
                // RME-S2A-001: Check lead pause. manager_action_runtime_gate_on
                // applies this for lead/container actions; integrate must
                // also respect a paused Epic lead. There is no explicit
                // resume for integrate, so any paused lead blocks it.
                let (_, lead_paused) = self.manager_v2_lead_pause(&authority.config, epic)?;
                if lead_paused {
                    return Err(refused("manager_v2_manager_paused"));
                }
            }
            // Policy pause: a paused policy or a paused Epic blocks integrate.
            if authority.grant.policy.paused
                || epic.is_some_and(|e| authority.grant.policy.paused_epic_ids.contains(&e))
            {
                return Err(refused("manager_v2_policy_paused"));
            }
            // Runtime holds: decision and resource holds for project and epic.
            for reason in [
                super::manager_actions::ManagerActionHoldReasonV2::Resources,
                super::manager_actions::ManagerActionHoldReasonV2::Decision,
            ] {
                for epic_scope in [None, epic] {
                    let hold_key =
                        super::manager_actions::manager_action_hold_key(reason, epic_scope);
                    if let Some(record) = self.manager_v2_record(
                        &authority.config,
                        super::manager_actions::MANAGER_ACTION_HOLD_KIND,
                        &hold_key,
                    )? {
                        let hold: super::manager_actions::ManagerActionRuntimeHoldV2 =
                            serde_json::from_value(record.payload)
                                .map_err(|_| refused("manager_v2_runtime_hold_malformed"))?;
                        if hold.blocked {
                            return Err(refused(match reason {
                                super::manager_actions::ManagerActionHoldReasonV2::Resources => {
                                    "manager_v2_resource_hold"
                                }
                                super::manager_actions::ManagerActionHoldReasonV2::Decision => {
                                    "manager_v2_decision_hold"
                                }
                            }));
                        }
                    }
                }
            }
            // RME-S2A-004: Recheck dependency blockers at the effect boundary.
            // The normal manager API can accept work and later add or enable
            // an unsatisfied dependency without clearing that acceptance.
            let blockers = self.manager_v2_dependency_blockers(&authority.config, &work_key)?;
            if !blockers.is_empty() {
                return Err(refused("manager_v2_dependency_blocked"));
            }
        }
        if effect {
            // D20: Recheck acceptance at the effect boundary, inside the same
            // IMMEDIATE transaction that marks effect_started. Queue-time
            // acceptance alone is insufficient: the work record's acceptance
            // may have been removed or superseded between queue and execution.
            if accepted.is_none() {
                return Err(refused("manager_v2_source_acceptance_required"));
            }
            // RME-S2A-002: Store custody evidence (target_ref, expected_tip) in
            // the execution record so crash recovery can reconcile the Git
            // effect against actual refs. The candidate OID is stored by the
            // executor after prepare_candidate via persist_integrate_candidate.
            let key = claim.id().to_string();
            let prior =
                self.manager_v2_record(&authority.config, Self::INTEGRATE_EXECUTION_KIND, &key)?;
            let mut payload = prior
                .as_ref()
                .map_or_else(|| serde_json::json!({}), |r| r.payload.clone());
            payload["effect_started"] = serde_json::json!(true);
            payload["target_ref"] = serde_json::json!(target_ref);
            payload["expected_tip"] = serde_json::json!(expected_tip);
            self.manager_v2_put_record(
                &authority.config,
                Self::INTEGRATE_EXECUTION_KIND,
                &key,
                None,
                prior.map_or(0, |r| r.row_version),
                &payload,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// RME-S2A-002/003: Persist the prepared candidate OID in the execution
    /// record so crash recovery can determine whether publish succeeded.
    pub(crate) fn persist_integrate_candidate(
        &self,
        claim: &super::manager_actions::ManagerActionClaimV2,
        candidate_oid: &str,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let op = self.manager_action_assert_claim(claim)?;
        let authority = self.manager_action_authority(&op.context.origin, &op.context.request)?;
        let key = claim.id().to_string();
        let prior =
            self.manager_v2_record(&authority.config, Self::INTEGRATE_EXECUTION_KIND, &key)?;
        let mut payload = prior
            .as_ref()
            .map_or_else(|| serde_json::json!({}), |r| r.payload.clone());
        payload["candidate_oid"] = serde_json::json!(candidate_oid);
        self.manager_v2_put_record(
            &authority.config,
            Self::INTEGRATE_EXECUTION_KIND,
            &key,
            None,
            prior.map_or(0, |r| r.row_version),
            &payload,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// RME-S2A-002: Verify the target ref still matches expected_tip before
    /// publish. Returns the resolved ref OID, or an error if the target moved.
    pub(crate) fn verify_integrate_custody(
        &self,
        claim: &super::manager_actions::ManagerActionClaimV2,
        resolved_ref: &str,
    ) -> Result<()> {
        let op = self.manager_action_assert_claim(claim)?;
        let key = claim.id().to_string();
        let authority = self.manager_action_authority(&op.context.origin, &op.context.request)?;
        let prior =
            self.manager_v2_record(&authority.config, Self::INTEGRATE_EXECUTION_KIND, &key)?;
        let payload = prior
            .as_ref()
            .map(|r| r.payload.clone())
            .unwrap_or(serde_json::json!({}));
        let expected_tip = payload["expected_tip"]
            .as_str()
            .ok_or_else(|| refused("manager_v2_custody_evidence_missing"))?;
        if resolved_ref != expected_tip {
            return Err(refused("manager_v2_target_changed"));
        }
        Ok(())
    }

    /// RME-S2A-003: Reconcile an uncertain integrate action against actual
    /// refs. If the target ref matches the stored candidate OID, the publish
    /// succeeded; if it still matches expected_tip, it did not; otherwise it
    /// remains genuinely uncertain.
    pub(crate) fn reconcile_integrate_claim(
        &self,
        claim: &super::manager_actions::ManagerActionClaimV2,
        resolved_ref: &str,
    ) -> Result<ManagerActionStateV2> {
        // Read the execution record without asserting the claim is still
        // Running — the action may already be terminal Uncertain from
        // crash recovery. We only need the stored custody evidence.
        let key = claim.id().to_string();
        let op = self
            .manager_action_operation(claim.id())?
            .ok_or_else(|| refused("manager_v2_action_unavailable"))?;
        let config = rsi_common::harness_manager::HarnessManagerConfigV1 {
            project_id: op.project_id,
            manager_session_id: op.manager_session_id,
            current_session_id: None,
            scope_mode: rsi_common::harness_manager::HarnessManagerScopeModeV1::Selected,
            selected_epic_ids: None,
            group_ids: Vec::new(),
            epic_ids: Vec::new(),
            row_version: op.scope_version,
            updated_at: chrono::Utc::now(),
        };
        let prior = self.manager_v2_record(&config, Self::INTEGRATE_EXECUTION_KIND, &key)?;
        let payload = prior
            .as_ref()
            .map(|r| r.payload.clone())
            .unwrap_or(serde_json::json!({}));
        let expected_tip = payload["expected_tip"].as_str().unwrap_or("");
        let candidate_oid = payload["candidate_oid"].as_str().unwrap_or("");
        if !candidate_oid.is_empty() && resolved_ref == candidate_oid {
            Ok(ManagerActionStateV2::Succeeded)
        } else if resolved_ref == expected_tip {
            Ok(ManagerActionStateV2::Failed)
        } else {
            Ok(ManagerActionStateV2::Uncertain)
        }
    }

    /// RME-S2A-003: CAS-settle an uncertain integrate action without
    /// asserting a Running claim with the original boot_id. After a
    /// crash, the row is already terminal Uncertain with the old boot_id;
    /// `finish_manager_action` would reject a claim built with the new
    /// boot_id. This method does a direct CAS from uncertain to the
    /// reconciled state, recording the outcome and event.
    pub(crate) fn settle_integrate_claim(
        &self,
        operation_id: Uuid,
        project_id: Uuid,
        manager_session_id: Uuid,
        scope_version: i64,
        actor: Option<Uuid>,
        state: ManagerActionStateV2,
        outcome: &str,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        text(outcome, 256).map_err(refused)?;
        let row: Option<(i64, String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT row_version, outcome_json, claim_boot_id FROM harness_manager_v2_operations
                 WHERE id=?1 AND state='uncertain'",
                params![operation_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((row_version, _prior_outcome, _old_boot)) = row else {
            // Already settled by a concurrent reconcile pass.
            return Ok(());
        };
        let new_row_version = row_version + 1;
        let receipt = ManagerActionReceiptV2 {
            operation_id,
            state,
            action_kind: ManagerActionKindV2::Integrate,
            target_type: ManagerActionTargetTypeV2::IntegrationTarget,
            row_version: new_row_version,
            target_session_id: None,
            outcome: Some(outcome.into()),
            result: (state == ManagerActionStateV2::Succeeded)
                .then(|| ManagerActionResultV2::SourceIntegrated),
            deduplicated: false,
            operator_result: None,
        };
        let changed = self.conn.execute(
            "UPDATE harness_manager_v2_operations
             SET state=?2, row_version=?3, outcome_json=?4, updated_at=?5
             WHERE id=?1 AND state='uncertain' AND row_version=?6",
            params![
                operation_id.to_string(),
                state_name(state),
                new_row_version,
                serde_json::to_string(&receipt)?,
                now(),
                row_version,
            ],
        )?;
        if changed != 1 {
            // Concurrent settle won; no error.
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO harness_manager_v2_events(
                project_id, manager_session_id, scope_version, actor_session_id,
                kind, record_key, row_version, payload_json, created_at)
             VALUES(?1, ?2, ?3, ?4, 'action_result', ?5, ?6, ?7, ?8)",
            params![
                project_id.to_string(),
                manager_session_id.to_string(),
                scope_version,
                actor.map(|id| id.to_string()),
                operation_id.to_string(),
                new_row_version,
                serde_json::to_string(&receipt)?,
                now(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// Extract typed fields from an `Integrate` action variant.
fn extract_integrate_data(request: &AgentManagerControlRequestV2) -> Result<IntegrateActionData> {
    match &request.operation {
        ManagerActionV2::Integrate {
            work_key,
            target_ref,
            source_commit,
            ..
        } => Ok(IntegrateActionData {
            work_key: work_key.clone(),
            target_ref: target_ref.clone(),
            source_commit: source_commit.clone(),
        }),
        _ => Err(refused("manager_v2_not_integrate_action")),
    }
}

struct IntegrateActionData {
    work_key: String,
    target_ref: String,
    source_commit: String,
}

fn state_name(state: ManagerActionStateV2) -> &'static str {
    match state {
        ManagerActionStateV2::Queued => "queued",
        ManagerActionStateV2::Running => "running",
        ManagerActionStateV2::Succeeded => "succeeded",
        ManagerActionStateV2::Failed => "failed",
        ManagerActionStateV2::Blocked => "blocked",
        ManagerActionStateV2::Uncertain => "uncertain",
        ManagerActionStateV2::Revoked => "revoked",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::agent_verbs::tests::test_session;
    use rsi_common::agent_coordination::{AGENT_PROGRESS_MAX_COHORT, AgentSendMessageRequestV1};
    use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
    use rsi_common::types::Project;
    use std::path::PathBuf;

    fn fixture(store: &Store) -> (Uuid, Uuid, Uuid) {
        let project = Uuid::new_v4();
        let stamp = Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "Manager v2".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: stamp,
                updated_at: stamp,
            })
            .unwrap();
        let mut manager = test_session(Uuid::new_v4(), PathBuf::from("/tmp/manager-v2"));
        manager.session_kind = SessionKind::Standard;
        manager.status = SessionStatus::Completed;
        manager.project_id = Some(project);
        store.insert_session(&manager).unwrap();
        let mut group = manager.clone();
        group.id = Uuid::new_v4();
        group.session_kind = SessionKind::Group;
        store.insert_session(&group).unwrap();
        let mut epic = manager.clone();
        epic.id = Uuid::new_v4();
        epic.session_kind = SessionKind::Epic;
        epic.parent_id = Some(group.id);
        store.insert_session(&epic).unwrap();
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: manager.id,
                epic_ids: Some(vec![epic.id]),
                expected_row_version: 0,
            })
            .unwrap();
        (project, manager.id, epic.id)
    }

    fn grant(project: Uuid) -> ConfigureHarnessManagerPolicyRequestV2 {
        ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: 1,
            expected_policy_version: 0,
            idempotency_key: "grant".into(),
            policy: ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                ..Default::default()
            },
        }
    }

    #[allow(clippy::unwrap_used)]
    fn session_control_fixture(store: &Store) -> (Uuid, Uuid, Uuid, [Uuid; 3]) {
        let (project, manager, epic) = fixture(store);
        let mut lead = store.get_session(manager).unwrap().unwrap();
        lead.id = Uuid::new_v4();
        lead.parent_id = Some(epic);
        lead.status = SessionStatus::Running;
        store.insert_session(&lead).unwrap();
        let mut worker = lead.clone();
        worker.id = Uuid::new_v4();
        store.insert_session(&worker).unwrap();
        let mut nested = worker.clone();
        nested.id = Uuid::new_v4();
        nested.parent_id = Some(worker.id);
        store.insert_session(&nested).unwrap();
        (project, manager, epic, [lead.id, worker.id, nested.id])
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn manager_progress_default_keeps_own_cohort_with_large_scoped_epic() {
        let store = Store::open_in_memory().unwrap();
        let (_, manager, epic, targets) = session_control_fixture(&store);
        let mut row = store.get_session(targets[0]).unwrap().unwrap();
        for _ in 0..=AGENT_PROGRESS_MAX_COHORT {
            row.id = Uuid::new_v4();
            row.parent_id = Some(epic);
            store.insert_session(&row).unwrap();
        }
        let own = store.agent_get_progress_snapshot(manager, &[]).unwrap();
        assert_eq!(own.cohort_size, 0);
        assert_eq!(own.rows.len(), 0);
        let named = store
            .agent_get_progress_snapshot(manager, &[targets[0], targets[2]])
            .unwrap();
        assert_eq!(named.cohort_size, 2);
        assert_eq!(
            named
                .rows
                .iter()
                .map(|row| row.cursor.session_id)
                .collect::<std::collections::BTreeSet<_>>(),
            [targets[0], targets[2]].into_iter().collect()
        );
    }

    #[test]
    #[allow(clippy::unwrap_used, clippy::too_many_lines)]
    fn manager_session_control_reads_and_mutation_gates_cover_lead_worker_and_nested_worker() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic, targets) = session_control_fixture(&store);
        for target in targets {
            assert!(
                store
                    .manager_session_control_scope(manager, target, false)
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                store
                    .manager_session_control_scope(manager, target, true)
                    .unwrap_err()
                    .to_string(),
                "Invalid parameter: manager_v2_capability_denied"
            );
        }
        let rows = store
            .agent_get_progress_snapshot(manager, &targets)
            .unwrap();
        assert_eq!(rows.rows.len(), 3);

        let mut request = grant(project);
        request.policy.capabilities = vec![ManagerCapabilityV2::SessionControl];
        request.policy.mode = ManagerOperatingModeV2::Execute;
        store.configure_harness_manager_policy(&request).unwrap();
        for target in targets {
            assert_eq!(
                store
                    .manager_session_control_scope(manager, target, true)
                    .unwrap()
                    .unwrap()
                    .epic_id,
                epic
            );
        }
        assert_eq!(
            store
                .manager_session_control_scope(manager, epic, true)
                .unwrap_err()
                .to_string(),
            "Invalid parameter: manager_v2_leaf_required"
        );
        assert!(
            store
                .manager_session_control_scope(targets[0], targets[2], false)
                .unwrap()
                .is_none()
        );

        store
            .conn
            .execute(
                "UPDATE sessions SET pending_question_json='{}' WHERE id=?1",
                [targets[0].to_string()],
            )
            .unwrap();
        assert_eq!(
            store
                .manager_session_control_scope(manager, targets[0], true)
                .unwrap_err()
                .to_string(),
            "Invalid parameter: manager_v2_human_or_recovery_owner"
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET pending_question_json=NULL WHERE id=?1",
                [targets[0].to_string()],
            )
            .unwrap();
        store
            .record_manager_operator_pause(targets[1], true)
            .unwrap();
        assert_eq!(
            store
                .manager_session_control_scope(manager, targets[1], true)
                .unwrap_err()
                .to_string(),
            "Invalid parameter: manager_v2_human_or_recovery_owner"
        );
        store
            .record_manager_operator_pause(targets[1], false)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET status='WaitingApproval' WHERE id=?1",
                [targets[2].to_string()],
            )
            .unwrap();
        assert_eq!(
            store
                .manager_session_control_scope(manager, targets[2], true)
                .unwrap_err()
                .to_string(),
            "Invalid parameter: manager_v2_human_or_recovery_owner"
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Running' WHERE id=?1",
                [targets[2].to_string()],
            )
            .unwrap();
        store.conn.execute(
            "INSERT INTO approvals(id,session_id,tool_name,tool_input,status,created_at) VALUES(?1,?2,'tool','{}','Pending',?3)",
            params![Uuid::new_v4().to_string(), targets[2].to_string(), now()],
        ).unwrap();
        assert_eq!(
            store
                .manager_session_control_scope(manager, targets[2], true)
                .unwrap_err()
                .to_string(),
            "Invalid parameter: manager_v2_human_or_recovery_owner"
        );
        store
            .conn
            .execute(
                "UPDATE approvals SET status='Approved' WHERE session_id=?1",
                [targets[2].to_string()],
            )
            .unwrap();

        let mut monitor = request;
        monitor.expected_policy_version = 1;
        monitor.idempotency_key = "monitor".into();
        monitor.policy.mode = ManagerOperatingModeV2::Monitor;
        store.configure_harness_manager_policy(&monitor).unwrap();
        assert_eq!(
            store
                .manager_session_control_scope(manager, targets[0], true)
                .unwrap_err()
                .to_string(),
            "Invalid parameter: manager_v2_paused"
        );

        let mut paused = monitor;
        paused.expected_policy_version = 2;
        paused.idempotency_key = "paused".into();
        paused.policy.mode = ManagerOperatingModeV2::Execute;
        paused.policy.paused = true;
        store.configure_harness_manager_policy(&paused).unwrap();
        assert_eq!(
            store
                .manager_session_control_scope(manager, targets[0], true)
                .unwrap_err()
                .to_string(),
            "Invalid parameter: manager_v2_paused"
        );
        let mut epic_paused = paused;
        epic_paused.expected_policy_version = 3;
        epic_paused.idempotency_key = "epic-paused".into();
        epic_paused.policy.paused = false;
        epic_paused.policy.paused_epic_ids = vec![epic];
        store
            .configure_harness_manager_policy(&epic_paused)
            .unwrap();
        assert_eq!(
            store
                .manager_session_control_scope(manager, targets[0], true)
                .unwrap_err()
                .to_string(),
            "Invalid parameter: manager_v2_paused"
        );
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: vec![],
                project_id: project,
                session_id: manager,
                epic_ids: Some(vec![]),
                expected_row_version: 1,
            })
            .unwrap();
        assert!(
            store
                .manager_session_control_scope(manager, targets[0], false)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .manager_session_control_scope(manager, targets[0], true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn manager_mail_is_accepted_audited_and_dispatchable_without_parent_custody() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic, targets) = session_control_fixture(&store);
        let mut grant = grant(project);
        grant.policy.capabilities = vec![ManagerCapabilityV2::SessionControl];
        grant.policy.mode = ManagerOperatingModeV2::Execute;
        store.configure_harness_manager_policy(&grant).unwrap();
        for (index, target) in targets.into_iter().enumerate() {
            let request = AgentSendMessageRequestV1 {
                target_session_id: target,
                message: format!("manager instruction {index}"),
                idempotency_key: format!("manager-mail-{index}"),
                expires_at: None,
            };
            let receipt = store
                .accept_authorized_agent_message(manager, None, Some(epic), &request)
                .unwrap();
            assert_eq!(receipt.receipt().target_session_id, target);
        }
        let page = store.list_dispatchable_agent_messages(None, 8).unwrap();
        assert_eq!(page.messages.len(), 3);
        assert!(page.messages.iter().all(|row| matches!(
            row.eligibility,
            super::super::agent_coordination::AgentMessageDispatchEligibility::Ready
        )));
        let plan = crate::session::agent_message_dispatcher::plan_dispatch_tick(
            page,
            &std::collections::HashSet::new(),
        );
        assert_eq!(plan.grant_requests.len(), 3);
        for grant in &plan.grant_requests {
            let rendered = grant.render_payload_for_delivery();
            assert!(rendered.contains("manager instruction"));
            assert!(rendered.contains(&manager.to_string()));
        }
        let events: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM harness_manager_v2_events WHERE kind='session_control' AND actor_session_id=?1",
            [manager.to_string()], |row| row.get(0)).unwrap();
        assert_eq!(events, 3);
    }

    #[test]
    fn manager_v2_appointment_requires_an_explicit_capability_grant() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        let fence = ManagerFenceV2 {
            scope_version: 1,
            policy_version: 1,
        };
        assert!(
            store
                .manager_v2_authorize(manager, &fence, Some(ManagerCapabilityV2::WorkPlan))
                .is_err()
        );
        store
            .configure_harness_manager_policy(&grant(project))
            .unwrap();
        let authority = store
            .manager_v2_authorize(manager, &fence, Some(ManagerCapabilityV2::WorkPlan))
            .unwrap();
        assert_eq!(
            store.manager_v2_require_epic(&authority, epic).unwrap().id,
            epic
        );
        assert!(
            store
                .manager_v2_authorize(manager, &fence, Some(ManagerCapabilityV2::LeadControl))
                .is_err()
        );
        assert!(
            store
                .manager_v2_require_epic(&authority, Uuid::new_v4())
                .is_err()
        );
    }

    #[test]
    fn manager_v2_operator_policy_replay_is_exact_and_scope_edits_revoke_grants() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, _epic) = fixture(&store);
        let request = grant(project);
        let first = store.configure_harness_manager_policy(&request).unwrap();
        let replay = store.configure_harness_manager_policy(&request).unwrap();
        assert_eq!(replay.row_version, first.row_version);
        let mut changed = request.clone();
        changed.policy.mode = ManagerOperatingModeV2::Execute;
        assert!(store.configure_harness_manager_policy(&changed).is_err());
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: manager,
                epic_ids: Some(vec![]),
                expected_row_version: 1,
            })
            .unwrap();
        assert!(
            store
                .get_harness_manager_policy(project)
                .unwrap()
                .unwrap()
                .revoked
        );
        assert!(
            store
                .manager_v2_authorize(
                    manager,
                    &ManagerFenceV2 {
                        scope_version: 1,
                        policy_version: 1
                    },
                    None
                )
                .is_err()
        );
        assert!(
            store
                .manager_v2_authorize(
                    manager,
                    &ManagerFenceV2 {
                        scope_version: 2,
                        policy_version: 1
                    },
                    None
                )
                .is_err()
        );
    }

    #[test]
    fn manager_v2_record_and_event_roll_back_together_on_a_failed_transition() {
        let store = Store::open_in_memory().unwrap();
        let (project, _, epic) = fixture(&store);
        let config = store.get_harness_manager(project).unwrap().unwrap();
        {
            let _tx =
                Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate).unwrap();
            store
                .manager_v2_put_record(
                    &config,
                    "work",
                    "feature",
                    Some(epic),
                    0,
                    &serde_json::json!({"title":"Visible feature"}),
                )
                .unwrap();
            store
                .manager_v2_event(
                    &config,
                    None,
                    "work",
                    "feature",
                    1,
                    &serde_json::json!({"state":"pending"}),
                )
                .unwrap();
        }
        assert!(
            store
                .manager_v2_record(&config, "work", "feature")
                .unwrap()
                .is_none()
        );
        let count: i64 = store
            .conn
            .query_row("SELECT count(*) FROM harness_manager_v2_events", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn manager_v2_policy_receipt_and_limits_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manager.db");
        let (project, manager, _) = {
            let store = Store::open(&path).unwrap();
            let f = fixture(&store);
            let mut request = grant(f.0);
            request.policy.max_recovery_attempts = 1;
            store.configure_harness_manager_policy(&request).unwrap();
            f
        };
        let reopened = Store::open(&path).unwrap();
        let config = reopened
            .get_harness_manager_policy(project)
            .unwrap()
            .unwrap();
        assert_eq!(config.policy.max_recovery_attempts, 1);
        assert_eq!(config.manager_session_id, manager);
        let mut replay = grant(project);
        replay.policy.max_recovery_attempts = 1;
        assert_eq!(
            reopened
                .configure_harness_manager_policy(&replay)
                .unwrap()
                .row_version,
            1
        );
    }

    // ── Slice 2a: integrate action journal tests ──────────────────────────

    use super::super::manager_actions::ManagerActionOriginV2;
    use super::super::manager_ledger::{Acceptance, WorkRecord};

    fn integration_grant(project: Uuid) -> ConfigureHarnessManagerPolicyRequestV2 {
        ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: 1,
            expected_policy_version: 0,
            idempotency_key: "grant-integration".into(),
            policy: ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::GitEffect],
                ..Default::default()
            },
        }
    }

    fn work_record(epic: Uuid, key: &str, source_commit: &str) -> WorkRecord {
        WorkRecord {
            key: key.into(),
            epic_id: epic,
            title: format!("Work {key}"),
            kind: ManagerWorkKindV2::Program,
            priority: 1,
            weight: 1,
            required_gates: vec![],
            spec_revision: 0,
            source_session_id: None,
            source_commit: Some(source_commit.into()),
            stages: vec![],
            acceptance: Some(Acceptance {
                source_commit: source_commit.into(),
                spec_revision: 0,
                evidence_digest: "sha256:abc".into(),
                method: "independent_review".into(),
                accepted_at: "2026-09-20T00:00:00Z".into(),
            }),
            integration: None,
            pending_acceptance: None,
        }
    }

    fn integrate_request(
        scope_version: i64,
        policy_version: i64,
        work_key: &str,
        target_ref: &str,
        expected_tip: &str,
        source_commit: &str,
        idempotency_key: &str,
    ) -> AgentManagerControlRequestV2 {
        AgentManagerControlRequestV2 {
            fence: ManagerFenceV2 {
                scope_version,
                policy_version,
            },
            idempotency_key: idempotency_key.into(),
            operation: ManagerActionV2::Integrate {
                work_key: work_key.into(),
                target_ref: target_ref.into(),
                expected_tip: expected_tip.into(),
                source_commit: source_commit.into(),
            },
        }
    }

    fn put_work(store: &Store, config: &HarnessManagerConfigV1, work: &WorkRecord) {
        store
            .manager_v2_put_record(
                config,
                "work",
                &work.key,
                Some(work.epic_id),
                0,
                &serde_json::to_value(work).unwrap(),
            )
            .unwrap();
    }

    fn harness_config(store: &Store, project: Uuid) -> HarnessManagerConfigV1 {
        store.get_harness_manager(project).unwrap().unwrap()
    }

    const FAKE_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const FAKE_SHA2: &str = "fedcba9876543210fedcba9876543210fedcba98";
    const INTEGRATE_TARGET: &str = "refs/heads/rolling";

    #[test]
    fn accepted_source_returns_acceptance_when_source_matches() {
        let store = Store::open_in_memory().unwrap();
        let (project, _manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let accepted = store
            .manager_v2_accepted_work_source(&config, "w1", FAKE_SHA)
            .unwrap();
        assert!(accepted.is_some());
        let accepted = accepted.unwrap();
        assert_eq!(accepted.work_key, "w1");
        assert_eq!(accepted.epic_id, epic);
        assert_eq!(accepted.source_commit, FAKE_SHA);
        assert_eq!(accepted.method, "independent_review");
    }

    #[test]
    fn accepted_source_returns_none_when_no_acceptance() {
        let store = Store::open_in_memory().unwrap();
        let (project, _manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        let mut work = work_record(epic, "w1", FAKE_SHA);
        work.acceptance = None;
        put_work(&store, &config, &work);
        let accepted = store
            .manager_v2_accepted_work_source(&config, "w1", FAKE_SHA)
            .unwrap();
        assert!(accepted.is_none());
    }

    #[test]
    fn accepted_source_returns_none_when_source_mismatch() {
        let store = Store::open_in_memory().unwrap();
        let (project, _manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let accepted = store
            .manager_v2_accepted_work_source(&config, "w1", FAKE_SHA2)
            .unwrap();
        assert!(accepted.is_none());
    }

    #[test]
    fn accepted_source_returns_none_when_work_missing() {
        let store = Store::open_in_memory().unwrap();
        let (project, _manager, _epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        let accepted = store
            .manager_v2_accepted_work_source(&config, "nonexistent", FAKE_SHA)
            .unwrap();
        assert!(accepted.is_none());
    }

    #[test]
    fn enqueue_integrate_rejects_without_integration_capability() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, _epic) = fixture(&store);
        // Grant only WorkPlan, not GitEffect.
        store
            .configure_harness_manager_policy(&grant(project))
            .unwrap();
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let result = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("capability_denied")
        );
    }

    #[test]
    fn enqueue_integrate_rejects_without_accepted_source() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, _epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        // No work record at all.
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let result = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("source_acceptance_required")
        );
    }

    #[test]
    fn enqueue_integrate_succeeds_with_accepted_source() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        assert_eq!(receipt.state, ManagerActionStateV2::Queued);
        assert_eq!(receipt.action_kind, ManagerActionKindV2::Integrate);
        assert_eq!(
            receipt.target_type,
            ManagerActionTargetTypeV2::IntegrationTarget
        );
        assert!(receipt.target_session_id.is_none());
    }

    #[test]
    fn enqueue_integrate_rejects_second_nonterminal_for_same_target() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        // Second enqueue for the same target with a different idempotency key.
        let request2 = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k2");
        let result = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request2);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("integrate_in_progress")
        );
    }

    #[test]
    fn enqueue_integrate_allows_different_targets() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request1 = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request1)
            .unwrap();
        // Different target ref is allowed.
        let request2 =
            integrate_request(1, 1, "w1", "refs/heads/staging", FAKE_SHA, FAKE_SHA, "k2");
        let result = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request2);
        assert!(result.is_ok());
    }

    #[test]
    fn enqueue_integrate_replays_identical_request() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let first = store
            .enqueue_integrate_action(
                ManagerActionOriginV2::Agent { caller: manager },
                request.clone(),
            )
            .unwrap();
        let second = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        assert_eq!(first.operation_id, second.operation_id);
        assert!(second.deduplicated);
    }

    #[test]
    fn enqueue_integrate_rejects_stale_fence() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        // Wrong scope_version.
        let request = integrate_request(99, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let result = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("scope_changed"));
    }

    #[test]
    fn integrate_action_is_claimed_and_finished() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        // The existing claim_manager_action picks up integrate operations
        // because they share kind='lifecycle_action'.
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        assert_eq!(claim.id(), receipt.operation_id);
        assert_eq!(claim.operation.receipt.state, ManagerActionStateV2::Running);
        // Runtime gate (pre-effect).
        store
            .manager_v2_integrate_runtime_gate(&claim, false)
            .unwrap();
        // Runtime gate (effect boundary — marks effect_started).
        store
            .manager_v2_integrate_runtime_gate(&claim, true)
            .unwrap();
        // Verify effect_started was recorded.
        let op = store.manager_action_operation(claim.id()).unwrap().unwrap();
        assert!(op.effect_started);
        // Finish as succeeded.
        let final_receipt = store
            .finish_manager_action(&claim, ManagerActionStateV2::Succeeded, "source_integrated")
            .unwrap();
        assert_eq!(final_receipt.state, ManagerActionStateV2::Succeeded);
        assert_eq!(final_receipt.outcome.as_deref(), Some("source_integrated"));
        assert_eq!(final_receipt.action_kind, ManagerActionKindV2::Integrate);
        for kind in ["lifecycle_context", "lifecycle_execution"] {
            assert!(
                store
                    .manager_v2_record(&config, kind, &claim.id().to_string())
                    .unwrap()
                    .unwrap()
                    .archived
            );
        }
        assert!(
            store
                .manager_action_operation(claim.id())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn integrate_action_recovered_as_uncertain_on_lost_claim() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        // Claim with boot A.
        let boot_a = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_a).unwrap().unwrap();
        assert_eq!(claim.id(), receipt.operation_id);
        // Mark effect_started so recovery classifies as Uncertain.
        store
            .manager_v2_integrate_runtime_gate(&claim, true)
            .unwrap();
        // Simulate a crash: recover abandoned claims (boot B reboots).
        let recovered = store.recover_abandoned_manager_action_claims().unwrap();
        assert_eq!(recovered, 1);
        // The operation should now be Uncertain.
        let op = store.manager_action_operation(claim.id()).unwrap().unwrap();
        assert_eq!(op.receipt.state, ManagerActionStateV2::Uncertain);
        // A new integrate for the same target is still blocked (uncertain is
        // non-terminal).
        let request2 = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k2");
        let result = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request2);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("integrate_in_progress")
        );
    }

    #[test]
    fn integrate_runtime_gate_rejects_revoked_policy() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        assert_eq!(claim.id(), receipt.operation_id);
        // Revoke the policy by reconfiguring with a different policy version.
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: 1,
                expected_policy_version: 1,
                idempotency_key: "revoke".into(),
                policy: ManagerPolicyV2 {
                    capabilities: vec![ManagerCapabilityV2::WorkPlan],
                    ..Default::default()
                },
            })
            .unwrap();
        // Runtime gate should reject due to policy version mismatch.
        let result = store.manager_v2_integrate_runtime_gate(&claim, false);
        assert!(result.is_err());
    }

    #[test]
    fn integrate_effect_gate_rejects_when_acceptance_removed_before_execution() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        // Queue the integrate action (acceptance exists at queue time).
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        // Claim the action.
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        assert_eq!(claim.id(), receipt.operation_id);
        // Pre-effect gate passes (acceptance still exists).
        store
            .manager_v2_integrate_runtime_gate(&claim, false)
            .unwrap();
        // Remove acceptance: replace the work record with one that has no
        // acceptance.
        let mut work = work_record(epic, "w1", FAKE_SHA);
        work.acceptance = None;
        let config = harness_config(&store, project);
        let prior = store
            .manager_v2_record(&config, "work", "w1")
            .unwrap()
            .unwrap();
        store
            .manager_v2_put_record(
                &config,
                "work",
                "w1",
                Some(epic),
                prior.row_version,
                &serde_json::to_value(&work).unwrap(),
            )
            .unwrap();
        // Effect gate must reject: acceptance was removed before execution.
        let result = store.manager_v2_integrate_runtime_gate(&claim, true);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("manager_v2_source_acceptance_required")
        );
        // effect_started must NOT be marked (the transaction failed before
        // marking it).
        let op = store.manager_action_operation(claim.id()).unwrap().unwrap();
        assert!(!op.effect_started);
        // The action is still Running (the gate returned an error but did not
        // finish the action). The reconcile loop would classify it as Blocked.
        assert_eq!(op.receipt.state, ManagerActionStateV2::Running);
    }

    // ── RME-S2A-001: live pause/decision stop gates ──────────────────────

    #[test]
    fn integrate_runtime_gate_rejects_when_policy_paused() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        // First save non-paused policy (row_version 1).
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        // Then save paused policy (row_version 2).
        let mut paused = integration_grant(project);
        paused.idempotency_key = "grant-paused".into();
        paused.expected_policy_version = 1;
        paused.policy.paused = true;
        store.configure_harness_manager_policy(&paused).unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 2, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        assert_eq!(claim.id(), receipt.operation_id);
        // Pre-effect gate must reject: policy is paused.
        let result = store.manager_v2_integrate_runtime_gate(&claim, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("policy_paused"));
    }

    #[test]
    fn integrate_runtime_gate_rejects_when_epic_paused() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        // First save non-paused policy (row_version 1).
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        // Then save with paused Epic (row_version 2).
        let mut paused = integration_grant(project);
        paused.idempotency_key = "grant-epic-paused".into();
        paused.expected_policy_version = 1;
        paused.policy.paused_epic_ids = vec![epic];
        store.configure_harness_manager_policy(&paused).unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 2, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        let result = store.manager_v2_integrate_runtime_gate(&claim, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("policy_paused"));
    }

    #[test]
    fn integrate_runtime_gate_rejects_when_decision_hold_active() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        // Set a decision hold for the project.
        let hold = super::super::manager_actions::ManagerActionRuntimeHoldV2 { blocked: true };
        let hold_key = super::super::manager_actions::manager_action_hold_key(
            super::super::manager_actions::ManagerActionHoldReasonV2::Decision,
            None,
        );
        store
            .manager_v2_put_record(
                &config,
                super::super::manager_actions::MANAGER_ACTION_HOLD_KIND,
                &hold_key,
                None,
                0,
                &serde_json::to_value(&hold).unwrap(),
            )
            .unwrap();
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        let result = store.manager_v2_integrate_runtime_gate(&claim, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("decision_hold"));
    }

    // ── RME-S2A-004: dependency blockers ────────────────────────────────

    #[test]
    fn integrate_runtime_gate_rejects_when_dependency_unsatisfied() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        // Add an unsatisfied dependency: w1 depends on w2 which has no acceptance.
        let dep = super::super::manager_ledger::Dependency {
            work_key: "w1".into(),
            prerequisite: "w2".into(),
            require_integrated: false,
            enabled: true,
        };
        store
            .manager_v2_put_record(
                &config,
                "dependency",
                "w1->w2",
                None,
                0,
                &serde_json::to_value(&dep).unwrap(),
            )
            .unwrap();
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        let result = store.manager_v2_integrate_runtime_gate(&claim, false);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("dependency_blocked")
        );
    }

    // ── RME-S2A-005: bounded pending admission ──────────────────────────

    #[test]
    fn integrate_enqueue_rejects_when_pending_limit_exceeded() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        // Enqueue 64 integrate operations with distinct target refs.
        for i in 0..64 {
            let target = format!("refs/heads/branch-{i}");
            let request =
                integrate_request(1, 1, "w1", &target, FAKE_SHA, FAKE_SHA, &format!("k{i}"));
            store
                .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
                .unwrap();
        }
        // The 65th must be rejected.
        let request = integrate_request(
            1,
            1,
            "w1",
            "refs/heads/branch-64",
            FAKE_SHA,
            FAKE_SHA,
            "k64",
        );
        let result = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("action_queue_full")
        );
    }

    // ── RME-S2A-002/003: custody evidence and crash reconciliation ──────

    #[test]
    fn persist_integrate_candidate_stores_oid_in_execution_record() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        // Mark effect_started (stores target_ref and expected_tip).
        store
            .manager_v2_integrate_runtime_gate(&claim, true)
            .unwrap();
        // Persist candidate OID.
        store
            .persist_integrate_candidate(&claim, FAKE_SHA2)
            .unwrap();
        // Verify the execution record has the candidate OID.
        let op = store.manager_action_operation(claim.id()).unwrap().unwrap();
        let key = claim.id().to_string();
        let exec = store
            .manager_v2_record(&config, "lifecycle_execution", &key)
            .unwrap()
            .unwrap();
        assert_eq!(exec.payload["candidate_oid"], serde_json::json!(FAKE_SHA2));
        assert_eq!(
            exec.payload["target_ref"],
            serde_json::json!(INTEGRATE_TARGET)
        );
        assert_eq!(exec.payload["expected_tip"], serde_json::json!(FAKE_SHA));
    }

    #[test]
    fn reconcile_integrate_claim_succeeds_when_ref_matches_candidate() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        store
            .manager_v2_integrate_runtime_gate(&claim, true)
            .unwrap();
        store
            .persist_integrate_candidate(&claim, FAKE_SHA2)
            .unwrap();
        // Simulate crash: finish as Uncertain.
        store
            .finish_manager_action(&claim, ManagerActionStateV2::Uncertain, "crash")
            .unwrap();
        // Reconcile: ref matches candidate OID → Succeeded.
        let state = store.reconcile_integrate_claim(&claim, FAKE_SHA2).unwrap();
        assert_eq!(state, ManagerActionStateV2::Succeeded);
    }

    #[test]
    fn reconcile_integrate_claim_fails_when_ref_matches_expected_tip() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        store
            .manager_v2_integrate_runtime_gate(&claim, true)
            .unwrap();
        store
            .persist_integrate_candidate(&claim, FAKE_SHA2)
            .unwrap();
        store
            .finish_manager_action(&claim, ManagerActionStateV2::Uncertain, "crash")
            .unwrap();
        // Reconcile: ref still at expected_tip → publish did not happen → Failed.
        let state = store.reconcile_integrate_claim(&claim, FAKE_SHA).unwrap();
        assert_eq!(state, ManagerActionStateV2::Failed);
    }

    #[test]
    fn reconcile_integrate_claim_remains_uncertain_when_ref_is_unknown() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        store
            .manager_v2_integrate_runtime_gate(&claim, true)
            .unwrap();
        store
            .persist_integrate_candidate(&claim, FAKE_SHA2)
            .unwrap();
        store
            .finish_manager_action(&claim, ManagerActionStateV2::Uncertain, "crash")
            .unwrap();
        // Reconcile: ref is neither candidate nor expected_tip → genuinely uncertain.
        let unknown = "11111111111111111111111111111111111111111";
        let state = store.reconcile_integrate_claim(&claim, unknown).unwrap();
        assert_eq!(state, ManagerActionStateV2::Uncertain);
    }

    #[test]
    fn verify_integrate_custody_rejects_when_target_moved() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        store
            .manager_v2_integrate_runtime_gate(&claim, true)
            .unwrap();
        // Custody verification: ref matches expected_tip → OK.
        store.verify_integrate_custody(&claim, FAKE_SHA).unwrap();
        // Custody verification: ref moved → reject.
        let result = store.verify_integrate_custody(&claim, FAKE_SHA2);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("target_changed"));
    }

    #[test]
    fn settle_integrate_claim_cas_from_uncertain_to_succeeded() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        store
            .manager_v2_integrate_runtime_gate(&claim, true)
            .unwrap();
        store
            .persist_integrate_candidate(&claim, FAKE_SHA2)
            .unwrap();
        // Simulate lost-boot crash: finish as Uncertain.
        store
            .finish_manager_action(&claim, ManagerActionStateV2::Uncertain, "crash")
            .unwrap();
        // Verify the row is Uncertain.
        let op = store.manager_action_operation(claim.id()).unwrap().unwrap();
        assert_eq!(op.receipt.state, ManagerActionStateV2::Uncertain);
        // Settle: ref matches candidate OID → CAS to Succeeded.
        store
            .settle_integrate_claim(
                receipt.operation_id,
                project,
                manager,
                1,
                Some(manager),
                ManagerActionStateV2::Succeeded,
                "integrated_crash_reconciled",
            )
            .unwrap();
        let op = store.manager_action_operation(claim.id()).unwrap().unwrap();
        assert_eq!(op.receipt.state, ManagerActionStateV2::Succeeded);
        assert_eq!(
            op.receipt.outcome.as_deref(),
            Some("integrated_crash_reconciled")
        );
        // Verify the singleton is released: a new integrate for the same target can enqueue.
        let request2 = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k2");
        let result = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request2);
        assert!(result.is_ok());
    }

    #[test]
    fn settle_integrate_claim_cas_from_uncertain_to_failed() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        store
            .manager_v2_integrate_runtime_gate(&claim, true)
            .unwrap();
        store
            .persist_integrate_candidate(&claim, FAKE_SHA2)
            .unwrap();
        store
            .finish_manager_action(&claim, ManagerActionStateV2::Uncertain, "crash")
            .unwrap();
        // Settle: ref still at expected_tip → CAS to Failed.
        store
            .settle_integrate_claim(
                receipt.operation_id,
                project,
                manager,
                1,
                Some(manager),
                ManagerActionStateV2::Failed,
                "not_integrated_crash_reconciled",
            )
            .unwrap();
        let op = store.manager_action_operation(claim.id()).unwrap().unwrap();
        assert_eq!(op.receipt.state, ManagerActionStateV2::Failed);
        assert_eq!(
            op.receipt.outcome.as_deref(),
            Some("not_integrated_crash_reconciled")
        );
        // Singleton released: new integrate can enqueue.
        let request2 = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k3");
        assert!(
            store
                .enqueue_integrate_action(
                    ManagerActionOriginV2::Agent { caller: manager },
                    request2
                )
                .is_ok()
        );
    }

    #[test]
    fn settle_integrate_claim_no_ops_on_non_uncertain_row() {
        let store = Store::open_in_memory().unwrap();
        let (project, manager, epic) = fixture(&store);
        store
            .configure_harness_manager_policy(&integration_grant(project))
            .unwrap();
        let config = harness_config(&store, project);
        put_work(&store, &config, &work_record(epic, "w1", FAKE_SHA));
        let request = integrate_request(1, 1, "w1", INTEGRATE_TARGET, FAKE_SHA, FAKE_SHA, "k1");
        let receipt = store
            .enqueue_integrate_action(ManagerActionOriginV2::Agent { caller: manager }, request)
            .unwrap();
        let boot_id = Uuid::new_v4();
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        // Row is Running, not Uncertain. settle must no-op.
        store
            .settle_integrate_claim(
                receipt.operation_id,
                project,
                manager,
                1,
                Some(manager),
                ManagerActionStateV2::Succeeded,
                "should_not_apply",
            )
            .unwrap();
        let op = store.manager_action_operation(claim.id()).unwrap().unwrap();
        // Row is still Running — settle silently no-op'd.
        assert_eq!(op.receipt.state, ManagerActionStateV2::Running);
    }
}
