//! Persistent operating intent, cohort admission, and exact operator decisions.
//!
//! The lifecycle journal is the sole owner of provider effects. This module
//! records scheduling facts and answer delivery obligations; it never borrows
//! a feature lead's agent identity to manufacture authority.

use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::types::Session;
use rsi_common::types::SessionStatus;
use rusqlite::{Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::Store;
use super::harness_manager_v2::{ManagerRecordV2, bookkeeping_class, fingerprint, refused};
use crate::error::Result;

#[derive(Debug, Clone)]
pub(crate) struct ManagerCohortMemberV2 {
    pub session: Session,
    pub epic_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ManagerDecisionDeliveryV2 {
    pub project_id: Uuid,
    pub manager_session_id: Uuid,
    pub scope_version: i64,
    pub policy_version: i64,
    pub key: String,
    pub decision_key: String,
    #[serde(default)]
    pub epic_id: Option<Uuid>,
    pub target_digest: String,
    pub target: Value,
    pub answer: String,
    pub state: String,
    pub effect_started: bool,
    pub boot_id: Option<Uuid>,
    pub outcome: Option<String>,
}

/// Live keys of one kind in a scope, read from the live-row index so archived
/// history never enters the scanned range.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "shares manager_v2_records_of_kind's reachability in the non-test lib"
    )
)]
pub(super) const LIVE_KIND_KEYS_SQL: &str =
    "SELECT record_key FROM harness_manager_v2_records INDEXED BY harness_manager_v2_live_records
     WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 AND archived=0
     ORDER BY record_key LIMIT 1025";

impl Store {
    /// Keyset paging keeps a large installation from starving later projects.
    pub(crate) fn manager_v2_projects_after(
        &self,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<Uuid>> {
        if !(1..=32).contains(&limit) {
            return Err(refused("manager_v2_page_limit"));
        }
        let mut stmt = self.conn.prepare(
            "SELECT project_id FROM harness_manager_v2_policies
             WHERE project_id > ?1 ORDER BY project_id LIMIT ?2",
        )?;
        let ids = stmt
            .query_map(
                params![after.map(|id| id.to_string()).unwrap_or_default(), limit],
                |r| r.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| {
                Uuid::parse_str(&id).map_err(|_| refused("manager_v2_invalid_stored_identity"))
            })
            .collect()
    }

    pub(crate) fn manager_v2_records_of_kind(
        &self,
        config: &HarnessManagerConfigV1,
        kind: &str,
    ) -> Result<Vec<ManagerRecordV2>> {
        let mut stmt = self.conn.prepare(LIVE_KIND_KEYS_SQL)?;
        let keys = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    kind
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if keys.len() > 1024 {
            return Err(refused(
                bookkeeping_class(kind).map_or("manager_v2_record_limit", |(_, code)| code),
            ));
        }
        keys.into_iter()
            .map(|key| {
                self.manager_v2_record(config, kind, &key)?
                    .ok_or_else(|| refused("manager_v2_record_missing"))
            })
            .collect()
    }

    /// Avoid polling noise in the immutable event stream. Timestamps describing
    /// an observation belong in the projection; only changed meaning is logged.
    pub(crate) fn manager_v2_record_changed(
        &self,
        config: &HarnessManagerConfigV1,
        kind: &str,
        key: &str,
        epic: Option<Uuid>,
        payload: &Value,
    ) -> Result<bool> {
        let old = self.manager_v2_record(config, kind, key)?;
        if old
            .as_ref()
            .is_some_and(|r| r.epic_id == epic && r.payload == *payload)
        {
            return Ok(false);
        }
        let record = self.manager_v2_put_record(
            config,
            kind,
            key,
            epic,
            old.map_or(0, |r| r.row_version),
            payload,
        )?;
        self.manager_v2_event(config, None, kind, key, record.row_version, payload)?;
        Ok(true)
    }

    /// Only an exact, producer-published event is answerable. Legacy and
    /// interrupted publications remain visible unresolved decision gates.
    pub(crate) fn manager_v2_question_target(&self, session_id: Uuid) -> Result<Option<Value>> {
        self.pending_question_target(session_id)
    }

    #[allow(clippy::too_many_lines)] // Bounded terminal selection and the existing decision projection share one transaction.
    pub(crate) fn manager_v2_refresh_question_decisions(
        &self,
        config: &HarnessManagerConfigV1,
    ) -> Result<usize> {
        let cohort = self.manager_v2_cohort(config)?;
        let approval_changed = self.manager_v2_refresh_approval_decisions(config)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut changed = approval_changed;
        let mut sessions = std::collections::BTreeMap::new();
        for member in cohort
            .iter()
            .filter(|m| m.epic_id.is_some() && rsi_common::is_leaf_kind(m.session.session_kind))
        {
            sessions.insert(member.session.id, member.session.clone());
        }
        // Pending decisions survive retirement. Page their exact session IDs
        // instead of reloading every historical descendant into the cohort.
        let mut stmt = tx.prepare(
            "SELECT substr(r.record_key,10) FROM harness_manager_v2_records r
             JOIN sessions s ON s.id=substr(r.record_key,10)
             WHERE r.project_id=?1 AND r.manager_session_id=?2 AND r.scope_version=?3
               AND r.kind='decision' AND r.record_key LIKE 'question:%'
               AND (json_extract(r.payload_json,'$.status') IN ('pending','answer_queued')
                 OR (json_extract(r.payload_json,'$.status')='target_unavailable'
                     AND s.status IN ('Archived','Deleted')))
             ORDER BY r.record_key LIMIT 128",
        )?;
        let pending = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for id in pending {
            let id =
                Uuid::parse_str(&id).map_err(|_| refused("manager_v2_invalid_stored_identity"))?;
            if let Some(session) = self.get_session(id)? {
                sessions.insert(id, session);
            }
        }
        for session in sessions.into_values() {
            let key = format!("question:{}", session.id);
            let live_epic = self
                .manager_v2_live_epic_for_session(config, session.id)
                .ok();
            if live_epic.is_none() {
                if let Some(record) = self.manager_v2_record(config, "decision", &key)? {
                    let mut payload = record.payload;
                    payload["status"] = json!("scope_revoked");
                    changed += usize::from(self.manager_v2_record_changed(
                        config,
                        "decision",
                        &key,
                        record.epic_id,
                        &payload,
                    )?);
                }
                continue;
            }
            if matches!(
                session.status,
                SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Interrupted
            ) && let Some(record) = self.manager_v2_record(config, "decision", &key)?
                && matches!(
                    record.payload["status"].as_str(),
                    Some("pending" | "answer_queued")
                )
            {
                let mut payload = record.payload;
                payload["status"] = json!("target_unavailable");
                payload["next_action"] =
                    json!("Session ended; inspect the exact question before any retry.");
                changed += usize::from(
                    self.manager_v2_record_changed(config, "decision", &key, live_epic, &payload)?,
                );
                continue;
            }
            let target = self.manager_v2_question_target(session.id);
            if target.is_ok() {
                if let Some(record) = self.manager_v2_record(config, "intent", &key)? {
                    if record.payload["state"] == "blocked" {
                        changed += usize::from(self.manager_v2_record_changed(config,"intent",&key,live_epic,&json!({"state":"resolved","reason":"question_projection_current","session_id":session.id}))?);
                    }
                }
            }
            match target {
                Ok(Some(target)) => {
                    let digest = fingerprint(&target)?;
                    if self
                        .manager_v2_record(config, "decision", &key)?
                        .is_some_and(|r| {
                            r.payload["target_digest"] == digest
                                && r.epic_id == live_epic
                                && r.payload["status"] != "scope_revoked"
                        })
                    {
                        continue;
                    }
                    let question = session
                        .pending_question
                        .as_ref()
                        .map(|q| {
                            q.questions
                                .iter()
                                .map(|item| item.question.as_str())
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_else(|| "Pending provider question".into());
                    self.manager_v2_record_changed(
                        config,
                        "decision_target",
                        &key,
                        live_epic,
                        &target,
                    )?;
                    changed += usize::from(self.manager_v2_record_changed(config,"decision",&key,live_epic,&json!({
                        "key":key,"epic_id":live_epic,"question":question,"request_id":null,"work_key":null,
                        "target_digest":digest,"target_row_version":null,"status":"pending","answer":null,"delivery":null
                    }))?);
                }
                Ok(None) => {
                    if let Some(record) = self.manager_v2_record(config, "decision", &key)? {
                        if matches!(
                            record.payload["status"].as_str(),
                            Some("pending" | "answer_queued" | "target_unavailable")
                        ) {
                            let mut payload = record.payload;
                            payload["status"] = json!("resolved_externally");
                            changed += usize::from(self.manager_v2_record_changed(
                                config, "decision", &key, live_epic, &payload,
                            )?);
                        }
                    }
                }
                Err(error) => {
                    // Preserve malformed/partially persisted questions as an
                    // explicit gate without minting a fictitious answer target.
                    changed += usize::from(self.manager_v2_record_changed(config,"intent",&key,live_epic,&json!({"state":"blocked","reason":error.to_string(),"session_id":session.id}))?);
                    if let Some(record) = self.manager_v2_record(config, "decision", &key)? {
                        let mut payload = record.payload;
                        payload["status"] = json!("target_unavailable");
                        changed += usize::from(self.manager_v2_record_changed(
                            config, "decision", &key, live_epic, &payload,
                        )?);
                    }
                }
            }
        }
        tx.commit()?;
        Ok(changed)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::session::agent_verbs::tests::test_session;
    use chrono::Utc;
    use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
    use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
    use rsi_common::types::{ConversationEvent, EventType, PendingQuestion, Project, QuestionItem};
    use rsi_common::types::{SessionKind, SessionStatus};
    use std::path::PathBuf;

    pub(crate) fn fixture(
        store: &Store,
        policy: ManagerPolicyV2,
    ) -> (HarnessManagerConfigV1, Session) {
        let stamp = Utc::now();
        let project = Uuid::new_v4();
        store
            .insert_project(&Project {
                id: project,
                name: format!("Manager coordinator {project}"),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: stamp,
                updated_at: stamp,
            })
            .unwrap();
        let mut manager = test_session(
            Uuid::new_v4(),
            PathBuf::from("/var/tmp/manager-coordinator"),
        );
        manager.session_kind = SessionKind::Standard;
        manager.project_id = Some(project);
        manager.status = SessionStatus::Completed;
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
        let mut lead = manager.clone();
        lead.id = Uuid::new_v4();
        lead.parent_id = Some(epic.id);
        lead.session_kind = SessionKind::Feature;
        lead.cost_usd = Some(0.0);
        store.insert_session(&lead).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
                params![epic.id.to_string(), lead.id.to_string()],
            )
            .unwrap();
        let config = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: manager.id,
                epic_ids: Some(vec![epic.id]),
                expected_row_version: 0,
            })
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: config.row_version,
                expected_policy_version: 0,
                idempotency_key: "coordinator-policy".into(),
                policy,
            })
            .unwrap();
        (config, lead)
    }

    pub(crate) fn raise_question(store: &Store, session: Uuid, sequence: i64) {
        // The shared resource fixture defaults to Codex. Structured provider
        // questions currently normalize only on Claude; model that producer.
        store
            .conn
            .execute(
                "UPDATE sessions SET provider='Claude' WHERE id=?1",
                [session.to_string()],
            )
            .unwrap();
        let question = PendingQuestion {
            questions: vec![QuestionItem {
                question: "Use the migration allocation?".into(),
                header: "Migration".into(),
                options: vec![],
                multi_select: false,
            }],
        };
        store
            .publish_pending_question_event(
                &ConversationEvent {
                    id: 0,
                    session_id: session,
                    sequence: i32::try_from(sequence).unwrap(),
                    event_type: EventType::ToolUse,
                    role: None,
                    created_at: Utc::now(),
                    content: String::new(),
                    tool_name: Some("AskUserQuestion".into()),
                    tool_input: Some(Box::new(json!(question))),
                    offload_id: None,
                    tool_use_id: Some(format!("question-{sequence}")),
                    metadata: None,
                },
                None,
                &question,
            )
            .unwrap();
        store
            .update_session_status(session, SessionStatus::WaitingApproval)
            .unwrap();
    }

    #[test]
    fn manager_v2_coordinator_unknown_spend_blocks_finite_cap_and_stays_unknown_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manager.db");
        let store = Store::open(&path).unwrap();
        let (config, lead) = fixture(
            &store,
            ManagerPolicyV2 {
                max_spend_usd: Some(10.0),
                ..Default::default()
            },
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET cost_usd=NULL WHERE id=?1",
                [lead.id.to_string()],
            )
            .unwrap();
        assert_eq!(
            store.manager_v2_resource_snapshot(&config).unwrap()["unknown_spend_observations"],
            1
        );
        assert!(
            store
                .manager_v2_resource_gate(&config, lead.parent_id, lead.provider, None)
                .unwrap_err()
                .to_string()
                .contains("spend_unknown")
        );
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert!(
            reopened
                .manager_v2_resource_gate(&config, lead.parent_id, lead.provider, None)
                .unwrap_err()
                .to_string()
                .contains("spend_unknown")
        );
    }

    #[test]
    fn manager_v2_coordinator_counts_whole_epic_and_admits_replacement_of_own_slot() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(
            &store,
            ManagerPolicyV2 {
                max_active_sessions: 1,
                ..Default::default()
            },
        );
        store
            .update_session_status(lead.id, SessionStatus::Running)
            .unwrap();
        assert_eq!(
            store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
            1
        );
        assert!(
            store
                .manager_v2_resource_gate(&config, lead.parent_id, lead.provider, None)
                .unwrap_err()
                .to_string()
                .contains("concurrency_capacity")
        );
        store
            .manager_v2_resource_gate_for_session(lead.id, lead.provider)
            .unwrap();
    }

    #[test]
    fn manager_v2_coordinator_enforces_cap_in_actual_model_admission_and_releases_capacity() {
        use crate::model_control::{ExpectedUsage, ModelAdmissionRequest};
        use crate::store::model_control::StoreAdmissionOutcome;
        use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose, ModelTier};
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(
            &store,
            ManagerPolicyV2 {
                max_active_sessions: 1,
                ..Default::default()
            },
        );
        store
            .update_session_status(lead.id, SessionStatus::Running)
            .unwrap();
        let mut child = lead.clone();
        child.id = Uuid::new_v4();
        store.insert_session(&child).unwrap();
        let purpose = ModelInvocationPurpose::SessionLaunchFresh;
        let mut request = ModelAdmissionRequest {
            purpose,
            provider: Some("Claude".into()),
            model: Some("claude-sonnet-4-6".into()),
            backend: Some("Claude".into()),
            effort: None,
            trigger: "test".into(),
            owner: InvocationOwner {
                session_id: Some(child.id),
                project_id: Some(config.project_id),
                ..Default::default()
            },
            dedup_key: Some("budget-admission-1".into()),
            request_fingerprint: Some("sha256:manager-budget-1".into()),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(ExpectedUsage {
                input_tokens: 100,
                output_tokens: 40,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                reasoning_tokens: 0,
                embedding_input_count: 0,
                wall_time_ms: 100,
            }),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let registry = *crate::model_control::registry::entry(purpose);
        let result = store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Standard, &request)
            .unwrap();
        assert!(
            matches!(result,StoreAdmissionOutcome::Denied {reason,..} if reason.contains("manager_v2_concurrency_capacity"))
        );
        assert!(
            store
                .manager_v2_resource_gate_for_parent(lead.parent_id.unwrap(), lead.provider)
                .is_err()
        );
        store
            .update_session_status(lead.id, SessionStatus::Completed)
            .unwrap();
        request.dedup_key = Some("budget-admission-2".into());
        let result = store
            .admit_model_invocation(Uuid::new_v4(), registry, ModelTier::Standard, &request)
            .unwrap();
        assert!(
            matches!(result, StoreAdmissionOutcome::Admitted(_)),
            "{result:?}"
        );
        assert_eq!(
            store.manager_v2_resource_snapshot(&config).unwrap()["active_sessions"],
            1
        );
    }

    #[test]
    fn manager_v2_coordinator_repeated_question_text_has_distinct_exact_target() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        raise_question(&store, lead.id, 1);
        assert_eq!(
            store
                .manager_v2_refresh_question_decisions(&config)
                .unwrap(),
            1
        );
        let key = format!("question:{}", lead.id);
        let first = store
            .manager_v2_record(&config, "decision", &key)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .manager_v2_refresh_question_decisions(&config)
                .unwrap(),
            0
        );
        store
            .update_session_pending_question_json(lead.id, None)
            .unwrap();
        raise_question(&store, lead.id, 3);
        assert_eq!(
            store
                .manager_v2_refresh_question_decisions(&config)
                .unwrap(),
            1
        );
        let next = store
            .manager_v2_record(&config, "decision", &key)
            .unwrap()
            .unwrap();
        assert!(next.row_version > first.row_version);
        assert_ne!(
            first.payload["target_digest"],
            next.payload["target_digest"]
        );
        assert_eq!(next.payload["question"], "Use the migration allocation?");
        assert_eq!(next.payload["status"], "pending");
    }

    #[test]
    fn manager_v2_coordinator_malformed_question_becomes_explicit_blocker() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        store
            .update_session_pending_question_json(lead.id, Some("{"))
            .unwrap();
        store
            .manager_v2_refresh_question_decisions(&config)
            .unwrap();
        let row = store
            .manager_v2_record(&config, "intent", &format!("question:{}", lead.id))
            .unwrap()
            .unwrap();
        assert_eq!(row.payload["state"], "blocked");
        assert!(
            row.payload["reason"]
                .as_str()
                .unwrap()
                .contains("question_malformed")
        );
    }

    #[test]
    fn manager_v2_coordinator_meaningful_events_coalesce_and_scope_change_starts_new_projection() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, ManagerPolicyV2::default());
        let payload = json!({"state":"dependency_wait","prerequisite":"shared schema"});
        assert!(
            store
                .manager_v2_record_changed(&config, "intent", "epic", lead.parent_id, &payload)
                .unwrap()
        );
        assert!(
            !store
                .manager_v2_record_changed(&config, "intent", "epic", lead.parent_id, &payload)
                .unwrap()
        );
        let version = store
            .manager_v2_record(&config, "intent", "epic")
            .unwrap()
            .unwrap()
            .row_version;
        assert_eq!(version, 1);
        let mut later = config;
        later.row_version += 1;
        assert!(
            store
                .manager_v2_record_changed(&later, "intent", "epic", lead.parent_id, &payload)
                .unwrap()
        );
    }
}
