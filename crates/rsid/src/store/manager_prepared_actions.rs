//! V115 storage foundation for daemon-authored manager preparations.
//! Admission and transport APIs deliberately live outside this migration.

use super::harness_manager_v2::{ManagerAuthorityV2, fingerprint, now, refused};
use super::manager_actions::{
    MANAGER_ACTION_HOLD_KIND, ManagerActionAdmissionV2, ManagerActionHoldReasonV2,
    ManagerActionOriginV2, ManagerActionRuntimeHoldV2, action_epic, manager_action_hold_key,
};
use super::{Store, capacity_recovery};
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rsi_common::harness_manager_v2::*;
use rsi_common::types::{Session, SessionStatus};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

const PREPARED_TTL_SECONDS: i64 = 900;
const MAX_ACTIVE_PREPARATIONS: i64 = 128;

// RSI-RELEASED-MIGRATION-BEGIN: v115-prepared-actions-catalog
pub(super) const V114_SOURCE_FINGERPRINT: &str =
    "sha256:bddfb3ed61fdbff0cdf21077c35478a2898c3aa65d0384b095bdce931c8a1293";
pub(super) const V115_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:886c7e01fc28a62168ee33923b823ed5bdd86c355d0d23b913d4824be112e413";

#[cfg(test)]
pub(super) const CATALOG_OBJECTS: [(&str, &str); 7] = [
    ("table", "manager_prepared_actions"),
    ("index", "manager_prepared_actions_scope"),
    ("index", "manager_prepared_actions_expiry"),
    ("trigger", "manager_prepared_actions_insert"),
    ("trigger", "manager_prepared_actions_identity"),
    ("trigger", "manager_prepared_actions_transition"),
    ("trigger", "manager_prepared_actions_no_delete"),
];

const SCHEMA: &str = "
CREATE TABLE manager_prepared_actions (
    id TEXT PRIMARY KEY NOT NULL CHECK(rsi_uuid_is_canonical(id)),
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(project_id)),
    manager_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(manager_session_id)),
    caller_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(caller_session_id)),
    scope_version INTEGER NOT NULL CHECK(scope_version>0),
    policy_version INTEGER NOT NULL CHECK(policy_version>=0),
    action_kind TEXT NOT NULL CHECK(action_kind IN ('resume_lead','pause_lead','retry_lead','replace_lead','create_session','assign_lead')),
    action_json TEXT NOT NULL CHECK(json_valid(action_json) AND json_type(action_json)='object' AND length(CAST(action_json AS BLOB))<=65536),
    resolved_fence_json TEXT NOT NULL CHECK(json_valid(resolved_fence_json) AND json_type(resolved_fence_json)='object' AND length(CAST(resolved_fence_json AS BLOB))<=16384),
    target_digest TEXT NOT NULL CHECK(rsi_sha256_digest_is_canonical(target_digest)),
    snapshot_blockers_json TEXT NOT NULL CHECK(json_valid(snapshot_blockers_json) AND json_type(snapshot_blockers_json)='array' AND length(CAST(snapshot_blockers_json AS BLOB))<=16384),
    state TEXT NOT NULL DEFAULT 'prepared' CHECK(state IN ('prepared','reserved','consumed','expired','revoked')),
    operation_id TEXT UNIQUE REFERENCES harness_manager_v2_operations(id) ON DELETE RESTRICT CHECK(operation_id IS NULL OR rsi_uuid_is_canonical(operation_id)),
    row_version INTEGER NOT NULL DEFAULT 1 CHECK(row_version>0),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    expires_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(expires_at)),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
    consumed_at TEXT CHECK(consumed_at IS NULL OR rsi_rfc3339_nanos_is_canonical(consumed_at)),
    CHECK(caller_session_id=manager_session_id),
    CHECK(expires_at>created_at AND expires_at<=strftime('%Y-%m-%dT%H:%M:%S',substr(created_at,1,19),'+900 seconds') || substr(created_at,20)),
    CHECK(updated_at>=created_at),
    CHECK((state IN ('reserved','consumed') AND operation_id IS NOT NULL) OR (state='prepared' AND operation_id IS NULL) OR state IN ('expired','revoked')),
    CHECK((state='consumed' AND consumed_at IS NOT NULL AND consumed_at=updated_at) OR (state<>'consumed' AND consumed_at IS NULL))
);
CREATE INDEX manager_prepared_actions_scope
    ON manager_prepared_actions(project_id,manager_session_id,scope_version,policy_version,state,id);
CREATE INDEX manager_prepared_actions_expiry
    ON manager_prepared_actions(expires_at,id) WHERE state IN ('prepared','reserved');
CREATE TRIGGER manager_prepared_actions_insert BEFORE INSERT ON manager_prepared_actions
WHEN NEW.state<>'prepared' OR NEW.row_version<>1 OR NEW.updated_at<>NEW.created_at
BEGIN SELECT RAISE(ABORT,'manager_preparation_initial_state'); END;
CREATE TRIGGER manager_prepared_actions_identity BEFORE UPDATE ON manager_prepared_actions
WHEN NEW.id IS NOT OLD.id OR NEW.project_id IS NOT OLD.project_id
  OR NEW.manager_session_id IS NOT OLD.manager_session_id OR NEW.caller_session_id IS NOT OLD.caller_session_id
  OR NEW.scope_version IS NOT OLD.scope_version OR NEW.policy_version IS NOT OLD.policy_version
  OR NEW.action_kind IS NOT OLD.action_kind OR NEW.action_json IS NOT OLD.action_json
  OR NEW.resolved_fence_json IS NOT OLD.resolved_fence_json OR NEW.target_digest IS NOT OLD.target_digest
  OR NEW.snapshot_blockers_json IS NOT OLD.snapshot_blockers_json
  OR NEW.created_at IS NOT OLD.created_at OR NEW.expires_at IS NOT OLD.expires_at
BEGIN SELECT RAISE(ABORT,'manager_preparation_identity_immutable'); END;
CREATE TRIGGER manager_prepared_actions_transition BEFORE UPDATE ON manager_prepared_actions
WHEN NEW.row_version<>OLD.row_version+1 OR NEW.updated_at<OLD.updated_at
  OR NOT ((OLD.state='prepared' AND NEW.state IN ('reserved','expired','revoked'))
       OR (OLD.state='reserved' AND NEW.state IN ('consumed','expired','revoked')))
  OR (NEW.state IN ('reserved','consumed') AND NEW.updated_at>=NEW.expires_at)
  OR (NEW.state='expired' AND NEW.updated_at<NEW.expires_at)
  OR (OLD.operation_id IS NOT NULL AND NEW.operation_id IS NOT OLD.operation_id)
  OR (NEW.state IN ('reserved','consumed') AND NOT EXISTS (
      SELECT 1 FROM harness_manager_v2_operations o WHERE o.id=NEW.operation_id
        AND o.project_id=NEW.project_id AND o.manager_session_id=NEW.manager_session_id
        AND o.actor_session_id=NEW.caller_session_id AND o.scope_version=NEW.scope_version
        AND o.policy_version=NEW.policy_version AND o.kind='lifecycle_action'))
BEGIN SELECT RAISE(ABORT,'manager_preparation_invalid_transition'); END;
CREATE TRIGGER manager_prepared_actions_no_delete BEFORE DELETE ON manager_prepared_actions
BEGIN SELECT RAISE(ABORT,'manager_preparation_retained'); END;
";
// RSI-RELEASED-MIGRATION-END: v115-prepared-actions-catalog

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MigrationFault {
    AfterPreflight,
    AfterCatalog,
    AfterForeignKeys,
    AfterVersion,
}

#[cfg(test)]
thread_local! {
    static FAIL_NEXT: std::cell::Cell<Option<MigrationFault>> = const { std::cell::Cell::new(None) };
}

fn migration_fault(point: MigrationFault) -> Result<()> {
    #[cfg(test)]
    if FAIL_NEXT.with(|slot| slot.get() == Some(point) && slot.take().is_some()) {
        return Err(DaemonError::Store(format!(
            "injected V115 fault: {point:?}"
        )));
    }
    let _ = point;
    Ok(())
}

// RSI-RELEASED-MIGRATION-BEGIN: v115-prepared-actions-driver
impl Store {
    pub(super) fn apply_manager_prepared_actions_v115_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version != 114 {
            return Err(DaemonError::Store(format!(
                "V115 requires exact V114 source, found V{version}"
            )));
        }
        let source = capacity_recovery::v88_full_catalog_fingerprint(&tx)?;
        if source != V114_SOURCE_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V115 requires exact V114 catalog, found {source}"
            )));
        }
        migration_fault(MigrationFault::AfterPreflight)?;
        tx.execute_batch(SCHEMA)?;
        let target = capacity_recovery::v88_full_catalog_fingerprint(&tx)?;
        if target != V115_FULL_CATALOG_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V115 prepared actions catalog mismatch: {target}"
            )));
        }
        migration_fault(MigrationFault::AfterCatalog)?;
        let violations: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if violations != 0 {
            return Err(DaemonError::Store(format!(
                "V115 migration found {violations} foreign-key violation(s)"
            )));
        }
        migration_fault(MigrationFault::AfterForeignKeys)?;
        tx.execute("PRAGMA user_version = 115", [])?;
        migration_fault(MigrationFault::AfterVersion)?;
        tx.commit()?;
        tracing::info!("V115 migration complete: prepared manager action journal installed");
        Ok(())
    }
}
// RSI-RELEASED-MIGRATION-END: v115-prepared-actions-driver

// RSI-RELEASED-MIGRATION-BEGIN: v116-prepared-actions-caller-catalog
pub(super) const V116_FULL_CATALOG_FINGERPRINT: &str =
    "sha256:6feae8c50c70826eb00fe61ef2eb341b0272a7d92dee425cebbcd061198664cf";

const V116_SCHEMA: &str = "
DROP TRIGGER manager_prepared_actions_insert;
DROP TRIGGER manager_prepared_actions_identity;
DROP TRIGGER manager_prepared_actions_transition;
DROP TRIGGER manager_prepared_actions_no_delete;
DROP INDEX manager_prepared_actions_scope;
DROP INDEX manager_prepared_actions_expiry;
ALTER TABLE manager_prepared_actions RENAME TO manager_prepared_actions_v115;
CREATE TABLE manager_prepared_actions (
    id TEXT PRIMARY KEY NOT NULL CHECK(rsi_uuid_is_canonical(id)),
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(project_id)),
    manager_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(manager_session_id)),
    caller_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(caller_session_id)),
    scope_version INTEGER NOT NULL CHECK(scope_version>0),
    policy_version INTEGER NOT NULL CHECK(policy_version>=0),
    action_kind TEXT NOT NULL CHECK(action_kind IN ('resume_lead','pause_lead','retry_lead','replace_lead','create_session','assign_lead')),
    action_json TEXT NOT NULL CHECK(json_valid(action_json) AND json_type(action_json)='object' AND length(CAST(action_json AS BLOB))<=65536),
    resolved_fence_json TEXT NOT NULL CHECK(json_valid(resolved_fence_json) AND json_type(resolved_fence_json)='object' AND length(CAST(resolved_fence_json AS BLOB))<=16384),
    target_digest TEXT NOT NULL CHECK(rsi_sha256_digest_is_canonical(target_digest)),
    snapshot_blockers_json TEXT NOT NULL CHECK(json_valid(snapshot_blockers_json) AND json_type(snapshot_blockers_json)='array' AND length(CAST(snapshot_blockers_json AS BLOB))<=16384),
    state TEXT NOT NULL DEFAULT 'prepared' CHECK(state IN ('prepared','reserved','consumed','expired','revoked')),
    operation_id TEXT UNIQUE REFERENCES harness_manager_v2_operations(id) ON DELETE RESTRICT CHECK(operation_id IS NULL OR rsi_uuid_is_canonical(operation_id)),
    row_version INTEGER NOT NULL DEFAULT 1 CHECK(row_version>0),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
    expires_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(expires_at)),
    updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
    consumed_at TEXT CHECK(consumed_at IS NULL OR rsi_rfc3339_nanos_is_canonical(consumed_at)),
    CHECK(expires_at>created_at AND expires_at<=strftime('%Y-%m-%dT%H:%M:%S',substr(created_at,1,19),'+900 seconds') || substr(created_at,20)),
    CHECK(updated_at>=created_at),
    CHECK((state IN ('reserved','consumed') AND operation_id IS NOT NULL) OR (state='prepared' AND operation_id IS NULL) OR state IN ('expired','revoked')),
    CHECK((state='consumed' AND consumed_at IS NOT NULL AND consumed_at=updated_at) OR (state<>'consumed' AND consumed_at IS NULL))
);
INSERT INTO manager_prepared_actions
    (id,project_id,manager_session_id,caller_session_id,scope_version,policy_version,
     action_kind,action_json,resolved_fence_json,target_digest,snapshot_blockers_json,
     state,operation_id,row_version,created_at,expires_at,updated_at,consumed_at)
SELECT id,project_id,manager_session_id,caller_session_id,scope_version,policy_version,
       action_kind,action_json,resolved_fence_json,target_digest,snapshot_blockers_json,
       state,operation_id,row_version,created_at,expires_at,updated_at,consumed_at
FROM manager_prepared_actions_v115;
DROP TABLE manager_prepared_actions_v115;
CREATE INDEX manager_prepared_actions_scope
    ON manager_prepared_actions(project_id,manager_session_id,scope_version,policy_version,state,id);
CREATE INDEX manager_prepared_actions_expiry
    ON manager_prepared_actions(expires_at,id) WHERE state IN ('prepared','reserved');
CREATE TRIGGER manager_prepared_actions_insert BEFORE INSERT ON manager_prepared_actions
WHEN NEW.state<>'prepared' OR NEW.row_version<>1 OR NEW.updated_at<>NEW.created_at
BEGIN SELECT RAISE(ABORT,'manager_preparation_initial_state'); END;
CREATE TRIGGER manager_prepared_actions_identity BEFORE UPDATE ON manager_prepared_actions
WHEN NEW.id IS NOT OLD.id OR NEW.project_id IS NOT OLD.project_id
  OR NEW.manager_session_id IS NOT OLD.manager_session_id OR NEW.caller_session_id IS NOT OLD.caller_session_id
  OR NEW.scope_version IS NOT OLD.scope_version OR NEW.policy_version IS NOT OLD.policy_version
  OR NEW.action_kind IS NOT OLD.action_kind OR NEW.action_json IS NOT OLD.action_json
  OR NEW.resolved_fence_json IS NOT OLD.resolved_fence_json OR NEW.target_digest IS NOT OLD.target_digest
  OR NEW.snapshot_blockers_json IS NOT OLD.snapshot_blockers_json
  OR NEW.created_at IS NOT OLD.created_at OR NEW.expires_at IS NOT OLD.expires_at
BEGIN SELECT RAISE(ABORT,'manager_preparation_identity_immutable'); END;
CREATE TRIGGER manager_prepared_actions_transition BEFORE UPDATE ON manager_prepared_actions
WHEN NEW.row_version<>OLD.row_version+1 OR NEW.updated_at<OLD.updated_at
  OR NOT ((OLD.state='prepared' AND NEW.state IN ('reserved','expired','revoked'))
       OR (OLD.state='reserved' AND NEW.state IN ('consumed','expired','revoked')))
  OR (NEW.state IN ('reserved','consumed') AND NEW.updated_at>=NEW.expires_at)
  OR (NEW.state='expired' AND NEW.updated_at<NEW.expires_at)
  OR (OLD.operation_id IS NOT NULL AND NEW.operation_id IS NOT OLD.operation_id)
  OR (NEW.state IN ('reserved','consumed') AND NOT EXISTS (
      SELECT 1 FROM harness_manager_v2_operations o WHERE o.id=NEW.operation_id
        AND o.project_id=NEW.project_id AND o.manager_session_id=NEW.manager_session_id
        AND o.actor_session_id=NEW.caller_session_id AND o.scope_version=NEW.scope_version
        AND o.policy_version=NEW.policy_version AND o.kind='lifecycle_action'))
BEGIN SELECT RAISE(ABORT,'manager_preparation_invalid_transition'); END;
CREATE TRIGGER manager_prepared_actions_no_delete BEFORE DELETE ON manager_prepared_actions
BEGIN SELECT RAISE(ABORT,'manager_preparation_retained'); END;
";
// RSI-RELEASED-MIGRATION-END: v116-prepared-actions-caller-catalog

// RSI-RELEASED-MIGRATION-BEGIN: v116-prepared-actions-caller-driver
impl Store {
    pub(super) fn apply_manager_prepared_actions_v116_migration(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version != 115 {
            return Err(DaemonError::Store(format!(
                "V116 requires exact V115 source, found V{version}"
            )));
        }
        let source = capacity_recovery::v88_full_catalog_fingerprint(&tx)?;
        if source != V115_FULL_CATALOG_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V116 requires exact V115 catalog, found {source}"
            )));
        }
        tx.execute_batch(V116_SCHEMA)?;
        let target = capacity_recovery::v88_full_catalog_fingerprint(&tx)?;
        if target != V116_FULL_CATALOG_FINGERPRINT {
            return Err(DaemonError::Store(format!(
                "V116 prepared action caller catalog mismatch: {target}"
            )));
        }
        let violations: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if violations != 0 {
            return Err(DaemonError::Store(format!(
                "V116 migration found {violations} foreign-key violation(s)"
            )));
        }
        tx.execute("PRAGMA user_version = 116", [])?;
        tx.commit()?;
        tracing::info!("V116 migration complete: prepared actions bind the live manager caller");
        Ok(())
    }
}
// RSI-RELEASED-MIGRATION-END: v116-prepared-actions-caller-driver

#[cfg(test)]
pub(super) fn rewind_prepared_actions_fixture_to_v115(connection: &rusqlite::Connection) {
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM manager_prepared_actions", [], |row| {
            row.get(0)
        })
        .expect("count V116 prepared-action fixture rows");
    assert_eq!(
        rows, 0,
        "V116 rewind fixture must not discard prepared rows"
    );
    connection
        .execute_batch("DROP TABLE manager_prepared_actions;")
        .expect("drop empty V116 prepared-action fixture");
    connection
        .execute_batch(SCHEMA)
        .expect("recreate exact V115 prepared-action catalog");
    connection
        .execute_batch("PRAGMA user_version=115;")
        .expect("restore V115 fixture version");
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedResolvedFenceV2 {
    manager: ManagerFenceV2,
    lead: Option<ManagerLeadFenceV2>,
}

#[derive(Debug)]
struct PreparedActionRowV2 {
    project_id: Uuid,
    manager_session_id: Uuid,
    caller_session_id: Uuid,
    scope_version: i64,
    policy_version: i64,
    action_kind: String,
    action: PreparedManagerActionV2,
    resolved: PreparedResolvedFenceV2,
    target_digest: String,
    state: String,
    operation_id: Option<Uuid>,
    expires_at: DateTime<Utc>,
}

fn parse_uuid(value: String) -> Result<Uuid> {
    Uuid::parse_str(&value).map_err(|_| refused("manager_v2_invalid_stored_identity"))
}

fn parse_optional_uuid(value: Option<String>) -> Result<Option<Uuid>> {
    value.map(parse_uuid).transpose()
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|stamp| stamp.with_timezone(&Utc))
        .map_err(|_| refused("manager_v2_invalid_stored_timestamp"))
}

fn prepared_action_kind(action: &PreparedManagerActionV2) -> &'static str {
    match action {
        PreparedManagerActionV2::ResumeLead { .. } => "resume_lead",
        PreparedManagerActionV2::PauseLead { .. } => "pause_lead",
        PreparedManagerActionV2::RetryLead { .. } => "retry_lead",
        PreparedManagerActionV2::ReplaceLead { .. } => "replace_lead",
        PreparedManagerActionV2::CreateSession { .. } => "create_session",
        PreparedManagerActionV2::AssignLead { .. } => "assign_lead",
    }
}

fn prepared_action_epic(action: &PreparedManagerActionV2) -> Option<Uuid> {
    match action {
        PreparedManagerActionV2::ResumeLead { epic_id, .. }
        | PreparedManagerActionV2::PauseLead { epic_id, .. }
        | PreparedManagerActionV2::RetryLead { epic_id, .. }
        | PreparedManagerActionV2::ReplaceLead { epic_id, .. }
        | PreparedManagerActionV2::AssignLead { epic_id, .. } => Some(*epic_id),
        PreparedManagerActionV2::CreateSession { .. } => None,
    }
}

fn materialize_prepared_action(
    action: &PreparedManagerActionV2,
    lead: Option<ManagerLeadFenceV2>,
) -> Result<ManagerActionV2> {
    let expected = || {
        lead.clone()
            .ok_or_else(|| refused("manager_v2_prepared_fence_missing"))
    };
    Ok(match action {
        PreparedManagerActionV2::ResumeLead { epic_id, message } => ManagerActionV2::ResumeLead {
            epic_id: *epic_id,
            expected: expected()?,
            message: message.clone(),
        },
        PreparedManagerActionV2::PauseLead { epic_id, reason } => ManagerActionV2::PauseLead {
            epic_id: *epic_id,
            expected: expected()?,
            reason: reason.clone(),
        },
        PreparedManagerActionV2::RetryLead {
            epic_id,
            message,
            launch,
        } => ManagerActionV2::RetryLead {
            epic_id: *epic_id,
            expected: expected()?,
            message: message.clone(),
            launch: launch.clone(),
        },
        PreparedManagerActionV2::ReplaceLead {
            epic_id,
            query,
            launch,
        } => ManagerActionV2::ReplaceLead {
            epic_id: *epic_id,
            expected: expected()?,
            query: query.clone(),
            launch: launch.clone(),
        },
        PreparedManagerActionV2::CreateSession {
            parent_id,
            kind,
            query,
            launch,
        } => ManagerActionV2::CreateSession {
            parent_id: *parent_id,
            kind: *kind,
            query: query.clone(),
            launch: launch.clone(),
        },
        PreparedManagerActionV2::AssignLead {
            epic_id,
            session_id,
        } => ManagerActionV2::AssignLead {
            epic_id: *epic_id,
            expected: expected()?,
            session_id: *session_id,
        },
    })
}

fn target_projection(target: Option<&Session>) -> Value {
    target.map_or(Value::Null, |target| {
        json!({
            "id": target.id,
            "project_id": target.project_id,
            "parent_id": target.parent_id,
            "kind": target.session_kind,
            "status": target.status,
            "provider": target.provider,
            "model": target.model,
            "effort": target.effort,
            "lead_session_id": target.lead_session_id,
            "updated_at": target.updated_at,
        })
    })
}

fn admission_digest(
    request: &AgentManagerControlRequestV2,
    admission: &ManagerActionAdmissionV2,
) -> Result<String> {
    fingerprint(&json!({
        "fence": request.fence,
        "operation": request.operation,
        "target": target_projection(admission.target.as_ref()),
        "source": admission.source,
        "launch": admission.launch,
        "delay_seconds": admission.delay_seconds,
    }))
}

fn blocker_for_error(error: &DaemonError) -> Option<ManagerPreparedActionBlockerV2> {
    let DaemonError::InvalidParam(code) = error else {
        return None;
    };
    use ManagerPreparedActionBlockerCodeV2 as Code;
    use ManagerPreparedActionRequiredActionV2 as Required;
    let (code, required_action) = match code.as_str() {
        "manager_v2_pending_operator_decision" | "manager_v2_decision_hold" => (
            Code::PendingOperatorDecision,
            Required::AnswerOperatorDecision,
        ),
        "manager_v2_human_or_recovery_owner" => (
            Code::HumanOrRecoveryOwner,
            Required::ResolveHumanOrRecoveryOwner,
        ),
        "manager_v2_manager_paused" | "manager_v2_policy_paused" => {
            (Code::OperatorPause, Required::ResumeManagerOrPolicy)
        }
        "manager_v2_program_evidence_unknown" => {
            (Code::ProgramEvidence, Required::InspectProgramEvidence)
        }
        "manager_v2_resource_hold"
        | "manager_v2_concurrency_capacity"
        | "manager_v2_provider_capacity"
        | "manager_v2_provider_usage_limit"
        | "manager_v2_spend_exhausted"
        | "manager_v2_spend_unknown" => (Code::CapacityRecovery, Required::WaitOrAdjustCapacity),
        _ => return None,
    };
    Some(ManagerPreparedActionBlockerV2 {
        code,
        required_action,
    })
}

fn capture_blocker(
    result: Result<()>,
    blockers: &mut Vec<ManagerPreparedActionBlockerV2>,
) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let blocker = blocker_for_error(&error).ok_or(error)?;
            if !blockers.contains(&blocker) {
                blockers.push(blocker);
            }
            Ok(())
        }
    }
}

impl Store {
    fn current_prepared_manager_authority(
        &self,
        caller: Uuid,
        capability: ManagerCapabilityV2,
    ) -> Result<ManagerAuthorityV2> {
        let (config, is_manager) = self.manager_config_for_caller(caller)?;
        let grant = self
            .get_harness_manager_policy(config.project_id)?
            .ok_or_else(|| refused("manager_v2_grant_required"))?;
        if grant.revoked || grant.scope_version != config.row_version {
            return Err(refused("manager_v2_policy_changed"));
        }
        if !is_manager || !grant.policy.capabilities.contains(&capability) {
            return Err(refused("manager_v2_capability_denied"));
        }
        Ok(ManagerAuthorityV2 {
            config,
            grant,
            caller,
            is_manager,
        })
    }

    fn prepared_snapshot_blockers(
        &self,
        authority: &ManagerAuthorityV2,
        request: &AgentManagerControlRequestV2,
        admission: &ManagerActionAdmissionV2,
    ) -> Result<Vec<ManagerPreparedActionBlockerV2>> {
        let action = &request.operation;
        let effect_epic = action_epic(action).or_else(|| {
            admission
                .target
                .as_ref()
                .filter(|session| session.session_kind == rsi_common::types::SessionKind::Epic)
                .map(|session| session.id)
        });
        let mut blockers = Vec::new();
        if !matches!(action, ManagerActionV2::PauseLead { .. }) {
            if let Some(epic) = effect_epic {
                capture_blocker(
                    self.manager_v2_decision_gate(&authority.config, epic),
                    &mut blockers,
                )?;
                let (_, paused) = self.manager_v2_lead_pause(&authority.config, epic)?;
                let explicit_resume = matches!(
                    action,
                    ManagerActionV2::ResumeLead { .. }
                        | ManagerActionV2::RetryLead { .. }
                        | ManagerActionV2::ReplaceLead { .. }
                );
                if paused
                    && !explicit_resume
                    && !matches!(action, ManagerActionV2::AssignLead { .. })
                {
                    capture_blocker(Err(refused("manager_v2_manager_paused")), &mut blockers)?;
                }
            }
        }
        for reason in [
            ManagerActionHoldReasonV2::Resources,
            ManagerActionHoldReasonV2::Decision,
        ] {
            if matches!(action, ManagerActionV2::PauseLead { .. })
                || matches!(reason, ManagerActionHoldReasonV2::Resources)
                    && admission.launch.is_none()
            {
                continue;
            }
            for epic in [None, effect_epic] {
                let key = manager_action_hold_key(reason, epic);
                if let Some(record) =
                    self.manager_v2_record(&authority.config, MANAGER_ACTION_HOLD_KIND, &key)?
                {
                    let hold: ManagerActionRuntimeHoldV2 =
                        serde_json::from_value(record.payload)
                            .map_err(|_| refused("manager_v2_runtime_hold_malformed"))?;
                    if hold.blocked {
                        let code = match reason {
                            ManagerActionHoldReasonV2::Resources => "manager_v2_resource_hold",
                            ManagerActionHoldReasonV2::Decision => "manager_v2_decision_hold",
                        };
                        capture_blocker(Err(refused(code)), &mut blockers)?;
                    }
                }
            }
        }
        if !matches!(action, ManagerActionV2::PauseLead { .. }) {
            if authority.grant.policy.paused
                || effect_epic
                    .is_some_and(|epic| authority.grant.policy.paused_epic_ids.contains(&epic))
            {
                capture_blocker(Err(refused("manager_v2_policy_paused")), &mut blockers)?;
            }
            if let Some(target) = admission
                .target
                .as_ref()
                .filter(|session| rsi_common::is_leaf_kind(session.session_kind))
            {
                let allow_interrupted_resume = matches!(action, ManagerActionV2::ResumeLead { .. })
                    && target.status == SessionStatus::Interrupted;
                capture_blocker(
                    self.manager_action_human_gate_with_interrupted_resume(
                        target.id,
                        allow_interrupted_resume,
                    ),
                    &mut blockers,
                )?;
            }
        } else if let Some(target) = admission.target.as_ref() {
            capture_blocker(self.manager_action_human_gate(target.id), &mut blockers)?;
        }
        if let Some(launch) = &admission.launch {
            let existing = admission
                .target
                .as_ref()
                .filter(|_| {
                    matches!(
                        action,
                        ManagerActionV2::ResumeLead { .. }
                            | ManagerActionV2::ReplaceLead { .. }
                            | ManagerActionV2::RetryLead { .. }
                    )
                })
                .map(|session| session.id);
            capture_blocker(
                self.manager_action_resources(authority, launch, existing),
                &mut blockers,
            )?;
        }
        Ok(blockers)
    }

    fn prepared_action_row(&self, prepared_id: Uuid) -> Result<Option<PreparedActionRowV2>> {
        let raw:Option<(String,String,String,i64,i64,String,String,String,String,String,Option<String>,String)>=self.conn.query_row(
            "SELECT project_id,manager_session_id,caller_session_id,scope_version,policy_version,
                    action_kind,action_json,resolved_fence_json,target_digest,state,operation_id,expires_at
             FROM manager_prepared_actions WHERE id=?1",
            [prepared_id.to_string()],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?,row.get(8)?,row.get(9)?,row.get(10)?,row.get(11)?)),
        ).optional()?;
        raw.map(
            |(
                project,
                manager,
                caller,
                scope,
                policy,
                action_kind,
                action,
                resolved,
                digest,
                state,
                operation,
                expires,
            )| {
                Ok(PreparedActionRowV2 {
                    project_id: parse_uuid(project)?,
                    manager_session_id: parse_uuid(manager)?,
                    caller_session_id: parse_uuid(caller)?,
                    scope_version: scope,
                    policy_version: policy,
                    action_kind,
                    action: serde_json::from_str(&action)?,
                    resolved: serde_json::from_str(&resolved)?,
                    target_digest: digest,
                    state,
                    operation_id: parse_optional_uuid(operation)?,
                    expires_at: parse_timestamp(&expires)?,
                })
            },
        )
        .transpose()
    }

    pub(crate) fn prepare_manager_action(
        &self,
        caller: Uuid,
        request: AgentManagerPrepareControlRequestV2,
    ) -> Result<ManagerPreparedActionReceiptV2> {
        request.validate().map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority =
            self.current_prepared_manager_authority(caller, request.operation.capability())?;
        let lead = prepared_action_epic(&request.operation)
            .map(|epic| self.manager_action_lead_fence(epic))
            .transpose()?;
        let resolved = PreparedResolvedFenceV2 {
            manager: ManagerFenceV2 {
                scope_version: authority.config.row_version,
                policy_version: authority.grant.row_version,
            },
            lead,
        };
        let materialized = materialize_prepared_action(&request.operation, resolved.lead.clone())?;
        let full_request = AgentManagerControlRequestV2 {
            fence: resolved.manager.clone(),
            idempotency_key: "prepared-snapshot".into(),
            operation: materialized,
        };
        let origin = ManagerActionOriginV2::Agent { caller };
        let admission =
            self.manager_action_admission(&authority, &origin, &full_request, false, false, None)?;
        let target_digest = admission_digest(&full_request, &admission)?;
        let blockers = self.prepared_snapshot_blockers(&authority, &full_request, &admission)?;
        let created = Utc::now();
        let expires_at = created + Duration::seconds(PREPARED_TTL_SECONDS);
        let created_at = created.to_rfc3339_opts(SecondsFormat::Nanos, true);
        let expires_at_text = expires_at.to_rfc3339_opts(SecondsFormat::Nanos, true);
        self.conn.execute(
            "UPDATE manager_prepared_actions SET state='expired',row_version=row_version+1,updated_at=?1
             WHERE state='prepared' AND expires_at<=?1",
            [&created_at],
        )?;
        let active: i64 = self.conn.query_row(
            "SELECT count(*) FROM manager_prepared_actions
             WHERE project_id=?1 AND manager_session_id=?2 AND state='prepared'",
            params![
                authority.config.project_id.to_string(),
                authority.config.manager_session_id.to_string()
            ],
            |row| row.get(0),
        )?;
        if active >= MAX_ACTIVE_PREPARATIONS {
            return Err(refused("manager_v2_prepared_action_limit"));
        }
        let prepared_id = Uuid::new_v4();
        self.conn.execute(
            "INSERT INTO manager_prepared_actions
             (id,project_id,manager_session_id,caller_session_id,scope_version,policy_version,
              action_kind,action_json,resolved_fence_json,target_digest,snapshot_blockers_json,
              created_at,updated_at,expires_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12,?13)",
            params![
                prepared_id.to_string(),
                authority.config.project_id.to_string(),
                authority.config.manager_session_id.to_string(),
                caller.to_string(),
                authority.config.row_version,
                authority.grant.row_version,
                prepared_action_kind(&request.operation),
                serde_json::to_string(&request.operation)?,
                serde_json::to_string(&resolved)?,
                target_digest,
                serde_json::to_string(&blockers)?,
                created_at,
                expires_at_text,
            ],
        )?;
        let receipt = ManagerPreparedActionReceiptV2 {
            prepared_id,
            target_digest,
            readiness: if blockers.is_empty() {
                ManagerPreparedActionReadinessV2::Ready
            } else {
                ManagerPreparedActionReadinessV2::Blocked
            },
            blockers,
            expires_at,
        };
        tx.commit()?;
        Ok(receipt)
    }

    pub(crate) fn commit_prepared_manager_action(
        &self,
        caller: Uuid,
        request: AgentManagerCommitPreparedControlRequestV2,
    ) -> Result<ManagerPreparedActionCommitResultV2> {
        request.validate().map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let row = self
            .prepared_action_row(request.prepared_id)?
            .ok_or_else(|| refused("manager_v2_prepared_action_unavailable"))?;
        if row.action_kind != prepared_action_kind(&row.action) {
            return Err(refused("manager_v2_prepared_action_malformed"));
        }
        if row.target_digest != request.target_digest {
            return Err(refused("manager_v2_prepared_digest_changed"));
        }
        let (current_config, is_manager) = self.manager_config_for_caller(caller)?;
        if !is_manager
            || current_config.project_id != row.project_id
            || current_config.manager_session_id != row.manager_session_id
        {
            return Err(refused("manager_v2_prepared_action_unavailable"));
        }
        if row.state == "consumed" {
            let operation_id = row
                .operation_id
                .ok_or_else(|| refused("manager_v2_prepared_action_malformed"))?;
            let stored_key: String = self.conn.query_row(
                "SELECT idempotency_key FROM harness_manager_v2_operations WHERE id=?1",
                [operation_id.to_string()],
                |record| record.get(0),
            )?;
            if stored_key != request.idempotency_key {
                return Err(refused("manager_v2_prepared_commit_conflict"));
            }
            let mut receipt = self
                .manager_action_operation(operation_id)?
                .ok_or_else(|| refused("manager_v2_action_unavailable"))?
                .receipt;
            receipt.deduplicated = true;
            tx.commit()?;
            return Ok(ManagerPreparedActionCommitResultV2::Queued { receipt });
        }
        if row.state != "prepared" {
            return Err(refused("manager_v2_prepared_action_unavailable"));
        }
        let current = Utc::now();
        if current >= row.expires_at {
            let stamp = current.to_rfc3339_opts(SecondsFormat::Nanos, true);
            self.conn.execute(
                "UPDATE manager_prepared_actions SET state='expired',row_version=row_version+1,updated_at=?2
                 WHERE id=?1 AND state='prepared'",
                params![request.prepared_id.to_string(), stamp],
            )?;
            tx.commit()?;
            return Err(refused("manager_v2_prepared_action_expired"));
        }
        let authority = self.current_prepared_manager_authority(caller, row.action.capability())?;
        if authority.config.row_version != row.scope_version
            || authority.grant.row_version != row.policy_version
            || row.resolved.manager.scope_version != row.scope_version
            || row.resolved.manager.policy_version != row.policy_version
            || row.caller_session_id != caller
        {
            return Err(refused("manager_v2_prepared_authority_changed"));
        }
        let operation = materialize_prepared_action(&row.action, row.resolved.lead.clone())?;
        let full_request = AgentManagerControlRequestV2 {
            fence: row.resolved.manager,
            idempotency_key: request.idempotency_key,
            operation,
        };
        let origin = ManagerActionOriginV2::Agent { caller };
        let admission =
            self.manager_action_admission(&authority, &origin, &full_request, false, false, None)?;
        if admission_digest(&full_request, &admission)? != row.target_digest {
            return Err(refused("manager_v2_prepared_target_changed"));
        }
        let blockers = self.prepared_snapshot_blockers(&authority, &full_request, &admission)?;
        if !blockers.is_empty() {
            tx.commit()?;
            return Ok(ManagerPreparedActionCommitResultV2::Blocked {
                prepared_id: request.prepared_id,
                target_digest: row.target_digest,
                blockers,
            });
        }
        let receipt = self.enqueue_manager_action_on(origin, full_request)?;
        let reserved_at = now();
        let changed = self.conn.execute(
            "UPDATE manager_prepared_actions SET state='reserved',operation_id=?2,
             row_version=row_version+1,updated_at=?3 WHERE id=?1 AND state='prepared'",
            params![
                request.prepared_id.to_string(),
                receipt.operation_id.to_string(),
                reserved_at
            ],
        )?;
        if changed != 1 {
            return Err(refused("manager_v2_prepared_action_changed"));
        }
        let consumed_at = now();
        self.conn.execute(
            "UPDATE manager_prepared_actions SET state='consumed',row_version=row_version+1,
             updated_at=?2,consumed_at=?2 WHERE id=?1 AND state='reserved'",
            params![request.prepared_id.to_string(), consumed_at],
        )?;
        tx.commit()?;
        Ok(ManagerPreparedActionCommitResultV2::Queued { receipt })
    }

    pub(crate) fn manager_action_receipt_for_caller(
        &self,
        caller: Uuid,
        request: AgentManagerGetActionRequestV2,
    ) -> Result<ManagerActionReceiptV2> {
        request.validate().map_err(refused)?;
        let (config, is_manager) = self.manager_config_for_caller(caller)?;
        if !is_manager {
            return Err(refused("manager_v2_action_unavailable"));
        }
        let owner: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT project_id,manager_session_id FROM harness_manager_v2_operations
                 WHERE id=?1 AND kind='lifecycle_action'",
                [request.operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if owner.as_ref().is_none_or(|(project, manager)| {
            project != &config.project_id.to_string()
                || manager != &config.manager_session_id.to_string()
        }) {
            return Err(refused("manager_v2_action_unavailable"));
        }
        self.manager_action_operation(request.operation_id)?
            .map(|operation| operation.receipt)
            .ok_or_else(|| refused("manager_v2_action_unavailable"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LATEST_SCHEMA_VERSION;
    use crate::store::tests::{make_test_session, rewind_store_to_schema_version};
    use rusqlite::{Connection, params};
    use uuid::Uuid;

    const V117_FULL_CATALOG_FINGERPRINT: &str =
        "sha256:daf5382ad5f63f8892ec136deb5724f57bc94a4d14b06c89ffd2037652aaf214";

    // RSI-RELEASED-MIGRATION-BEGIN: v118-full-catalog-fingerprint
    const V118_FULL_CATALOG_FINGERPRINT: &str =
        "sha256:391bd93dad3ced9c8663eedc948c39f9fd01f07a81fd34cbafc7e5cb21040e26";
    // RSI-RELEASED-MIGRATION-END: v118-full-catalog-fingerprint

    fn fingerprint(conn: &Connection) -> String {
        let tx = conn.unchecked_transaction().unwrap();
        let result = capacity_recovery::v88_full_catalog_fingerprint(&tx).unwrap();
        tx.commit().unwrap();
        result
    }

    fn version(conn: &Connection) -> i32 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn v115_fresh_upgrade_reopen_and_exact_rewind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prepared.db");
        let store = Store::open(&path).unwrap();
        let session = make_test_session();
        store.insert_session(&session).unwrap();
        rewind_store_to_schema_version(&store.conn, 114);
        store
            .apply_manager_prepared_actions_v115_migration()
            .unwrap();
        assert_eq!(version(&store.conn), 115);
        assert_eq!(fingerprint(&store.conn), V115_FULL_CATALOG_FINGERPRINT);
        rewind_store_to_schema_version(&store.conn, 114);
        assert_eq!(fingerprint(&store.conn), V114_SOURCE_FINGERPRINT);
        drop(store);
        let mut reopened_fingerprint: Option<String> = None;
        for _ in 0..2 {
            let store = Store::open(&path).unwrap();
            let current_fingerprint = fingerprint(&store.conn);
            if let Some(expected_fingerprint) = reopened_fingerprint.as_ref() {
                assert_eq!(&current_fingerprint, expected_fingerprint);
            } else {
                reopened_fingerprint = Some(current_fingerprint);
            }
            assert_eq!(version(&store.conn), LATEST_SCHEMA_VERSION);
            assert_eq!(
                store.get_session(session.id).unwrap().unwrap().title,
                session.title
            );
            assert_eq!(
                store
                    .conn
                    .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                "ok"
            );
            assert_eq!(
                store
                    .conn
                    .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                        .get::<_, i64>(
                        0
                    ))
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn v115_each_atomic_boundary_rolls_back_and_retries() {
        for fault in [
            MigrationFault::AfterPreflight,
            MigrationFault::AfterCatalog,
            MigrationFault::AfterForeignKeys,
            MigrationFault::AfterVersion,
        ] {
            let store = Store::open_in_memory().unwrap();
            let session = make_test_session();
            store.insert_session(&session).unwrap();
            rewind_store_to_schema_version(&store.conn, 114);
            FAIL_NEXT.with(|slot| slot.set(Some(fault)));
            let error = store
                .apply_manager_prepared_actions_v115_migration()
                .unwrap_err();
            assert!(error.to_string().contains("injected V115 fault"), "{error}");
            assert_eq!(version(&store.conn), 114);
            assert_eq!(fingerprint(&store.conn), V114_SOURCE_FINGERPRINT);
            assert_eq!(
                store.get_session(session.id).unwrap().unwrap().title,
                session.title
            );
            store
                .apply_manager_prepared_actions_v115_migration()
                .unwrap();
            assert_eq!(version(&store.conn), 115);
            assert_eq!(fingerprint(&store.conn), V115_FULL_CATALOG_FINGERPRINT);
        }
    }

    #[test]
    fn v115_refuses_wrong_version_and_catalog_drift_without_mutation() {
        for mutation in [
            "PRAGMA user_version=113",
            "CREATE TABLE unexpected_v114_table(id TEXT)",
            "DROP INDEX session_lineage_detachments_predecessor",
            "DROP TRIGGER session_lineage_detachments_v114_no_delete",
        ] {
            let store = Store::open_in_memory().unwrap();
            rewind_store_to_schema_version(&store.conn, 114);
            store.conn.execute_batch(mutation).unwrap();
            let before = fingerprint(&store.conn);
            let before_version = version(&store.conn);
            let error = store
                .apply_manager_prepared_actions_v115_migration()
                .unwrap_err();
            assert!(
                error.to_string().contains("V115 requires exact V114"),
                "{error}"
            );
            assert_eq!(fingerprint(&store.conn), before);
            assert_eq!(version(&store.conn), before_version);
        }
    }

    #[test]
    fn v115_foreign_key_violation_rolls_back_catalog_and_version() {
        let journal = Journal::at_v115();
        journal.insert_operation(1);
        let store = &journal.store;
        rewind_store_to_schema_version(&store.conn, 114);
        store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        store
            .conn
            .execute(
                "UPDATE harness_manager_v2_operations SET actor_session_id=?1 WHERE id=?2",
                params![Uuid::new_v4().to_string(), journal.operation],
            )
            .unwrap();
        store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let error = store
            .apply_manager_prepared_actions_v115_migration()
            .unwrap_err();
        assert!(
            error.to_string().contains("foreign-key violation"),
            "{error}"
        );
        assert_eq!(version(&store.conn), 114);
        assert_eq!(fingerprint(&store.conn), V114_SOURCE_FINGERPRINT);
        store
            .conn
            .execute(
                "UPDATE harness_manager_v2_operations SET actor_session_id=?1 WHERE id=?2",
                params![journal.manager, journal.operation],
            )
            .unwrap();
        store
            .apply_manager_prepared_actions_v115_migration()
            .unwrap();
    }

    #[test]
    fn v116_forward_upgrade_preserves_exact_v115_and_reopens() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("prepared-v116.db");
        let store = Store::open(&path).unwrap();
        rewind_store_to_schema_version(&store.conn, 115);
        assert_eq!(version(&store.conn), 115);
        assert_eq!(fingerprint(&store.conn), V115_FULL_CATALOG_FINGERPRINT);
        store
            .apply_manager_prepared_actions_v116_migration()
            .unwrap();
        assert_eq!(version(&store.conn), 116);
        assert_eq!(fingerprint(&store.conn), V116_FULL_CATALOG_FINGERPRINT);
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(version(&reopened.conn), LATEST_SCHEMA_VERSION);
        // Compare against a fresh store's catalog rather than a fixed,
        // point-in-time head fingerprint constant: the latter goes stale
        // every time a later migration lands past whatever version was head
        // when it was hardcoded (issue #604), and the pinned per-version
        // fingerprints above are release-frozen and must not be repurposed
        // to stand in for a moving head.
        let head_reference = Store::open_in_memory().unwrap();
        assert_eq!(
            fingerprint(&reopened.conn),
            fingerprint(&head_reference.conn),
            "reopening after the V115->V116 forward migration must converge on \
             the exact catalog a fresh Store::open_in_memory() produces at head"
        );
        rewind_store_to_schema_version(&reopened.conn, 117);
        assert_eq!(fingerprint(&reopened.conn), V117_FULL_CATALOG_FINGERPRINT);
        reopened
            .apply_manager_notice_retirement_v118_migration()
            .unwrap();
        assert_eq!(fingerprint(&reopened.conn), V118_FULL_CATALOG_FINGERPRINT);
    }

    #[test]
    fn v116_refuses_wrong_version_or_v115_catalog_drift_without_mutation() {
        for mutation in [
            "PRAGMA user_version=114",
            "CREATE TABLE unexpected_v115_table(id TEXT)",
            "DROP INDEX manager_prepared_actions_scope",
        ] {
            let store = Store::open_in_memory().unwrap();
            rewind_store_to_schema_version(&store.conn, 115);
            store.conn.execute_batch(mutation).unwrap();
            let before = fingerprint(&store.conn);
            let before_version = version(&store.conn);
            let error = store
                .apply_manager_prepared_actions_v116_migration()
                .unwrap_err();
            assert!(
                error.to_string().contains("V116 requires exact V115"),
                "{error}"
            );
            assert_eq!(fingerprint(&store.conn), before);
            assert_eq!(version(&store.conn), before_version);
        }
    }

    struct Journal {
        store: Store,
        project: String,
        manager: String,
        prepared: String,
        operation: String,
    }

    impl Journal {
        fn new() -> Self {
            let store = Store::open_in_memory().unwrap();
            Self::with_store(store)
        }

        fn at_v115() -> Self {
            let store = Store::open_in_memory().unwrap();
            rewind_store_to_schema_version(&store.conn, 115);
            Self::with_store(store)
        }

        fn with_store(store: Store) -> Self {
            let project = Uuid::new_v4().to_string();
            store.conn.execute("INSERT INTO projects(id,name,path,created_at,updated_at) VALUES (?1,'prepared','/prepared',?2,?2)",
                params![project, "2026-09-10T12:00:00.000000000Z"]).unwrap();
            let session = make_test_session();
            store.insert_session(&session).unwrap();
            let result = Self {
                store,
                project,
                manager: session.id.to_string(),
                prepared: Uuid::new_v4().to_string(),
                operation: Uuid::new_v4().to_string(),
            };
            result
                .insert_preparation(&result.prepared, "2026-09-10T12:15:00.123456789Z")
                .unwrap();
            result
        }

        fn insert_preparation(&self, id: &str, expires: &str) -> rusqlite::Result<usize> {
            self.store.conn.execute("INSERT INTO manager_prepared_actions
                (id,project_id,manager_session_id,caller_session_id,scope_version,policy_version,
                 action_kind,action_json,resolved_fence_json,target_digest,snapshot_blockers_json,created_at,updated_at,expires_at)
                 VALUES (?1,?2,?3,?3,1,1,'resume_lead','{}','{}',?4,'[]',?5,?5,?6)",
                 params![id,self.project,self.manager,format!("sha256:{}", "a".repeat(64)),
                         "2026-09-10T12:00:00.123456789Z",expires])
        }

        fn insert_operation(&self, scope: i64) {
            self.store.conn.execute("INSERT INTO harness_manager_v2_operations
                (id,project_id,manager_session_id,actor_session_id,scope_version,policy_version,
                 idempotency_key,fingerprint,kind,payload_json,state,not_before,created_at,updated_at)
                VALUES (?1,?2,?3,?3,?4,1,'commit','digest','lifecycle_action','{}','queued',?5,?5,?5)",
                params![self.operation,self.project,self.manager,scope,"2026-09-10T12:01:00.000000000Z"]).unwrap();
        }

        fn update(&self, changes: &str) -> rusqlite::Result<usize> {
            self.store.conn.execute(
                &format!("UPDATE manager_prepared_actions SET {changes} WHERE id=?1"),
                [&self.prepared],
            )
        }

        fn reserve(&self) -> rusqlite::Result<usize> {
            self.store.conn.execute(
                "UPDATE manager_prepared_actions SET state='reserved', operation_id=?1,
                row_version=2, updated_at='2026-09-10T12:01:00.000000000Z' WHERE id=?2",
                params![self.operation, self.prepared],
            )
        }
    }

    #[test]
    fn v115_journal_retains_identity_and_consumes_one_matching_operation() {
        let journal = Journal::new();
        journal.insert_operation(1);
        assert!(journal.update("state='consumed',row_version=2").is_err());
        for change in [
            "action_json='{} '",
            "resolved_fence_json='{} '",
            "snapshot_blockers_json='[{}]'",
            "scope_version=2",
            "policy_version=2",
            "action_kind='pause_lead'",
            "expires_at='2026-09-10T12:14:00.000000000Z'",
        ] {
            assert!(journal.update(change).is_err(), "{change}");
        }
        journal.reserve().unwrap();
        assert!(journal.reserve().is_err());
        journal.update("state='consumed',row_version=3,updated_at='2026-09-10T12:02:00.000000000Z',consumed_at='2026-09-10T12:02:00.000000000Z'").unwrap();
        let receipt: (String, String, i64) = journal
            .store
            .conn
            .query_row(
                "SELECT state,operation_id,row_version FROM manager_prepared_actions WHERE id=?1",
                [&journal.prepared],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(receipt, ("consumed".into(), journal.operation.clone(), 3));
        assert!(
            journal
                .update("state='prepared',operation_id=NULL,consumed_at=NULL,row_version=4")
                .is_err()
        );
        assert!(
            journal
                .store
                .conn
                .execute(
                    "DELETE FROM manager_prepared_actions WHERE id=?1",
                    [&journal.prepared]
                )
                .is_err()
        );
        let second = Uuid::new_v4().to_string();
        journal
            .insert_preparation(&second, "2026-09-10T12:10:00.000000000Z")
            .unwrap();
        assert!(journal.store.conn.execute("UPDATE manager_prepared_actions SET state='reserved',operation_id=?1,row_version=2,
            updated_at='2026-09-10T12:01:00.000000000Z' WHERE id=?2",params![journal.operation,second]).is_err());
    }

    #[test]
    fn v115_ttl_and_terminal_transitions_are_bounded() {
        let journal = Journal::new();
        for expires in [
            "2026-09-10T12:15:00.123456790Z",
            "2026-09-10T12:00:00.123456789Z",
            "2026-09-10T12:01:00Z",
            "invalid",
        ] {
            assert!(
                journal
                    .insert_preparation(&Uuid::new_v4().to_string(), expires)
                    .is_err(),
                "{expires}"
            );
        }
        journal.insert_operation(1);
        assert!(
            journal
                .update("state='expired',row_version=2,updated_at='2026-09-10T12:01:00.000000000Z'")
                .is_err()
        );
        assert!(journal.store.conn.execute("UPDATE manager_prepared_actions SET state='reserved',operation_id=?1,row_version=2,
            updated_at=expires_at WHERE id=?2",params![journal.operation,journal.prepared]).is_err());
        journal
            .update("state='expired',row_version=2,updated_at=expires_at")
            .unwrap();
        assert!(journal.reserve().is_err());
        let journal = Journal::new();
        journal.update("state='revoked',row_version=2").unwrap();
        assert!(journal.update("state='prepared',row_version=3").is_err());
    }

    #[test]
    fn v115_reservation_rejects_mismatched_authority_and_retains_revoked_receipt() {
        let journal = Journal::new();
        journal.insert_operation(2);
        assert!(journal.reserve().is_err());
        journal
            .store
            .conn
            .execute(
                "UPDATE harness_manager_v2_operations SET scope_version=1 WHERE id=?1",
                [&journal.operation],
            )
            .unwrap();
        journal.reserve().unwrap();
        assert!(
            journal
                .update("state='revoked',operation_id=NULL,row_version=3")
                .is_err()
        );
        journal.update("state='revoked',row_version=3").unwrap();
        let receipt: (String, String) = journal
            .store
            .conn
            .query_row(
                "SELECT state,operation_id FROM manager_prepared_actions WHERE id=?1",
                [&journal.prepared],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(receipt, ("revoked".into(), journal.operation));
    }
    #[test]
    fn v115_journal_validates_new_rows_and_supported_semantic_actions() {
        let journal = Journal::new();
        let columns: Vec<String> = journal
            .store
            .conn
            .prepare("PRAGMA table_info(manager_prepared_actions)")
            .unwrap()
            .query_map([], |r| r.get(1))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let clone_with = |field: &str, value: &str| {
            let expressions = columns
                .iter()
                .map(|column| {
                    if column == field {
                        value.to_string()
                    } else if column == "id" {
                        "?1".to_string()
                    } else {
                        column.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            journal.store.conn.execute(&format!("INSERT INTO manager_prepared_actions SELECT {expressions} FROM manager_prepared_actions WHERE id=?2"),
                params![Uuid::new_v4().to_string(),journal.prepared])
        };
        for (field, value) in [
            ("id", "'INVALID'"),
            ("project_id", "'00000000-0000-0000-0000-000000000001'"),
            (
                "caller_session_id",
                "'00000000-0000-0000-0000-000000000001'",
            ),
            ("scope_version", "0"),
            ("policy_version", "-1"),
            ("action_kind", "'succeed_manager'"),
            ("action_kind", "'create_container'"),
            ("action_json", "'not json'"),
            ("action_json", "'[]'"),
            (
                "action_json",
                "json_object('message',printf('%.*c',65536,'x'))",
            ),
            ("resolved_fence_json", "'null'"),
            ("snapshot_blockers_json", "'{}'"),
            ("target_digest", "'sha256:invalid'"),
            ("created_at", "'2026-09-10T12:00:00Z'"),
            ("state", "'revoked'"),
            ("row_version", "2"),
            ("operation_id", "'00000000-0000-0000-0000-000000000001'"),
            ("consumed_at", "'2026-09-10T12:01:00.000000000Z'"),
        ] {
            assert!(
                clone_with(field, value).is_err(),
                "accepted invalid {field}: {value}"
            );
        }
        for kind in [
            "resume_lead",
            "pause_lead",
            "retry_lead",
            "replace_lead",
            "create_session",
            "assign_lead",
        ] {
            clone_with("action_kind", &format!("'{kind}'")).unwrap();
        }
        let count: i64 = journal
            .store
            .conn
            .query_row(
                "SELECT count(*) FROM manager_prepared_actions WHERE state='prepared'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 7);
    }
}
