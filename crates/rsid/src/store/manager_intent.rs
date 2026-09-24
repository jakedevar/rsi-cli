//! Bounded reconciliation of an operator's persistent manager program.

use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::harness_manager_v2::*;
use rsi_common::types::SessionStatus;
use rusqlite::{Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::Store;
use super::harness_manager_v2::{fingerprint, refused};
use super::manager_actions::{
    MANAGER_ACTION_HOLD_KIND, ManagerActionHoldReasonV2, ManagerActionOriginV2,
    ManagerActionRuntimeHoldV2, UNCERTAINTY_CURRENT_AUTHORITY, manager_action_hold_key,
};
use crate::error::Result;

const LEAD_PAUSE_KIND: &str = "manager_lead_pause";

// Issue #669: daemon-owned manager seat recovery lives beside lead intent.
#[path = "manager_seat.rs"]
pub(crate) mod manager_seat;

/// Manager-owned stop intent is scoped to the stable appointment and Epic,
/// not a transient provider turn or lead. Rotation/reassignment retains it.
#[derive(Serialize, Deserialize)]
struct ManagerLeadPauseV2 {
    paused: bool,
    operation_id: Uuid,
    actor_session_id: Option<Uuid>,
    lead_session_id: Option<Uuid>,
    reason: String,
    cleared_by: Option<String>,
}

#[derive(Default)]
pub(crate) struct ManagerIntentReconciliationV2 {
    pub changed: usize,
    pub unfinished: usize,
    pub blocked: usize,
    pub queued: usize,
}

/// Operator spend cap hold: unknown spend or known spend at/above the cap
/// withholds automatic recovery. Shared by lead intent and the manager seat.
pub(crate) fn manager_v2_spend_blocked(policy: &ManagerPolicyV2, resources: &Value) -> bool {
    policy.max_spend_usd.is_some_and(|cap| {
        resources["unknown_spend_observations"].as_u64() != Some(0)
            || resources["known_spend_usd"]
                .as_f64()
                .is_none_or(|used| used >= cap)
    })
}

fn work_finished(work: &Value) -> bool {
    work["integrated"] == true || (work["kind"] == "program" && work["source_accepted"] == true)
}

impl Store {
    pub(crate) fn manager_v2_lead_pause(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
    ) -> Result<(i64, bool)> {
        let Some(record) = self.manager_v2_record(config, LEAD_PAUSE_KIND, &epic.to_string())?
        else {
            return Ok((0, false));
        };
        let pause: ManagerLeadPauseV2 = serde_json::from_value(record.payload)
            .map_err(|_| refused("manager_v2_pause_malformed"))?;
        Ok((record.row_version, pause.paused))
    }

    // Caller holds the action admission transaction and has checked exact
    // authority/fences. A pause survives an interrupted executor or restart.
    pub(crate) fn manager_v2_set_lead_pause(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        operation_id: Uuid,
        actor_session_id: Option<Uuid>,
        lead_session_id: Option<Uuid>,
        reason: &str,
    ) -> Result<()> {
        self.manager_v2_record_changed(
            config,
            LEAD_PAUSE_KIND,
            &epic.to_string(),
            Some(epic),
            &serde_json::to_value(ManagerLeadPauseV2 {
                paused: true,
                operation_id,
                actor_session_id,
                lead_session_id,
                reason: reason.to_owned(),
                cleared_by: None,
            })?,
        )?;
        Ok(())
    }

    pub(crate) fn manager_v2_clear_lead_pause(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        expected_version: i64,
        cleared_by: &str,
    ) -> Result<()> {
        let Some(record) = self.manager_v2_record(config, LEAD_PAUSE_KIND, &epic.to_string())?
        else {
            return Ok(());
        };
        // A pause arriving after admission wins. Its new operation is still
        // queued to settle the provider; never erase it with an older resume.
        if record.row_version != expected_version {
            return Ok(());
        }
        let mut pause: ManagerLeadPauseV2 = serde_json::from_value(record.payload)?;
        if pause.paused {
            pause.paused = false;
            pause.cleared_by = Some(cleared_by.to_owned());
            self.manager_v2_record_changed(
                config,
                LEAD_PAUSE_KIND,
                &epic.to_string(),
                Some(epic),
                &serde_json::to_value(pause)?,
            )?;
        }
        Ok(())
    }

    /// The explicit operator continuation can release this Epic's manager
    /// pause, even if its appointed manager is currently unavailable. It must
    /// target the current lead; continuing a retired predecessor is unrelated.
    pub(crate) fn clear_manager_pause_for_operator(&self, session: Uuid) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(lead) = self.get_session(session)? {
            if let (Some(project), Some(epic)) = (lead.project_id, lead.parent_id) {
                if self
                    .get_session(epic)?
                    .is_some_and(|e| e.lead_session_id == Some(session))
                {
                    if let Some(config) = self.get_harness_manager(project)? {
                        let (version, _) = self.manager_v2_lead_pause(&config, epic)?;
                        self.manager_v2_clear_lead_pause(
                            &config,
                            epic,
                            version,
                            &format!("operator_continue:{session}"),
                        )?;
                    }
                }
            }
        }
        self.record_manager_operator_pause(session, false)?;
        tx.commit()?;
        Ok(())
    }

    /// Re-read work/dependencies at the effect guard too: a prerequisite can
    /// be invalidated while a persisted delayed continuation is queued.
    pub(crate) fn manager_v2_intent_work_gate(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
    ) -> Result<()> {
        if !self
            .manager_v2_work_rows(config, Some(epic))?
            .iter()
            .any(|work| !work_finished(work) && work["ready"] == true)
        {
            return Err(refused("manager_v2_no_ready_work"));
        }
        Ok(())
    }

    /// This predicate is re-read under the actual effect guard, not inferred
    /// from a coordinator's last poll. An operator answer waiting on delivery
    /// retains the gate until the precise target is settled.
    pub(crate) fn manager_v2_decision_gate(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
    ) -> Result<()> {
        let pending:bool=self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_records WHERE project_id=?1
             AND manager_session_id=?2 AND scope_version=?3 AND epic_id=?4 AND kind='decision'
             AND archived=0 AND json_extract(payload_json,'$.status') IN ('pending','answer_queued'))",
            params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,epic.to_string()],|r|r.get(0))?;
        if pending {
            return Err(refused("manager_v2_pending_operator_decision"));
        }
        Ok(())
    }

    fn manager_v2_intent_hold(
        &self,
        config: &HarnessManagerConfigV1,
        reason: ManagerActionHoldReasonV2,
        epic: Option<Uuid>,
        blocked: bool,
    ) -> Result<()> {
        let key = manager_action_hold_key(reason, epic);
        self.manager_v2_record_changed(
            config,
            MANAGER_ACTION_HOLD_KIND,
            &key,
            epic,
            &serde_json::to_value(ManagerActionRuntimeHoldV2 { blocked })?,
        )?;
        self.conn.execute(
            "UPDATE harness_manager_v2_records SET archived=?5,updated_at=?6
             WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3
               AND kind='lifecycle_hold' AND record_key=?4 AND archived<>?5",
            params![
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                key,
                !blocked,
                super::harness_manager_v2::now()
            ],
        )?;
        Ok(())
    }

    pub(crate) fn reconcile_manager_intent(
        &self,
        project: Uuid,
        retry_enabled: bool,
    ) -> Result<ManagerIntentReconciliationV2> {
        let mut result = ManagerIntentReconciliationV2::default();
        let Some(config) = self.get_harness_manager(project)? else {
            return Ok(result);
        };
        let Some(grant) = self.get_harness_manager_policy(project)? else {
            return Ok(result);
        };
        if grant.revoked {
            return Ok(result);
        };
        result.changed += self.manager_v2_refresh_question_decisions(&config)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let cohort = self.manager_v2_cohort(&config)?;
        let work = self.manager_v2_work_rows(&config, None)?;
        let requests = self.manager_v2_request_rows(&config, None, "", 129, true)?;
        let resources = self.manager_v2_resource_snapshot(&config)?;
        let spend_blocked = manager_v2_spend_blocked(&grant.policy, &resources);
        self.manager_v2_intent_hold(
            &config,
            ManagerActionHoldReasonV2::Resources,
            None,
            spend_blocked,
        )?;
        result.changed += usize::from(
            self.manager_v2_record_changed(&config, "resource", "cohort", None, &resources)?,
        );
        let mut epics = config.epic_ids.clone();
        epics.sort_by_key(|epic| {
            (
                work.iter()
                    .filter(|w| w["epic_id"] == epic.to_string() && !work_finished(w))
                    .filter_map(|w| w["priority"].as_u64())
                    .min()
                    .unwrap_or(255),
                *epic,
            )
        });
        let mut candidates = Vec::new();
        for epic in epics {
            let members: Vec<_> = cohort.iter().filter(|m| m.epic_id == Some(epic)).collect();
            let items: Vec<_> = work
                .iter()
                .filter(|w| w["epic_id"] == epic.to_string())
                .collect();
            let remaining: Vec<_> = items
                .iter()
                .filter(|w| !work_finished(w))
                .copied()
                .collect();
            result.unfinished += remaining.len();
            let ready: Vec<_> = remaining
                .iter()
                .filter(|w| w["ready"] == true)
                .copied()
                .collect();
            let active: Vec<_> = members
                .iter()
                .filter(|m| {
                    rsi_common::is_leaf_kind(m.session.session_kind)
                        && matches!(
                            m.session.status,
                            SessionStatus::Starting
                                | SessionStatus::Running
                                | SessionStatus::WaitingApproval
                        )
                })
                .map(|m| m.session.id)
                .collect();
            let lead = self.manager_lead(project, epic).ok();
            let pending_decision = self.manager_v2_decision_gate(&config, epic).is_err();
            self.manager_v2_intent_hold(
                &config,
                ManagerActionHoldReasonV2::Decision,
                Some(epic),
                pending_decision,
            )?;
            let mut state = json!({"mode":grant.policy.mode,"policy_version":grant.row_version,"epic_id":epic,
                "work_count":items.len(),"unfinished_count":remaining.len(),"lead_session_id":lead.as_ref().map(|s|s.id),
                "work_versions":items.iter().map(|w|json!({"key":w["key"],"version":w["row_version"],"source_accepted":w["source_accepted"],"integrated":w["integrated"]})).collect::<Vec<_>>()});
            let overdue:Vec<_>=requests.iter().filter(|r|r["epic_id"]==epic.to_string()&&r["overdue"]==true).map(|r|json!({"request_id":r["request_id"],"state":r["state"],"deadline":r["deadline"],"delivery_issue":r["delivery_issue"]})).collect();
            state["overdue_requests"] = json!(overdue);
            let pending_actions = self.manager_v2_pending_intent_actions(&config, epic, true)?;
            let historical = self.manager_v2_pending_intent_actions(&config, epic, false)?;
            state["historical_uncertainties"] = json!(historical);
            state["historical_uncertainties_limit"] = json!(16);
            let (pause_version, manager_paused) = self.manager_v2_lead_pause(&config, epic)?;
            state["manager_pause_version"] = json!(pause_version);
            state["manager_paused"] = json!(manager_paused);
            state["manager_pause"] = self
                .manager_v2_record(&config, LEAD_PAUSE_KIND, &epic.to_string())?
                .map(|r| r.payload)
                .unwrap_or(Value::Null);
            let operator_paused = lead
                .as_ref()
                .map(|lead| self.manager_action_operator_paused(lead.id))
                .transpose()?
                .unwrap_or(false);
            let paused = grant.policy.paused
                || grant.policy.paused_epic_ids.contains(&epic)
                || operator_paused;
            if items.is_empty() {
                state["state"] = json!("blocked");
                state["reason"] = json!("work_plan_required");
                state["next_action"] = json!(
                    "Declare selected deliverables and their gates with AgentManagerUpdate work."
                );
            } else if remaining.is_empty() {
                state["state"] = json!("complete");
                state["reason"] = json!("declared_work_satisfied");
            } else if paused {
                state["state"] = json!("operator_paused");
                state["reason"] = json!(if operator_paused {
                    "persisted_operator_pause"
                } else {
                    "persisted_operator_policy"
                });
            } else if manager_paused {
                state["state"] = json!("manager_paused");
                state["reason"] = json!("persisted_manager_pause");
                state["next_action"] = json!(
                    "Explicitly resume, retry or replace the lead with AgentManagerControl, or continue the current lead as operator."
                );
            } else if pending_decision {
                state["state"] = json!("blocked");
                state["reason"] = json!("pending_operator_decision");
            } else if grant.policy.mode == ManagerOperatingModeV2::Status {
                state["state"] = json!("status_only");
            } else if !active.is_empty() {
                state["state"] = json!("worker");
                state["active_session_ids"] = json!(active);
            } else if !pending_actions.is_empty() {
                state["state"] = json!(
                    if pending_actions.iter().any(|v| v["state"] == "uncertain") {
                        "blocked"
                    } else {
                        "recovery_action"
                    }
                );
                state["reason"] = json!(
                    if pending_actions.iter().any(|v| v["state"] == "uncertain") {
                        "unconfirmed_effect_for_current_lead"
                    } else {
                        "durable_action_pending"
                    }
                );
                state["actions"] = json!(pending_actions);
            } else if ready.is_empty() {
                state["state"] = json!("dependency_wait");
                state["dependencies"] = json!(
                    remaining
                        .iter()
                        .map(|w| json!({"work_key":w["key"],"blockers":w["blockers"]}))
                        .collect::<Vec<_>>()
                );
            } else if grant.policy.mode == ManagerOperatingModeV2::Monitor {
                state["state"] = json!("monitoring");
                state["reason"] = json!("ready_work_idle");
            } else if let Some(lead) = lead {
                match self.manager_v2_intent_action(
                    &config,
                    &grant,
                    epic,
                    &lead,
                    &ready,
                    retry_enabled,
                ) {
                    Ok(action) => {
                        // Do not publish a transient state before admission.
                        // A refused candidate must have one stable semantic
                        // outcome, not pending/blocked writes on every poll.
                        candidates.push((epic, action, state));
                        continue;
                    }
                    Err(error) => {
                        state["state"] = json!("blocked");
                        state["reason"] = json!(error.to_string());
                    }
                }
            } else {
                state["state"] = json!("blocked");
                state["reason"] = json!("authoritative_lead_required");
                state["next_action"] = json!(
                    "Create a legal session and explicitly assign it using AgentManagerControl."
                );
            }
            if matches!(
                state["state"].as_str(),
                Some("blocked" | "operator_paused" | "manager_paused")
            ) {
                result.blocked += 1;
            }
            result.changed += usize::from(self.manager_v2_record_changed(
                &config,
                "intent",
                &epic.to_string(),
                Some(epic),
                &state,
            )?);
        }
        tx.commit()?;
        // Each lifecycle admission owns its immediate transaction. The caller
        // still holds the daemon Store lock, and the journal rechecks all fences.
        for (epic, request, mut state) in candidates {
            let intent_id = Uuid::new_v5(
                &Uuid::NAMESPACE_OID,
                format!(
                    "manager-intent:{}:{}:{}",
                    project, config.manager_session_id, config.row_version
                )
                .as_bytes(),
            );
            let admitted = self.enqueue_manager_action(
                ManagerActionOriginV2::OperatingIntent {
                    project_id: project,
                    intent_id,
                },
                request,
            );
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            match admitted {
                Ok(receipt) => {
                    state["state"] = json!(if matches!(
                        receipt.state,
                        ManagerActionStateV2::Queued | ManagerActionStateV2::Running
                    ) {
                        "recovery_action"
                    } else {
                        "blocked"
                    });
                    if !matches!(
                        receipt.state,
                        ManagerActionStateV2::Queued | ManagerActionStateV2::Running
                    ) {
                        state["reason"] = json!(receipt.outcome);
                        result.blocked += 1;
                    }
                    if matches!(
                        receipt.state,
                        ManagerActionStateV2::Queued | ManagerActionStateV2::Running
                    ) {
                        state["reason"] = json!("durable_action_pending");
                        state["actions"] =
                            json!(self.manager_v2_pending_intent_actions(&config, epic, true)?);
                    } else {
                        state["operation_id"] = json!(receipt.operation_id);
                        state["action_state"] = json!(receipt.state);
                    }
                    result.queued += usize::from(!receipt.deduplicated);
                }
                Err(error) => {
                    state["state"] = json!("blocked");
                    state["reason"] = json!(error.to_string());
                    result.blocked += 1;
                }
            }
            result.changed += usize::from(self.manager_v2_record_changed(
                &config,
                "intent",
                &epic.to_string(),
                Some(epic),
                &state,
            )?);
            tx.commit()?;
        }
        Ok(result)
    }

    fn manager_v2_pending_intent_actions(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
        current: bool,
    ) -> Result<Vec<Value>> {
        let predicate = if current {
            format!(
                "(state IN ('queued','running') OR (state='uncertain' AND {UNCERTAINTY_CURRENT_AUTHORITY}))"
            )
        } else {
            format!("state='uncertain' AND NOT {UNCERTAINTY_CURRENT_AUTHORITY}")
        };
        let limit = if current { 129 } else { 16 };
        let mut stmt=self.conn.prepare(&format!(
            "SELECT id,state,not_before,outcome_json FROM harness_manager_v2_operations a WHERE project_id=?1
             AND manager_session_id=?2 AND scope_version=?3 AND kind='lifecycle_action'
             AND {predicate} AND json_extract(payload_json,'$.request.operation.epic_id')=?4
             ORDER BY created_at,id LIMIT {limit}"))?;
        let rows = stmt
            .query_map(
                params![
                    config.project_id.to_string(),
                    config.manager_session_id.to_string(),
                    config.row_version,
                    epic.to_string()
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.len() > 128 {
            return Err(refused("manager_v2_pending_action_limit"));
        }
        rows.into_iter().map(|(id,state,due,outcome)|Ok(json!({"operation_id":id,"state":state,"not_before":due,"outcome":outcome.map(|s|serde_json::from_str::<Value>(&s)).transpose()?}))).collect()
    }

    fn manager_v2_intent_action(
        &self,
        config: &HarnessManagerConfigV1,
        grant: &HarnessManagerPolicyConfigV2,
        epic: Uuid,
        lead: &rsi_common::types::Session,
        ready: &[&Value],
        retry_enabled: bool,
    ) -> Result<AgentManagerControlRequestV2> {
        self.manager_action_human_gate(lead.id)?;
        if self.manager_v2_lead_pause(config, epic)?.1 {
            return Err(refused("manager_v2_manager_paused"));
        }
        if !grant
            .policy
            .capabilities
            .contains(&ManagerCapabilityV2::LeadControl)
        {
            return Err(refused("manager_v2_lead_control_grant_required"));
        }
        if lead.status == SessionStatus::Failed && !retry_enabled {
            return Err(refused("manager_v2_retry_disabled"));
        }
        let used:i64=self.conn.query_row("SELECT COUNT(*) FROM harness_manager_v2_operations WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind='lifecycle_action' AND json_extract(payload_json,'$.origin.origin')='operating_intent' AND json_extract(payload_json,'$.request.operation.epic_id')=?4",params![config.project_id.to_string(),config.manager_session_id.to_string(),config.row_version,epic.to_string()],|r|r.get(0))?;
        if used >= i64::from(grant.policy.max_recovery_attempts) {
            return Err(refused("manager_v2_intent_recovery_budget_exhausted"));
        }
        let current = lead.model.as_ref().map(|model| ManagerLaunchChoiceV2 {
            provider: lead.provider,
            model: model.clone(),
            effort: lead.effort.clone(),
        });
        // An empty list leaves launch choice unrestricted. Automatic recovery
        // retains the current lead's configuration; explicit manager actions
        // can choose another valid provider/model/effort when needed.
        let mut choices = if grant.policy.allowed_launches.is_empty() {
            current.iter().cloned().collect()
        } else {
            grant.policy.allowed_launches.clone()
        };
        choices.sort_by_key(|choice| Some(choice) != current.as_ref());
        let choice = choices
            .into_iter()
            .find(|choice| {
                self.manager_v2_resource_gate(config, Some(epic), choice.provider, Some(lead.id))
                    .is_ok()
            })
            .ok_or_else(|| refused("manager_v2_no_admitted_launch_choice"))?;
        let expected = self.manager_action_lead_fence(epic)?;
        let work_summary = ready
            .iter()
            .take(16)
            .map(|w| {
                format!(
                    "{}: {} (source accepted={}, integrated={})",
                    w["key"].as_str().unwrap_or("unknown"),
                    w["title"].as_str().unwrap_or("unknown"),
                    w["source_accepted"],
                    w["integrated"]
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let message = format!(
            "Continue the operator's persisted Execute intent for this Epic. An ordinary completed provider turn does not finish the declared work. Read AgentManagerInspect work/requests/decisions for current scope and exact fences. Preserve operator decisions and existing custody. Gather independent acceptance evidence, obey dependencies and coordinate rolling integration through the established lead workflow. Ready work (first 16; inspect for all):\n{work_summary}"
        );
        let operation = if lead.status == SessionStatus::Failed {
            ManagerActionV2::RetryLead {
                epic_id: epic,
                expected,
                message,
                launch: Some(choice),
            }
        } else if Some(&choice) == current.as_ref()
            && lead.provider != rsi_common::types::SessionProvider::CodexAppServer
        {
            ManagerActionV2::ResumeLead {
                epic_id: epic,
                expected,
                message,
            }
        } else {
            ManagerActionV2::ReplaceLead {
                epic_id: epic,
                expected,
                query: message,
                launch: choice,
            }
        };
        let decision_versions = self.manager_v2_decision_generation(config, Some(epic))?;
        // Cross-Epic/transitive prerequisite revisions can invalidate and
        // restore readiness without changing this work row or lead cursor.
        // Hash the bounded ledger revision vector: no poll timestamps, and no
        // unbounded expansion into the action payload/fingerprint budget.
        let mut readiness_versions = Vec::new();
        for kind in ["work", "dependency"] {
            for record in self.manager_v2_records(config, kind)? {
                readiness_versions.push(json!([kind, record.key, record.row_version]));
            }
        }
        let gate_revision = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                decision_versions,
                readiness_versions
            ),)?)
        );
        let fingerprint = fingerprint(
            &json!({"scope":config.row_version,"policy":grant.row_version,"operation":operation,
            "manager_pause_version":self.manager_v2_lead_pause(config, epic)?.0,
            "gate_revision":gate_revision,
            "work":ready.iter().map(|w|json!([w["key"],w["row_version"]])).collect::<Vec<_>>()}),
        )?;
        let key = Uuid::new_v5(&Uuid::NAMESPACE_OID, fingerprint.as_bytes());
        Ok(AgentManagerControlRequestV2 {
            fence: ManagerFenceV2 {
                scope_version: config.row_version,
                policy_version: grant.row_version,
            },
            idempotency_key: format!("intent:{key}"),
            operation,
        })
    }
}
