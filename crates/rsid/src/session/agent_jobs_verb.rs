//! `AgentSubmitJob` / `AgentGetJob` / `AgentListJobs` / `AgentCancelJob` (#1002 slice 1, #1106): the
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
    AgentCancelJobRequestV1, AgentCancelJobResultV1, AgentGetJobRequestV1, AgentJobV1,
    AgentListJobsRequestV1, AgentListJobsResultV1, AgentSubmitJobReceiptV1,
    AgentSubmitJobRequestV1, JOB_DIR_NOT_ALLOWED, JOB_INVALID_PARAMS, JOB_KIND_NOT_AUTHORIZED,
    JOB_NOT_FOUND, JOB_QA_LANE_SHA_MISMATCH, JOB_QA_LANE_TIMEOUT_MAX_MINS,
    JOB_SANDBOX_SESSION_LIVE, JOB_TEST_TIMEOUT_DEFAULT_MINS, JOB_TIMEOUT_NOT_AUTHORIZED, JobKind,
    JobParams,
};
use std::path::PathBuf;
use uuid::Uuid;

const DEFAULT_LIST: usize = 20;
const MAX_LIST: usize = 100;

// A QA SHA is a HEAD label only. Never inspect worktree bytes during admission.
// Keep this policy separate from shared fork/terminal cleanliness observations.
#[cfg(test)]
type QaHeadProbe = Box<
    dyn FnOnce(&mut tokio::process::Command, &mut crate::process_control::CaptureLimits) + Send,
>;
#[cfg(test)]
static QA_HEAD_PROBES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, QaHeadProbe>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

async fn observe_qa_head(root: &std::path::Path) -> Result<String> {
    use crate::process_control::{
        CaptureLimits, OverflowBehavior, ProcessContainment, capture_bounded,
    };
    let refuse = || DaemonError::InvalidParam(JOB_QA_LANE_SHA_MISMATCH.into());
    // The dedicated probe has the shared runner's deadlines, caps and group
    // cleanup; unsupported platforms fail closed rather than invoking helpers.
    #[cfg(not(unix))]
    return Err(refuse());
    #[cfg(unix)]
    {
        let mut command = tokio::process::Command::new("git");
        command
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "protocol.allow=never",
            ])
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"]);
        let mut limits = CaptureLimits::new(
            64 * 1024,
            64 * 1024,
            std::time::Duration::from_secs(30),
            OverflowBehavior::Error,
            ProcessContainment::GroupNoEscape,
        );
        #[cfg(test)]
        if let Some(probe) = QA_HEAD_PROBES.lock().unwrap().remove(root) {
            probe(&mut command, &mut limits);
        }
        // Retain only PATH and HOME, including explicit test command overrides.
        // No inherited Git/config/object/transport redirect reaches the process.
        let retained: Vec<_> = ["PATH", "HOME"]
            .into_iter()
            .filter_map(|key| {
                let value = command
                    .as_std()
                    .get_envs()
                    .find(|(k, _)| *k == key)
                    .map_or_else(
                        || std::env::var_os(key),
                        |(_, v)| v.map(std::ffi::OsStr::to_os_string),
                    );
                value.map(|value| (key, value))
            })
            .collect();
        command
            .env_clear()
            .envs(retained)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("GIT_SSH_COMMAND", "/bin/false")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_OPTIONAL_LOCKS", "0");
        let output = capture_bounded(command, limits, &tokio_util::sync::CancellationToken::new())
            .await
            .map_err(|_| refuse())?;
        if !output.status.success()
            || !output.stderr.is_empty()
            || output.stdout_truncated
            || output.stderr_truncated
        {
            return Err(refuse());
        }
        let head = std::str::from_utf8(&output.stdout).map_err(|_| refuse())?;
        let head = head.strip_suffix('\n').ok_or_else(refuse)?;
        if head.len() != 40
            || !head
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(refuse());
        }
        Ok(head.to_owned())
    }
}

/// `test` and `build` are open to any leaf; `landing`, `cloud_gate` and
/// `cloud_sweep` need the
/// current appointed manager or current Epic lead.
pub(crate) fn job_kind_permitted(kind: JobKind, is_manager: bool, is_lead: bool) -> bool {
    match kind {
        JobKind::Test | JobKind::Build => true,
        JobKind::Landing | JobKind::CloudGate | JobKind::CloudSweep => is_manager || is_lead,
    }
}

/// [`job_kind_permitted`] plus #1099: a `test` job with `candidate_receipt`
/// fetches and builds an arbitrary branch, so it is a manager/Epic-lead job
/// like landing.
pub(crate) fn job_permitted(params: &JobParams, is_manager: bool, is_lead: bool) -> bool {
    let candidate_receipt = matches!(params, JobParams::Test(p) if p.is_candidate_receipt());
    job_kind_permitted(params.kind(), is_manager, is_lead)
        && (!candidate_receipt || is_manager || is_lead)
}

/// #1337: stamp the operator's default wall-clock timeout on a `test` job
/// that names none (a `candidate_receipt` run keeps the unit cap), and report
/// whether an explicit timeout raises the default. Admission checks the
/// manager/Epic-lead role or the bounded QA Issue-worker binding.
pub(crate) fn stamp_test_timeout(mut params: JobParams, default_minutes: u32) -> (JobParams, bool) {
    let JobParams::Test(test) = &mut params else {
        return (params, false);
    };
    let raises = match test.timeout_minutes {
        Some(minutes) => minutes > default_minutes,
        None => {
            if !test.is_candidate_receipt() {
                test.timeout_minutes = Some(if test.qa_lane.is_some() {
                    default_minutes.min(JOB_QA_LANE_TIMEOUT_MAX_MINS)
                } else {
                    default_minutes
                });
            }
            false
        }
    };
    (params, raises)
}

/// #1235: the session whose sandbox a manager acts on (`AgentSubmitJob`
/// `sandbox_session_id`, `AgentEnqueueLandingSource` `source_session_id`).
/// The caller's manager scope (project manager, area node or global seat)
/// must reach it, a named `project_id` must be its project, and the scope's
/// policy must execute. Any other caller is refused `denied`.
pub(crate) fn manager_reached_sandbox(
    store: &crate::store::Store,
    caller: Uuid,
    target: Uuid,
    project: Option<Uuid>,
    denied: &'static str,
) -> Result<rsi_common::types::Session> {
    use rsi_common::harness_manager_v2::ManagerOperatingModeV2;
    let scope = store
        .manager_session_control_scope(caller, target, false)?
        .ok_or_else(|| DaemonError::PolicyDenied(denied.into()))?;
    if project.is_some_and(|project| scope.target.project_id != Some(project)) {
        return Err(DaemonError::InvalidParam(
            rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE.into(),
        ));
    }
    let executes = store
        .manager_policy_for_config(&scope.config)?
        .is_some_and(|grant| {
            !grant.revoked
                && grant.policy.mode == ManagerOperatingModeV2::Execute
                && !grant.policy.paused
        });
    if !executes {
        return Err(DaemonError::PolicyDenied(denied.into()));
    }
    Ok(scope.target)
}

/// The request fields an admission reads; the effect-time recheck replays them.
#[derive(Clone)]
struct JobTarget {
    sandbox_session_id: Option<Uuid>,
    project_id: Option<Uuid>,
    has_worktree: bool,
    /// #1337: the job's timeout is above the operator default.
    raises_timeout: bool,
}

impl JobTarget {
    fn of(request: &AgentSubmitJobRequestV1, raises_timeout: bool) -> Self {
        Self {
            sandbox_session_id: request.sandbox_session_id,
            project_id: request.project_id,
            has_worktree: request.worktree.is_some(),
            raises_timeout,
        }
    }
}

/// What a submit resolved from the token-bound caller.
#[derive(Clone)]
struct JobAdmission {
    session: rsi_common::types::Session,
    is_manager: bool,
    is_lead: bool,
    /// The in-reach terminal session whose sandbox runs the job (#1235).
    sandbox: Option<rsi_common::types::Session>,
    /// #1278: the authority fence, set when the job needed manager or lead
    /// authority or a reached sandbox (a plain test or build needs none).
    fence: Option<String>,
    /// #1520: exact live (project, Issue, launch) binding, rechecked at effect.
    qa_binding: Option<(Uuid, Uuid, Uuid)>,
}

impl JobAdmission {
    /// The facts the job directory was resolved from.
    fn sandbox_dirs(&self) -> Option<(Option<Uuid>, Option<PathBuf>, PathBuf)> {
        self.sandbox.as_ref().map(|target| {
            (
                target.project_id,
                target.sandbox_root.clone(),
                target.working_dir.clone(),
            )
        })
    }
}

/// The admission shared by the first resolution and the effect-time recheck
/// under the store lock (#1278).
fn admit_job(
    store: &crate::store::Store,
    caller: Uuid,
    request: &JobTarget,
    params: &JobParams,
) -> Result<JobAdmission> {
    let projection = store
        .agent_authority_projection(caller)
        .map_err(|_| DaemonError::PolicyDenied(JOB_DIR_NOT_ALLOWED.into()))?;
    let session = store
        .get_session(caller)?
        .ok_or_else(|| DaemonError::PolicyDenied(JOB_DIR_NOT_ALLOWED.into()))?;
    // #1235: a landing or cloud-gate job in a terminal, in-reach
    // session's sandbox (one writer per sandbox).
    let sandbox = match request.sandbox_session_id {
        Some(target) => {
            if !matches!(params.kind(), JobKind::Landing | JobKind::CloudGate)
                || request.has_worktree
            {
                return Err(DaemonError::InvalidParam(JOB_INVALID_PARAMS.into()));
            }
            let target = manager_reached_sandbox(
                store,
                caller,
                target,
                request.project_id,
                JOB_KIND_NOT_AUTHORIZED,
            )?;
            if !target.status.is_terminal() {
                return Err(DaemonError::InvalidParam(JOB_SANDBOX_SESSION_LIVE.into()));
            }
            Some(target)
        }
        None => {
            if request
                .project_id
                .is_some_and(|project| session.project_id != Some(project))
            {
                return Err(DaemonError::InvalidParam(
                    rsi_common::global_manager::MANAGER_PROJECT_NOT_IN_SCOPE.into(),
                ));
            }
            None
        }
    };
    if sandbox.is_none() && !job_permitted(params, projection.is_manager, projection.is_lead) {
        return Err(DaemonError::PolicyDenied(JOB_KIND_NOT_AUTHORIZED.into()));
    }
    let qa = matches!(params, JobParams::Test(test) if test.qa_lane.is_some());
    let qa_binding = if qa {
        if request.has_worktree || request.sandbox_session_id.is_some() {
            return Err(DaemonError::InvalidParam(JOB_INVALID_PARAMS.into()));
        }
        store
            .live_qa_lane_binding(caller)?
            .filter(|(project, _, _)| session.project_id == Some(*project))
    } else {
        None
    };
    if (request.raises_timeout || qa)
        && !(projection.is_manager || projection.is_lead || qa_binding.is_some())
    {
        return Err(DaemonError::PolicyDenied(JOB_TIMEOUT_NOT_AUTHORIZED.into()));
    }
    let needs_authority =
        sandbox.is_some() || !job_permitted(params, false, false) || request.raises_timeout || qa;
    let fence = needs_authority
        .then(|| store.agent_authority_fence(caller, request.sandbox_session_id))
        .transpose()
        .map_err(|_| DaemonError::PolicyDenied(JOB_KIND_NOT_AUTHORIZED.into()))?;
    Ok(JobAdmission {
        session,
        is_manager: projection.is_manager,
        is_lead: projection.is_lead,
        sandbox,
        fence,
        qa_binding,
    })
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
        let default_timeout =
            self.runtime_config
                .as_ref()
                .map_or(JOB_TEST_TIMEOUT_DEFAULT_MINS, |config| {
                    config
                        .job_test_timeout_mins
                        .load(std::sync::atomic::Ordering::Relaxed)
                });
        let (params, raises_timeout) = stamp_test_timeout(params, default_timeout);
        let admission = {
            let store = self.store.lock().await;
            admit_job(
                &store,
                caller,
                &JobTarget::of(&request, raises_timeout),
                &params,
            )?
        };
        let JobAdmission {
            session,
            is_manager,
            sandbox,
            ..
        } = admission.clone();
        // #1073: a new job would keep a waiting deploy from its quiet point.
        // #1566: a `test` or `build` job is accepted and held `queued` (the
        // job loop launches it once the drain ends or the deploy has
        // restarted); landing, cloud-gate and cloud-sweep jobs keep the typed
        // retryable refusal. The deploy's caller and parentless sessions run.
        let mut hold = false;
        if let Some(drain) = &self.deploy_drain {
            if matches!(params.kind(), JobKind::Test | JobKind::Build) {
                hold = drain.holds(Some(caller), session.parent_id.is_some());
            } else {
                drain.refuse_if_draining(Some(caller), session.parent_id.is_some())?;
            }
        }
        let recheck = JobTarget::of(&request, raises_timeout);
        let requested = request.worktree.as_deref().map(PathBuf::from);
        if requested.as_deref().is_some_and(|p| !p.is_absolute()) {
            return Err(DaemonError::InvalidParam(JOB_INVALID_PARAMS.into()));
        }
        let project_id = sandbox
            .as_ref()
            .map_or(session.project_id, |target| target.project_id);
        let (sandbox_root, working_dir) = match &sandbox {
            Some(target) => (target.sandbox_root.clone(), target.working_dir.clone()),
            None => (session.sandbox_root.clone(), session.working_dir.clone()),
        };
        let is_manager = is_manager && sandbox.is_none();
        let qa_sha = match &params {
            JobParams::Test(test) => test.qa_lane.as_ref().map(|qa| qa.sha.clone()),
            _ => None,
        };
        // Repository observation is bounded and runs without the shared Store
        // mutex. The pin audits submission HEAD, not the bytes later tested.
        let cwd = tokio::task::spawn_blocking(move || {
            let cwd = crate::agent_jobs::resolve_cwd(
                sandbox_root.as_deref(),
                &working_dir,
                is_manager,
                requested.as_deref(),
            )?;
            Ok::<_, DaemonError>(cwd)
        })
        .await
        .map_err(|error| DaemonError::Process(format!("job directory probe: {error}")))??;
        if let Some(expected) = qa_sha {
            // QA admission forbids requested worktrees; resolve_cwd returned
            // the canonical owner sandbox, never a working_dir fallback.
            if observe_qa_head(&cwd).await? != expected {
                return Err(DaemonError::InvalidParam(JOB_QA_LANE_SHA_MISMATCH.into()));
            }
        }
        let ctx = SubmitContext {
            owner: caller,
            project_id,
            cwd,
            name: request.name,
            params,
            idempotency_key: request.idempotency_key,
            wake: request.wake.unwrap_or_default(),
        };
        #[cfg(test)]
        super::effect_fence::seam::run(&self.store, caller, "submit_job").await;
        let store = std::sync::Arc::clone(&self.store);
        let (row, replayed) = tokio::task::spawn_blocking(move || {
            let jobs_dir = crate::agent_jobs::jobs_dir()
                .map_err(|error| DaemonError::Process(format!("job directory: {error}")))?;
            // #1278: the lock was released for the directory probe. Re-resolve
            // the same admission under the lock that stays held through the
            // insert and launch; refuse if caller, grant, project or the
            // target's reach and status changed.
            let store = store.blocking_lock();
            let current = admit_job(&store, caller, &recheck, &ctx.params)?;
            if current.fence != admission.fence
                || current.qa_binding != admission.qa_binding
                || current.is_manager != admission.is_manager
                || current.is_lead != admission.is_lead
                || current.sandbox_dirs() != admission.sandbox_dirs()
            {
                return Err(DaemonError::PolicyDenied(JOB_KIND_NOT_AUTHORIZED.into()));
            }
            // Binding/delegation recheck and insert share this daemon's Store
            // mutex. Only the capacity check is inside insert's IMMEDIATE
            // transaction; independent SQL writers are outside this fence.
            crate::agent_jobs::submit_with_hold(
                &store,
                &*runtime,
                &tools,
                &jobs_dir,
                ctx,
                hold,
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

    /// `AgentCancelJob` (#1106): the owner stops a running job.
    ///
    /// # Errors
    /// `job_not_found` for an unknown job or one owned by another session.
    pub async fn agent_cancel_job(
        &self,
        caller: Uuid,
        request: AgentCancelJobRequestV1,
        runtime: std::sync::Arc<dyn JobRuntime>,
    ) -> Result<AgentCancelJobResultV1> {
        let store = std::sync::Arc::clone(&self.store);
        let (job, cancelled) = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            let owned = store
                .get_agent_job(request.job_id)?
                .is_some_and(|row| row.job.owner_session_id == caller);
            if !owned {
                return Err(DaemonError::InvalidParam(JOB_NOT_FOUND.into()));
            }
            crate::agent_jobs::cancel_job(&store, &*runtime, request.job_id, chrono::Utc::now())
        })
        .await
        .map_err(|error| DaemonError::Process(format!("job cancel: {error}")))??;
        Ok(AgentCancelJobResultV1 { job, cancelled })
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
    use super::QA_HEAD_PROBES;
    use super::job_kind_permitted;
    use super::job_permitted;
    use rsi_common::agent_jobs::{JobKind, JobParams};

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn qa_lane_omitted_timeout_obeys_default_and_fixed_ceiling() {
        use super::stamp_test_timeout;
        use rsi_common::agent_jobs::AgentSubmitJobRequestV1;
        for (default, expected) in [(20, 20), (180, 90)] {
            let params = AgentSubmitJobRequestV1 {
                kind: JobKind::Test,
                params: serde_json::json!({"shard":"store-01",
                    "qa_lane":{"sha":"a".repeat(40)}}),
                project_id: None,
                sandbox_session_id: None,
                name: None,
                idempotency_key: None,
                worktree: None,
                wake: None,
            }
            .typed_params()
            .unwrap();
            let (JobParams::Test(test), raises) = stamp_test_timeout(params, default) else {
                panic!("test job")
            };
            assert_eq!(test.timeout_minutes, Some(expected));
            assert!(!raises);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[derive(Default)]
    struct QaRecorder(std::sync::Mutex<Vec<crate::agent_jobs::LaunchSpec>>);

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    impl crate::agent_jobs::JobRuntime for QaRecorder {
        fn launch(&self, spec: &crate::agent_jobs::LaunchSpec) -> std::result::Result<(), String> {
            self.0.lock().unwrap().push(spec.clone());
            Ok(())
        }
        fn unit_active(&self, _: &str) -> bool {
            true
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    impl Drop for QaRecorder {
        fn drop(&mut self) {
            for spec in self.0.get_mut().unwrap().iter() {
                let _ = std::fs::remove_dir_all(spec.log_path.with_extension("tmp"));
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    struct QaFixture {
        dir: tempfile::TempDir,
        control: super::AgentControlHandle,
        worker: uuid::Uuid,
        operation: uuid::Uuid,
        sha: String,
        recorder: std::sync::Arc<QaRecorder>,
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    impl QaFixture {
        fn git(&self, args: &[&str]) -> String {
            let out = std::process::Command::new("git")
                .current_dir(self.dir.path())
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        }
        fn request(&self) -> rsi_common::agent_jobs::AgentSubmitJobRequestV1 {
            rsi_common::agent_jobs::AgentSubmitJobRequestV1 {
                kind: JobKind::Test,
                params: serde_json::json!({"shard":"store-01",
                    "qa_lane":{"sha":self.sha},"timeout_minutes":90}),
                name: None,
                idempotency_key: None,
                worktree: None,
                wake: Some(rsi_common::agent_jobs::JobWake::None),
                project_id: None,
                sandbox_session_id: None,
            }
        }
        async fn submit(
            &self,
        ) -> crate::error::Result<rsi_common::agent_jobs::AgentSubmitJobReceiptV1> {
            self.control
                .agent_submit_job(
                    self.worker,
                    self.request(),
                    self.recorder.clone(),
                    crate::agent_jobs::JobTools {
                        cargo_slot: "/x/slot".into(),
                        lander: "/x/lander".into(),
                    },
                )
                .await
        }
        async fn assert_no_effects(&self) {
            assert_eq!(self.recorder.0.lock().unwrap().len(), 0);
            assert_eq!(
                self.control
                    .store
                    .lock()
                    .await
                    .list_agent_jobs(self.worker, 10)
                    .unwrap()
                    .len(),
                0
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn qa_fixture(delegated: bool) -> QaFixture {
        qa_fixture_for_role(delegated, None)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn qa_fixture_for_role(delegated: bool, role: Option<&str>) -> QaFixture {
        use rsi_common::types::{NewIssue, SessionStatus};
        use std::sync::Arc;
        let dir = crate::test_support::disk_backed_tempdir("qa-review");
        let store = crate::store::Store::open_in_memory().unwrap();
        let mut worker = rsid_store::test_support::make_test_session();
        worker.status = SessionStatus::Running;
        worker.agent_role = role.map(str::to_owned);
        worker.title = Some("QA runner".into());
        worker.query = "run QA".into();
        worker.sandbox_root = Some(dir.path().into());
        worker.working_dir = dir.path().into();
        store.insert_session(&worker).unwrap();
        let manager = rsid_store::test_support::make_test_session();
        store.insert_session(&manager).unwrap();
        let project = worker.project_id.unwrap();
        let issue = store
            .create_issue(&NewIssue {
                project_id: project,
                title: "QA".into(),
                body: String::new(),
                priority: None,
                labels: vec![],
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap();
        let operation = uuid::Uuid::new_v4();
        let payload = serde_json::json!({"issue_binding":{"issue_id":issue.id,
            "display_number":issue.display_number,"qa_lane":delegated}});
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,
            scope_version,policy_version,idempotency_key,fingerprint,kind,payload_json,
            state,target_session_id,outcome_json,not_before,created_at,updated_at)
            VALUES(?1,?2,?3,1,1,?1,'fixture','lifecycle_action',?4,'succeeded',?5,'{}',?6,?6,?6)",
                rusqlite::params![
                    operation.to_string(),
                    project.to_string(),
                    manager.id.to_string(),
                    payload.to_string(),
                    worker.id.to_string(),
                    now
                ],
            )
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let control = super::AgentControlHandle::new(
            Default::default(),
            Default::default(),
            Arc::new(tokio::sync::Mutex::new(store)),
            Arc::new(crate::bus::EventBus::new(16)),
            Arc::new(crate::session::spawn_coordinator::SpawnCoordinator::new(tx)),
        );
        let mut f = QaFixture {
            dir,
            control,
            worker: worker.id,
            operation,
            sha: String::new(),
            recorder: Arc::new(QaRecorder::default()),
        };
        f.git(&["init", "-q"]);
        std::fs::write(f.dir.path().join("tracked"), "baseline\n").unwrap();
        f.git(&["add", "tracked"]);
        f.git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "pin",
        ]);
        f.sha = f.git(&["rev-parse", "HEAD"]);
        f
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn qa_lane_submit_refuses_undelegated_bound_workers_and_reviewers() {
        let f = qa_fixture(false);
        let err = f.submit().await.unwrap_err();
        assert!(
            err.to_string().contains("job_timeout_not_authorized"),
            "{err}"
        );
        f.assert_no_effects().await;
        // A reviewer with a live but undelegated Issue binding is also refused;
        // both fixtures carry a QA-looking title/brief from creation.
        let reviewer = qa_fixture_for_role(false, Some("reviewer"));
        let err = reviewer.submit().await.unwrap_err();
        assert!(
            err.to_string().contains("job_timeout_not_authorized"),
            "{err}"
        );
        reviewer.assert_no_effects().await;
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn qa_lane_submit_rechecks_manager_delegation_at_effect() {
        let f = qa_fixture(true);
        let operation = f.operation;
        super::super::effect_fence::seam::install(f.worker, "submit_job", move |store| {
            store.conn.execute(
                "UPDATE harness_manager_v2_operations SET payload_json=json_set(payload_json,'$.issue_binding.qa_lane',json('false')) WHERE id=?1",
                [operation.to_string()]).unwrap();
        });
        let error = f.submit().await.unwrap_err();
        assert!(
            error.to_string().contains("job_timeout_not_authorized"),
            "{error}"
        );
        f.assert_no_effects().await;
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn qa_lane_submit_ignores_redirected_git_environment() {
        let f = qa_fixture(true);
        let other = qa_fixture(true);
        other.git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "--allow-empty",
            "-qm",
            "other HEAD",
        ]);
        assert_ne!(f.sha, other.git(&["rev-parse", "HEAD"]));
        let redirect = other.dir.path().join(".git");
        assert!(
            QA_HEAD_PROBES
                .lock()
                .unwrap()
                .insert(
                    f.dir.path().into(),
                    Box::new(move |command, _| {
                        command.env("GIT_DIR", redirect);
                        command
                            .env("GIT_CONFIG_COUNT", "1")
                            .env("GIT_CONFIG_KEY_0", "core.fsmonitor")
                            .env("GIT_CONFIG_VALUE_0", "/nonexistent/injected-helper");
                    })
                )
                .is_none()
        );
        // Dirty bytes are deliberately outside this HEAD-only admission policy.
        std::fs::write(f.dir.path().join("tracked"), "uncommitted bytes").unwrap();
        let receipt = f.submit().await.unwrap();
        let JobParams::Test(params) = receipt.job.params else {
            panic!("QA test params")
        };
        assert_eq!(params.qa_lane.unwrap().sha, f.sha);
        let launches = f.recorder.0.lock().unwrap();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].cwd, f.dir.path());
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn qa_lane_submit_ignores_global_include_helpers() {
        use std::os::unix::fs::PermissionsExt;
        let f = qa_fixture(true);
        let home = crate::test_support::disk_backed_tempdir("qa-git-home");
        let marker = f.dir.path().join(".git/helper-ran");
        let helper = f.dir.path().join(".git/monitor");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\ntouch '{}'\nprintf 'token\\0'\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let included = home.path().join("included.config");
        std::fs::write(
            &included,
            format!(
                "[core]\nfsmonitor = {}\nfsmonitorHookVersion = 2\n",
                helper.display()
            ),
        )
        .unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            format!(
                "[includeIf \"gitdir:{}/.git\"]\npath = {}\n",
                f.dir.path().display(),
                included.display()
            ),
        )
        .unwrap();
        let plain = std::process::Command::new("git")
            .current_dir(f.dir.path())
            .env("HOME", home.path())
            .env_remove("GIT_CONFIG_GLOBAL")
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(plain.status.success(), "{plain:?}");
        assert!(
            marker.exists(),
            "positive control: includeIf fsmonitor executes"
        );
        std::fs::remove_file(&marker).unwrap();
        let install_home = || {
            let home_path = home.path().to_owned();
            assert!(
                QA_HEAD_PROBES
                    .lock()
                    .unwrap()
                    .insert(
                        f.dir.path().into(),
                        Box::new(move |command, _| {
                            command
                                .env("HOME", &home_path)
                                .env("GIT_CONFIG_GLOBAL", home_path.join(".gitconfig"));
                        })
                    )
                    .is_none()
            );
        };
        install_home();
        f.submit().await.unwrap();
        assert!(
            !marker.exists(),
            "HEAD-only probe must not execute the helper"
        );
        // Malformed included config also proves global config is excluded.
        std::fs::write(&included, "invalid config without a section\n").unwrap();
        install_home();
        f.submit().await.unwrap();
        assert!(
            !marker.exists(),
            "HEAD-only probe must not execute the helper"
        );
        assert_eq!(f.recorder.0.lock().unwrap().len(), 2);
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn qa_lane_submit_probe_deadline_does_not_hold_store_or_insert() {
        let f = std::sync::Arc::new(qa_fixture(true));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        assert!(
            QA_HEAD_PROBES
                .lock()
                .unwrap()
                .insert(
                    f.dir.path().into(),
                    Box::new(move |command, limits| {
                        *command = tokio::process::Command::new("/bin/sh");
                        command.args(["-c", "exec sleep 60"]);
                        limits.execution_timeout = std::time::Duration::from_millis(400);
                        let _ = started_tx.send(());
                    })
                )
                .is_none()
        );
        let task_fixture = f.clone();
        let begin = std::time::Instant::now();
        let submit = tokio::spawn(async move { task_fixture.submit().await });
        tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
            .await
            .expect("submit must reach the bounded HEAD probe")
            .unwrap();
        let unlocked = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            f.control.store.lock(),
        )
        .await
        .expect("a hanging Git probe must not hold the shared Store mutex");
        assert_eq!(unlocked.list_agent_jobs(f.worker, 10).unwrap().len(), 0);
        drop(unlocked);
        let err = submit.await.unwrap().unwrap_err();
        assert!(
            err.to_string().contains("job_qa_lane_sha_mismatch"),
            "{err}"
        );
        assert!(begin.elapsed() < std::time::Duration::from_secs(3));
        f.assert_no_effects().await;
    }

    /// A real submit with a fake service backend: no provider, shell or long job runs.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn qa_lane_submit_requires_live_binding_and_preserves_timeout_guards() {
        use super::*;
        use crate::agent_jobs::{JobRuntime, LaunchSpec};
        use rsi_common::types::{NewIssue, SessionStatus};
        use std::sync::Arc;

        #[derive(Default)]
        struct Recorder(std::sync::Mutex<Vec<LaunchSpec>>);
        impl JobRuntime for Recorder {
            fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
                self.0.lock().unwrap().push(spec.clone());
                Ok(())
            }
            fn unit_active(&self, _: &str) -> bool {
                true
            }
        }
        let dir = crate::test_support::disk_backed_tempdir("qa-lane");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "--allow-empty",
            "-qm",
            "pin",
        ]);
        let sha = git(&["rev-parse", "HEAD"]);
        let store = crate::store::Store::open_in_memory().unwrap();
        let mut worker = rsid_store::test_support::make_test_session();
        worker.status = SessionStatus::Running;
        worker.sandbox_root = Some(dir.path().into());
        worker.working_dir = dir.path().into();
        store.insert_session(&worker).unwrap();
        let manager = rsid_store::test_support::make_test_session();
        store.insert_session(&manager).unwrap();
        let project = worker.project_id.unwrap();
        let issue = store
            .create_issue(&NewIssue {
                project_id: project,
                title: "QA lane".into(),
                body: String::new(),
                priority: None,
                labels: vec![],
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap();
        let operation = Uuid::new_v4();
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let control = AgentControlHandle::new(
            Default::default(),
            Default::default(),
            store.clone(),
            Arc::new(crate::bus::EventBus::new(16)),
            Arc::new(crate::session::spawn_coordinator::SpawnCoordinator::new(tx)),
        );
        let recorder = Arc::new(Recorder::default());
        let tools = || JobTools {
            cargo_slot: "/x/cargo-slot".into(),
            lander: "/x/lander".into(),
        };
        let request = |qa: bool, minutes: u32| {
            let mut params = serde_json::json!({"shard":"store-01","timeout_minutes":minutes});
            if qa {
                params["qa_lane"] = serde_json::json!({"sha":sha});
            }
            AgentSubmitJobRequestV1 {
                kind: JobKind::Test,
                params,
                name: None,
                idempotency_key: None,
                worktree: None,
                wake: Some(rsi_common::agent_jobs::JobWake::None),
                project_id: None,
                sandbox_session_id: None,
            }
        };
        // Ordinary workers cannot opt into QA or exceed the operator default.
        for req in [request(false, 21), request(true, 90), request(true, 10)] {
            let err = control
                .agent_submit_job(worker.id, req, recorder.clone(), tools())
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("job_timeout_not_authorized"),
                "{err}"
            );
        }
        {
            let store = store.lock().await;
            let payload = serde_json::json!({"issue_binding":{"issue_id":issue.id,"display_number":issue.display_number,"qa_lane":true}});
            let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            store.conn.execute("INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,
                scope_version,policy_version,idempotency_key,fingerprint,kind,payload_json,
                state,target_session_id,outcome_json,not_before,created_at,updated_at)
                VALUES(?1,?2,?3,1,1,?1,'fixture','lifecycle_action',?4,'succeeded',?5,'{}',?6,?6,?6)",
                rusqlite::params![operation.to_string(),project.to_string(),manager.id.to_string(),
                    payload.to_string(),worker.id.to_string(),now]).unwrap();
        }
        let accepted = control
            .agent_submit_job(worker.id, request(true, 90), recorder.clone(), tools())
            .await
            .unwrap();
        assert_eq!(
            crate::agent_jobs::job_timeout_secs(&accepted.job.params),
            Some(90 * 60)
        );
        assert_eq!(
            recorder.0.lock().unwrap()[0].command.runtime_max_secs,
            95 * 60
        );
        assert!(
            recorder.0.lock().unwrap()[0]
                .command
                .argv
                .ends_with(&["shard".into(), "store-01".into()])
        );
        assert_eq!(accepted.job.kind, JobKind::Test);
        // Binding grants only the QA form, never ordinary long test jobs.
        let err = control
            .agent_submit_job(worker.id, request(false, 90), recorder.clone(), tools())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("job_timeout_not_authorized"),
            "{err}"
        );
        let err = control
            .agent_submit_job(worker.id, request(true, 91), recorder.clone(), tools())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("job_invalid_params"), "{err}");
        // Delegation never raises the ceiling for the newly landed recipe form.
        let mut recipe = request(false, 21);
        recipe.params = serde_json::json!({"recipe":"e2e","timeout_minutes":21});
        let error = control
            .agent_submit_job(worker.id, recipe, recorder.clone(), tools())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("job_timeout_not_authorized"),
            "{error}"
        );
        let mut mismatch = request(true, 90);
        mismatch.params["qa_lane"]["sha"] = "0".repeat(40).into();
        let err = control
            .agent_submit_job(worker.id, mismatch, recorder.clone(), tools())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("job_qa_lane_sha_mismatch"),
            "{err}"
        );
        // A revoked launch in the probe/effect window must not start a service.
        super::super::effect_fence::seam::install(worker.id, "submit_job", move |store| {
            store
                .conn
                .execute(
                    "UPDATE harness_manager_v2_operations SET state='failed' WHERE id=?1",
                    [operation.to_string()],
                )
                .unwrap();
        });
        let err = control
            .agent_submit_job(worker.id, request(true, 90), recorder.clone(), tools())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("job_timeout_not_authorized"),
            "{err}"
        );
        assert_eq!(recorder.0.lock().unwrap().len(), 1);
        assert_eq!(
            store
                .lock()
                .await
                .list_agent_jobs(worker.id, 10)
                .unwrap()
                .len(),
            1
        );
        // Terminal worker cannot use a binding even when the launch succeeded.
        {
            let store = store.lock().await;
            store
                .conn
                .execute(
                    "UPDATE harness_manager_v2_operations SET state='succeeded' WHERE id=?1",
                    [operation.to_string()],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE sessions SET status='Completed' WHERE id=?1",
                    [worker.id.to_string()],
                )
                .unwrap();
        }
        let err = control
            .agent_submit_job(worker.id, request(true, 90), recorder.clone(), tools())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("job_timeout_not_authorized"),
            "{err}"
        );
        // Rotation inherits lineage metadata, not the manager Issue binding.
        let mut rotated = worker.clone();
        rotated.id = Uuid::new_v4();
        rotated.continued_from = Some(worker.id);
        store.lock().await.insert_session(&rotated).unwrap();
        let err = control
            .agent_submit_job(rotated.id, request(true, 90), recorder.clone(), tools())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("job_timeout_not_authorized"),
            "{err}"
        );
        // An admitted later launch supersedes a resumed predecessor, even if
        // that later launch failed. Never delete the admission journal row.
        {
            let store = store.lock().await;
            store
                .conn
                .execute(
                    "UPDATE sessions SET status='Running' WHERE id=?1",
                    [worker.id.to_string()],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,
                scope_version,policy_version,idempotency_key,fingerprint,kind,payload_json,
                state,target_session_id,outcome_json,not_before,created_at,updated_at)
                SELECT ?2,project_id,manager_session_id,scope_version,policy_version,?2,fingerprint,
                  kind,payload_json,'failed',?3,outcome_json,not_before,created_at,updated_at
                FROM harness_manager_v2_operations WHERE id=?1",
                    rusqlite::params![
                        operation.to_string(),
                        Uuid::new_v4().to_string(),
                        rotated.id.to_string()
                    ],
                )
                .unwrap();
        }
        let err = control
            .agent_submit_job(worker.id, request(true, 90), recorder.clone(), tools())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("job_timeout_not_authorized"),
            "{err}"
        );
        assert_eq!(recorder.0.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(
            std::path::Path::new(&accepted.job.log_path).with_extension("tmp"),
        );
    }

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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_candidate_receipt_is_a_manager_or_lead_job_while_plain_tests_stay_open() {
        use rsi_common::agent_jobs::{AgentSubmitJobRequestV1, JobKind};
        let params = |value: serde_json::Value| {
            AgentSubmitJobRequestV1 {
                project_id: None,
                sandbox_session_id: None,
                kind: JobKind::Test,
                params: value,
                name: None,
                idempotency_key: None,
                worktree: None,
                wake: None,
            }
            .typed_params()
            .expect("valid test params")
        };
        let receipt = params(serde_json::json!({"candidate_receipt":"rsi/abc-123"}));
        assert!(!job_permitted(&receipt, false, false));
        assert!(job_permitted(&receipt, true, false));
        assert!(job_permitted(&receipt, false, true));
        let plain = params(serde_json::json!({"package":"rsid","lib_only":true}));
        assert!(job_permitted(&plain, false, false));
    }

    /// #1337: the default is stamped on a plain test job, a candidate receipt
    /// keeps the unit cap, an explicit timeout above the default is a raise,
    /// and other kinds are untouched.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn test_jobs_get_the_default_timeout_and_raises_are_flagged() {
        use super::stamp_test_timeout;
        use rsi_common::agent_jobs::{AgentSubmitJobRequestV1, JobKind, JobParams};
        let params = |kind: JobKind, value: serde_json::Value| {
            AgentSubmitJobRequestV1 {
                project_id: None,
                sandbox_session_id: None,
                kind,
                params: value,
                name: None,
                idempotency_key: None,
                worktree: None,
                wake: None,
            }
            .typed_params()
            .expect("valid params")
        };
        let timeout = |params: &JobParams| match params {
            JobParams::Test(test) => test.timeout_minutes,
            _ => None,
        };
        let (plain, raises) = stamp_test_timeout(
            params(JobKind::Test, serde_json::json!({"package":"rsid"})),
            20,
        );
        assert_eq!((timeout(&plain), raises), (Some(20), false));
        let recipe = params(JobKind::Test, serde_json::json!({"recipe":"check-cpu"}));
        assert!(super::job_permitted(&recipe, false, false));
        let (recipe, raises) = stamp_test_timeout(recipe, 20);
        assert_eq!((timeout(&recipe), raises), (Some(20), false));
        let (_, raises) = stamp_test_timeout(
            params(
                JobKind::Test,
                serde_json::json!({"recipe":"e2e","timeout_minutes":21}),
            ),
            20,
        );
        assert!(raises);
        let (lower, raises) = stamp_test_timeout(
            params(
                JobKind::Test,
                serde_json::json!({"shard":"other-01","timeout_minutes":10}),
            ),
            20,
        );
        assert_eq!((timeout(&lower), raises), (Some(10), false));
        let (higher, raises) = stamp_test_timeout(
            params(
                JobKind::Test,
                serde_json::json!({"package":"rsid","timeout_minutes":21}),
            ),
            20,
        );
        assert_eq!((timeout(&higher), raises), (Some(21), true));
        let (receipt, raises) = stamp_test_timeout(
            params(
                JobKind::Test,
                serde_json::json!({"candidate_receipt":"rsi/abc-123"}),
            ),
            20,
        );
        assert_eq!((timeout(&receipt), raises), (None, false));
        let build = params(
            JobKind::Build,
            serde_json::json!({"command":"check","workspace":true}),
        );
        assert_eq!(stamp_test_timeout(build.clone(), 20), (build, false));
    }
}
