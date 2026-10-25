//! `AgentManagerLaunchIssueWorker` (#1100): the session-layer half. The store
//! admits the Issue-bound `create_session` action atomically with the Issue
//! note and InProgress status; this layer arms the caller's terminal watch,
//! which needs the worker session to exist: now when it already does, and the
//! moment the launch is established otherwise (and on any replay).

use super::SessionManager;
use super::agent_verbs::AgentControlHandle;
use crate::error::Result;
use crate::store::manager_actions::{
    ManagerActionClaimV2, ManagerActionOperationV2, ManagerActionOriginV2,
};
use rsi_common::manager_issue_worker::{
    AgentManagerLaunchIssueWorkerRequestV1, AgentManagerLaunchIssueWorkerResultV1,
    ManagerIssueWorkerWatchV1,
};
use uuid::Uuid;

impl AgentControlHandle {
    pub async fn agent_manager_launch_issue_worker(
        &self,
        caller: Uuid,
        mut request: AgentManagerLaunchIssueWorkerRequestV1,
    ) -> Result<AgentManagerLaunchIssueWorkerResultV1> {
        // #1988: published work is useful context for the worker, but Git
        // discovery is advisory and must never prevent an otherwise valid
        // launch.
        // #2529: the hint is scoped to the bound Issue itself. A satellite
        // mirror's local number is reused by older unrelated Issues, so the
        // search follows the mirror's recorded hub origin and floors the local
        // number at the Issue's creation.
        let (project_path, queries) = {
            let store = crate::store_lock::lock(
                &self.store,
                "manager_issue_worker::agent_manager_launch_issue_worker",
            )
            .await;
            let project = request
                .project_id
                .or_else(|| store.get_session(caller).ok().flatten()?.project_id);
            let path = project.and_then(|id| store.get_project(id).ok().flatten()?.path);
            let queries = project
                .and_then(|id| {
                    store
                        .get_issue_by_display_number(id, request.issue)
                        .ok()
                        .flatten()
                })
                .map(|issue| crate::integration::issue_commit_queries(&issue))
                .unwrap_or_default();
            (path, queries)
        };
        let rolling_issue_commits = match project_path {
            Some(path) => crate::integration::issue_commits_on_rolling(&path, &queries).await,
            None => Vec::new(),
        };
        if !rolling_issue_commits.is_empty() {
            let names = queries
                .iter()
                .map(|query| format!("#{}", query.number))
                .collect::<Vec<_>>()
                .join(" / ");
            let mut section = format!("Commits already on rolling naming {names}:\n");
            for commit in &rolling_issue_commits {
                section.push_str(&format!("- {} {}\n", commit.sha, commit.subject));
            }
            // A number in a subject is a lead, not proof: the same number can
            // belong to another project or an older Issue (#212).
            section.push_str(
                "Advisory only: a commit subject naming a number does not prove it covers \
                 this Issue. Verify against the source before relying on it; it is never \
                 closure evidence.\n",
            );
            // Keep the existing brief limit authoritative. Subjects are
            // bounded by the Git helper; if the supplied brief is at its
            // ceiling, preserve its beginning and make room for the published
            // commits rather than refusing the launch.
            let max = rsi_common::manager_issue_worker::MANAGER_ISSUE_WORKER_MAX_BRIEF_BYTES;
            if request.brief.len() + 2 + section.len() > max {
                let mut keep = max.saturating_sub(section.len() + 2);
                while !request.brief.is_char_boundary(keep) {
                    keep -= 1;
                }
                request.brief.truncate(keep);
            }
            request.brief.push_str("\n\n");
            request.brief.push_str(&section);
        }
        // #3688: a manager whose scope is the whole project and holds no live
        // Epic launches its first worker without an operator or global-manager
        // round trip: the daemon creates the default Epic (and Group). #3786:
        // the containers are written inside the admission's own transaction, so
        // a launch that is refused (no SessionCreate capability, unavailable
        // Issue, ...) leaves nothing behind.
        let (admission, created) = {
            let store = crate::store_lock::lock(
                &self.store,
                "manager_issue_worker::agent_manager_launch_issue_worker",
            )
            .await;
            let created = Self::default_issue_worker_containers(&store, caller, &request)?;
            let admission =
                store.enqueue_manager_issue_worker_provisioning(caller, &request, &created)?;
            (admission, created)
        };
        for session in created {
            self.project_default_container(session).await;
        }
        let watch = self
            .arm_issue_worker_watch(caller, admission.worker_session_id)
            .await;
        Ok(AgentManagerLaunchIssueWorkerResultV1 {
            deduplicated: admission.receipt.deduplicated,
            action: admission.receipt,
            worker_session_id: admission.worker_session_id,
            issue_id: admission.issue.id,
            issue_display_number: admission.issue.display_number,
            issue_status: admission.issue.status,
            issue_row_version: admission.issue.row_version,
            watch,
            rolling_issue_commits,
            carried_related: admission.carried_related,
            skipped_related: admission.skipped_related,
        })
    }

    /// #2725: the caller's own Issue-bound launch of `worker` is still queued,
    /// so no session row exists yet. The launch arms the launcher's terminal
    /// watch itself, so an `on_terminal` request on this id is answered as
    /// covered instead of refused `session_not_found`.
    pub(crate) async fn issue_worker_launch_watch_pending(
        &self,
        caller: Uuid,
        worker: Uuid,
    ) -> bool {
        crate::store_lock::lock(
            &self.store,
            "manager_issue_worker::issue_worker_launch_watch_pending",
        )
        .await
        .issue_worker_launch_watch_pending(caller, worker)
        .unwrap_or(false)
    }

    /// Arm the manager's `on_terminal` watch on the worker when the worker
    /// session exists. Idempotent on `(manager, worker)`.
    pub(crate) async fn arm_issue_worker_watch(
        &self,
        manager: Uuid,
        worker: Uuid,
    ) -> ManagerIssueWorkerWatchV1 {
        let exists =
            crate::store_lock::lock(&self.store, "manager_issue_worker::arm_issue_worker_watch")
                .await
                .get_session(worker)
                .ok()
                .flatten()
                .is_some();
        if !exists {
            return ManagerIssueWorkerWatchV1::PendingLaunch;
        }
        match self.arm_automatic_child_watch(manager, worker).await {
            Ok(_) => ManagerIssueWorkerWatchV1::Armed,
            Err(error) => {
                tracing::warn!(
                    target: "agent_coordination",
                    manager_session_id = %manager,
                    worker_session_id = %worker,
                    %error,
                    "issue worker terminal watch not armed"
                );
                ManagerIssueWorkerWatchV1::PendingLaunch
            }
        }
    }
}

impl SessionManager {
    /// The caller's terminal-watch row for an Issue-bound launch claim, to be
    /// committed with the launch success (#1115). `None` for any other claim;
    /// a build failure is logged and the reconcile below repairs it.
    ///
    /// Takes the already-held store guard: the owner is resolved to the node's
    /// current seat in the same lock hold as the commit, so a succession
    /// cannot publish between the two (#2114).
    pub(super) fn issue_worker_watch_candidate_locked(
        store: &crate::store::Store,
        claim: &ManagerActionClaimV2,
    ) -> Option<rsi_common::types::ScheduledJob> {
        let (manager, worker) = issue_worker_parties(&claim.operation)?;
        match AgentControlHandle::automatic_child_watch_candidate_locked(store, manager, worker) {
            Ok(job) => Some(job),
            Err(error) => {
                tracing::warn!(
                    target: "agent_coordination",
                    manager_session_id = %manager,
                    worker_session_id = %worker,
                    %error,
                    "issue worker terminal watch row not built for the launch commit"
                );
                None
            }
        }
    }

    /// #2462: an Issue-bound launch ended (failed, blocked or revoked) without
    /// establishing its worker, so the success-time watch commit never ran.
    /// Name the cause on the worker's own log when it already ended, and arm the
    /// launcher's terminal watch now: a worker that is already terminal fires
    /// the wake on the first attempt, one still settling fires when it does.
    /// Best effort: the receipt and Issue note are already durable.
    pub(super) async fn settle_ended_issue_worker_launch(
        &self,
        operation: &ManagerActionOperationV2,
        code: &str,
    ) {
        let Some((manager, worker)) = issue_worker_parties(operation) else {
            return;
        };
        if let Err(error) = crate::store_lock::lock(
            &self.store,
            "manager_issue_worker::settle_ended_issue_worker_launch",
        )
        .await
        .record_session_failure_cause(
            worker,
            &format!("launch_failed:{code}"),
            &format!("Launch failed before the provider produced any output: {code}"),
        ) {
            tracing::warn!(
                target: "agent_coordination",
                worker_session_id = %worker,
                %error,
                "ended issue worker launch cause not recorded"
            );
        }
        self.agent_control()
            .arm_issue_worker_watch(manager, worker)
            .await;
    }

    /// #1115 startup/periodic reconcile: re-arm the terminal watch of every
    /// succeeded Issue-bound launch whose worker is still live and has no
    /// enabled watch (a pre-fix crash window). Returns the number repaired.
    pub(super) async fn reconcile_issue_worker_watches(&self) -> usize {
        let pending = match crate::store_lock::lock(
            &self.store,
            "manager_issue_worker::reconcile_issue_worker_watches",
        )
        .await
        .list_issue_worker_unwatched()
        {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "issue worker watch reconcile deferred");
                return 0;
            }
        };
        let mut repaired = 0;
        for (manager, worker) in pending {
            if matches!(
                self.agent_control()
                    .arm_issue_worker_watch(manager, worker)
                    .await,
                ManagerIssueWorkerWatchV1::Armed
            ) {
                repaired += 1;
            }
        }
        repaired
    }
}

impl SessionManager {
    /// #1930, #2109: relaunch every Issue-bound worker that failed on provider
    /// capacity, once the same-session resumes are spent and its backoff
    /// elapsed. Each relaunch is an idempotent Issue-bound admission; a refusal
    /// is logged and the manager's watch fires once its hold window passes.
    /// Returns the number of relaunches admitted.
    pub(super) async fn reconcile_issue_worker_capacity_retries(&self) -> usize {
        let now = chrono::Utc::now();
        match crate::store_lock::lock(
            &self.store,
            "manager_issue_worker::reconcile_issue_worker_capacity_retries",
        )
        .await
        .retire_superseded_capacity_resumes(now)
        {
            Ok(0) => {}
            Ok(retired) => tracing::info!(
                target: "agent_coordination",
                retired,
                "capacity resumes of manager-replaced workers retired"
            ),
            Err(error) => tracing::warn!(%error, "capacity resume retirement deferred"),
        }
        let due = match crate::store_lock::lock(
            &self.store,
            "manager_issue_worker::reconcile_issue_worker_capacity_retries",
        )
        .await
        .list_issue_worker_capacity_retries_due(now)
        {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!(%error, "issue worker capacity retry scan deferred");
                return 0;
            }
        };
        let mut admitted = 0;
        for item in due {
            let result = crate::store_lock::lock(
                &self.store,
                "manager_issue_worker::reconcile_issue_worker_capacity_retries",
            )
            .await
            .enqueue_issue_worker_capacity_retry(&item);
            match result {
                Ok(Some(admission)) => {
                    admitted += 1;
                    tracing::info!(
                        target: "agent_coordination",
                        failed_worker = %item.worker,
                        worker_session_id = %admission.worker_session_id,
                        issue = item.issue_display,
                        "capacity: issue worker relaunched after provider capacity failure"
                    );
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    target: "agent_coordination",
                    failed_worker = %item.worker,
                    issue = item.issue_display,
                    %error,
                    "issue worker capacity relaunch refused"
                ),
            }
        }
        admitted
    }
}

fn issue_worker_parties(operation: &ManagerActionOperationV2) -> Option<(Uuid, Uuid)> {
    let context = &operation.context;
    match (
        context.issue_binding.as_ref(),
        &context.origin,
        context.target_session_id,
    ) {
        (Some(_), ManagerActionOriginV2::Agent { caller }, Some(worker)) => Some((*caller, worker)),
        _ => None,
    }
}

/// Display names and tags of the containers #3688 provisions. The tags are the
/// normalized form `create_container` would store.
const DEFAULT_GROUP_NAME: &str = "Managed work";
const DEFAULT_GROUP_TAG: &str = "managed-work";
const DEFAULT_EPIC_NAME: &str = "Issue workers";
const DEFAULT_EPIC_TAG: &str = "issue-workers";

impl AgentControlHandle {
    /// #3688: the default Group (when the project has no live top-level Group)
    /// and Epic a first launch needs, when the store's plan says the caller is a
    /// project-wide manager with no live Epic in scope. Only built here; the
    /// admission transaction writes them (#3786), so a refusal persists nothing.
    fn default_issue_worker_containers(
        store: &crate::store::Store,
        caller: Uuid,
        request: &AgentManagerLaunchIssueWorkerRequestV1,
    ) -> Result<Vec<rsi_common::types::Session>> {
        use rsi_common::rpc::CreateContainerParams;
        use rsi_common::types::SessionKind;
        {
            let Some(plan) = store.issue_worker_default_container_plan(caller, request)? else {
                return Ok(Vec::new());
            };
            let mut created = Vec::new();
            let group = match plan.existing_group {
                Some(group) => group,
                None => {
                    let session = super::hierarchy_ops::build_container_session(
                        Uuid::new_v4(),
                        CreateContainerParams {
                            kind: SessionKind::Group,
                            name: DEFAULT_GROUP_NAME.into(),
                            parent_id: None,
                            project_id: Some(plan.project_id),
                            tags: vec![DEFAULT_GROUP_TAG.into()],
                            topology_id: None,
                        },
                        plan.working_dir.clone(),
                        vec![DEFAULT_GROUP_TAG.into()],
                    );
                    let id = session.id;
                    created.push(session);
                    id
                }
            };
            created.push(super::hierarchy_ops::build_container_session(
                Uuid::new_v4(),
                CreateContainerParams {
                    kind: SessionKind::Epic,
                    name: DEFAULT_EPIC_NAME.into(),
                    parent_id: Some(group),
                    project_id: Some(plan.project_id),
                    tags: vec![DEFAULT_EPIC_TAG.into()],
                    topology_id: None,
                },
                plan.working_dir,
                vec![DEFAULT_EPIC_TAG.into()],
            ));
            Ok(created)
        }
    }
}
