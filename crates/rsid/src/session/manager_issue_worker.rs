//! `AgentManagerLaunchIssueWorker` (#1100): the session-layer half. The store
//! admits the Issue-bound `create_session` action atomically with the Issue
//! note and InProgress status; this layer arms the caller's terminal watch,
//! which needs the worker session to exist: now when it already does, and the
//! moment the launch is established otherwise (and on any replay).

use super::SessionManager;
use super::agent_verbs::AgentControlHandle;
use crate::error::Result;
use crate::store::manager_actions::{ManagerActionClaimV2, ManagerActionOriginV2};
use rsi_common::manager_issue_worker::{
    AgentManagerLaunchIssueWorkerRequestV1, AgentManagerLaunchIssueWorkerResultV1,
    ManagerIssueWorkerWatchV1,
};
use uuid::Uuid;

impl AgentControlHandle {
    pub async fn agent_manager_launch_issue_worker(
        &self,
        caller: Uuid,
        request: AgentManagerLaunchIssueWorkerRequestV1,
    ) -> Result<AgentManagerLaunchIssueWorkerResultV1> {
        let admission = self
            .store
            .lock()
            .await
            .enqueue_manager_issue_worker(caller, &request)?;
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
        })
    }

    /// Arm the manager's `on_terminal` watch on the worker when the worker
    /// session exists. Idempotent on `(manager, worker)`.
    pub(crate) async fn arm_issue_worker_watch(
        &self,
        manager: Uuid,
        worker: Uuid,
    ) -> ManagerIssueWorkerWatchV1 {
        let exists = self
            .store
            .lock()
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
    pub(super) async fn issue_worker_watch_candidate(
        &self,
        claim: &ManagerActionClaimV2,
    ) -> Option<rsi_common::types::ScheduledJob> {
        let (manager, worker) = issue_worker_parties(claim)?;
        match self
            .agent_control()
            .build_automatic_child_watch_candidate(manager, worker)
            .await
        {
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

    /// #1115 startup/periodic reconcile: re-arm the terminal watch of every
    /// succeeded Issue-bound launch whose worker is still live and has no
    /// enabled watch (a pre-fix crash window). Returns the number repaired.
    pub(super) async fn reconcile_issue_worker_watches(&self) -> usize {
        let pending = match self.store.lock().await.list_issue_worker_unwatched() {
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

fn issue_worker_parties(claim: &ManagerActionClaimV2) -> Option<(Uuid, Uuid)> {
    let context = &claim.operation.context;
    match (
        context.issue_binding.as_ref(),
        &context.origin,
        context.target_session_id,
    ) {
        (Some(_), ManagerActionOriginV2::Agent { caller }, Some(worker)) => Some((*caller, worker)),
        _ => None,
    }
}
