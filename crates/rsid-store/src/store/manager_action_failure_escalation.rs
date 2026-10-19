//! #2455: a failed, blocked, uncertain or revoked manager action keeps its
//! notice obligation when the manager scope changes.
//!
//! A scope edit or a manager replacement retires every old-scope notice and
//! queue obligation (`manager_notices`), so an unfinished failure used to
//! vanish from every inbox while its receipt stayed stored. Retiring the old
//! scope must revoke the old *authority*, not erase the *responsibility*: the
//! failure is routed once to whoever is responsible now.
//!
//! the project's current manager (its action-result transport for the
//! current scope): a `ledger_change` notice `action_failure_escalation`,
//! subject `action_failure:<operation>`, version = the receipt row version.
//!
//! #2475: a project with no manager that can act (its scope is revoked or
//! absent) is not ownerless while a portfolio node covers it. The failure goes
//! to the nearest live covering portfolio seat (the `on_call` fallback, PM
//! ruling P1) as one ordinary `Resume` wake whose id derives from the
//! operation and the receipt version. With no live covering seat either, the
//! failure is retained as one operator to-do keyed the same way. Both writes
//! run inside the caller's transaction.
//!
//! Decisions:
//! - The notice is informational and redacted: operation id, action kind,
//!   terminal state, the bounded reason code and the original manager/scope
//!   only. It never carries the request, operator result or sandbox source.
//! - It is a plain information notice, never an action: the old operation
//!   stays in its terminal state, nothing is re-queued, and the old authority
//!   stays revoked. The current owner decides whether to re-issue it under its
//!   own scope.
//! - It targets the current owner only, so the predecessor of a handoff is
//!   never woken by it. Successful obsolete action notices stay routine and
//!   suppressed.
//! - Idempotent on `(current transport, operation, receipt version)` so a retry, a restart or an interrupted pass records at
//!   most one escalation. A later scope change carries a still-unsettled
//!   escalation forward to the then-current owner.

use super::Store;
use super::manager_notices::{ManagerActionNoticeFault, manager_action_notice_fault};
use super::scheduled_jobs::{get_scheduled_job_conn, insert_scheduled_job_conn};
use crate::error::{DaemonError, Result};
use chrono::Utc;
use rsi_common::harness_manager_v2::{ManagerActionReceiptV2, ManagerActionStateV2};
use rsi_common::operator_todos::{OPERATOR_TODO_CLOSED, OPERATOR_TODO_LIMIT};
use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, WakeMode};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

/// `record_kind` of the escalation notice.
pub const ACTION_FAILURE_ESCALATION: &str = "action_failure_escalation";
/// Longest reason text carried in an escalation.
const MAX_REASON_CHARS: usize = 256;

const PORTFOLIO_NAMESPACE: Uuid = Uuid::from_u128(0x2475_c3a1_7e5d_4b92_9a60_f1d8_0e4c_b7a3);
/// Priority of the operator to-do (urgent: a failure has no reader).
const TODO_PRIORITY: u8 = 2;

/// What an escalation needs; nothing else of the receipt is carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionFailureFacts {
    pub operation_id: Uuid,
    pub receipt_row_version: i64,
    pub state: ManagerActionStateV2,
    pub outcome: Option<String>,
    pub action_kind: String,
    pub target_session_id: Option<Uuid>,
    pub original_manager: Uuid,
    pub original_scope: i64,
}

/// Where the escalation went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionFailureRoute {
    /// New notice on the current manager's transport.
    ManagerNotice(Uuid),
    /// This failure was already escalated; nothing new was written.
    AlreadyRecorded,
    /// The receipt is a success, or still queued or running.
    NotAFailure,
    /// The scope the action ran under is still current: the normal notice owns it.
    StillCurrent,
    /// New wake for the nearest live covering portfolio seat (the session).
    PortfolioSeat(Uuid),
    /// No manager scope and no live covering portfolio seat: retained as an
    /// operator to-do (or logged when the to-do list cannot take it).
    Unrouted,
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

fn is_failure_state(state: ManagerActionStateV2) -> bool {
    matches!(
        state,
        ManagerActionStateV2::Failed
            | ManagerActionStateV2::Blocked
            | ManagerActionStateV2::Uncertain
            | ManagerActionStateV2::Revoked
    )
}

fn bounded(text: &str) -> String {
    text.chars().take(MAX_REASON_CHARS).collect()
}

impl ActionFailureFacts {
    pub(crate) fn from_receipt(
        receipt: &ManagerActionReceiptV2,
        original_manager: Uuid,
        original_scope: i64,
    ) -> Self {
        Self {
            operation_id: receipt.operation_id,
            receipt_row_version: receipt.row_version,
            state: receipt.state,
            outcome: receipt.outcome.as_deref().map(bounded),
            action_kind: serde_json::to_value(receipt.action_kind)
                .ok()
                .and_then(|kind| kind.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".into()),
            target_session_id: receipt.target_session_id,
            original_manager,
            original_scope,
        }
    }

    /// The facts of an unsettled old-scope notice that must outlive its scope:
    /// a materialized non-success `action_result`, or an earlier escalation
    /// being carried to the next owner. `None` for every other notice.
    pub(crate) fn from_pending_notice(
        kind: &str,
        state_json: &str,
        manager: Uuid,
        scope: i64,
    ) -> Option<Self> {
        match kind {
            "action_result" => {
                let receipt: ManagerActionReceiptV2 = serde_json::from_str(state_json).ok()?;
                is_failure_state(receipt.state)
                    .then(|| Self::from_receipt(&receipt, manager, scope))
            }
            "ledger_change" => {
                let value: Value = serde_json::from_str(state_json).ok()?;
                if value["record_kind"] != ACTION_FAILURE_ESCALATION {
                    return None;
                }
                let state: ManagerActionStateV2 =
                    serde_json::from_value(value["state"].clone()).ok()?;
                is_failure_state(state).then_some(())?;
                Some(Self {
                    operation_id: serde_json::from_value(value["operation_id"].clone()).ok()?,
                    receipt_row_version: value["receipt_row_version"].as_i64()?,
                    state,
                    outcome: value["outcome"].as_str().map(bounded),
                    action_kind: value["action_kind"].as_str()?.to_owned(),
                    target_session_id: serde_json::from_value(value["target_session_id"].clone())
                        .ok()
                        .flatten(),
                    original_manager: serde_json::from_value(
                        value["original_manager_session_id"].clone(),
                    )
                    .ok()?,
                    original_scope: value["original_scope_version"].as_i64()?,
                })
            }
            _ => None,
        }
    }

    /// The wake / to-do identity of the portfolio route: one per failure.
    fn portfolio_key(&self) -> String {
        format!("{}:{}", self.operation_id, self.receipt_row_version)
    }

    fn portfolio_detail(&self, project_id: Uuid) -> String {
        format!(
            "[rsi daemon, #2475 action_failure] Manager action {operation} ({kind}) in project \
             {project} ended {state} (reason: {reason}) under manager {manager} scope {scope}, \
             and the project has no manager that can act now. You are the nearest live covering \
             portfolio seat. That authority stays revoked and the action is not re-run; this is \
             the only notice of the failure. Decide whether to appoint a project manager and \
             re-issue the action under its scope.",
            operation = self.operation_id,
            kind = self.action_kind,
            project = project_id,
            state = state_name(self.state),
            reason = self.outcome.as_deref().unwrap_or("none recorded"),
            manager = self.original_manager,
            scope = self.original_scope,
        )
    }

    fn subject(&self) -> String {
        format!("action_failure:{}", self.operation_id)
    }

    fn version(&self) -> String {
        self.receipt_row_version.to_string()
    }

    fn detail(&self) -> String {
        format!(
            "[rsi daemon, #2455 action_failure] Manager action {operation} ({kind}) ended {state} \
             (reason: {reason}) under manager {manager} scope {scope}, which has since been \
             replaced or reconfigured. That authority stays revoked and the action is not \
             re-run; this is the only notice of the failure. Inspect the receipt with \
             AgentManagerGetAction and re-issue the action under your own scope if it is \
             still wanted.",
            operation = self.operation_id,
            kind = self.action_kind,
            state = state_name(self.state),
            reason = self.outcome.as_deref().unwrap_or("none recorded"),
            manager = self.original_manager,
            scope = self.original_scope,
        )
    }

    fn state_json(&self) -> Value {
        json!({
            "record_kind": ACTION_FAILURE_ESCALATION,
            "operation_id": self.operation_id,
            "action_kind": self.action_kind,
            "state": state_name(self.state),
            "outcome": self.outcome,
            "receipt_row_version": self.receipt_row_version,
            "target_session_id": self.target_session_id,
            "original_manager_session_id": self.original_manager,
            "original_scope_version": self.original_scope,
            "detail": self.detail(),
        })
    }
}

impl Store {
    /// Route one failure to the project's current responsible owner (see the
    /// module docs). Runs inside the caller's transaction when there is one;
    /// every write is idempotent on the failure's identity.
    ///
    /// # Errors
    /// A persistence error; the caller's transaction rolls back and the next
    /// pass retries.
    pub(crate) fn escalate_manager_action_failure(
        &self,
        project_id: Uuid,
        facts: &ActionFailureFacts,
    ) -> Result<ActionFailureRoute> {
        if !is_failure_state(facts.state) {
            return Ok(ActionFailureRoute::NotAFailure);
        }
        if let Some(config) = self.get_harness_manager_notice_config(project_id)? {
            if config.manager_session_id == facts.original_manager
                && config.row_version == facts.original_scope
            {
                return Ok(ActionFailureRoute::StillCurrent);
            }
            // A vacant seat during a handoff still has a successor to read the
            // durable notice, and a scope edit revokes the saved grant but not
            // the seat's right to be told; only a revoked scope has no owner.
            if !config.is_revoked() {
                return self.record_action_failure_manager_notice(&config, facts);
            }
        }
        self.escalate_action_failure_without_manager(project_id, facts)
    }

    /// #2475: no manager can act for `project_id`. The nearest live covering
    /// portfolio seat holds the failure; with none, the operator to-do list
    /// does, so the gap is never silent.
    fn escalate_action_failure_without_manager(
        &self,
        project_id: Uuid,
        facts: &ActionFailureFacts,
    ) -> Result<ActionFailureRoute> {
        let key = facts.portfolio_key();
        let Some(&(_, seat)) = self.on_call_portfolio_seats(project_id)?.first() else {
            return self.record_unrouted_action_failure(project_id, facts);
        };
        let wake_id = Uuid::new_v5(&PORTFOLIO_NAMESPACE, key.as_bytes());
        if get_scheduled_job_conn(&self.conn, &wake_id)?.is_some() {
            return Ok(ActionFailureRoute::AlreadyRecorded);
        }
        let now = Utc::now();
        insert_scheduled_job_conn(
            &self.conn,
            &ScheduledJob {
                id: wake_id,
                name: format!("action-failure-escalation:{key}"),
                message: facts.portfolio_detail(project_id),
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
                project_id: None,
                created_at: now,
                updated_at: now,
                wake_mode: WakeMode::Resume,
                wake_session_id: Some(seat),
            },
        )?;
        Ok(ActionFailureRoute::PortfolioSeat(seat))
    }

    /// Nobody can be told: keep one operator to-do per failure. A full or
    /// already-closed to-do list must not fail the scope change that raised
    /// this, so those refusals are logged instead.
    fn record_unrouted_action_failure(
        &self,
        project_id: Uuid,
        facts: &ActionFailureFacts,
    ) -> Result<ActionFailureRoute> {
        let key = facts.portfolio_key();
        let written = self.operator_todo_system_notice(
            &format!("action-failure:{key}"),
            Some(project_id),
            &format!(
                "Manager action {} {} with no owner",
                facts.operation_id,
                state_name(facts.state)
            ),
            &facts.portfolio_detail(project_id).replace(
                "You are the nearest live covering portfolio seat.",
                "No portfolio seat covers this project either.",
            ),
            TODO_PRIORITY,
        );
        match written {
            Ok(_) => {}
            Err(DaemonError::InvalidParam(code))
                if code == OPERATOR_TODO_LIMIT || code == OPERATOR_TODO_CLOSED =>
            {
                tracing::warn!(
                    operation = %facts.operation_id,
                    project = %project_id,
                    reason = %code,
                    "manager action failure has no responsible owner and no operator to-do"
                );
            }
            Err(error) => return Err(error),
        }
        Ok(ActionFailureRoute::Unrouted)
    }

    fn record_action_failure_manager_notice(
        &self,
        config: &rsi_common::harness_manager::HarnessManagerConfigV1,
        facts: &ActionFailureFacts,
    ) -> Result<ActionFailureRoute> {
        let (job_id, _, _) = self.manager_action_watch_identity(config);
        let subject = facts.subject();
        let version = facts.version();
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_notices
             WHERE job_id=?1 AND kind='ledger_change' AND subject_id=?2 AND subject_version=?3)",
            params![job_id.to_string(), subject, version],
            |row| row.get(0),
        )?;
        if exists {
            return Ok(ActionFailureRoute::AlreadyRecorded);
        }
        self.ensure_manager_action_watch(config, &format!("{subject}:{version}"))?;
        manager_action_notice_fault(ManagerActionNoticeFault::AfterEscalationWatch)?;
        let recorded_at = super::harness_manager_v2::now();
        self.conn.execute(
            "INSERT INTO harness_manager_notices
             (id,job_id,project_id,manager_session_id,scope_version,epic_id,direction,
              source_session_id,recipient_session_id,kind,subject_id,subject_version,
              state_json,recorded_at,queued_at)
             VALUES(?1,?2,?3,?4,?5,NULL,'to_manager',NULL,?4,'ledger_change',?6,?7,?8,?9,?9)",
            params![
                Uuid::new_v4().to_string(),
                job_id.to_string(),
                config.project_id.to_string(),
                config.manager_session_id.to_string(),
                config.row_version,
                subject,
                version,
                serde_json::to_string(&facts.state_json())?,
                recorded_at,
            ],
        )?;
        manager_action_notice_fault(ManagerActionNoticeFault::AfterEscalationNotice)?;
        self.refresh_manager_notice_job(job_id)?;
        Ok(ActionFailureRoute::ManagerNotice(job_id))
    }

    /// Escalate `receipt` when its queue obligation was retired because the
    /// scope it ran under is no longer current (retired at insertion, as the
    /// trigger does for a finish after a scope change). A pending obligation is
    /// left to the normal notice or to the scope-retirement drain.
    pub(crate) fn escalate_manager_action_failure_if_retired(
        &self,
        project_id: Uuid,
        manager_session_id: Uuid,
        scope_version: i64,
        receipt: &ManagerActionReceiptV2,
    ) -> Result<ActionFailureRoute> {
        if !is_failure_state(receipt.state) {
            return Ok(ActionFailureRoute::NotAFailure);
        }
        let retired: Option<bool> = self
            .conn
            .query_row(
                "SELECT retired_at IS NOT NULL AND reconciled_at IS NULL
                 FROM harness_manager_action_notice_queue
                 WHERE operation_id=?1 AND operation_row_version=?2",
                params![receipt.operation_id.to_string(), receipt.row_version],
                |row| row.get(0),
            )
            .optional()?;
        if retired != Some(true) {
            return Ok(ActionFailureRoute::StillCurrent);
        }
        self.escalate_manager_action_failure(
            project_id,
            &ActionFailureFacts::from_receipt(receipt, manager_session_id, scope_version),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::manager_actions::ManagerActionOriginV2;
    use crate::store::manager_coordinator::tests::fixture;
    use crate::store::manager_notices::{
        ManagerActionNoticeFault, manager_action_notice_fail_next,
    };
    use rsi_common::harness_manager::{ConfigureHarnessManagerRequestV1, HarnessManagerConfigV1};
    use rsi_common::harness_manager_v2::ManagerLaunchChoiceV2;
    use rsi_common::harness_manager_v2::{
        AgentManagerControlRequestV2, ManagerActionV2, ManagerCapabilityV2, ManagerFenceV2,
        ManagerOperatingModeV2, ManagerPolicyV2,
    };
    use rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1;
    use rsi_common::types::{Session, SessionProvider, SessionStatus};

    /// A root portfolio node whose live seat covers `project`; returns the seat.
    fn cover_with_portfolio_seat(store: &Store, project: Uuid, status: SessionStatus) -> Uuid {
        let seat = Uuid::new_v4();
        let mut row = crate::test_support::test_session(seat, "/tmp/portfolio-2475".into());
        row.status = status;
        store.insert_session(&row).unwrap();
        store
            .configure_portfolio_node(
                &ConfigurePortfolioNodeRequestV1 {
                    node_id: None,
                    parent_node_id: None,
                    adopt_node_ids: Vec::new(),
                    expected_parent_grant_version: None,
                    tier_label: "area".into(),
                    seat_session_id: seat,
                    project_ids: vec![project],
                    allowed_launches: vec![ManagerLaunchChoiceV2 {
                        provider: SessionProvider::Claude,
                        model: "claude-opus-5-5".into(),
                        effort: Some("high".into()),
                    }],
                    policy: ManagerPolicyV2::default(),
                    child_policy: None,
                    max_direct_reports: 5,
                    expected_node_grant_version: 0,
                    expected_authority_epoch: 0,
                    idempotency_key: format!("cover-{seat}"),
                },
                crate::store::portfolio_nodes::PortfolioGrantor::Operator,
                "operator:test",
            )
            .unwrap();
        seat
    }

    fn portfolio_wakes(store: &Store, operation: Uuid) -> Vec<ScheduledJob> {
        store
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| {
                job.name
                    .starts_with(&format!("action-failure-escalation:{operation}:"))
            })
            .collect()
    }

    fn failure_todos(store: &Store, operation: Uuid) -> Vec<(String, String)> {
        let mut statement = store
            .conn
            .prepare(
                "SELECT title,how FROM operator_todos
                 WHERE todo_key LIKE ?1 AND status='open'",
            )
            .unwrap();
        statement
            .query_map([format!("action-failure:{operation}:%")], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }

    fn policy() -> ManagerPolicyV2 {
        ManagerPolicyV2 {
            mode: ManagerOperatingModeV2::Execute,
            capabilities: vec![ManagerCapabilityV2::LeadControl],
            ..Default::default()
        }
    }

    /// Queue and claim one `pause_lead` under the current scope.
    fn claim_action(
        store: &Store,
        config: &HarnessManagerConfigV1,
        lead: &Session,
        key: &str,
    ) -> crate::store::manager_actions::ManagerActionClaimV2 {
        let policy_version = store
            .get_harness_manager_policy(config.project_id)
            .unwrap()
            .unwrap()
            .row_version;
        let epic = lead.parent_id.unwrap();
        store
            .enqueue_manager_action(
                ManagerActionOriginV2::Agent {
                    caller: config.manager_session_id,
                },
                AgentManagerControlRequestV2::new(
                    ManagerFenceV2 {
                        scope_version: config.row_version,
                        policy_version,
                    },
                    key.into(),
                    ManagerActionV2::PauseLead {
                        epic_id: epic,
                        expected: store.manager_action_lead_fence(epic).unwrap(),
                        reason: "exercise the failure escalation".into(),
                    },
                ),
            )
            .unwrap();
        store.claim_manager_action(Uuid::new_v4()).unwrap().unwrap()
    }

    fn reconfigure(
        store: &Store,
        config: &HarnessManagerConfigV1,
        epic_ids: Option<Vec<Uuid>>,
    ) -> Result<HarnessManagerConfigV1> {
        let current = store
            .get_harness_manager(config.project_id)?
            .unwrap()
            .row_version;
        store.configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: config.project_id,
            session_id: config.manager_session_id,
            epic_ids,
            group_ids: Vec::new(),
            expected_row_version: current,
        })
    }

    fn escalations(store: &Store, operation: Uuid) -> Vec<(String, i64, bool, Value)> {
        let mut statement = store
            .conn
            .prepare(
                "SELECT manager_session_id,scope_version,retired_at IS NULL,state_json
                 FROM harness_manager_notices
                 WHERE kind='ledger_change' AND subject_id=?1 ORDER BY sequence",
            )
            .unwrap();
        statement
            .query_map([format!("action_failure:{operation}")], |row| {
                let state: String = row.get(3)?;
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    serde_json::from_str(&state).unwrap(),
                ))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }

    fn result_notices(store: &Store, operation: Uuid) -> i64 {
        store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE kind='action_result' AND subject_id=?1",
                [operation.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn interrupted_failure_notice_then_scope_change_escalates_once_to_the_current_owner_2455() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("action-failure-escalation.db");
        let store = Store::open_test_fixture(&database).unwrap();
        let (config, lead) = fixture(&store, policy());
        let claim = claim_action(&store, &config, &lead, "fails-before-scope-change");
        let receipt = store
            .finish_manager_action(
                &claim,
                ManagerActionStateV2::Failed,
                "worker_launch_refused",
            )
            .unwrap();
        // Notice construction is interrupted: only the queue obligation exists.
        manager_action_notice_fail_next(ManagerActionNoticeFault::AfterWatch);
        assert!(
            store
                .reconcile_manager_action_notice(receipt.operation_id)
                .is_err()
        );
        assert_eq!(result_notices(&store, receipt.operation_id), 0);

        // The operator reconfigures the manager to a project-wide scope.
        let current = reconfigure(&store, &config, None).unwrap();
        assert!(current.row_version > config.row_version);
        drop(store);

        let reopened = Store::open(&database).unwrap();
        reopened.reconcile_harness_manager_watches().unwrap();
        for _ in 0..2 {
            // A repeated sweep or a late reconcile adds nothing.
            reopened.reconcile_harness_manager_watches().unwrap();
            assert_eq!(
                reopened
                    .reconcile_manager_action_notice(receipt.operation_id)
                    .unwrap(),
                None
            );
        }
        let notices = escalations(&reopened, receipt.operation_id);
        assert_eq!(notices.len(), 1, "{notices:?}");
        let (manager, scope, live, state) = &notices[0];
        assert_eq!(manager, &current.manager_session_id.to_string());
        assert_eq!(*scope, current.row_version);
        assert!(live);
        assert_eq!(state["record_kind"], ACTION_FAILURE_ESCALATION);
        assert_eq!(state["operation_id"], receipt.operation_id.to_string());
        assert_eq!(state["state"], "failed");
        assert_eq!(state["outcome"], "worker_launch_refused");
        assert_eq!(state["receipt_row_version"], receipt.row_version);
        assert_eq!(state["original_scope_version"], config.row_version);
        // Redacted: no request, reason text or operator result rides along.
        assert!(state.get("request").is_none() && state.get("operator_result").is_none());
        assert!(
            !state
                .to_string()
                .contains("exercise the failure escalation")
        );

        // Old authority stays revoked: the receipt is still terminal, the old
        // routine notice stays retired and unmaterialized, nothing re-queued.
        assert_eq!(result_notices(&reopened, receipt.operation_id), 0);
        let (state_now, queued): (String, i64) = reopened
            .conn
            .query_row(
                "SELECT state,(SELECT count(*) FROM harness_manager_v2_operations
                               WHERE state IN ('queued','running'))
                 FROM harness_manager_v2_operations WHERE id=?1",
                [receipt.operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((state_now.as_str(), queued), ("failed", 0));
        let obligation: (bool, bool) = reopened
            .conn
            .query_row(
                "SELECT reconciled_at IS NULL,retired_at IS NOT NULL
                 FROM harness_manager_action_notice_queue WHERE operation_id=?1",
                [receipt.operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(obligation, (true, true));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn each_unfinished_terminal_state_escalates_and_success_stays_suppressed_2455() {
        for (state, expected) in [
            (ManagerActionStateV2::Failed, 1),
            (ManagerActionStateV2::Blocked, 1),
            (ManagerActionStateV2::Uncertain, 1),
            (ManagerActionStateV2::Revoked, 1),
            (ManagerActionStateV2::Succeeded, 0),
        ] {
            let store = Store::open_in_memory().unwrap();
            let (config, lead) = fixture(&store, policy());
            let claim = claim_action(&store, &config, &lead, "terminal-before-scope-change");
            let receipt = store
                .finish_manager_action(&claim, state, "test_terminal")
                .unwrap();
            reconfigure(&store, &config, None).unwrap();
            assert_eq!(
                escalations(&store, receipt.operation_id).len(),
                expected,
                "{state:?}"
            );
            // The routine notice of the old scope is never materialized.
            assert_eq!(result_notices(&store, receipt.operation_id), 0, "{state:?}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn failure_finishing_after_the_scope_changed_escalates_in_the_finish_commit_2455() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let claim = claim_action(&store, &config, &lead, "finishes-after-scope-change");
        let current = reconfigure(&store, &config, None).unwrap();
        let receipt = store
            .finish_manager_action(&claim, ManagerActionStateV2::Revoked, "scope_changed")
            .unwrap();
        // A repeat of the post-commit reconcile adds nothing.
        assert_eq!(
            store
                .reconcile_manager_action_notice(receipt.operation_id)
                .unwrap(),
            None
        );
        let notices = escalations(&store, receipt.operation_id);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].0, current.manager_session_id.to_string());
        assert_eq!(notices[0].3["state"], "revoked");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn interrupted_escalation_rolls_back_and_retry_records_exactly_one_2455() {
        for fault in [
            ManagerActionNoticeFault::AfterEscalationWatch,
            ManagerActionNoticeFault::AfterEscalationNotice,
        ] {
            let store = Store::open_in_memory().unwrap();
            let (config, lead) = fixture(&store, policy());
            let claim = claim_action(&store, &config, &lead, "escalation-interrupted");
            let receipt = store
                .finish_manager_action(&claim, ManagerActionStateV2::Failed, "test_terminal")
                .unwrap();
            manager_action_notice_fail_next(fault);
            assert!(reconfigure(&store, &config, None).is_err(), "{fault:?}");
            // The edit and its drain are one transaction: nothing half-done.
            assert_eq!(
                store
                    .get_harness_manager(config.project_id)
                    .unwrap()
                    .unwrap()
                    .row_version,
                config.row_version
            );
            assert!(escalations(&store, receipt.operation_id).is_empty());
            reconfigure(&store, &config, None).unwrap();
            assert_eq!(
                escalations(&store, receipt.operation_id).len(),
                1,
                "{fault:?}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_later_scope_change_carries_the_unsettled_escalation_to_the_next_owner_2455() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let claim = claim_action(&store, &config, &lead, "carried-forward");
        let receipt = store
            .finish_manager_action(&claim, ManagerActionStateV2::Blocked, "test_terminal")
            .unwrap();
        let second = reconfigure(&store, &config, None).unwrap();
        let third = reconfigure(&store, &config, Some(vec![lead.parent_id.unwrap()])).unwrap();
        assert!(third.row_version > second.row_version);
        let notices = escalations(&store, receipt.operation_id);
        let live: Vec<_> = notices.iter().filter(|notice| notice.2).collect();
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert_eq!(live.len(), 1, "{notices:?}");
        assert_eq!(live[0].1, third.row_version);
        // The original identity survives the carry.
        assert_eq!(live[0].3["original_scope_version"], config.row_version);
        assert_eq!(live[0].3["state"], "blocked");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn a_still_current_scope_keeps_the_normal_notice_and_adds_no_escalation_2455() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let claim = claim_action(&store, &config, &lead, "scope-unchanged");
        let receipt = store
            .finish_manager_action(&claim, ManagerActionStateV2::Failed, "test_terminal")
            .unwrap();
        store
            .reconcile_manager_action_notice(receipt.operation_id)
            .unwrap();
        assert_eq!(result_notices(&store, receipt.operation_id), 1);
        assert!(escalations(&store, receipt.operation_id).is_empty());
    }

    fn assert_redacted_portfolio_wake(wake: &ScheduledJob, operation: Uuid, seat: Uuid) {
        assert_eq!(wake.wake_mode, WakeMode::Resume);
        assert_eq!(wake.wake_session_id, Some(seat));
        for needle in [
            operation.to_string(),
            "failed".to_string(),
            "worker_launch_refused".to_string(),
        ] {
            assert!(wake.message.contains(&needle), "{needle}: {}", wake.message);
        }
        // Redacted: no request text rides along.
        assert!(!wake.message.contains("exercise the failure escalation"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn revoked_scope_routes_the_failure_once_to_the_live_covering_portfolio_seat_2475() {
        let store = Store::open_in_memory().unwrap();
        let (config, lead) = fixture(&store, policy());
        let seat = cover_with_portfolio_seat(&store, config.project_id, SessionStatus::Running);
        let claim = claim_action(&store, &config, &lead, "fails-before-revocation");
        let receipt = store
            .finish_manager_action(
                &claim,
                ManagerActionStateV2::Failed,
                "worker_launch_refused",
            )
            .unwrap();
        // The operator revokes the project manager's scope outright.
        let revoked = reconfigure(&store, &config, Some(Vec::new())).unwrap();
        assert!(revoked.is_revoked());
        for _ in 0..2 {
            // A restart sweep, a late reconcile and a direct retry add nothing.
            store.reconcile_harness_manager_watches().unwrap();
            store
                .reconcile_manager_action_notice(receipt.operation_id)
                .unwrap();
            let facts = ActionFailureFacts::from_receipt(
                &receipt,
                config.manager_session_id,
                config.row_version,
            );
            assert_eq!(
                store
                    .escalate_manager_action_failure(config.project_id, &facts)
                    .unwrap(),
                ActionFailureRoute::AlreadyRecorded
            );
        }
        let wakes = portfolio_wakes(&store, receipt.operation_id);
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert_redacted_portfolio_wake(&wakes[0], receipt.operation_id, seat);
        // No manager notice, no re-queue, no to-do: the seat is the owner.
        assert!(escalations(&store, receipt.operation_id).is_empty());
        assert!(failure_todos(&store, receipt.operation_id).is_empty());
        assert_eq!(result_notices(&store, receipt.operation_id), 0);
        let state: String = store
            .conn
            .query_row(
                "SELECT state FROM harness_manager_v2_operations WHERE id=?1",
                [receipt.operation_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "failed");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn absent_scope_routes_the_failure_once_to_the_live_covering_portfolio_seat_2475() {
        let store = Store::open_in_memory().unwrap();
        let (config, _lead) = fixture(&store, policy());
        // A project that never had a manager scope.
        let project = Uuid::new_v4();
        let now = chrono::Utc::now();
        store
            .insert_project(&rsi_common::types::Project {
                id: project,
                name: format!("No manager {project}"),
                path: None,
                description: None,
                color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        assert!(
            store
                .get_harness_manager_notice_config(project)
                .unwrap()
                .is_none()
        );
        let seat = cover_with_portfolio_seat(&store, project, SessionStatus::Running);
        let operation = Uuid::new_v4();
        let facts = ActionFailureFacts {
            operation_id: operation,
            receipt_row_version: 3,
            state: ManagerActionStateV2::Failed,
            outcome: Some("worker_launch_refused".into()),
            action_kind: "pause_lead".into(),
            target_session_id: None,
            original_manager: config.manager_session_id,
            original_scope: 1,
        };
        let first = store
            .escalate_manager_action_failure(project, &facts)
            .unwrap();
        assert_eq!(first, ActionFailureRoute::PortfolioSeat(seat));
        for _ in 0..2 {
            assert_eq!(
                store
                    .escalate_manager_action_failure(project, &facts)
                    .unwrap(),
                ActionFailureRoute::AlreadyRecorded
            );
        }
        let wakes = portfolio_wakes(&store, operation);
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert_redacted_portfolio_wake(&wakes[0], operation, seat);
        // A different receipt version of the same operation is a new failure.
        let later = ActionFailureFacts {
            receipt_row_version: 4,
            ..facts
        };
        store
            .escalate_manager_action_failure(project, &later)
            .unwrap();
        assert_eq!(portfolio_wakes(&store, operation).len(), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn no_live_covering_seat_stays_unrouted_but_is_kept_as_one_operator_todo_2475() {
        for covered_by_dead_seat in [false, true] {
            let store = Store::open_in_memory().unwrap();
            let (config, lead) = fixture(&store, policy());
            if covered_by_dead_seat {
                // The covering seat exists but is no longer live.
                let seat =
                    cover_with_portfolio_seat(&store, config.project_id, SessionStatus::Running);
                store
                    .conn
                    .execute(
                        "UPDATE sessions SET status='Archived' WHERE id=?1",
                        [seat.to_string()],
                    )
                    .unwrap();
                assert!(
                    store
                        .on_call_portfolio_seats(config.project_id)
                        .unwrap()
                        .is_empty()
                );
            }
            let claim = claim_action(&store, &config, &lead, "fails-before-revocation");
            let receipt = store
                .finish_manager_action(
                    &claim,
                    ManagerActionStateV2::Failed,
                    "worker_launch_refused",
                )
                .unwrap();
            reconfigure(&store, &config, Some(Vec::new())).unwrap();
            let facts = ActionFailureFacts::from_receipt(
                &receipt,
                config.manager_session_id,
                config.row_version,
            );
            for _ in 0..2 {
                assert_eq!(
                    store
                        .escalate_manager_action_failure(config.project_id, &facts)
                        .unwrap(),
                    ActionFailureRoute::Unrouted
                );
            }
            assert!(portfolio_wakes(&store, receipt.operation_id).is_empty());
            let todos = failure_todos(&store, receipt.operation_id);
            assert_eq!(todos.len(), 1, "{todos:?}");
            assert!(todos[0].1.contains(&receipt.operation_id.to_string()));
            assert!(todos[0].1.contains("worker_launch_refused"));
            assert!(!todos[0].1.contains("exercise the failure escalation"));
        }
    }
}
