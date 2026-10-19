//! Bounded reconciliation of an operator's persistent manager program.

use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::harness_manager_v2::*;
use rsi_common::types::SessionStatus;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
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
use super::worker_no_result::ResultWakeBound;
use crate::error::{DaemonError, Result};

const LEAD_PAUSE_KIND: &str = "manager_lead_pause";

/// #3209: a Completed lead's wake that has no deadline (a `when` predicate
/// without `timeout_seconds`, an `on_terminal` watch) still reads as parked for
/// this long after it was armed; past it the lead is reported as blocked on an
/// unbounded wake, so a wake that never fires cannot hide a stranded Epic.
const PARKED_WAKE_HORIZON_SECS: i64 = 3600;

pub(crate) mod idle;
pub use idle::{IntentInputPin, IntentInputVersion};

/// #2939: the read-only part of one intent pass, computed on another
/// connection ([`Store::observe_manager_intent`]). Valid only while the
/// daemon Store's inputs still equal `pin`.
pub struct ManagerIntentObservation {
    project: Uuid,
    manager_session_id: Uuid,
    scope_version: i64,
    pin: IntentInputPin,
    cohort: Vec<super::manager_coordinator::ManagerCohortMemberV2>,
    work: Vec<Value>,
    requests: Vec<Value>,
    resources: Value,
}

// Issue #669: daemon-owned manager seat recovery lives beside lead intent.
#[path = "manager_seat.rs"]
pub mod manager_seat;

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

#[derive(Clone, Default)]
pub struct ManagerIntentReconciliationV2 {
    pub changed: usize,
    pub unfinished: usize,
    pub blocked: usize,
    pub queued: usize,
    /// #3021: the pass read its work, requests and resources under the caller's
    /// Store guard (no usable off-lock observation, no memo hit). The
    /// coordinator logs it with the hold time of a slow pass.
    pub read_under_lock: bool,
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

/// #2607: a topology execution owns the review Work rows it creates and settles
/// them itself (`topology_settle_review_work`), so a lead has nothing to do for
/// them: they never count as unfinished work for operating-intent recovery.
fn topology_owned(work: &Value) -> bool {
    work["key"]
        .as_str()
        .is_some_and(|key| key.starts_with("topology:"))
}

fn work_open(work: &Value) -> bool {
    !work_finished(work) && !topology_owned(work)
}

fn with_overdue_requests(reason: &str, state: &Value) -> String {
    let ids = state["overdue_requests"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|request| request["request_id"].as_str())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        reason.to_owned()
    } else {
        format!("{reason}; overdue requests: {}", ids.join(", "))
    }
}

/// Interpret the recovery gate for intent reporting only; its refusal and
/// the authority it protects stay unchanged.
fn intent_refusal_reason(
    error: &DaemonError,
    state: &Value,
    lead: Option<&rsi_common::types::Session>,
) -> String {
    let reason = match error {
        DaemonError::InvalidParam(code) if code == "manager_v2_human_or_recovery_owner" => {
            match lead {
                Some(lead) if lead.pending_question.is_some() => "lead awaiting operator answer",
                Some(lead) if lead.pending_archive => "lead awaiting archive",
                Some(lead) if lead.status == SessionStatus::WaitingApproval => {
                    "lead awaiting approval"
                }
                _ => "lead held by a human or daemon recovery owner",
            }
            .to_owned()
        }
        _ => error.to_string(),
    };
    with_overdue_requests(&reason, state)
}

impl Store {
    pub fn manager_v2_lead_pause(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
    ) -> Result<(i64, bool)> {
        if let Some(record) = self.manager_v2_record(config, LEAD_PAUSE_KIND, &epic.to_string())? {
            let pause: ManagerLeadPauseV2 = serde_json::from_value(record.payload)
                .map_err(|_| refused("manager_v2_pause_malformed"))?;
            return Ok((record.row_version, pause.paused));
        }
        Ok((
            0,
            self.manager_v2_previous_lead_pause(config, epic)?
                .is_some_and(|p| p.paused),
        ))
    }

    /// A retirement stop belongs to automatic intent recovery. Explicit
    /// manager actions must remain available to settle work on that Epic.
    pub(crate) fn manager_v2_action_pause(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
    ) -> Result<(i64, bool)> {
        let (version, paused) = self.manager_v2_lead_pause(config, epic)?;
        if !paused {
            return Ok((version, false));
        }
        let pause = match self.manager_v2_record(config, LEAD_PAUSE_KIND, &epic.to_string())? {
            Some(record) => Some(
                serde_json::from_value::<ManagerLeadPauseV2>(record.payload)
                    .map_err(|_| refused("manager_v2_pause_malformed"))?,
            ),
            None => self.manager_v2_previous_lead_pause(config, epic)?,
        };
        let Some(pause) = pause.filter(|pause| pause.paused) else {
            return Ok((version, false));
        };
        // The free-form PauseLead reason cannot identify retirement: prove
        // that this pause was written by an admitted retirement operation.
        let retirement: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_operations
             WHERE id=?1 AND project_id=?2 AND kind='lifecycle_action'
               AND json_extract(payload_json,'$.request.operation.action')='retire_lead_continuations')",
            params![pause.operation_id.to_string(), config.project_id.to_string()],
            |row| row.get(0),
        )?;
        Ok((version, !retirement))
    }

    /// #1104: who paused this Epic's lead and since when, for blockers.
    pub(crate) fn manager_v2_lead_pause_origin(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
    ) -> Result<Option<(Option<Uuid>, Option<String>)>> {
        let Some(record) = self.manager_v2_record(config, LEAD_PAUSE_KIND, &epic.to_string())?
        else {
            return Ok(None);
        };
        let pause: ManagerLeadPauseV2 = serde_json::from_value(record.payload)
            .map_err(|_| refused("manager_v2_pause_malformed"))?;
        Ok(pause
            .paused
            .then_some((pause.actor_session_id, Some(record.updated_at))))
    }

    fn manager_v2_previous_lead_pause(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
    ) -> Result<Option<ManagerLeadPauseV2>> {
        let payload: Option<String> = self
            .conn
            .query_row(
                "SELECT payload_json FROM harness_manager_v2_records
             WHERE project_id=?1 AND scope_version<?2 AND kind=?3
               AND epic_id=?4 AND record_key=?4
             ORDER BY scope_version DESC LIMIT 1",
                params![
                    config.project_id.to_string(),
                    config.row_version,
                    LEAD_PAUSE_KIND,
                    epic.to_string()
                ],
                |row| row.get(0),
            )
            .optional()?;
        payload
            .map(|json| {
                serde_json::from_str(&json).map_err(|_| refused("manager_v2_pause_malformed"))
            })
            .transpose()
    }

    /// Materialize an inherited stop before recovery or explicit action
    /// admission captures a pause version. A local clear always wins.
    pub(crate) fn manager_v2_inherit_lead_pause(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
    ) -> Result<()> {
        if self
            .manager_v2_record(config, LEAD_PAUSE_KIND, &epic.to_string())?
            .is_none()
        {
            if let Some(pause) = self
                .manager_v2_previous_lead_pause(config, epic)?
                .filter(|p| p.paused)
            {
                self.manager_v2_record_changed(
                    config,
                    LEAD_PAUSE_KIND,
                    &epic.to_string(),
                    Some(epic),
                    &serde_json::to_value(pause)?,
                )?;
            }
        }
        Ok(())
    }

    // Caller holds the action admission transaction and has checked exact
    // authority/fences. A pause survives an interrupted executor or restart.
    pub fn manager_v2_set_lead_pause(
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
        self.manager_v2_inherit_lead_pause(config, epic)?;
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
    pub fn clear_manager_pause_for_operator(&self, session: Uuid) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(lead) = self.get_session(session)? {
            if let (Some(project), Some(epic)) = (lead.project_id, lead.parent_id) {
                if self
                    .get_session(epic)?
                    .is_some_and(|e| e.lead_session_id == Some(session))
                {
                    if let Some(config) = self.get_harness_manager(project)? {
                        self.manager_v2_inherit_lead_pause(&config, epic)?;
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
            .any(|work| work_open(work) && work["ready"] == true)
        {
            return Err(refused("manager_v2_no_ready_work"));
        }
        Ok(())
    }

    /// This predicate is re-read under the actual effect guard, not inferred
    /// from a coordinator's last poll. An operator answer waiting on delivery
    /// retains the gate until the precise target is settled.
    pub fn manager_v2_decision_gate(
        &self,
        config: &HarnessManagerConfigV1,
        epic: Uuid,
    ) -> Result<()> {
        let mut statement = self.conn.prepare(
            "SELECT record_key,payload_json FROM harness_manager_v2_records WHERE project_id=?1
             AND manager_session_id=?2 AND scope_version=?3 AND epic_id=?4 AND kind='decision'
             AND archived=0 AND json_extract(payload_json,'$.status') IN ('pending','answer_queued')",
        )?;
        let pending = statement.query_map(
            params![
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                epic.to_string()
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        for row in pending {
            let (key, payload) = row?;
            let payload: Value = serde_json::from_str(&payload)?;
            // The executor holds the asking run while it waits. This must
            // not hold unrelated work under the same Epic, even for a real
            // operator gate: only the operator can release that run's wait.
            if super::manager_decision_rulings::topology_decision_execution(&key, &payload)
                .is_none()
            {
                return Err(refused("manager_v2_pending_operator_decision"));
            }
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

    pub fn reconcile_manager_intent(
        &self,
        project: Uuid,
        retry_enabled: bool,
    ) -> Result<ManagerIntentReconciliationV2> {
        self.reconcile_manager_intent_observed(project, retry_enabled, None)
    }

    /// #2939: whether `project`'s last write-free pass can be reused as is
    /// (its inputs are unchanged). The coordinator asks under the daemon Store
    /// lock before it computes an observation off the lock.
    pub fn manager_intent_memoized(&self, project: Uuid) -> Result<bool> {
        Ok(self
            .manager_intent_memo(project, chrono::Utc::now())?
            .is_some())
    }

    /// #2939: the read-only half of [`Self::reconcile_manager_intent`] (cohort,
    /// work, requests, resources: nearly all of a pass's SQL), in one read
    /// snapshot. Meant for a [`Store::open_read_only_observer`] connection, so
    /// the daemon's Store lock is not held while it runs. It decides nothing:
    /// [`Self::reconcile_manager_intent_observed`] uses it only while every
    /// input is unchanged since `pin`, which the caller took on the daemon's
    /// Store before this snapshot began.
    pub fn observe_manager_intent(
        &self,
        project: Uuid,
        pin: IntentInputPin,
    ) -> Result<Option<ManagerIntentObservation>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let Some(config) = self.get_harness_manager(project)? else {
            return Ok(None);
        };
        let cohort = self.manager_v2_cohort(&config)?;
        let work = self.manager_v2_work_rows(&config, None)?;
        let requests = self.manager_v2_request_rows(&config, None, "", 129, true)?;
        let resources = self.manager_v2_resource_snapshot_in(&config, Some(&cohort))?;
        tx.commit()?;
        Ok(Some(ManagerIntentObservation {
            project,
            manager_session_id: config.manager_session_id,
            scope_version: config.row_version,
            pin,
            cohort,
            work,
            requests,
            resources,
        }))
    }

    /// [`Self::reconcile_manager_intent`], reusing `observed` (taken on
    /// another connection) only for this exact appointment and only while no
    /// input changed since its pin; otherwise the reads run here. Every
    /// write, hold, admission and fence below is unchanged.
    pub fn reconcile_manager_intent_observed(
        &self,
        project: Uuid,
        _retry_enabled: bool,
        observed: Option<ManagerIntentObservation>,
    ) -> Result<ManagerIntentReconciliationV2> {
        // This is an observation cache, never an authority or admission cache.
        // Only a pass that performed no writes can be reused. Effects, seat
        // recovery, action claims and their live fences still run separately.
        let observed_at = chrono::Utc::now();
        if let Some(result) = self.manager_intent_memo(project, observed_at)? {
            return Ok(result);
        }
        let pin = self.manager_intent_input_pin()?;
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
        let mut observed = match observed {
            Some(observation)
                if observation.project == project
                    && observation.manager_session_id == config.manager_session_id
                    && observation.scope_version == config.row_version
                    && self.manager_intent_inputs_unchanged(&observation.pin, &[])? =>
            {
                Some(observation)
            }
            _ => None,
        };
        // #2494: one cohort read serves the question refresh, the epic
        // loop and the resource snapshot of this pass.
        let cohort = match observed.as_mut() {
            Some(observation) => std::mem::take(&mut observation.cohort),
            None => self.manager_v2_cohort(&config)?,
        };
        result.changed += self.manager_v2_refresh_question_decisions_in(&config, &cohort)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // The refresh may have written an input: then read again, as before.
        let observed = match observed {
            Some(observation) if self.manager_intent_inputs_unchanged(&observation.pin, &[])? => {
                Some(observation)
            }
            _ => None,
        };
        let (work, requests, resources) = match observed {
            Some(observation) => (
                observation.work,
                observation.requests,
                observation.resources,
            ),
            None => {
                result.read_under_lock = true;
                (
                    self.manager_v2_work_rows(&config, None)?,
                    self.manager_v2_request_rows(&config, None, "", 129, true)?,
                    self.manager_v2_resource_snapshot_in(&config, Some(&cohort))?,
                )
            }
        };
        let valid_until = idle::next_observation_at(observed_at, &requests, &resources);
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
                    .filter(|w| w["epic_id"] == epic.to_string() && work_open(w))
                    .filter_map(|w| w["priority"].as_u64())
                    .min()
                    .unwrap_or(255),
                *epic,
            )
        });
        let mut candidates = Vec::new();
        let mut stale_leads = Vec::new();
        for epic in epics {
            self.manager_v2_inherit_lead_pause(&config, epic)?;
            let members: Vec<_> = cohort.iter().filter(|m| m.epic_id == Some(epic)).collect();
            let items: Vec<_> = work
                .iter()
                .filter(|w| w["epic_id"] == epic.to_string())
                .collect();
            let remaining: Vec<_> = items.iter().filter(|w| work_open(w)).copied().collect();
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
            } else if let Some(lead) = lead.as_ref().filter(|lead| {
                // #3209 / #1443: a lead idle since before the seat appointment
                // is `stale_lead` whatever old wake it still owns.
                lead.status == SessionStatus::Completed
                    && !self.manager_v2_lead_stale(&config, lead).unwrap_or(false)
            }) && let Some(bound) = self.owner_result_wake_bound(
                lead.id,
                chrono::Duration::seconds(PARKED_WAKE_HORIZON_SECS),
                observed_at,
            )? {
                // A completed provider turn with a bounded own wake is parked,
                // not idle work requiring another recovery owner. Preserve
                // genuine operator/daemon holds even while a wake is armed.
                // #3209: a wake nothing bounds (a predicate without a deadline,
                // past the horizon) may never fire, so it is not "parked": the
                // manager is told, naming the wake.
                match self.manager_action_operator_gate(lead.id) {
                    Ok(()) => match bound {
                        ResultWakeBound::Bounded(_) => {
                            state["state"] = json!("parked");
                            state["reason"] = json!(with_overdue_requests(
                                "lead parked on its own wakes; it will see pending requests on its next wake",
                                &state,
                            ));
                        }
                        ResultWakeBound::Unbounded {
                            wake_id,
                            predicate,
                            armed_at,
                        } => {
                            state["state"] = json!("blocked");
                            state["reason"] = json!(with_overdue_requests(
                                "lead_parked_on_unbounded_wake",
                                &state,
                            ));
                            state["wake_id"] = json!(wake_id);
                            state["wake_predicate"] = json!(predicate);
                            state["wake_armed_at"] = json!(armed_at.to_rfc3339());
                            state["next_action"] = json!(format!(
                                "The lead's wake {wake_id} ({predicate}, armed {}) has no deadline and may never fire. Check its predicate, then resume or replace the lead explicitly with AgentManagerControl.",
                                armed_at.to_rfc3339()
                            ));
                        }
                    },
                    Err(error) => {
                        state["state"] = json!("blocked");
                        state["reason"] = json!(intent_refusal_reason(&error, &state, Some(lead)));
                    }
                }
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
            } else if let Some(lead) = lead
                .as_ref()
                .filter(|lead| self.manager_v2_lead_stale(&config, lead).unwrap_or(false))
            {
                // #2939: a new event of this lead ends staleness; the memo
                // watches it (other events cannot change this observation).
                stale_leads.push(lead.id);
                // #1443: a seat change never wakes a lead that has been idle
                // since before the appointment. Only the manager's explicit
                // resume continues it; list it for the manager instead.
                state["state"] = json!("stale_lead");
                state["reason"] = json!("lead_idle_since_before_seat_appointment");
                state["lead_last_activity_at"] =
                    json!(self.manager_v2_lead_last_activity(lead)?.to_rfc3339());
                state["seat_appointed_at"] = json!(config.updated_at.to_rfc3339());
                state["next_action"] = json!(
                    "Resume this stale lead explicitly with AgentManagerControl resume_lead if its work should continue; automatic Execute intent leaves it idle."
                );
            } else if let Some(lead) = lead {
                match self.manager_v2_intent_action(&config, &grant, epic, &lead, &ready) {
                    Ok(action) => {
                        // Do not publish a transient state before admission.
                        // A refused candidate must have one stable semantic
                        // outcome, not pending/blocked writes on every poll.
                        candidates.push((epic, action, state));
                        continue;
                    }
                    Err(error) => {
                        state["state"] = json!("blocked");
                        state["reason"] = json!(intent_refusal_reason(&error, &state, Some(&lead)));
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
                    state["reason"] = json!(intent_refusal_reason(
                        &error,
                        &state,
                        self.manager_lead(project, epic).ok().as_ref(),
                    ));
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
        // Do not publish a memo after writes (including our own projections
        // and admissions), concurrent commits, errors, or an outer transaction
        // that could roll back. The next pass must observe those effects.
        if let Some(pin) = pin
            && result.changed == 0
            && result.queued == 0
            && self.conn.is_autocommit()
            && self.manager_intent_inputs_unchanged(&pin, &stale_leads)?
        {
            self.manager_intent_idle.borrow_mut().remember(
                project,
                pin,
                stale_leads,
                observed_at,
                valid_until,
                result.clone(),
            );
        }
        Ok(result)
    }

    /// Newest of the lead row's own update stamp and its last conversation
    /// event: the lead's last sign of life.
    fn manager_v2_lead_last_activity(
        &self,
        lead: &rsi_common::types::Session,
    ) -> Result<chrono::DateTime<chrono::Utc>> {
        let last_event: Option<String> = self.conn.query_row(
            "SELECT MAX(created_at) FROM conversation_events WHERE session_id=?1",
            params![lead.id.to_string()],
            |row| row.get(0),
        )?;
        let last_event = last_event
            .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(&stamp).ok())
            .map(|stamp| stamp.with_timezone(&chrono::Utc));
        Ok(last_event.map_or(lead.updated_at, |event| event.max(lead.updated_at)))
    }

    /// #1443: a lead whose last activity predates the current seat
    /// appointment is stale; the Execute intent must not wake it by itself.
    fn manager_v2_lead_stale(
        &self,
        config: &HarnessManagerConfigV1,
        lead: &rsi_common::types::Session,
    ) -> Result<bool> {
        Ok(self.manager_v2_lead_last_activity(lead)? < config.updated_at)
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
        // An inherited ancestor allowlist also authorizes automatic recovery;
        // an unbounded root policy still cannot select a launch automatically.
        let mut choices =
            self.manager_effective_launches(config, &grant.policy.allowed_launches)?;
        if choices.is_empty() {
            return Err(refused("manager_v2_intent_launch_allowlist_required"));
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
        // A missing tool output may represent an uncertain mutation. Keep it
        // available to an explicit guarded retry_lead, but do not let Execute
        // intent choose that boundary automatically.
        if self.manager_lead_invalid_tool_history(lead)? {
            return Err(refused("manager_v2_transcript_manual_recovery_required"));
        }
        let operation =
            if lead.status == SessionStatus::Failed || self.manager_lead_torn_tail(lead)? {
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
        Ok(AgentManagerControlRequestV2::new(
            ManagerFenceV2 {
                scope_version: config.row_version,
                policy_version: grant.row_version,
            },
            format!("intent:{key}"),
            operation,
        ))
    }
}
