//! Scoped delivery ledger. Reported stage state is never an acceptance receipt.
use super::{
    Store,
    harness_manager_v2::{ManagerAuthorityV2, ManagerRecordV2, fingerprint, now, refused},
};
use crate::error::Result;
use chrono::{DateTime, Utc};
use rsi_common::{harness_manager::HarnessManagerConfigV1, harness_manager_v2::*};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

mod facts;
mod inspect;
mod mutations;
mod review_source;
mod work_view;
pub(crate) use facts::{FactReach, apply_work_facts_migration, is_work_fact};
#[cfg(test)]
pub(crate) use facts::{V122_CATALOG_OBJECTS, WORK_FACTS_SCHEMA_VERSION};
#[cfg(test)]
mod tests;

pub(crate) const GRAPH_BUDGET: usize = 1024;
pub(crate) const STAGES: [ManagerWorkStageV2; 5] = [
    ManagerWorkStageV2::Planning,
    ManagerWorkStageV2::Implementation,
    ManagerWorkStageV2::Review,
    ManagerWorkStageV2::Verification,
    ManagerWorkStageV2::Integration,
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkRecord {
    pub key: String,
    pub epic_id: Uuid,
    pub title: String,
    pub kind: ManagerWorkKindV2,
    pub priority: u8,
    pub weight: u16,
    pub required_gates: Vec<ManagerWorkStageV2>,
    pub spec_revision: i64,
    pub source_session_id: Option<Uuid>,
    pub source_commit: Option<String>,
    pub stages: Vec<StageRecord>,
    pub acceptance: Option<Acceptance>,
    pub integration: Option<Integration>,
    pub pending_acceptance: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StageRecord {
    pub stage: ManagerWorkStageV2,
    pub state: ManagerStageStateV2,
    pub note: String,
    pub evidence: Option<ManagerEvidenceV2>,
    pub admission: Option<EvidenceAdmission>,
    pub updated_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceAdmission {
    pub source_commit: String,
    pub artifact_commit: String,
    pub artifact_digest: String,
    pub policy_digest: String,
    pub reviewer_session_id: Option<Uuid>,
    pub reviewer_invocation_id: Option<Uuid>,
    pub review_handoff_event_id: Option<i64>,
    pub closure_evidence_id: Option<Uuid>,
    pub observed_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Acceptance {
    pub source_commit: String,
    pub spec_revision: i64,
    pub evidence_digest: String,
    pub method: String,
    pub accepted_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Integration {
    pub source_commit: String,
    pub target_commit: String,
    pub verification: Option<EvidenceAdmission>,
    pub integrated_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Dependency {
    pub work_key: String,
    pub prerequisite: String,
    pub require_integrated: bool,
    pub enabled: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ownership {
    pub work_key: String,
    pub domain: String,
    pub mode: ManagerOwnershipModeV2,
    pub files: Vec<String>,
    pub active: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Migration {
    pub work_key: String,
    pub version: u32,
    pub baseline_commit: String,
    pub inventory_digest: String,
    #[serde(default = "migration_active_default")]
    pub active: bool,
}

const fn migration_active_default() -> bool {
    true
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestRecord {
    pub request_id: Uuid,
    pub state: ManagerRequestStateV2,
    pub message: String,
    pub work_key: Option<String>,
    pub lead_session_id: Uuid,
    #[serde(default)]
    pub execution_evidence: Option<Value>,
    pub updated_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DecisionRecord {
    pub key: String,
    pub epic_id: Uuid,
    pub question: String,
    pub request_id: Option<Uuid>,
    pub work_key: Option<String>,
    pub target_digest: String,
    pub target_row_version: Option<i64>,
    #[serde(default)]
    pub request_row_version: Option<i64>,
    pub status: String,
    #[serde(default)]
    pub answer: Option<String>,
    #[serde(default)]
    pub delivery: Option<Value>,
}

/// Snapshot passed to bounded filesystem observation, never to an agent.
#[derive(Clone)]
pub(crate) struct LedgerObservationContext {
    pub authority: ManagerAuthorityV2,
    pub work: Option<WorkRecord>,
    pub work_version: i64,
    pub source_session: Option<rsi_common::types::Session>,
}
#[derive(Default)]
pub(crate) struct LedgerObservation {
    pub evidence: Option<EvidenceAdmission>,
    pub source_commit: Option<String>,
    pub custody: Vec<(Uuid, Uuid, u64)>,
    pub migration: Option<(String, String)>,
    pub target_commit: Option<String>,
}

pub(crate) fn policy_digest(work: &WorkRecord, head: &str) -> Result<String> {
    policy_digest_projection(&serde_json::to_value(work)?, head)
}
pub(crate) fn policy_digest_projection(work: &Value, head: &str) -> Result<String> {
    fingerprint(
        &json!({"policy":"manager_independent_v2", "epic_id":work["epic_id"],"key":work["key"],
        "revision":work["spec_revision"],"required_gates":work["required_gates"],"source_commit":work["source_commit"],"source_session_id":work["source_session_id"],
        "verified_head":head}),
    )
}
pub(crate) fn integration_policy_digest(work: &WorkRecord, head: &str) -> Result<String> {
    integration_policy_digest_projection(&serde_json::to_value(work)?, head)
}
pub(crate) fn integration_policy_digest_projection(work: &Value, head: &str) -> Result<String> {
    fingerprint(
        &json!({"policy":"manager_combined_v2","work_policy":policy_digest_projection(work,head)?,"original_source":work["source_commit"],"target":head}),
    )
}
pub(crate) fn scope_label(work: &WorkRecord) -> String {
    format!(
        "manager-work:{}:{}:{}",
        work.epic_id, work.key, work.spec_revision
    )
}
pub(crate) fn canonical_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn record_key(parts: &[&str]) -> Result<String> {
    fingerprint(&json!(parts))
}
pub(crate) fn decode<T: serde::de::DeserializeOwned>(record: &ManagerRecordV2) -> Result<T> {
    Ok(serde_json::from_value(record.payload.clone())?)
}

impl Store {
    pub(crate) fn manager_v2_records(
        &self,
        config: &HarnessManagerConfigV1,
        kind: &str,
    ) -> Result<Vec<ManagerRecordV2>> {
        if is_work_fact(kind) {
            return self.manager_v2_facts(config, kind, FactReach::Scoped);
        }
        let mut stmt = self.conn.prepare("SELECT record_key,epic_id,row_version,payload_json,archived,created_at,updated_at FROM harness_manager_v2_records WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 ORDER BY record_key LIMIT ?5")?;
        let raw = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    kind,
                    MANAGER_V2_MAX_RECORDS as i64 + 1
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, bool>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if raw.len() > MANAGER_V2_MAX_RECORDS {
            return Err(refused("manager_v2_record_budget"));
        }
        raw.into_iter()
            .map(
                |(key, epic, version, payload, archived, created, updated)| {
                    Ok(ManagerRecordV2 {
                        kind: kind.into(),
                        key,
                        epic_id: epic
                            .map(|s| Uuid::parse_str(&s))
                            .transpose()
                            .map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
                        row_version: version,
                        payload: serde_json::from_str(&payload)?,
                        archived,
                        created_at: created,
                        updated_at: updated,
                    })
                },
            )
            .collect()
    }
    pub(crate) fn manager_v2_work(
        &self,
        config: &HarnessManagerConfigV1,
        key: &str,
    ) -> Result<(ManagerRecordV2, WorkRecord)> {
        let row = self
            .manager_v2_record(config, "work", key)?
            .ok_or_else(|| refused("manager_v2_work_missing"))?;
        let work = decode(&row)?;
        Ok((row, work))
    }
    pub(crate) fn manager_v2_descendant_epic(
        &self,
        config: &HarnessManagerConfigV1,
        session: Uuid,
    ) -> Result<Uuid> {
        let mut at = session;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            if !seen.insert(at) {
                return Err(refused("manager_v2_hierarchy_cycle"));
            }
            let row = self
                .get_session(at)?
                .ok_or_else(|| refused("manager_v2_session_unavailable"))?;
            if row.project_id != Some(config.project_id) {
                return Err(refused("manager_v2_session_out_of_scope"));
            }
            if row.session_kind == rsi_common::types::SessionKind::Epic {
                if config.epic_ids.contains(&at) {
                    return Ok(at);
                }
                return Err(refused("manager_v2_epic_out_of_scope"));
            }
            at = row
                .parent_id
                .ok_or_else(|| refused("manager_v2_session_out_of_scope"))?;
        }
        Err(refused("manager_v2_hierarchy_budget"))
    }
    pub(crate) fn manager_v2_track_retrieval(
        &self,
        config: &HarnessManagerConfigV1,
        caller: Uuid,
        messages: &[rsi_common::harness_manager::HarnessManagerMessageV1],
    ) -> Result<()> {
        // No implicit grant and no write to immutable v1 messages. Called in inbox transaction.
        if self
            .get_harness_manager_policy(config.project_id)?
            .is_none_or(|p| p.revoked)
        {
            return Ok(());
        }
        // A returned inbox page has no in-flight read after its transaction
        // commits. Historical exact-key receipts remain available for request
        // transitions, inspection, and duplicate-read suppression.
        self.manager_v2_retire_bookkeeping(config)?;
        for message in messages {
            let key = format!("{}:{caller}", message.message_id);
            if self.manager_v2_record(config, "retrieval", &key)?.is_none() {
                let value = json!({"message_id":message.message_id,"request_id":message.request_id,"caller":caller,"retrieved_at":now()});
                self.manager_v2_put_record(
                    config,
                    "retrieval",
                    &key,
                    Some(message.epic_id),
                    0,
                    &value,
                )?;
                self.manager_v2_event(config, Some(caller), "retrieval", &key, 1, &value)?;
                self.conn.execute(
                    "UPDATE harness_manager_v2_records SET archived=1,updated_at=?6
                     WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
                       AND kind='retrieval' AND record_key=?4 AND archived=0 AND row_version=?5",
                    params![
                        config.project_id.to_string(),
                        config.manager_session_id.to_string(),
                        config.row_version,
                        key,
                        1,
                        now()
                    ],
                )?;
            }
        }
        Ok(())
    }
}
