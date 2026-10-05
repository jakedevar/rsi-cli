//! `AgentManagerLaunchIssueWorker` (#1100): admission of one Issue-bound
//! `create_session` action. The action receipt, the Issue note naming the
//! worker, the Issue's InProgress status and the Issue binding all commit in
//! ONE immediate transaction: any refusal (policy, allowlist, capacity, Issue
//! state, authority) rolls everything back, leaving no action, note or status
//! change. The binding is the action's own journalled payload, so no schema
//! change is needed.

use super::{ACTION_KIND, ManagerActionOriginV2, ManagerIssueBindingV1};
use crate::error::Result;
use crate::store::Store;
use crate::store::harness_manager_v2::refused;
use rsi_common::harness_manager_v2::{
    AgentManagerControlRequestV2, ManagerActionReceiptV2, ManagerActionV2, ManagerCapabilityV2,
    ManagerFenceV2,
};
use rsi_common::manager_issue_worker::AgentManagerLaunchIssueWorkerRequestV1;
use rsi_common::rpc::{AgentUpdateIssueRequestV1, AgentUpdateIssueStatusRequestV1};
use rsi_common::types::{Issue, IssueStatus, SessionKind};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

/// The admitted launch and the Issue as the call left it.
#[derive(Debug, Clone)]
pub struct ManagerIssueWorkerAdmission {
    pub receipt: ManagerActionReceiptV2,
    pub worker_session_id: Uuid,
    pub issue: Issue,
}

fn unavailable() -> crate::error::DaemonError {
    refused("manager_issue_worker_issue_unavailable")
}

impl Store {
    /// The `(project, Issue)` a worker was bound to by a launch that created
    /// it (`running` or `succeeded` action). Read-only; used for the worker's
    /// own-Issue read authority and its catalog projection.
    pub(crate) fn bound_issue_for_worker_on(
        conn: &Connection,
        worker: Uuid,
    ) -> Result<Option<(Uuid, Uuid)>> {
        let found: Option<(String, String)> = conn
            .query_row(
                "SELECT o.project_id, json_extract(o.payload_json,'$.issue_binding.issue_id')
                 FROM harness_manager_v2_operations o
                 JOIN issues i ON i.project_id=o.project_id
                  AND i.id=json_extract(o.payload_json,'$.issue_binding.issue_id')
                 WHERE o.kind=?1 AND o.target_session_id=?2
                   AND o.state IN ('running','succeeded')
                 ORDER BY o.created_at DESC, o.id DESC LIMIT 1",
                params![ACTION_KIND, worker.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        found
            .map(|(project, issue)| {
                Ok((
                    Uuid::parse_str(&project)
                        .map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
                    Uuid::parse_str(&issue)
                        .map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
                ))
            })
            .transpose()
    }

    /// The current manager's `create_session` request for one bound worker.
    /// The fence is the daemon's own current scope and policy versions: the
    /// caller names the Issue and the brief, never authority.
    fn manager_issue_worker_control_request(
        &self,
        caller: Uuid,
        request: &AgentManagerLaunchIssueWorkerRequestV1,
    ) -> Result<AgentManagerControlRequestV2> {
        let (config, is_manager) = self.manager_config_for_caller(caller)?;
        if !is_manager {
            return Err(refused("manager_v2_scope_denied"));
        }
        let grant = self
            .get_harness_manager_policy(config.project_id)?
            .ok_or_else(|| refused("manager_v2_grant_required"))?;
        let parent_id = match (request.parent_epic_id, config.epic_ids.as_slice()) {
            (Some(epic), _) => epic,
            (None, [only]) => *only,
            (None, _) => return Err(refused("manager_issue_worker_parent_required")),
        };
        let query = format!(
            "{}\n\nYou are bound to Issue #{}: read it with AgentGetIssue {{\"display_number\": {}}} (you may read only this Issue).",
            request.brief.trim_end(),
            request.issue,
            request.issue
        );
        Ok(AgentManagerControlRequestV2 {
            fence: ManagerFenceV2 {
                scope_version: config.row_version,
                policy_version: grant.row_version,
            },
            idempotency_key: request.idempotency_key.clone(),
            operation: ManagerActionV2::CreateSession {
                parent_id,
                kind: SessionKind::Task,
                query,
                launch: request.launch.clone(),
            },
        })
    }

    /// Admit the launch and record the Issue effects in one transaction. A
    /// replay under the same key returns the original receipt and applies no
    /// effect again.
    pub fn enqueue_manager_issue_worker(
        &self,
        caller: Uuid,
        request: &AgentManagerLaunchIssueWorkerRequestV1,
    ) -> Result<ManagerIssueWorkerAdmission> {
        request.validate().map_err(refused)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let issue_authority = Self::resolve_agent_issue_coordinator_authority_tx(&tx, caller)
            .map_err(|_| refused("manager_issue_worker_authority_denied"))?;
        let issue_id = Self::resolve_agent_issue_target_tx(
            &tx,
            issue_authority.project_id,
            None,
            Some(request.issue),
        )
        .map_err(|_| unavailable())?;
        let control = self.manager_issue_worker_control_request(caller, request)?;
        let capability = control.operation.capability();
        debug_assert_eq!(capability, ManagerCapabilityV2::SessionCreate);
        let binding = ManagerIssueBindingV1 {
            issue_id,
            display_number: request.issue,
        };
        let receipt = self.enqueue_manager_action_with_source_on(
            ManagerActionOriginV2::Agent { caller },
            control,
            None,
            Some(&binding),
        )?;
        let worker = receipt
            .target_session_id
            .ok_or_else(|| refused("manager_v2_target_unavailable"))?;
        let prior = Self::get_issue_in_project_tx(&tx, issue_authority.project_id, issue_id)?
            .ok_or_else(unavailable)?;
        if receipt.deduplicated {
            tx.commit()?;
            return Ok(ManagerIssueWorkerAdmission {
                receipt,
                worker_session_id: worker,
                issue: prior,
            });
        }
        if prior.archived_at.is_some()
            || !matches!(prior.status, IssueStatus::Open | IssueStatus::InProgress)
        {
            return Err(unavailable());
        }
        let key = |step: &str| format!("launch-issue-worker:{}:{step}", receipt.operation_id);
        let note = format!(
            "\n\n---\nWorker {worker} launched by manager {caller} for this Issue (action {}).",
            receipt.operation_id
        );
        let noted = Self::agent_update_issue_in_tx(
            &tx,
            issue_authority,
            &AgentUpdateIssueRequestV1 {
                issue_id: Some(issue_id),
                display_number: None,
                expected_row_version: prior.row_version,
                idempotency_key: key("note"),
                title: None,
                body: Some(format!("{}{note}", prior.body)),
                labels: None,
                priority: None,
                clear_priority: false,
                assignee: None,
                clear_assignee: false,
            },
        )?;
        let issue = if noted.issue.status == IssueStatus::InProgress {
            noted.issue
        } else {
            Self::agent_update_issue_status_in_tx(
                &tx,
                issue_authority,
                &AgentUpdateIssueStatusRequestV1 {
                    issue_id: Some(issue_id),
                    display_number: None,
                    status: IssueStatus::InProgress,
                    expected_row_version: noted.issue.row_version,
                    idempotency_key: key("status"),
                },
            )?
            .issue
        };
        tx.commit()?;
        Ok(ManagerIssueWorkerAdmission {
            receipt,
            worker_session_id: worker,
            issue,
        })
    }

    /// `(manager, worker)` of every succeeded Issue-bound launch whose worker
    /// is still live and has no enabled terminal watch from that manager (#1115).
    /// A terminal worker is skipped: its watch either fired or cannot be told
    /// apart from a consumed one.
    pub fn list_issue_worker_unwatched(&self) -> Result<Vec<(Uuid, Uuid)>> {
        let mut stmt = self.conn.prepare(
            "SELECT json_extract(o.payload_json,'$.origin.caller'), o.target_session_id
             FROM harness_manager_v2_operations o
             JOIN sessions w ON w.id=o.target_session_id
             WHERE o.kind=?1 AND o.state='succeeded'
               AND json_extract(o.payload_json,'$.origin.origin')='agent'
               AND json_extract(o.payload_json,'$.issue_binding') IS NOT NULL
               AND w.status IN ('Starting','Running','WaitingApproval')
               AND NOT EXISTS (
                 SELECT 1 FROM scheduled_jobs j
                 WHERE j.enabled=1
                   AND j.wake_mode='on_terminal:' || o.target_session_id
                   AND j.wake_session_id=json_extract(o.payload_json,'$.origin.caller'))",
        )?;
        let rows = stmt
            .query_map(params![ACTION_KIND], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::new();
        for (caller, worker) in rows {
            out.push((
                Uuid::parse_str(&caller)
                    .map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
                Uuid::parse_str(&worker)
                    .map_err(|_| refused("manager_v2_invalid_stored_identity"))?,
            ));
        }
        Ok(out)
    }

    /// The Issue binding journalled with one admitted action, if any.
    pub(crate) fn manager_action_issue_binding(
        &self,
        operation_id: Uuid,
    ) -> Result<Option<ManagerIssueBindingV1>> {
        let raw: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT json_extract(payload_json,'$.issue_binding')
                 FROM harness_manager_v2_operations WHERE id=?1 AND kind=?2",
                params![operation_id.to_string(), ACTION_KIND],
                |row| row.get(0),
            )
            .optional()?;
        raw.flatten()
            .map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }
}
