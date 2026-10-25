//! The five Bench methods (#3178 items 3 and 5), scoped to one run.
//!
//! * [`BenchService::start_task`]: idempotent per (run, trial). It checks the
//!   run is active, the task is in the run's pinned set, `allowed_launches` for
//!   (OpenRouter, model, effort), the M4 run budget against the per-task cap,
//!   the concurrency cap and the container target against the run's pinned
//!   digests; then it launches an `is_eval`, `skip_context_pipeline` Harness
//!   session with the bench profile in an empty per-task host scratch directory.
//! * [`BenchService::task_status`], [`BenchService::cancel_task`] and
//!   [`BenchService::finish_task`] read, stop and settle a trial. A trial
//!   settles when its session ended: usage from the increment SQL, the route
//!   audit, the `harness_build_digest` and the sha256 of the live trace.
//! * [`BenchService::read_trace`] pages the full, untruncated trace.

use crate::bench::{BenchError, BenchService, private_dir};
use crate::claude::LaunchConfig;
use crate::store::bench::{BenchClaim, BenchRunRow, BenchSettlement, BenchTaskRow, NewBenchTask};
use rsi_common::bench::{
    BENCH_AGENT_TIMEOUT, BENCH_CANCELLED, BENCH_CONCURRENCY_EXCEEDED, BENCH_LAUNCH_NOT_ALLOWED,
    BENCH_RUN_BUDGET_INSUFFICIENT, BENCH_RUN_NOT_ACTIVE, BENCH_TASK_NOT_FINISHED,
    BENCH_TASK_UNKNOWN, BENCH_TRACE_UNAVAILABLE, BENCH_TRACE_WRITE_FAILED, BENCH_TRIAL_CONFLICT,
    BENCH_TRIAL_UNKNOWN, BenchRunState, BenchTaskStartParams, BenchTaskState, BenchTaskStatusV1,
    BenchTraceChunkV1, BenchTraceHandle, BenchTraceReadParams, BenchUsage,
    DEFAULT_TRACE_PAGE_BYTES, MAX_TRACE_PAGE_BYTES,
};
use rsi_common::bench_overlay::BenchOverlay;
use rsi_common::bench_profile::{BENCH_INVALID_ROUTE, BenchLaunchProfile};
use rsi_common::exec_target::{
    ContainerExecTarget, DEFAULT_PER_CALL_CEILING_SECS, EXEC_TARGET_INVALID, ExecTarget,
};
use rsi_common::harness_manager_v2::ManagerLaunchChoiceV2;
use rsi_common::types::{Session, SessionKind, SessionProvider, SessionStatus};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// Grace added to a trial's `agent_timeout_sec` before the daemon stops a
/// session its adapter forgot (a vanished laptop). The adapter's own timeout
/// and cancel come first.
pub const TIMEOUT_GRACE_SECS: u64 = 30;

/// The longest trace line a page extends to (a window with no newline).
const MAX_TRACE_LINE_BYTES: u64 = 16 * 1024 * 1024;

fn request_digest(params: &BenchTaskStartParams) -> String {
    let bytes = serde_json::to_vec(params).unwrap_or_default();
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn micro_usd(usd: f64) -> u64 {
    (usd * 1_000_000.0).round().max(0.0) as u64
}

fn not_active(run_id: Uuid) -> BenchError {
    BenchError::new(
        BENCH_RUN_NOT_ACTIVE,
        format!("{BENCH_RUN_NOT_ACTIVE}: run {run_id}"),
    )
}

fn unknown_trial(trial: &str) -> BenchError {
    BenchError::new(
        BENCH_TRIAL_UNKNOWN,
        format!("{BENCH_TRIAL_UNKNOWN}: no trial {trial} in this run"),
    )
}

/// The tag of the shared project-level Group that holds the trials of runs with no
/// Epic above their submitter (#3992).
pub(crate) const BENCH_FALLBACK_TAG: &str = "bench-benchmarks";

/// Orphan trials the #4013 backfill considers per pass; a larger backlog is
/// finished by the next pass.
const ORPHAN_SCAN_LIMIT: usize = 2000;

/// The Harness session of one trial, as a launch.
fn launch_config(
    run: &BenchRunRow,
    params: &BenchTaskStartParams,
    scratch: &Path,
    trace_path: &Path,
    target: ContainerExecTarget,
    request_digest: &str,
    overlay: Option<BenchOverlay>,
) -> LaunchConfig {
    // The overlay's knobs go on top of the run config's; a session with no
    // overlay keeps the config's exactly.
    let loop_knobs = overlay.as_ref().map_or(run.loop_knobs, |overlay| {
        overlay.apply_knobs(run.loop_knobs)
    });
    LaunchConfig {
        query: params.instruction.clone(),
        title: Some(format!("bench {} {}", params.task, params.trial)),
        agent_role: None,
        epic_spawn_ordinal: None,
        // The empty per-task host scratch directory: nothing of the hub's
        // filesystem is the task's working tree.
        working_dir: Some(scratch.to_path_buf()),
        provider: Some(SessionProvider::OpenRouter),
        model: Some(run.model.clone()),
        configured_context_window: None,
        max_turns: None,
        system_prompt: None,
        resume_session_id: None,
        session_kind: Some(SessionKind::Standard),
        project_id: Some(run.project_id),
        rsi_session_id: None,
        rsi_socket: None,
        rsi_session_token: None,
        continued_from: None,
        openai_base_url: None,
        openai_api_key: None,
        conversation_history: None,
        workflow_id: None,
        workflow_id_override: None,
        max_retries: None,
        group_id: None,
        parent_id: None,
        effort: Some(run.effort.clone()),
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        model_invocation_owner: None,
        model_invocation_dedup_key: Some(format!("bench:{}:{}", run.id, params.trial)),
        model_invocation_request_fingerprint: Some(request_digest.to_owned()),
        completion_gates: None,
        skip_project_model_default: true,
        tool_policy: None,
        model_invocation_purpose:
            rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
        sandbox: None,
        cargo_target_dir: None,
        execution_scratch: None,
        is_eval: true,
        skip_context_pipeline: true,
        capability_class: None,
        tags: vec!["bench".to_owned()],
        topology_node_id: None,
        topology_iteration: 0,
        closure_selector: None,
        structured_handoff: false,
        exec_target: ExecTarget::Container(target),
        bench_profile: Some(BenchLaunchProfile {
            task_cap_micro_usd: Some(micro_usd(run.per_task_cap_usd)),
            trace_path: Some(trace_path.to_string_lossy().into_owned()),
            overlay,
            ..BenchLaunchProfile::default()
        }),
        loop_knobs,
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// One page of `path` from `offset`, ending on a line boundary.
fn read_page(path: &Path, offset: u64, limit: u64) -> Result<BenchTraceChunkV1, BenchError> {
    let unavailable = |reason: String| {
        BenchError::new(
            BENCH_TRACE_UNAVAILABLE,
            format!("{BENCH_TRACE_UNAVAILABLE}: {reason}"),
        )
    };
    let mut file = std::fs::File::open(path).map_err(|e| unavailable(e.to_string()))?;
    let len = file
        .metadata()
        .map_err(|e| unavailable(e.to_string()))?
        .len();
    if offset > len {
        return Err(BenchError::invalid("offset"));
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| unavailable(e.to_string()))?;
    let mut window = limit;
    let bytes = loop {
        let mut buf = Vec::new();
        (&mut file)
            .take(window)
            .read_to_end(&mut buf)
            .map_err(|e| unavailable(e.to_string()))?;
        match buf.iter().rposition(|b| *b == b'\n') {
            Some(at) => {
                buf.truncate(at + 1);
                break buf;
            }
            // No complete line in the window: at the end of the file this is
            // a line still being written (nothing to return yet); otherwise
            // widen the window to the end of the long line.
            None if offset + window >= len => break Vec::new(),
            None if window >= MAX_TRACE_LINE_BYTES => {
                return Err(unavailable("a trace line exceeds the page bound".into()));
            }
            None => {
                window = (window * 2).min(MAX_TRACE_LINE_BYTES);
                file.seek(SeekFrom::Start(offset))
                    .map_err(|e| unavailable(e.to_string()))?;
            }
        }
    };
    let next_offset = offset + bytes.len() as u64;
    let data =
        String::from_utf8(bytes).map_err(|_| BenchError::invalid("offset_not_a_line_boundary"))?;
    Ok(BenchTraceChunkV1 {
        offset,
        next_offset,
        eof: next_offset >= len,
        data,
    })
}

impl BenchService {
    /// Start one trial. Idempotent per (run, trial): a repeat with the same
    /// parameters answers the existing trial; other parameters conflict.
    ///
    /// # Errors
    /// A typed [`BenchError`] for every refusal.
    pub async fn start_task(
        self: &Arc<Self>,
        run_id: Uuid,
        params: BenchTaskStartParams,
    ) -> Result<BenchTaskStatusV1, BenchError> {
        params.validate().map_err(BenchError::invalid)?;
        let digest = request_digest(&params);

        let (run, existing) = {
            let store = self.store.lock().await;
            let run = store
                .get_bench_run(run_id)?
                .filter(|run| run.state == BenchRunState::Active)
                .ok_or_else(|| not_active(run_id))?;
            let existing = store.get_bench_task(run_id, &params.trial)?;
            (run, existing)
        };
        if let Some(row) = existing {
            if row.request_digest != digest {
                return Err(BenchError::new(
                    BENCH_TRIAL_CONFLICT,
                    format!(
                        "{BENCH_TRIAL_CONFLICT}: trial {} was started with other parameters",
                        params.trial
                    ),
                ));
            }
            return self.status_of(row).await;
        }
        let pinned = run.task(&params.task).ok_or_else(|| {
            BenchError::new(
                BENCH_TASK_UNKNOWN,
                format!(
                    "{BENCH_TASK_UNKNOWN}: {} is not in the run's pinned task set",
                    params.task
                ),
            )
        })?;

        // `allowed_launches`: (OpenRouter, model, effort) must be granted.
        let wanted = ManagerLaunchChoiceV2 {
            provider: SessionProvider::OpenRouter,
            model: run.model.clone(),
            effort: Some(run.effort.clone()),
        };
        let allowed = {
            let store = self.store.lock().await;
            self.host.allowed_launches(&store, run.project_id)?
        };
        if !crate::topology::agent::launch_granted(&allowed, &wanted) {
            return Err(BenchError::new(
                BENCH_LAUNCH_NOT_ALLOWED,
                format!(
                    "{BENCH_LAUNCH_NOT_ALLOWED}: {} is not in the project's allowed_launches",
                    wanted.display()
                ),
            ));
        }

        // The budget check reads what live trials may still spend, and the claim
        // below is what makes this trial one of them: hold one admission slot
        // across both so concurrent starts cannot each see the same remainder.
        let admission = self.admission.lock().await;
        // A concurrent start of this very trial may have claimed it while this
        // one waited: answer it (or conflict) rather than judge the budget again.
        let existing = self
            .store
            .lock()
            .await
            .get_bench_task(run_id, &params.trial)?;
        if let Some(row) = existing {
            drop(admission);
            if row.request_digest != digest {
                return Err(BenchError::new(
                    BENCH_TRIAL_CONFLICT,
                    format!(
                        "{BENCH_TRIAL_CONFLICT}: trial {} was started with other parameters",
                        params.trial
                    ),
                ));
            }
            return self.status_of(row).await;
        }
        self.admit_budget(&run).await?;

        // A cheap pre-check, so a full run never pays for a docker inspect; the
        // claim below enforces the cap atomically.
        let live = self
            .store
            .lock()
            .await
            .list_bench_tasks(run_id)?
            .iter()
            .filter(|task| task.state.is_live())
            .count();
        if live >= run.concurrency as usize {
            return Err(concurrency_full(run.concurrency));
        }

        let target = ContainerExecTarget {
            docker_host: run.docker_host.clone(),
            container: params.container.clone(),
            workdir: params.workdir.clone(),
            user: params.user.clone(),
            compose_project: params.compose_project.clone(),
            pinned_digests: pinned.pinned_digests.clone(),
            per_call_ceiling_secs: DEFAULT_PER_CALL_CEILING_SECS,
        };
        target.validate_shape().map_err(|reason| {
            BenchError::new(
                EXEC_TARGET_INVALID,
                format!("{EXEC_TARGET_INVALID}:{reason}"),
            )
        })?;
        self.host
            .validate_container(&target)
            .await
            .map_err(|code| {
                BenchError::from_daemon(&crate::error::DaemonError::PolicyDenied(code))
            })?;

        let claim = NewBenchTask {
            trial: params.trial.clone(),
            task: params.task.clone(),
            compose_project: params.compose_project.clone(),
            container: params.container.clone(),
            workdir: params.workdir.clone(),
            user: params.user.clone(),
            agent_timeout_sec: params.agent_timeout_sec,
            request_digest: digest.clone(),
        };
        let claimed =
            self.store
                .lock()
                .await
                .claim_bench_task(run_id, &claim, chrono::Utc::now())?;
        let row = match claimed {
            BenchClaim::Claimed(row) => row,
            // Two starts of one trial raced: the loser answers the winner's row.
            BenchClaim::Existing(row) if row.request_digest == digest => {
                return self.status_of(row).await;
            }
            BenchClaim::Existing(_) => {
                return Err(BenchError::new(
                    BENCH_TRIAL_CONFLICT,
                    format!(
                        "{BENCH_TRIAL_CONFLICT}: trial {} was started with other parameters",
                        params.trial
                    ),
                ));
            }
            BenchClaim::RunNotActive => return Err(not_active(run_id)),
            BenchClaim::ConcurrencyFull => return Err(concurrency_full(run.concurrency)),
        };
        // The claimed row now counts against the budget of the next start.
        drop(admission);

        let trial_dir = self.trial_dir(run_id, &params.trial);
        let scratch = trial_dir.join("scratch");
        let trace_path = trial_dir.join("trace.jsonl");
        let prepared = private_dir(&trial_dir).and_then(|()| private_dir(&scratch));
        if let Err(error) = prepared {
            let refusal = BenchError::new(
                "bench_scratch_unavailable",
                format!("bench_scratch_unavailable: {error}"),
            );
            self.settle_failed_start(&row, &refusal).await;
            return Err(refusal);
        }
        // #3191: the run's overlay applies to this trial and to nothing else.
        let overlay = match self.run_overlay(run_id) {
            Ok(overlay) => overlay,
            Err(refusal) => {
                self.settle_failed_start(&row, &refusal).await;
                return Err(refusal);
            }
        };
        let mut config = launch_config(
            &run,
            &params,
            &scratch,
            &trace_path,
            target,
            &digest,
            overlay,
        );
        config.parent_id = self.trial_group(&run).await;
        let session_id = match self.host.launch(config).await {
            Ok(session_id) => session_id,
            Err(error) => {
                let refusal = BenchError::from_daemon(&error);
                self.settle_failed_start(&row, &refusal).await;
                return Err(refusal);
            }
        };
        let bound = self.store.lock().await.bind_bench_task_session(
            run_id,
            &params.trial,
            session_id,
            &trace_path.to_string_lossy(),
        )?;
        if !bound {
            // The trial was settled before its session was bound (the run was
            // cancelled, a restart interrupted it): the session must not run.
            let _ = self.host.interrupt(session_id).await;
        } else {
            self.spawn_timeout_watch(run_id, params.trial.clone(), params.agent_timeout_sec);
        }
        let row = self
            .store
            .lock()
            .await
            .get_bench_task(run_id, &params.trial)?
            .ok_or_else(|| unknown_trial(&params.trial))?;
        self.status_of(row).await
    }

    /// The daemon's own stop for a session its adapter forgot.
    fn spawn_timeout_watch(self: &Arc<Self>, run_id: Uuid, trial: String, agent_timeout_sec: u64) {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(
                agent_timeout_sec + TIMEOUT_GRACE_SECS,
            ))
            .await;
            if let Err(error) = service
                .cancel_with(run_id, &trial, BENCH_AGENT_TIMEOUT)
                .await
            {
                tracing::debug!(%run_id, trial, %error, "bench timeout watch");
            }
        });
    }

    /// #3992: the Group this run's trials launch under.
    ///
    /// The submitter is the owner of the run's job (`bench_runs.job_id`). When
    /// an Epic sits above it, the run gets its own Group, titled
    /// `bench <model> <run id short>` and tagged with the run and the Epic.
    /// With no Epic (an operator-run bench, a job that cannot be resolved) all
    /// such trials share one project-level "Benchmarks" Group. Either way the
    /// Group is a root Group of the project, the only place the containment
    /// matrix (`legal_children`) lets a Group live; parenting it under the Epic
    /// would also bring the trials into the Epic's manager resource cohort.
    ///
    /// A failure here is logged and the trial launches ungrouped, as before.
    async fn trial_group(&self, run: &BenchRunRow) -> Option<Uuid> {
        let epic = self.submitter_epic(run).await;
        match epic {
            Some(epic) => {
                let run_tag = format!("bench-run-{}", run.id);
                self.find_or_create_group(
                    run.project_id,
                    run.id,
                    &run_tag,
                    format!("bench {} {}", run.model, &run.id.to_string()[..8]),
                    vec![
                        "bench".to_owned(),
                        run_tag.clone(),
                        format!("bench-epic-{epic}"),
                    ],
                )
                .await
            }
            None => self.benchmarks_group(run.project_id, run.id).await,
        }
    }

    /// The shared project-level "Benchmarks" Group.
    async fn benchmarks_group(&self, project: Uuid, run_id: Uuid) -> Option<Uuid> {
        self.find_or_create_group(
            project,
            run_id,
            BENCH_FALLBACK_TAG,
            "Benchmarks".to_owned(),
            vec!["bench".to_owned(), BENCH_FALLBACK_TAG.to_owned()],
        )
        .await
    }

    /// The oldest live Group of `project` tagged `find_tag`, created when none exists.
    /// `group_lock` serializes the find-or-create across concurrent starts.
    async fn find_or_create_group(
        &self,
        project: Uuid,
        run_id: Uuid,
        find_tag: &str,
        name: String,
        tags: Vec<String>,
    ) -> Option<Uuid> {
        let _guard = self.group_lock.lock().await;
        let existing = self.store.lock().await.find_group_by_tag(project, find_tag);
        match existing {
            Ok(Some(group)) => return Some(group.id),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%run_id, %error, "bench group lookup");
                return None;
            }
        }
        match self.host.create_group(project, name, tags).await {
            Ok(id) => Some(id),
            Err(error) => {
                tracing::warn!(%run_id, %error, "bench group create");
                None
            }
        }
    }

    /// #4013: one-off, idempotent repair of the trials that launched at the
    /// project root before #3992. Every finished `is_eval` bench trial session
    /// still without a parent moves into its run's Group (found or created with
    /// the same title and tags as a new trial's), or into the project's
    /// "Benchmarks" Group when no run row binds it. A run that is still active
    /// is never touched. Only `parent_id` changes: no row is deleted.
    ///
    /// The first query is the whole cost when nothing is left to move, so it is
    /// cheap to call at every start. Returns the number of sessions moved.
    pub async fn group_orphan_trials(&self) -> Result<usize, BenchError> {
        let orphans = self
            .store
            .lock()
            .await
            .list_ungrouped_finished_bench_sessions(ORPHAN_SCAN_LIMIT)?;
        if orphans.is_empty() {
            return Ok(0);
        }
        let mut groups: std::collections::HashMap<(Uuid, Option<Uuid>), Option<Uuid>> =
            std::collections::HashMap::new();
        let (mut moved, mut skipped) = (0usize, 0usize);
        for orphan in &orphans {
            let run = match orphan.run_id {
                Some(run_id) => self.store.lock().await.get_bench_run(run_id)?,
                None => None,
            };
            let project = run.as_ref().map(|run| run.project_id).or(orphan.project_id);
            let Some(project) = project else {
                skipped += 1;
                continue;
            };
            let key = (project, run.as_ref().map(|run| run.id));
            let group = match groups.get(&key) {
                Some(group) => *group,
                None => {
                    let group = match &run {
                        Some(run) => self.trial_group(run).await,
                        None => self.benchmarks_group(project, Uuid::nil()).await,
                    };
                    groups.insert(key, group);
                    group
                }
            };
            let Some(group) = group else {
                skipped += 1;
                continue;
            };
            let parented = self
                .store
                .lock()
                .await
                .parent_ungrouped_bench_session(orphan.session_id, group)?;
            if parented {
                moved += 1;
                tracing::info!(session_id = %orphan.session_id, %group, "bench trial grouped (#4013)");
            }
        }
        tracing::info!(
            moved,
            skipped,
            scanned = orphans.len(),
            "bench trial backfill (#4013)"
        );
        Ok(moved)
    }

    /// The nearest Epic at or above the session that submitted the run's job.
    async fn submitter_epic(&self, run: &BenchRunRow) -> Option<Uuid> {
        let job_id = run
            .job_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())?;
        let store = self.store.lock().await;
        let owner = store
            .get_agent_job(job_id)
            .ok()
            .flatten()?
            .job
            .owner_session_id;
        let mut current = store.get_session(owner).ok().flatten()?;
        // The hierarchy is three levels deep; the bound only guards a corrupt cycle.
        for _ in 0..16 {
            if current.session_kind == SessionKind::Epic {
                return Some(current.id);
            }
            current = store.get_session(current.parent_id?).ok().flatten()?;
        }
        None
    }

    /// A start that failed after the claim: the trial is over (a new attempt is
    /// a new trial id), and the refusal is its stop reason.
    async fn settle_failed_start(&self, row: &BenchTaskRow, refusal: &BenchError) {
        let settlement = BenchSettlement {
            state: BenchTaskState::Cancelled,
            stop_reason: Some(format!("bench_start_failed:{}", refusal.code)),
            cost_usd: None,
            prompt_tokens: 0,
            cached_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            wall_ms: 0,
            harness_build_digest: None,
            trace_sha256: None,
            route_verdict: None,
            route_violations: Vec::new(),
            finished_at: chrono::Utc::now(),
        };
        if let Err(error) =
            self.store
                .lock()
                .await
                .settle_bench_task(row.run_id, &row.trial, &settlement)
        {
            tracing::error!(run_id = %row.run_id, trial = row.trial, %error, "bench failed start not recorded");
        }
    }

    /// The M4 run budget, and room under it for one more task's cap on top of
    /// what the live trials may still spend.
    async fn admit_budget(&self, run: &BenchRunRow) -> Result<(), BenchError> {
        let (sessions, live_sessions, starting) = {
            let store = self.store.lock().await;
            let tasks = store.list_bench_tasks(run.id)?;
            let sessions: Vec<Uuid> = tasks.iter().filter_map(|t| t.session_id).collect();
            let live: Vec<Uuid> = tasks
                .iter()
                .filter(|t| t.state.is_live())
                .filter_map(|t| t.session_id)
                .collect();
            let starting = tasks
                .iter()
                .filter(|t| t.state.is_live() && t.session_id.is_none())
                .count();
            (sessions, live, starting)
        };
        let report = self.host.run_budget(run.budget_usd, &sessions).await?;
        if !report.admitted {
            let code = report
                .refusal_code
                .unwrap_or_else(|| BENCH_RUN_BUDGET_INSUFFICIENT.to_owned());
            let detail = report.refusal_detail.unwrap_or_default();
            return Err(BenchError::new(&code, format!("{code}: {detail}")));
        }
        // What the live trials may still spend before their own caps stop them.
        let mut committed = run.per_task_cap_usd * starting as f64;
        {
            let store = self.store.lock().await;
            for session in &live_sessions {
                let spent = store.bench_session_usage(*session)?.cost_usd.unwrap_or(0.0);
                committed += (run.per_task_cap_usd - spent).max(0.0);
            }
        }
        let available = report.status.remaining_usd - committed;
        if available + 1e-9 < run.per_task_cap_usd {
            return Err(BenchError::new(
                BENCH_RUN_BUDGET_INSUFFICIENT,
                format!(
                    "{BENCH_RUN_BUDGET_INSUFFICIENT}: ${:.4} of the ${:.2} run budget is left and \
                     the live trials may still spend ${committed:.4}; the per-task cap is ${:.4}",
                    report.status.remaining_usd, run.budget_usd, run.per_task_cap_usd
                ),
            ));
        }
        Ok(())
    }

    /// The state of a trial of this run.
    ///
    /// # Errors
    /// `bench_trial_unknown` for a trial that is not this run's.
    pub async fn task_status(
        &self,
        run_id: Uuid,
        trial: &str,
    ) -> Result<BenchTaskStatusV1, BenchError> {
        let row = self.trial_row(run_id, trial).await?;
        self.status_of(row).await
    }

    async fn trial_row(&self, run_id: Uuid, trial: &str) -> Result<BenchTaskRow, BenchError> {
        self.store
            .lock()
            .await
            .get_bench_task(run_id, trial)?
            .ok_or_else(|| unknown_trial(trial))
    }

    /// Stop a trial (the adapter's timeout or cancellation).
    ///
    /// # Errors
    /// `bench_trial_unknown` for a trial that is not this run's.
    pub async fn cancel_task(
        &self,
        run_id: Uuid,
        trial: &str,
    ) -> Result<BenchTaskStatusV1, BenchError> {
        self.cancel_with(run_id, trial, BENCH_CANCELLED).await
    }

    /// Cancel a trial of a run being ended.
    pub(crate) async fn cancel_trial(
        &self,
        run_id: Uuid,
        trial: &str,
    ) -> Result<BenchTaskStatusV1, BenchError> {
        self.cancel_with(run_id, trial, BENCH_CANCELLED).await
    }

    async fn cancel_with(
        &self,
        run_id: Uuid,
        trial: &str,
        reason: &str,
    ) -> Result<BenchTaskStatusV1, BenchError> {
        let row = self.trial_row(run_id, trial).await?;
        if row.state.is_live() {
            // Mark it first: the spend guard refuses a model call of any task
            // that is no longer running, so the stop below cannot race a call.
            let usage = match row.session_id {
                Some(session) => self.store.lock().await.bench_session_usage(session)?,
                None => crate::store::bench::BenchSessionUsage::default(),
            };
            let now = chrono::Utc::now();
            let settlement = BenchSettlement {
                state: BenchTaskState::Cancelled,
                stop_reason: Some(reason.to_owned()),
                cost_usd: usage.cost_usd,
                prompt_tokens: usage.prompt_tokens,
                cached_tokens: usage.cached_tokens,
                output_tokens: usage.output_tokens,
                reasoning_tokens: usage.reasoning_tokens,
                wall_ms: wall_ms(&row, now),
                harness_build_digest: None,
                trace_sha256: None,
                route_verdict: None,
                route_violations: Vec::new(),
                finished_at: now,
            };
            self.store
                .lock()
                .await
                .settle_bench_task(run_id, trial, &settlement)?;
            if let Some(session) = row.session_id
                && let Err(error) = self.host.interrupt(session).await
            {
                tracing::warn!(%session, %error, "bench cancel could not interrupt the session");
            }
        }
        let row = self.trial_row(run_id, trial).await?;
        self.status_of(row).await
    }

    /// The final usage, build digest and trace handle of a finished trial.
    ///
    /// # Errors
    /// `bench_task_not_finished` (retryable) while its session still runs.
    pub async fn finish_task(
        &self,
        run_id: Uuid,
        trial: &str,
    ) -> Result<BenchTaskStatusV1, BenchError> {
        let mut row = self.trial_row(run_id, trial).await?;
        row = self.refresh(row).await?;
        if row.state == BenchTaskState::InfraInterrupted {
            // A restart interrupted it: nothing more will happen, so seal what
            // exists (the usage so far, the audit and the trace as written).
            if row.trace_sha256.is_none() {
                row = self.finalize(&row, None).await?;
            }
        } else if row.state.is_live() || !self.session_ended(&row).await? {
            return Err(BenchError::new(
                BENCH_TASK_NOT_FINISHED,
                format!(
                    "{BENCH_TASK_NOT_FINISHED}: trial {trial} is {}; its session has not ended",
                    row.state.as_str()
                ),
            )
            .retryable());
        }
        self.status_of(row).await
    }

    /// Whether the trial's session reached a terminal status (or never existed).
    async fn session_ended(&self, row: &BenchTaskRow) -> Result<bool, BenchError> {
        let Some(session_id) = row.session_id else {
            return Ok(true);
        };
        Ok(self
            .store
            .lock()
            .await
            .get_session(session_id)?
            .is_none_or(|session| session.status.is_terminal()))
    }

    /// One page of the trace.
    ///
    /// # Errors
    /// `bench_trial_unknown`, `bench_trace_unavailable` or an invalid offset.
    pub async fn read_trace(
        &self,
        run_id: Uuid,
        params: BenchTraceReadParams,
    ) -> Result<BenchTraceChunkV1, BenchError> {
        let row = self.trial_row(run_id, &params.trial).await?;
        let path = row.trace_path.ok_or_else(|| {
            BenchError::new(
                BENCH_TRACE_UNAVAILABLE,
                format!(
                    "{BENCH_TRACE_UNAVAILABLE}: trial {} has no trace yet",
                    params.trial
                ),
            )
        })?;
        let limit = params
            .limit_bytes
            .unwrap_or(DEFAULT_TRACE_PAGE_BYTES)
            .clamp(1, MAX_TRACE_PAGE_BYTES);
        let offset = params.offset;
        tokio::task::spawn_blocking(move || read_page(Path::new(&path), offset, limit))
            .await
            .map_err(|e| BenchError::new("bench_internal_error", e.to_string()))?
    }

    /// Settle a trial whose session ended; otherwise return the row as it is.
    async fn refresh(&self, row: BenchTaskRow) -> Result<BenchTaskRow, BenchError> {
        let Some(session_id) = row.session_id else {
            return Ok(row);
        };
        if !row.state.is_live() && row.trace_sha256.is_some() {
            return Ok(row);
        }
        let session = self.store.lock().await.get_session(session_id)?;
        match session {
            Some(session) if session.status.is_terminal() => {
                self.finalize(&row, Some(&session)).await
            }
            _ => Ok(row),
        }
    }

    /// Record a trial's final usage (increment SQL), route verdict, build
    /// digest and trace sha256.
    async fn finalize(
        &self,
        row: &BenchTaskRow,
        session: Option<&Session>,
    ) -> Result<BenchTaskRow, BenchError> {
        let session_id = row.session_id;
        let (usage, verdict, build_digest) = match session_id {
            Some(session_id) => {
                let store = self.store.lock().await;
                (
                    store.bench_session_usage(session_id)?,
                    Some(crate::session::bench_route::audit_session_route(
                        &store, session_id,
                    )?),
                    crate::session::bench_route::read_build_digest(&store, session_id)?,
                )
            }
            None => (
                crate::store::bench::BenchSessionUsage::default(),
                None,
                None,
            ),
        };
        let trace_sha256 = match row.trace_path.clone() {
            Some(path) => tokio::task::spawn_blocking(move || sha256_file(Path::new(&path)).ok())
                .await
                .ok()
                .flatten(),
            None => None,
        };
        let now = chrono::Utc::now();
        let finished_at = row.finished_at.unwrap_or(now);
        let state = if row.state.is_live() {
            BenchTaskState::Finished
        } else {
            row.state
        };
        let stop_reason = session
            .and_then(|s| {
                s.stop_reason
                    .clone()
                    .filter(|reason| !reason.trim().is_empty())
                    .or_else(|| Some(default_stop_reason(s.status).to_owned()))
            })
            .or_else(|| row.stop_reason.clone());
        // #3349: a trace that lost a line is not a full trace, however the
        // session ended (the loop cannot see a failed final append). The trial
        // settles with the typed reason, never as a normal completion.
        let stop_reason = if row
            .trace_path
            .as_deref()
            .is_some_and(|path| crate::bench::trace::path_write_failed(Path::new(path)))
        {
            Some(BENCH_TRACE_WRITE_FAILED.to_owned())
        } else {
            stop_reason
        };
        let (route_verdict, route_violations) = match verdict {
            Some(v) if v.valid => (Some("valid".to_owned()), Vec::new()),
            Some(v) => (Some(BENCH_INVALID_ROUTE.to_owned()), v.violations),
            None => (None, Vec::new()),
        };
        let settlement = BenchSettlement {
            state,
            stop_reason,
            cost_usd: usage.cost_usd,
            prompt_tokens: usage.prompt_tokens,
            cached_tokens: usage.cached_tokens,
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            wall_ms: wall_ms(row, finished_at),
            harness_build_digest: build_digest,
            trace_sha256,
            route_verdict,
            route_violations,
            finished_at,
        };
        let store = self.store.lock().await;
        store.settle_bench_task(row.run_id, &row.trial, &settlement)?;
        store
            .get_bench_task(row.run_id, &row.trial)?
            .ok_or_else(|| unknown_trial(&row.trial))
    }

    /// The wire status of a row, settling a trial whose session ended first.
    async fn status_of(&self, row: BenchTaskRow) -> Result<BenchTaskStatusV1, BenchError> {
        let row = self.refresh(row).await?;
        let live_usage = match (row.session_id, row.state.is_live()) {
            (Some(session), _) => Some(self.store.lock().await.bench_session_usage(session)?),
            (None, _) => None,
        };
        let settled = row.finished_at.is_some() && row.wall_ms.is_some();
        let usage = match (&live_usage, settled) {
            (_, true) => BenchUsage {
                cost_usd: row.cost_usd,
                prompt_tokens: row.prompt_tokens.unwrap_or(0),
                cached_tokens: row.cached_tokens.unwrap_or(0),
                output_tokens: row.output_tokens.unwrap_or(0),
                reasoning_tokens: row.reasoning_tokens.unwrap_or(0),
                wall_ms: row.wall_ms.unwrap_or(0),
                calls: live_usage.as_ref().map_or(0, |u| u.calls),
            },
            (Some(u), false) => BenchUsage {
                cost_usd: u.cost_usd,
                prompt_tokens: u.prompt_tokens,
                cached_tokens: u.cached_tokens,
                output_tokens: u.output_tokens,
                reasoning_tokens: u.reasoning_tokens,
                wall_ms: wall_ms(&row, chrono::Utc::now()),
                calls: u.calls,
            },
            (None, false) => BenchUsage {
                wall_ms: wall_ms(&row, chrono::Utc::now()),
                ..BenchUsage::default()
            },
        };
        let trace = row.trace_path.as_ref().map(|path| BenchTraceHandle {
            path: path.clone(),
            bytes: std::fs::metadata(path).map_or(0, |m| m.len()),
            sha256: row.trace_sha256.clone(),
        });
        Ok(BenchTaskStatusV1 {
            run_id: row.run_id.to_string(),
            task: row.task,
            trial: row.trial,
            attempt: row.attempt,
            state: row.state,
            stop_reason: row.stop_reason,
            session_id: row.session_id.map(|id| id.to_string()),
            usage,
            route_verdict: row.route_verdict,
            route_violations: row.route_violations,
            harness_build_digest: row.harness_build_digest,
            trace,
        })
    }
}

fn concurrency_full(cap: u32) -> BenchError {
    BenchError::new(
        BENCH_CONCURRENCY_EXCEEDED,
        format!("{BENCH_CONCURRENCY_EXCEEDED}: the run already has {cap} live trial(s)"),
    )
    .retryable()
}

fn default_stop_reason(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Completed => "completed",
        SessionStatus::Interrupted => "interrupted",
        SessionStatus::Failed => "failed",
        _ => "ended",
    }
}

fn wall_ms(row: &BenchTaskRow, until: chrono::DateTime<chrono::Utc>) -> u64 {
    u64::try_from((until - row.started_at).num_milliseconds().max(0)).unwrap_or(0)
}
