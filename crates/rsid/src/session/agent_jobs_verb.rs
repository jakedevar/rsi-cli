//! `AgentSubmitJob` / `AgentGetJob` / `AgentListJobs` (#1002 slice 1): the
//! agent-facing side of daemon-owned durable jobs.
//!
//! Any live leaf session may submit `test` and `build`; `landing`,
//! `cloud_gate` and `cloud_sweep` (publish to rolling / spend the operator's
//! cloud grant) need the current appointed manager or current Epic lead, the
//! `AgentEnqueueLandingSource` rule. The job is always owned by the
//! token-bound caller and runs in the caller's own sandbox (an appointed
//! manager may instead name another worktree of its repository). Reads are
//! scoped to the owner.

use super::agent_verbs::AgentControlHandle;
use crate::agent_jobs::{JobRuntime, JobTools, SubmitContext};
use crate::error::{DaemonError, Result};
use rsi_common::agent_jobs::{
    AgentGetJobRequestV1, AgentJobV1, AgentListJobsRequestV1, AgentListJobsResultV1,
    AgentSubmitJobReceiptV1, AgentSubmitJobRequestV1, JOB_DIR_NOT_ALLOWED, JOB_INVALID_PARAMS,
    JOB_KIND_NOT_AUTHORIZED, JOB_NOT_FOUND, JobKind,
};
use std::path::PathBuf;
use uuid::Uuid;

const DEFAULT_LIST: usize = 20;
const MAX_LIST: usize = 100;

/// `test` and `build` are open to any leaf; `landing`, `cloud_gate` and
/// `cloud_sweep` need the
/// current appointed manager or current Epic lead.
pub(crate) fn job_kind_permitted(kind: JobKind, is_manager: bool, is_lead: bool) -> bool {
    match kind {
        JobKind::Test | JobKind::Build => true,
        JobKind::Landing | JobKind::CloudGate | JobKind::CloudSweep => is_manager || is_lead,
    }
}

impl AgentControlHandle {
    /// # Errors
    /// A stable `job_*` refusal, or a persistence/launch error.
    pub async fn agent_submit_job(
        &self,
        caller: Uuid,
        request: AgentSubmitJobRequestV1,
        runtime: std::sync::Arc<dyn JobRuntime>,
        tools: JobTools,
    ) -> Result<AgentSubmitJobReceiptV1> {
        let params = request
            .typed_params()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let (session, is_manager, is_lead) = {
            let store = self.store.lock().await;
            let projection = store
                .agent_authority_projection(caller)
                .map_err(|_| DaemonError::PolicyDenied(JOB_DIR_NOT_ALLOWED.into()))?;
            let session = store
                .get_session(caller)?
                .ok_or_else(|| DaemonError::PolicyDenied(JOB_DIR_NOT_ALLOWED.into()))?;
            (session, projection.is_manager, projection.is_lead)
        };
        if !job_kind_permitted(params.kind(), is_manager, is_lead) {
            return Err(DaemonError::PolicyDenied(JOB_KIND_NOT_AUTHORIZED.into()));
        }
        // #1073: a new job would keep a waiting deploy from its quiet point.
        // Refused with a typed retryable code; the caller resubmits after the
        // deploy settles. The deploy's caller and parentless sessions run.
        if let Some(drain) = &self.deploy_drain {
            drain.refuse_if_draining(Some(caller), session.parent_id.is_some())?;
        }
        let requested = request.worktree.as_deref().map(PathBuf::from);
        if requested.as_deref().is_some_and(|p| !p.is_absolute()) {
            return Err(DaemonError::InvalidParam(JOB_INVALID_PARAMS.into()));
        }
        let sandbox_root = session.sandbox_root.clone();
        let working_dir = session.working_dir.clone();
        let cwd = tokio::task::spawn_blocking(move || {
            crate::agent_jobs::resolve_cwd(
                sandbox_root.as_deref(),
                &working_dir,
                is_manager,
                requested.as_deref(),
            )
        })
        .await
        .map_err(|error| DaemonError::Process(format!("job directory probe: {error}")))??;
        let ctx = SubmitContext {
            owner: caller,
            project_id: session.project_id,
            cwd,
            name: request.name,
            params,
            idempotency_key: request.idempotency_key,
            wake: request.wake.unwrap_or_default(),
        };
        let store = std::sync::Arc::clone(&self.store);
        let (row, replayed) = tokio::task::spawn_blocking(move || {
            let jobs_dir = crate::agent_jobs::jobs_dir()
                .map_err(|error| DaemonError::Process(format!("job directory: {error}")))?;
            crate::agent_jobs::submit(
                &store.blocking_lock(),
                &*runtime,
                &tools,
                &jobs_dir,
                ctx,
                chrono::Utc::now(),
            )
        })
        .await
        .map_err(|error| DaemonError::Process(format!("job submit: {error}")))??;
        Ok(AgentSubmitJobReceiptV1 {
            job: row.job,
            replayed,
        })
    }

    /// # Errors
    /// `job_not_found` for an unknown job or one owned by another session.
    pub async fn agent_get_job(
        &self,
        caller: Uuid,
        request: AgentGetJobRequestV1,
    ) -> Result<AgentJobV1> {
        self.store
            .lock()
            .await
            .get_agent_job(request.job_id)?
            .map(|row| row.job)
            .filter(|job| job.owner_session_id == caller)
            .ok_or_else(|| DaemonError::InvalidParam(JOB_NOT_FOUND.into()))
    }

    /// # Errors
    /// A persistence error.
    pub async fn agent_list_jobs(
        &self,
        caller: Uuid,
        request: AgentListJobsRequestV1,
    ) -> Result<AgentListJobsResultV1> {
        let limit = request
            .limit
            .map_or(DEFAULT_LIST, |n| (n as usize).clamp(1, MAX_LIST));
        let jobs = self
            .store
            .lock()
            .await
            .list_agent_jobs(caller, limit)?
            .into_iter()
            .map(|row| row.job)
            .collect();
        Ok(AgentListJobsResultV1 { jobs })
    }
}

#[cfg(test)]
mod tests {
    use super::job_kind_permitted;
    use rsi_common::agent_jobs::JobKind;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn only_the_manager_or_lead_may_submit_landing_cloud_gate_and_cloud_sweep() {
        for kind in [JobKind::Test, JobKind::Build] {
            assert!(job_kind_permitted(kind, false, false), "{kind:?}");
        }
        for kind in [JobKind::Landing, JobKind::CloudGate, JobKind::CloudSweep] {
            assert!(!job_kind_permitted(kind, false, false), "{kind:?}");
            assert!(job_kind_permitted(kind, true, false), "{kind:?}");
            assert!(job_kind_permitted(kind, false, true), "{kind:?}");
        }
    }
}
