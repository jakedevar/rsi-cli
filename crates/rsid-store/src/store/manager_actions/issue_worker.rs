//! `AgentManagerLaunchIssueWorker` (#1100): admission of one Issue-bound
//! `create_session` action. The action receipt, the Issue note naming the
//! worker, the Issue's InProgress status and the Issue binding all commit in
//! ONE immediate transaction: any refusal (policy, allowlist, capacity, Issue
//! state, authority) rolls everything back, leaving no action, note or status
//! change. The binding is the action's own journalled payload, so no schema
//! change is needed.

use super::{
    ACTION_KIND, ManagerActionOperationV2, ManagerActionOriginV2, ManagerIssueBindingV1, state_name,
};
use crate::error::Result;
use crate::store::Store;
use crate::store::harness_manager_v2::{ManagerCallerV1, refused};
use rsi_common::global_manager::{MANAGER_ISSUE_WORKER_ALREADY_LIVE, MANAGER_PROJECT_NOT_IN_SCOPE};
use rsi_common::harness_manager_v2::{
    AgentManagerControlRequestV2, ManagerActionReceiptV2, ManagerActionStateV2, ManagerActionV2,
    ManagerCapabilityV2, ManagerFenceV2, ManagerSandboxSourceV1,
};
use rsi_common::manager_issue_worker::{
    AgentManagerLaunchIssueWorkerRequestV1, MANAGER_ISSUE_WORKER_INVALID_REQUEST,
    MANAGER_ISSUE_WORKER_PREDECESSOR_LIVE, MANAGER_ISSUE_WORKER_PREDECESSOR_OTHER_ISSUE,
    MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE,
    MANAGER_ISSUE_WORKER_PREDECESSOR_SANDBOX_UNAVAILABLE,
    MANAGER_ISSUE_WORKER_REVIEWED_UNAVAILABLE,
};
use rsi_common::rpc::{AgentUpdateIssueRequestV1, AgentUpdateIssueStatusRequestV1};
use rsi_common::types::{Issue, IssueStatus, SessionKind, SessionStatus};
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
    /// own-Issue read authority and its catalog projection. "Latest" is
    /// admission order (`rowid`, #1322), as in
    /// [`Self::live_issue_binding_for_worker_on`].
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
                 ORDER BY o.rowid DESC LIMIT 1",
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

    /// #1284: the worker's binding when it is live now: the worker's latest
    /// Issue-bound launch (as [`Self::bound_issue_for_worker_on`]), the worker
    /// session still live, and no later launch of the same Issue (a #1254
    /// successor, or a relaunch after this worker ended) admitted. Returns
    /// `(project, Issue, launch operation)`. A rotation successor is another
    /// session and holds no binding. This is the write authority's binding;
    /// the read authority keeps [`Self::bound_issue_for_worker_on`].
    ///
    /// #1321: a later launch supersedes whatever became of it: an admitted
    /// successor that later failed, blocked or was revoked still holds the
    /// Issue, so a resumed predecessor never regains append authority.
    ///
    /// #1322: "later" is admission order, the operation row's `rowid`, never
    /// `created_at` (wall clock, which can step backwards) or the random id.
    /// Admission inserts the row in its own immediate transaction and
    /// operation rows are never deleted, so SQLite's `max(rowid)+1` allocation
    /// orders them by admission; `harness_manager_v2_operations` has a TEXT
    /// primary key, so `rowid` is the implicit monotonic key and no migration
    /// is needed.
    pub(crate) fn live_issue_binding_for_worker_on(
        conn: &Connection,
        worker: Uuid,
    ) -> Result<Option<(Uuid, Uuid, Uuid)>> {
        let found: Option<(String, String, String)> = conn
            .query_row(
                "SELECT o.id, o.project_id, json_extract(o.payload_json,'$.issue_binding.issue_id')
                 FROM harness_manager_v2_operations o
                 JOIN issues i ON i.project_id=o.project_id
                  AND i.id=json_extract(o.payload_json,'$.issue_binding.issue_id')
                 JOIN sessions w ON w.id=o.target_session_id
                 WHERE o.id=(SELECT b.id FROM harness_manager_v2_operations b
                             WHERE b.kind=?1 AND b.target_session_id=?2
                               AND b.state IN ('running','succeeded')
                               AND json_extract(b.payload_json,'$.issue_binding') IS NOT NULL
                             ORDER BY b.rowid DESC LIMIT 1)
                   AND w.status IN ('Starting','Running','WaitingApproval')
                   AND NOT EXISTS (
                     SELECT 1 FROM harness_manager_v2_operations n
                     WHERE n.kind=?1 AND n.rowid>o.rowid
                       AND json_extract(n.payload_json,'$.issue_binding.issue_id')
                         =json_extract(o.payload_json,'$.issue_binding.issue_id'))",
                params![ACTION_KIND, worker.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let parse = |raw: &str| {
            Uuid::parse_str(raw).map_err(|_| refused("manager_v2_invalid_stored_identity"))
        };
        found
            .map(|(operation, project, issue)| {
                Ok((parse(&project)?, parse(&issue)?, parse(&operation)?))
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
        brief: &str,
    ) -> Result<AgentManagerControlRequestV2> {
        // #1235: the global seat launches inside a granted project with the
        // fence of its own principal; the project manager path is unchanged.
        let (config, policy_version) = match self
            .resolve_manager_caller(caller, request.project_id)?
        {
            ManagerCallerV1::Legacy {
                config,
                is_manager: true,
            } => {
                let grant = self
                    .get_harness_manager_policy(config.project_id)?
                    .ok_or_else(|| refused("manager_v2_grant_required"))?;
                (config, grant.row_version)
            }
            ManagerCallerV1::Global(authority) => (authority.config, authority.grant.row_version),
            ManagerCallerV1::Legacy { .. } | ManagerCallerV1::Area(_) => {
                return Err(refused("manager_v2_scope_denied"));
            }
        };
        let parent_id = match (request.parent_epic_id, config.epic_ids.as_slice()) {
            (Some(epic), _) => epic,
            (None, [only]) => *only,
            (None, _) => return Err(refused("manager_issue_worker_parent_required")),
        };
        let query = format!(
            "{}\n\nYou are bound to Issue #{}: read it with AgentGetIssue {{\"display_number\": {}}} (you may read only this Issue). While your binding is live you may append your handoff to this Issue, and only this one, with AgentUpdateIssue {{\"display_number\": {}, \"expected_row_version\": <its row_version>, \"idempotency_key\": \"<unique>\", \"body\": \"<the current body, unchanged, followed by your section>\"}}: the body must keep the current text as its prefix, and no other field, status change or Issue is allowed. End that handoff and your final message with `Friction: none | #N[, #M] | <one line, not filed because ...>`: the kaizen Issues you filed, or the problem you could not file (\"Improve the line\" in AgentGetAuthorityCatalog).",
            brief.trim_end(),
            request.issue,
            request.issue,
            request.issue
        );
        Ok(AgentManagerControlRequestV2 {
            fence: ManagerFenceV2 {
                scope_version: config.row_version,
                policy_version,
            },
            project_id: request.project_id,
            idempotency_key: request.idempotency_key.clone(),
            operation: ManagerActionV2::CreateSession {
                parent_id,
                kind: SessionKind::Task,
                query,
                launch: request.launch.clone(),
                sandbox_source: request.sandbox_source.clone(),
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
        let issue_authority =
            Self::resolve_agent_issue_manager_authority_tx(&tx, caller, request.project_id, true)
                .map_err(|error| match error {
                crate::error::DaemonError::InvalidParam(code)
                    if code == MANAGER_PROJECT_NOT_IN_SCOPE =>
                {
                    refused(MANAGER_PROJECT_NOT_IN_SCOPE)
                }
                _ => refused("manager_issue_worker_authority_denied"),
            })?;
        let issue_id = Self::resolve_agent_issue_target_tx(
            &tx,
            issue_authority.project_id,
            None,
            Some(request.issue),
        )
        .map_err(|_| unavailable())?;
        let control = match (request.continue_from, request.review_of) {
            (None, None) => {
                self.manager_issue_worker_control_request(caller, request, &request.brief)?
            }
            (None, Some(reviewed)) => {
                let mut control =
                    self.manager_issue_worker_control_request(caller, request, &request.brief)?;
                // A replay reuses the journalled brief: the reviewed Issue's
                // handoff may have grown since.
                if let Some(operation) = self.prior_review_operation(
                    issue_authority.project_id,
                    &request.idempotency_key,
                    reviewed,
                )? {
                    control.operation = operation;
                } else {
                    let reviewed_id = Self::resolve_agent_issue_target_tx(
                        &tx,
                        issue_authority.project_id,
                        None,
                        Some(reviewed),
                    )
                    .map_err(|_| refused(MANAGER_ISSUE_WORKER_REVIEWED_UNAVAILABLE))?;
                    let reviewed_issue = Self::get_issue_in_project_tx(
                        &tx,
                        issue_authority.project_id,
                        reviewed_id,
                    )?
                    .ok_or_else(|| refused(MANAGER_ISSUE_WORKER_REVIEWED_UNAVAILABLE))?;
                    let brief = review_brief(&reviewed_issue, &request.brief);
                    control = self.manager_issue_worker_control_request(caller, request, &brief)?;
                }
                control
            }
            (Some(_), Some(_)) => return Err(refused(MANAGER_ISSUE_WORKER_INVALID_REQUEST)),
            (Some(predecessor), None) => {
                let mut control =
                    self.manager_issue_worker_control_request(caller, request, &request.brief)?;
                // A replay reuses the journalled operation verbatim: the
                // predecessor's transcript or the Issue may have moved since.
                if let Some(operation) = self.prior_continuation_operation(
                    issue_authority.project_id,
                    &request.idempotency_key,
                    predecessor,
                )? {
                    control.operation = operation;
                } else {
                    let body =
                        Self::get_issue_in_project_tx(&tx, issue_authority.project_id, issue_id)?
                            .ok_or_else(unavailable)?
                            .body;
                    let (path, brief) = self.issue_worker_continuation(
                        predecessor,
                        issue_authority.project_id,
                        issue_id,
                        &body,
                        &request.brief,
                    )?;
                    control = self.manager_issue_worker_control_request(caller, request, &brief)?;
                    if let ManagerActionV2::CreateSession { sandbox_source, .. } =
                        &mut control.operation
                    {
                        *sandbox_source = Some(ManagerSandboxSourceV1::Path(path));
                    }
                }
                control
            }
        };
        let capability = control.operation.capability();
        debug_assert_eq!(capability, ManagerCapabilityV2::SessionCreate);
        let binding = ManagerIssueBindingV1 {
            qa_lane: request.qa_lane,
            issue_id,
            display_number: request.issue,
            continue_from: request.continue_from,
            review_of: request.review_of,
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
        // #1235 rule (b): one live Issue-bound worker per Issue across the
        // manager chain. The new action is already journalled in this
        // transaction, so any other live binding refuses and rolls it back.
        if self.manager_issue_has_other_live_worker(issue_id, receipt.operation_id)? {
            return Err(refused(MANAGER_ISSUE_WORKER_ALREADY_LIVE));
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
                project_id: None,
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
                    project_id: None,
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

    /// #1254: the operation an earlier `continue_from` launch under the same
    /// idempotency key journalled in `project`, so a replay matches it.
    /// #1325: use admission order, as in `live_issue_binding_for_worker_on`.
    fn prior_continuation_operation(
        &self,
        project: Uuid,
        idempotency_key: &str,
        predecessor: Uuid,
    ) -> Result<Option<ManagerActionV2>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT json_extract(payload_json,'$.request.operation')
                 FROM harness_manager_v2_operations
                 WHERE kind=?1 AND project_id=?2 AND idempotency_key=?3
                   AND json_extract(payload_json,'$.issue_binding.continue_from')=?4
                 ORDER BY rowid DESC LIMIT 1",
                params![
                    ACTION_KIND,
                    project.to_string(),
                    idempotency_key,
                    predecessor.to_string()
                ],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }

    /// #1590: the operation an earlier `review_of` launch under the same
    /// idempotency key journalled in `project`, so a replay matches it.
    fn prior_review_operation(
        &self,
        project: Uuid,
        idempotency_key: &str,
        reviewed: i64,
    ) -> Result<Option<ManagerActionV2>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT json_extract(payload_json,'$.request.operation')
                 FROM harness_manager_v2_operations
                 WHERE kind=?1 AND project_id=?2 AND idempotency_key=?3
                   AND json_extract(payload_json,'$.issue_binding.review_of')=?4
                 ORDER BY rowid DESC LIMIT 1",
                params![ACTION_KIND, project.to_string(), idempotency_key, reviewed],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }

    /// #1254: check `predecessor` for a `continue_from` launch of `issue_id`
    /// and return its sandbox path (the new worker's source: its committed
    /// `HEAD`) and the brief with the baton preamble in front.
    fn issue_worker_continuation(
        &self,
        predecessor: Uuid,
        project: Uuid,
        issue_id: Uuid,
        issue_body: &str,
        brief: &str,
    ) -> Result<(String, String)> {
        let Some((bound_project, bound_issue)) =
            Self::bound_issue_for_worker_on(&self.conn, predecessor)?
        else {
            return Err(refused(MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE));
        };
        if bound_project != project {
            return Err(refused(MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE));
        }
        if bound_issue != issue_id {
            return Err(refused(MANAGER_ISSUE_WORKER_PREDECESSOR_OTHER_ISSUE));
        }
        let session = self
            .get_session(predecessor)?
            .ok_or_else(|| refused(MANAGER_ISSUE_WORKER_PREDECESSOR_OUT_OF_SCOPE))?;
        if matches!(
            session.status,
            SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
        ) {
            return Err(refused(MANAGER_ISSUE_WORKER_PREDECESSOR_LIVE));
        }
        let root = session
            .sandbox_root
            .filter(|root| root.is_dir())
            .ok_or_else(|| refused(MANAGER_ISSUE_WORKER_PREDECESSOR_SANDBOX_UNAVAILABLE))?;
        let (head, dirty) =
            crate::sandbox::git_worktree::resolve_registered_worktree_head(&root, &root)?
                .ok_or_else(|| refused(MANAGER_ISSUE_WORKER_PREDECESSOR_SANDBOX_UNAVAILABLE))?;
        let final_message = self
            .last_assistant_message_event(predecessor)?
            .map(|event| event.content)
            .unwrap_or_default();
        let path = root
            .to_str()
            .ok_or_else(|| refused(MANAGER_ISSUE_WORKER_PREDECESSOR_SANDBOX_UNAVAILABLE))?
            .to_string();
        let brief = continuation_brief(
            predecessor,
            &head,
            dirty,
            &final_message,
            latest_handoff(issue_body),
            brief,
        );
        Ok((path, brief))
    }

    /// Whether another Issue-bound launch of `issue_id` is live: queued,
    /// running or uncertain, or succeeded with its worker still live (any
    /// manager principal). A terminal worker frees the Issue for a relaunch.
    ///
    /// #1553: a queued launch journalled under an older scope version of its
    /// project can never run (every execution gate refuses it with
    /// `manager_v2_scope_changed`), so it holds no binding. Without this a
    /// launch the displaced seat left behind a deploy drain kept the Issue
    /// "live" until the drain cleared, though no worker would ever start.
    fn manager_issue_has_other_live_worker(
        &self,
        issue_id: Uuid,
        operation_id: Uuid,
    ) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM harness_manager_v2_operations o
             LEFT JOIN sessions w ON w.id=o.target_session_id
             WHERE o.kind=?1 AND o.id<>?2
               AND json_extract(o.payload_json,'$.issue_binding.issue_id')=?3
               AND ((o.state='queued' AND o.scope_version>=COALESCE(
                       (SELECT s.row_version FROM harness_manager_scopes s
                        WHERE s.project_id=o.project_id),0))
                 OR o.state IN ('running','uncertain')
                 OR (o.state='succeeded'
                   AND w.status IN ('Starting','Running','WaitingApproval'))))",
            params![ACTION_KIND, operation_id.to_string(), issue_id.to_string()],
            |row| row.get(0),
        )?)
    }

    /// #1553: the Issue note an Issue-bound launch leaves when it ends without a
    /// worker (`failed`, `blocked` or `revoked`). The launch's own note said a
    /// worker was launched; without this the Issue keeps claiming a worker
    /// that never started. The binding needs no explicit release: it is the
    /// journalled action, and a terminal action holds none
    /// ([`Self::manager_issue_has_other_live_worker`]), so the Issue is free for a
    /// relaunch from this commit on.
    ///
    /// The note is written as the launch's caller while it still holds the
    /// seat, otherwise as the project's current seat (the caller may have been
    /// displaced by an appointment). Returns whether a note was appended;
    /// nothing is written when no manager can write to the Issue or the Issue
    /// is closed or archived.
    pub(crate) fn note_issue_worker_launch_ended(
        &self,
        operation: &ManagerActionOperationV2,
        state: ManagerActionStateV2,
        outcome: &str,
    ) -> Result<bool> {
        if !matches!(
            state,
            ManagerActionStateV2::Failed
                | ManagerActionStateV2::Blocked
                | ManagerActionStateV2::Revoked
        ) {
            return Ok(false);
        }
        let (Some(binding), ManagerActionOriginV2::Agent { caller }) = (
            operation.context.issue_binding.as_ref(),
            &operation.context.origin,
        ) else {
            return Ok(false);
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let authority =
            match Self::resolve_agent_issue_manager_authority_tx(&tx, *caller, None, true) {
                Ok(authority) => authority,
                Err(_) => {
                    let Some(seat) = self
                        .get_harness_manager(operation.project_id)?
                        .and_then(|config| config.current_session_id)
                    else {
                        return Ok(false);
                    };
                    let Ok(authority) =
                        Self::resolve_agent_issue_manager_authority_tx(&tx, seat, None, true)
                    else {
                        return Ok(false);
                    };
                    authority
                }
            };
        if authority.project_id != operation.project_id {
            return Ok(false);
        }
        let Some(prior) =
            Self::get_issue_in_project_tx(&tx, operation.project_id, binding.issue_id)?
        else {
            return Ok(false);
        };
        if prior.archived_at.is_some()
            || !matches!(prior.status, IssueStatus::Open | IssueStatus::InProgress)
        {
            return Ok(false);
        }
        let worker = operation
            .receipt
            .target_session_id
            .map_or_else(|| "its worker".to_string(), |id| format!("worker {id}"));
        let note = format!(
            "\n\n---\nLaunch (action {}) for this Issue ended {} ({outcome}) before {worker} started; its Issue binding is released and the Issue is free for a relaunch.",
            operation.receipt.operation_id,
            state_name(state),
        );
        Self::agent_update_issue_in_tx(
            &tx,
            authority,
            &AgentUpdateIssueRequestV1 {
                project_id: None,
                issue_id: Some(binding.issue_id),
                display_number: None,
                expected_row_version: prior.row_version,
                idempotency_key: format!(
                    "launch-issue-worker:{}:ended",
                    operation.receipt.operation_id
                ),
                title: None,
                body: Some(format!("{}{note}", prior.body)),
                labels: None,
                priority: None,
                clear_priority: false,
                assignee: None,
                clear_assignee: false,
            },
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// `(manager, worker)` of every succeeded Issue-bound launch whose worker
    /// is still live and has no enabled terminal watch from that manager (#1115).
    /// A watch addressed to the project's current seat counts: a displaced
    /// seat's watches move there on appointment (#1553), and re-arming one for
    /// the retired caller would address a session nobody reads.
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
                   AND (j.wake_session_id=json_extract(o.payload_json,'$.origin.caller')
                     OR j.wake_session_id=(SELECT s.manager_session_id
                        FROM harness_manager_scopes s WHERE s.project_id=o.project_id)))",
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

/// The largest query a `create_session` accepts (`text(query, 32_768)`), less
/// room for the Issue-binding sentence appended after the brief.
const CONTINUATION_QUERY_BUDGET: usize = 32_000;
/// The most bytes of each copied excerpt.
const CONTINUATION_EXCERPT_MAX: usize = 6_000;

/// #1254: the Issue body from its latest handoff on: the last line that is a
/// heading naming a handoff, or a `PIPELINE HANDOFF` line. `None` when the
/// Issue holds no handoff.
fn latest_handoff(body: &str) -> Option<&str> {
    let mut start = None;
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let lower = line.trim().to_ascii_lowercase();
        if (lower.starts_with('#') && lower.contains("handoff"))
            || lower.contains("pipeline handoff")
        {
            start = Some(offset);
        }
        offset += line.len();
    }
    start.map(|start| body[start..].trim())
}

/// Copied text as a quoted, secret-redacted block of at most `max` bytes
/// plus the truncation marker.
fn quoted_excerpt(text: &str, max: usize) -> String {
    let quoted = rsi_common::daemon_handoff::redact_secrets(text.trim())
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    if quoted.len() <= max {
        return quoted;
    }
    let mut end = max;
    while !quoted.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n> [truncated]", &quoted[..end])
}

/// The most bytes of the reviewed Issue's own text (intent, acceptance
/// criteria) copied into a reviewer's brief.
const REVIEW_ISSUE_TEXT_MAX: usize = 5_000;
/// The largest brief-plus-copied-handoff text: the `create_session` query limit
/// (32768) less the Issue-binding sentence the launch appends after the brief.
const REVIEW_QUERY_BUDGET: usize = 30_500;

/// #1590: the Issue body from its latest handoff heading on, preferring a
/// markdown heading naming a handoff (so a `PIPELINE HANDOFF` line inside a
/// handoff does not cut its start off); otherwise [`latest_handoff`].
fn review_handoff_start(body: &str) -> Option<usize> {
    let mut start = None;
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let lower = line.trim().to_ascii_lowercase();
        if lower.starts_with('#') && lower.contains("handoff") {
            start = Some(offset);
        }
        offset += line.len();
    }
    start.or_else(|| latest_handoff(body).map(|text| body.len() - text.len()))
}

/// Copied text as a quoted, secret-redacted block of at most about `max`
/// bytes. An oversized text keeps its head and its TAIL (a handoff ends with
/// its landing filters) and names the omitted byte count between them.
fn quoted_head_tail(text: &str, max: usize) -> String {
    let quote = |text: &str| {
        rsi_common::daemon_handoff::redact_secrets(text.trim())
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let quoted = quote(text);
    if quoted.len() <= max {
        return quoted;
    }
    let keep = max.saturating_sub(80);
    let mut head_end = keep * 2 / 5;
    while !quoted.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = quoted.len() - (keep - head_end);
    while !quoted.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    format!(
        "{}\n> [... {omitted} bytes omitted from the middle ...]\n{}",
        &quoted[..head_end],
        &quoted[tail_start..]
    )
}

/// #1590: a reviewer's brief: the reviewed Issue's text and its complete
/// latest handoff (the bound reviewer cannot read that Issue), then the
/// manager's brief.
fn review_brief(reviewed: &Issue, brief: &str) -> String {
    let (text, handoff) = match review_handoff_start(&reviewed.body) {
        Some(start) => (&reviewed.body[..start], Some(&reviewed.body[start..])),
        None => (reviewed.body.as_str(), None),
    };
    let header = format!(
        "You review the work of Issue #{} (\"{}\", status {:?}). You cannot read that Issue: the daemon copied its text and latest handoff below (#1590). Check them against the committed code the manager names, use the handoff's test filters and landing filters, and report on this Issue.\n\n{}",
        reviewed.display_number,
        reviewed.title.replace('\n', " "),
        reviewed.status,
        rsi_common::daemon_handoff::UNTRUSTED_TEXT_NOTICE
    );
    let text_block = quoted_head_tail(text, REVIEW_ISSUE_TEXT_MAX);
    let room = REVIEW_QUERY_BUDGET
        .saturating_sub(brief.len() + header.len() + text_block.len() + 256)
        .max(1_000);
    let handoff_block = handoff.map_or_else(
        || "> (the Issue holds no handoff section)".to_string(),
        |text| quoted_head_tail(text, room),
    );
    format!(
        "{header}\n\n## Issue #{} text\n\n{text_block}\n\n## Issue #{} latest handoff\n\n{handoff_block}\n\n---\n\n{}",
        reviewed.display_number,
        reviewed.display_number,
        brief.trim()
    )
}

/// The successor's brief: the baton preamble, then the manager's brief.
fn continuation_brief(
    predecessor: Uuid,
    head: &str,
    dirty: bool,
    final_message: &str,
    handoff: Option<&str>,
    brief: &str,
) -> String {
    let dirty_note = if dirty {
        " Its sandbox also had uncommitted changes; they were NOT copied, so rebuild anything its handoff names as uncommitted."
    } else {
        ""
    };
    let header = format!(
        "You continue worker {predecessor} on this Issue: its context filled and it passed the baton (#1254). Your sandbox branches from its committed HEAD {head}.{dirty_note} Read its final message and the Issue's latest handoff below, check them against the code, then carry on without redoing finished work.\n\n{}",
        rsi_common::daemon_handoff::UNTRUSTED_TEXT_NOTICE
    );
    let room = CONTINUATION_QUERY_BUDGET.saturating_sub(brief.len() + header.len() + 256) / 2;
    let max = room.min(CONTINUATION_EXCERPT_MAX);
    let message = if final_message.trim().is_empty() {
        "> (no final message was recorded)".to_string()
    } else {
        quoted_excerpt(final_message, max)
    };
    let handoff = handoff.map_or_else(
        || "> (the Issue holds no handoff section)".to_string(),
        |text| quoted_excerpt(text, max),
    );
    format!(
        "{header}\n\n## Predecessor's final message\n\n{message}\n\n## The Issue's latest handoff\n\n{handoff}\n\n---\n\n{}",
        brief.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::make_test_session;
    use rsi_common::types::NewIssue;

    /// A store with one Issue, a manager session and two live (Running)
    /// worker sessions A and B, all in the seeded test project.
    #[allow(clippy::unwrap_used)]
    fn binding_fixture() -> (Store, Uuid, Uuid, Uuid, Uuid, Uuid) {
        let store = Store::open_in_memory().unwrap();
        let project = crate::store::d04_test_project_id();
        let issue = store
            .create_issue(&NewIssue {
                project_id: project,
                title: "bound Issue".to_string(),
                body: String::new(),
                priority: None,
                labels: Vec::new(),
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap();
        let manager = make_test_session();
        store.insert_session(&manager).unwrap();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let mut worker = make_test_session();
            worker.status = SessionStatus::Running;
            store.insert_session(&worker).unwrap();
            ids.push(worker.id);
        }
        (store, project, issue.id, manager.id, ids[0], ids[1])
    }

    /// Admits (inserts) one Issue-bound launch of `worker` with an explicit
    /// operation id, state and `created_at`, as the journal row would read.
    #[allow(clippy::unwrap_used)]
    #[allow(clippy::too_many_arguments)]
    fn admit_launch(
        store: &Store,
        project: Uuid,
        issue: Uuid,
        manager: Uuid,
        worker: Uuid,
        operation: Uuid,
        state: &str,
        created_at: &str,
    ) {
        let payload = serde_json::json!({
            "issue_binding": {"issue_id": issue.to_string(), "display_number": 1}
        });
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,
                    scope_version,policy_version,idempotency_key,fingerprint,kind,payload_json,
                    state,target_session_id,outcome_json,not_before,created_at,updated_at)
                 VALUES(?1,?2,?3,1,1,?1,'fixture',?4,?5,?6,?7,'{}',?8,?8,?8)",
                params![
                    operation.to_string(),
                    project.to_string(),
                    manager.to_string(),
                    ACTION_KIND,
                    payload.to_string(),
                    state,
                    worker.to_string(),
                    created_at
                ],
            )
            .unwrap();
    }

    /// #1321: an admitted later launch of the same Issue supersedes the
    /// predecessor whatever its outcome; a successor that failed, blocked or
    /// was revoked does not hand a resumed predecessor its append right back.
    /// #1590: an oversized handoff keeps its head and its tail (the landing
    /// filters), names the omitted bytes, and fits the query budget.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn quoted_head_tail_keeps_the_tail_of_an_oversized_handoff() {
        let text = format!("start\n{}\nLANDING FILTERS: end", "x".repeat(20_000));
        let block = quoted_head_tail(&text, 4_000);
        assert!(block.len() <= 4_000, "{}", block.len());
        assert!(block.starts_with("> start"));
        assert!(block.ends_with("> LANDING FILTERS: end"));
        assert!(block.contains("bytes omitted from the middle"));
        let small = quoted_head_tail("a\nb", 4_000);
        assert_eq!(small, "> a\n> b");
    }

    /// #1590: the handoff starts at the last handoff HEADING, so a
    /// `PIPELINE HANDOFF` line inside it does not cut its start off.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn review_handoff_starts_at_the_last_handoff_heading() {
        let body = "Build it.\n\n## Handoff 1\nold\n\n## Handoff 2\nnew\nPIPELINE HANDOFF — IMPLEMENTATION:\nfilters";
        let start = review_handoff_start(body).unwrap();
        assert!(body[start..].starts_with("## Handoff 2"));
        let bare = "Build it.\nPIPELINE HANDOFF — X\nrest";
        assert!(bare[review_handoff_start(bare).unwrap()..].starts_with("PIPELINE HANDOFF"));
        assert_eq!(review_handoff_start("Build it."), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn any_admitted_successor_launch_supersedes_the_predecessor_whatever_its_outcome() {
        for outcome in [
            "queued",
            "running",
            "uncertain",
            "succeeded",
            "failed",
            "blocked",
            "revoked",
        ] {
            let (store, project, issue, manager, a, b) = binding_fixture();
            let a_launch = Uuid::new_v4();
            admit_launch(
                &store,
                project,
                issue,
                manager,
                a,
                a_launch,
                "succeeded",
                "2026-10-06T10:00:00.000000000Z",
            );
            assert_eq!(
                Store::live_issue_binding_for_worker_on(&store.conn, a).unwrap(),
                Some((project, issue, a_launch)),
                "A holds the binding before any successor ({outcome})"
            );
            admit_launch(
                &store,
                project,
                issue,
                manager,
                b,
                Uuid::new_v4(),
                outcome,
                "2026-10-06T11:00:00.000000000Z",
            );
            // A is resumed (Running) and its own launch still succeeded.
            assert_eq!(
                Store::live_issue_binding_for_worker_on(&store.conn, a).unwrap(),
                None,
                "a {outcome} successor launch keeps the resumed predecessor refused"
            );
            assert_eq!(
                Store::bound_issue_for_worker_on(&store.conn, a).unwrap(),
                Some((project, issue)),
                "read authority is unchanged ({outcome})"
            );
        }
    }

    /// #1322: supersession is admission order, not wall clock or id order. A
    /// successor admitted after a backward clock step (its `created_at` and
    /// its id both sort before the predecessor's) keeps append authority and
    /// the resumed predecessor stays refused.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn later_admitted_successor_with_an_earlier_timestamp_holds_the_binding() {
        let (store, project, issue, manager, a, b) = binding_fixture();
        let a_launch = Uuid::parse_str("ffffffff-ffff-4fff-bfff-ffffffffffff").unwrap();
        let b_launch = Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap();
        admit_launch(
            &store,
            project,
            issue,
            manager,
            a,
            a_launch,
            "succeeded",
            "2026-10-06T12:00:00.000000000Z",
        );
        admit_launch(
            &store,
            project,
            issue,
            manager,
            b,
            b_launch,
            "running",
            "2026-10-06T11:00:00.000000000Z",
        );
        assert_eq!(
            Store::live_issue_binding_for_worker_on(&store.conn, b).unwrap(),
            Some((project, issue, b_launch)),
            "the later-admitted successor keeps append authority"
        );
        assert_eq!(
            Store::live_issue_binding_for_worker_on(&store.conn, a).unwrap(),
            None,
            "the resumed predecessor is refused"
        );
    }

    /// #1325: a replay finds the last admission even after a backward clock
    /// step. The same key can be admitted in different manager scope versions.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn prior_continuation_operation_uses_later_admission_with_an_earlier_timestamp() {
        let (store, project, issue, manager, predecessor, worker) = binding_fixture();
        for (id, timestamp, scope, query) in [
            (
                "ffffffff-ffff-4fff-bfff-ffffffffffff",
                "2026-10-06T12:00:00.000000000Z",
                1,
                "earlier admission",
            ),
            (
                "00000000-0000-4000-8000-000000000001",
                "2026-10-06T11:00:00.000000000Z",
                2,
                "later admission",
            ),
        ] {
            admit_launch(
                &store,
                project,
                issue,
                manager,
                worker,
                Uuid::parse_str(id).unwrap(),
                "succeeded",
                timestamp,
            );
            let operation = ManagerActionV2::CreateSession {
                parent_id: manager,
                kind: SessionKind::Task,
                query: query.into(),
                launch: rsi_common::harness_manager_v2::ManagerLaunchChoiceV2 {
                    provider: rsi_common::types::SessionProvider::Codex,
                    model: "gpt-5".into(),
                    effort: None,
                },
                sandbox_source: None,
            };
            store
                .conn
                .execute(
                    "UPDATE harness_manager_v2_operations
                 SET scope_version=?2, idempotency_key='continuation-replay',
                     payload_json=json_set(payload_json,
                         '$.issue_binding.continue_from',?3,
                         '$.request.operation',json(?4))
                 WHERE id=?1",
                    params![
                        id,
                        scope,
                        predecessor.to_string(),
                        serde_json::to_string(&operation).unwrap()
                    ],
                )
                .unwrap();
        }
        let operation = store
            .prior_continuation_operation(project, "continuation-replay", predecessor)
            .unwrap()
            .unwrap();
        let ManagerActionV2::CreateSession { query, .. } = operation else {
            panic!("expected the journalled create_session operation");
        };
        assert_eq!(query, "later admission");
    }

    /// #1325: baton notices go to the latest admitted Issue-bound launcher,
    /// even when its timestamp and UUID sort before the previous launch.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn worker_baton_launcher_uses_later_admission_with_an_earlier_timestamp() {
        use crate::store::worker_baton::{WorkerBatonLauncher, WorkerLauncherKind};

        let (store, project, issue, manager, worker, later_manager) = binding_fixture();
        for (id, timestamp, launcher) in [
            (
                "ffffffff-ffff-4fff-bfff-ffffffffffff",
                "2026-10-06T12:00:00.000000000Z",
                manager,
            ),
            (
                "00000000-0000-4000-8000-000000000001",
                "2026-10-06T11:00:00.000000000Z",
                later_manager,
            ),
        ] {
            admit_launch(
                &store,
                project,
                issue,
                launcher,
                worker,
                Uuid::parse_str(id).unwrap(),
                "succeeded",
                timestamp,
            );
            store
                .conn
                .execute(
                    "UPDATE harness_manager_v2_operations
                 SET payload_json=json_set(payload_json,
                     '$.origin.origin','agent','$.origin.caller',?2)
                 WHERE id=?1",
                    params![id, launcher.to_string()],
                )
                .unwrap();
        }
        assert_eq!(
            store.worker_baton_launcher(worker).unwrap(),
            Some(WorkerBatonLauncher {
                launcher: later_manager,
                kind: WorkerLauncherKind::Manager,
                project_id: Some(project),
                issue_id: Some(issue),
                issue_display_number: Some(1),
            }),
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn continuation_brief_quotes_the_latest_handoff_and_fits_the_query_limit() {
        let body = "Build it.\n\n## Handoff 1\nold\n\n## Baton handoff\nnew state: tests red\nPIPELINE HANDOFF — BATON abc";
        assert_eq!(latest_handoff(body), Some("PIPELINE HANDOFF — BATON abc"));
        let body = "Build it.\n\n## Handoff 1\nold\n\n## Baton handoff\nnew state: tests red";
        assert_eq!(
            latest_handoff(body),
            Some("## Baton handoff\nnew state: tests red")
        );
        assert_eq!(latest_handoff("Build it."), None);
        let predecessor = Uuid::new_v4();
        let brief = continuation_brief(
            predecessor,
            "0123456789abcdef0123456789abcdef01234567",
            true,
            "Committed WIP.\nPIPELINE HANDOFF — BATON 0123456",
            latest_handoff(body),
            "Finish the Issue.",
        );
        assert!(brief.contains(&predecessor.to_string()));
        assert!(brief.contains("0123456789abcdef0123456789abcdef01234567"));
        assert!(brief.contains("NOT copied"));
        assert!(brief.contains("> PIPELINE HANDOFF — BATON 0123456"));
        assert!(brief.contains("> new state: tests red"));
        assert!(brief.ends_with("Finish the Issue."));
        // Huge copied text never pushes the query past create_session's limit.
        let huge = "é".repeat(40_000);
        let long_brief = "b".repeat(24_000);
        let brief = continuation_brief(predecessor, "h", false, &huge, Some(&huge), &long_brief);
        assert!(brief.len() <= CONTINUATION_QUERY_BUDGET, "{}", brief.len());
        assert!(brief.contains("[truncated]"));
    }

    /// #1555: a bound worker whose turn ended with no report and no wake of
    /// its own is stranded; the note names the daemon job and unit it still
    /// owns. A pending own wake, a final report or a missing binding is not.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn stranded_worker_note_names_owned_jobs_and_spares_pending_or_reported_workers() {
        use rsi_common::types::{
            ConversationEvent, EventType, Recurrence, Role, ScheduleSpec, ScheduledJob, WakeMode,
        };
        let (store, project, issue, manager, bound, unbound) = binding_fixture();
        admit_launch(
            &store,
            project,
            issue,
            manager,
            bound,
            Uuid::new_v4(),
            "succeeded",
            "2026-10-01T00:00:00.000000000Z",
        );
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Completed' WHERE id IN (?1,?2)",
                params![bound.to_string(), unbound.to_string()],
            )
            .unwrap();
        let note = |id: Uuid| {
            let session = store.get_session(id).unwrap().unwrap();
            store.worker_stranded_note(&session).unwrap()
        };
        let pending = |id: Uuid| {
            let session = store.get_session(id).unwrap().unwrap();
            store.worker_terminal_watch_pending(&session).unwrap()
        };
        let say = |sequence: i32, role: Role, content: &str| {
            store
                .insert_event(&ConversationEvent {
                    id: 0,
                    session_id: bound,
                    sequence,
                    event_type: EventType::Message,
                    role: Some(role),
                    content: content.into(),
                    tool_name: None,
                    tool_input: None,
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                })
                .unwrap();
        };

        // Bound, nothing reported, nothing owned.
        say(1, Role::Assistant, "waiting on my tests");
        assert_eq!(note(bound).as_deref(), Some("no-result: stranded"));
        assert!(!pending(bound));
        // An unbound worker is never flagged.
        assert_eq!(note(unbound), None);

        // A running daemon job without a wake is named with its unit.
        let job = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO agent_jobs(id,owner_session_id,kind,params_json,cwd,unit_name,
                    log_path,status_path,state,created_at,row_version)
                 VALUES(?1,?2,'test','{}','/tmp','rsi-job-unit','/tmp/l','/tmp/s','running',
                    '2026-10-01T00:00:00.000000000Z',1)",
                params![job.to_string(), bound.to_string()],
            )
            .unwrap();
        let named = note(bound).unwrap();
        assert!(
            named.starts_with("no-result: stranded; owns job "),
            "{named}"
        );
        assert!(named.contains(&job.to_string()[..8]) && named.contains("unit rsi-job-unit"));
        assert!(
            !pending(bound),
            "a running unit without a wake still flags stranding"
        );

        // An enabled wake of its own means the report is pending.
        let now = chrono::Utc::now();
        let wake = ScheduledJob {
            id: Uuid::new_v4(),
            name: "job-wake".into(),
            message: "m".into(),
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
            wake_session_id: Some(bound),
        };
        store.insert_scheduled_job(&wake).unwrap();
        assert_eq!(note(bound), None);
        assert!(pending(bound));
        // A daemon predicate wake also holds the watch across interim turns.
        store.conn.execute(
            "UPDATE scheduled_jobs SET wake_mode='fresh', schedule_json=json_set(schedule_json,'$.wake_when',json('{}')) WHERE id=?1",
            [wake.id.to_string()],
        ).unwrap();
        assert!(pending(bound));
        store.conn.execute(
            "UPDATE scheduled_jobs SET wake_mode='resume', schedule_json=json_remove(schedule_json,'$.wake_when') WHERE id=?1",
            [wake.id.to_string()],
        ).unwrap();
        // A watch of the manager's on this worker does not count as its wake.
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET wake_mode=?2 WHERE id=?1",
                params![wake.id.to_string(), format!("on_terminal:{bound}")],
            )
            .unwrap();
        assert!(note(bound).is_some());
        assert!(!pending(bound));
        // A refused/retired wake is no longer enabled.
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET wake_mode='resume', enabled=0 WHERE id=?1",
                params![wake.id.to_string()],
            )
            .unwrap();
        assert!(note(bound).is_some());

        // A final report in the latest turn is not a stranding.
        say(2, Role::Assistant, "PIPELINE HANDOFF — IMPLEMENTATION:");
        assert_eq!(note(bound), None);
        assert!(
            pending(bound),
            "a reported worker's running job still holds its watch"
        );
        store
            .conn
            .execute(
                "UPDATE agent_jobs SET state='succeeded' WHERE id=?1",
                [job.to_string()],
            )
            .unwrap();
        assert!(
            !pending(bound),
            "settled jobs and disabled wakes release the watch"
        );
        // A later user turn starts a new turn without a report.
        say(3, Role::User, "continue");
        say(4, Role::Assistant, "still working");
        assert!(note(bound).is_some());
        // #1616: a reviewer's closing `REVIEW APPROVE|CHANGES` line is a
        // report, annotated with its verdict instead of "stranded".
        let watch = |id: Uuid| {
            let session = store.get_session(id).unwrap().unwrap();
            store.worker_watch_note(&session).unwrap()
        };
        assert_eq!(watch(bound).as_deref(), Some("no-result: stranded"));
        say(
            5,
            Role::Assistant,
            "verdict follows\nREVIEW APPROVE commit=abc",
        );
        assert_eq!(note(bound), None);
        assert_eq!(watch(bound).as_deref(), Some("review: APPROVE"));
        say(6, Role::User, "again");
        say(7, Role::Assistant, "**REVIEW CHANGES** two blockers");
        assert_eq!(watch(bound).as_deref(), Some("review: CHANGES"));
    }

    /// #1553: a queued launch journalled under an older scope version of its
    /// project can never run, so it holds no Issue binding; one journalled
    /// under the current scope version still does.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_queued_launch_of_a_superseded_scope_holds_no_issue_binding() {
        let (store, project, issue, manager, worker, _) = binding_fixture();
        let queued = Uuid::new_v4();
        admit_launch(
            &store,
            project,
            issue,
            manager,
            worker,
            queued,
            "queued",
            "2026-10-07T12:00:00.000000000Z",
        );
        let relaunch = Uuid::new_v4();
        let held = |store: &Store| {
            store
                .manager_issue_has_other_live_worker(issue, relaunch)
                .unwrap()
        };
        // The launch's scope version is 1; the project's scope is still at 1.
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_scopes(project_id,manager_session_id,epic_ids_json,
                    row_version,updated_at,scope_mode,group_ids_json)
                 VALUES(?1,?2,'[]',1,'2026-10-07T12:00:00.000000000Z','project','[]')",
                params![project.to_string(), manager.to_string()],
            )
            .unwrap();
        assert!(held(&store), "a launch of the current scope can still run");
        // An appointment advances the scope: the queued launch is dead.
        store
            .conn
            .execute(
                "UPDATE harness_manager_scopes SET row_version=2 WHERE project_id=?1",
                [project.to_string()],
            )
            .unwrap();
        assert!(!held(&store), "a superseded queued launch binds nothing");
        // A running launch of the old scope is already past the gate: it stays.
        store
            .conn
            .execute(
                "UPDATE harness_manager_v2_operations SET state='running' WHERE id=?1",
                [queued.to_string()],
            )
            .unwrap();
        assert!(held(&store));
    }
}
