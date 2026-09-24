//! Disabled live recursive DAG executor adapter.
//!
//! This module prepares a future recursive-DAG-to-session launch path without
//! wiring it into RPC, the fake scheduler, topology execution, or a background
//! loop. The only production launch boundary delegates to
//! `SessionManager::launch_session`.

use crate::claude::LaunchConfig;
use crate::error::{DaemonError, Result};
use crate::model_control::hash_request_fingerprint;
use crate::session::SessionManager;
use crate::store::Store;
use crate::store::recursive_dag::{
    RecursiveExecutionArtifactCreate, RecursiveLiveAttemptCreate,
    RecursiveLiveAttemptHeartbeatRelease, RecursiveLiveAttemptHeartbeatRenew,
    RecursiveLiveAttemptHeartbeatStart, RecursiveLiveAttemptStatusUpdate,
    RecursiveLiveInterruptCreate, RecursiveLiveOutputSchedulerEventCreate,
    RecursiveLiveOutputValidationCommit, RecursiveLiveOutputValidationCommitResult,
    RecursiveLiveSchedulerRunStart, RecursiveSchedulerLeasePolicy,
};
use crate::terminal_output::select_last_terminal_output;
use async_trait::async_trait;
use rsi_common::recursive_dag::{
    RecursiveAttemptId, RecursiveAttemptPhase, RecursiveAttemptStatus,
    RecursiveCancellationRequestId, RecursiveExecutionArtifactKind, RecursiveExecutionMode,
    RecursiveLiveAttemptDetail, RecursiveLiveAttemptHeartbeatState, RecursiveLiveAttemptId,
    RecursiveLiveAttemptStatus, RecursiveLiveInterruptId, RecursiveLiveInterruptStatus,
    RecursiveLiveInterruptSummary, RecursiveLiveOutputParserSource, RecursiveRecoveryBudget,
    RecursiveSchedulerRunId, RecursiveSchedulerRunSource, RecursiveSchedulerRunStatus,
    RecursiveSchedulerRunSummary, RecursiveSchedulerStopReason, RecursiveTaskEdgeKind,
    RecursiveTaskGraphDetail, RecursiveTaskGraphId, RecursiveTaskId, RecursiveTaskLifecycleState,
};
use rsi_common::recursive_dag_validation::{
    RecursiveLiveOutputRepairRetryPolicy, RecursiveLiveOutputValidationContext,
    validate_recursive_live_output_json,
};
use rsi_common::types::{
    ConversationEvent, EventType, Role, SandboxKind, SandboxSpec, Session, SessionKind,
    SessionProvider, SessionStatus,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::scheduler_core::{
    RecursiveSchedulerStepPreparation, current_stop_reason, deterministic_attempt_id,
    failed_attempt_count, heartbeat_scheduler_run_lease, load_graph, next_attempt_no,
    observe_scheduler_cancellation, prepare_next_scheduler_step, retry_state_for_phase,
    saturating_usize_to_u32,
};

const LIVE_SCHEDULER_LAUNCH_BOUNDARY_FAILURE: &str = "recursive DAG live scheduler stopped after durable session launch; commit completed-session output with CommitRecursiveLiveAttemptOutput before scheduling more work";

#[derive(Debug, Clone)]
pub(crate) struct RecursiveDagLiveExecutionRequest {
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
    pub scheduler_run_id: RecursiveSchedulerRunId,
    pub attempt_id: RecursiveAttemptId,
    pub phase: RecursiveAttemptPhase,
    pub live_attempt_id: Option<RecursiveLiveAttemptId>,
    pub provider: Option<SessionProvider>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub working_dir: Option<PathBuf>,
    pub sandbox: Option<SandboxSpec>,
    pub sandbox_worktree_id: Option<String>,
    pub workflow_execution_id: Option<String>,
    pub topology_workflow_id: Option<Uuid>,
    pub max_wall_time_ms: Option<u64>,
    pub budgets: RecursiveDagLiveBudgetPlaceholders,
    pub approval_policy: RecursiveDagLiveApprovalPolicy,
    pub tool_policy: RecursiveDagLiveToolPolicy,
    pub sandbox_policy: RecursiveDagLiveSandboxPolicy,
}

#[derive(Debug, Clone)]
pub(crate) struct RecursiveDagLiveInterruptRequest {
    pub live_attempt_id: RecursiveLiveAttemptId,
    pub cancellation_request_id: Option<RecursiveCancellationRequestId>,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub(crate) struct RecursiveDagLiveHeartbeatStartRequest {
    pub live_attempt_id: RecursiveLiveAttemptId,
    pub heartbeat_owner: String,
    pub heartbeat_ttl_seconds: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct RecursiveDagLiveHeartbeatRequest {
    pub live_attempt_id: RecursiveLiveAttemptId,
    pub heartbeat_token: String,
    pub heartbeat_ttl_seconds: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct RecursiveDagLiveHeartbeatReleaseRequest {
    pub live_attempt_id: RecursiveLiveAttemptId,
    pub heartbeat_token: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecursiveDagLiveBudgetPlaceholders {
    pub max_wall_time_ms: Option<u64>,
    pub token_budget: Option<u64>,
    pub tool_call_budget: Option<u32>,
    pub artifact_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecursiveDagLiveApprovalPolicy {
    pub policy_name: Option<String>,
    pub require_operator_approval: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecursiveDagLiveToolPolicy {
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecursiveDagLiveSandboxPolicy {
    pub requested_kind: Option<SandboxKind>,
    pub requested_branch: Option<String>,
    pub preserve_on_failure: Option<bool>,
    pub allowed_write_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RecursiveDagLiveArtifactReference {
    pub artifact_id: i64,
    pub task_id: RecursiveTaskId,
    pub attempt_id: Option<RecursiveAttemptId>,
    pub kind: RecursiveExecutionArtifactKind,
    pub label: String,
    pub uri: Option<String>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RecursiveDagLiveLaunchEnvelope {
    pub graph_id: RecursiveTaskGraphId,
    pub graph_title: String,
    pub graph_objective: String,
    pub task_id: RecursiveTaskId,
    pub task_title: String,
    pub task_objective: String,
    pub task_scope: String,
    pub acceptance_criteria: Vec<String>,
    pub task_depth: u32,
    pub task_scope_units: u32,
    pub task_max_retries: u32,
    pub phase: RecursiveAttemptPhase,
    pub attempt_id: RecursiveAttemptId,
    pub attempt_no: u32,
    pub scheduler_run_id: RecursiveSchedulerRunId,
    pub recursive_live_attempt_id: RecursiveLiveAttemptId,
    pub provider: Option<SessionProvider>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub working_dir: Option<PathBuf>,
    pub requested_sandbox_kind: Option<SandboxKind>,
    pub requested_sandbox_branch: Option<String>,
    pub sandbox_root: Option<PathBuf>,
    pub sandbox_branch: Option<String>,
    pub sandbox_worktree_id: Option<String>,
    pub execution_mode: RecursiveExecutionMode,
    pub project_id: Option<Uuid>,
    pub workflow_id: Option<Uuid>,
    pub topology_id: Option<Uuid>,
    pub parent_session_id: Option<Uuid>,
    pub source_execution_id: Option<String>,
    pub workflow_execution_id: Option<String>,
    pub topology_workflow_id: Option<Uuid>,
    pub budgets: RecursiveDagLiveBudgetPlaceholders,
    pub approval_policy: RecursiveDagLiveApprovalPolicy,
    pub tool_policy: RecursiveDagLiveToolPolicy,
    pub sandbox_policy: RecursiveDagLiveSandboxPolicy,
    pub parent_task_id: Option<RecursiveTaskId>,
    pub dependency_task_ids: Vec<RecursiveTaskId>,
    pub parent_artifacts: Vec<RecursiveDagLiveArtifactReference>,
    pub dependency_artifacts: Vec<RecursiveDagLiveArtifactReference>,
    pub launch_query: String,
    pub system_prompt: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) enum RecursiveDagLiveExecutionResult {
    Launched {
        live_attempt: RecursiveLiveAttemptDetail,
        envelope: RecursiveDagLiveLaunchEnvelope,
        session_id: Uuid,
    },
    LaunchFailed {
        live_attempt: RecursiveLiveAttemptDetail,
        envelope: RecursiveDagLiveLaunchEnvelope,
        session_id: Option<Uuid>,
        failure_reason: String,
    },
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) enum RecursiveDagLiveOutputCommitResult {
    Committed {
        result: RecursiveLiveOutputValidationCommitResult,
    },
    NotReady {
        live_attempt: RecursiveLiveAttemptDetail,
        reason: RecursiveDagLiveOutputNotReadyReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum RecursiveDagLiveOutputNotReadyReason {
    LiveAttemptMissingSession,
    SessionNotFound,
    SessionNotCompleted,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct RecursiveDagLiveSchedulerRunRequest {
    pub graph_id: RecursiveTaskGraphId,
    pub max_steps: u32,
    pub source: RecursiveSchedulerRunSource,
    pub operator: Option<String>,
    pub idempotency_key: Option<String>,
    pub request_fingerprint: Option<String>,
    pub policy_snapshot: serde_json::Value,
    pub provider: Option<SessionProvider>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub working_dir: Option<PathBuf>,
    pub sandbox: Option<SandboxSpec>,
    pub sandbox_worktree_id: Option<String>,
    pub workflow_execution_id: Option<String>,
    pub topology_workflow_id: Option<Uuid>,
    pub max_wall_time_ms: Option<u64>,
    pub budgets: RecursiveDagLiveBudgetPlaceholders,
    pub approval_policy: RecursiveDagLiveApprovalPolicy,
    pub tool_policy: RecursiveDagLiveToolPolicy,
    pub sandbox_policy: RecursiveDagLiveSandboxPolicy,
    pub lease_policy: RecursiveSchedulerLeasePolicy,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct RecursiveDagLiveSchedulerDriverReport {
    pub scheduler_run: RecursiveSchedulerRunSummary,
    pub stop_reason: RecursiveSchedulerStopReason,
    pub step_count: u32,
    pub selected_task_order: Vec<RecursiveTaskId>,
    pub live_attempts: Vec<RecursiveLiveAttemptDetail>,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Default)]
struct RecursiveDagLiveSchedulerDriverProgress {
    selected_task_order: Vec<RecursiveTaskId>,
    live_attempts: Vec<RecursiveLiveAttemptDetail>,
}

impl RecursiveDagLiveSchedulerDriverProgress {
    fn record_launch(
        &mut self,
        task_id: RecursiveTaskId,
        live_attempt: RecursiveLiveAttemptDetail,
    ) {
        self.selected_task_order.push(task_id);
        self.live_attempts.push(live_attempt);
    }

    fn step_count(&self) -> u32 {
        saturating_usize_to_u32(self.selected_task_order.len())
    }

    fn report(
        &self,
        scheduler_run: RecursiveSchedulerRunSummary,
        stop_reason: RecursiveSchedulerStopReason,
        failure_reason: Option<String>,
    ) -> RecursiveDagLiveSchedulerDriverReport {
        RecursiveDagLiveSchedulerDriverReport {
            scheduler_run,
            stop_reason,
            step_count: self.step_count(),
            selected_task_order: self.selected_task_order.clone(),
            live_attempts: self.live_attempts.clone(),
            failure_reason,
        }
    }
}

#[derive(Debug)]
struct RecursiveDagLiveSchedulerLaunchedStep {
    task_id: RecursiveTaskId,
    attempt_id: RecursiveAttemptId,
    result: RecursiveDagLiveExecutionResult,
}

/// Request-scoped live scheduler driver.
///
/// This driver is intentionally daemon-internal and is not registered with RPC
/// or any background loop. It stops at the durable session correlation boundary;
/// completed-session output commit remains an internal binding method rather
/// than scheduler-loop reachability.
#[allow(dead_code)]
pub(crate) struct RecursiveDagLiveSchedulerDriver<L> {
    store: Arc<Mutex<Store>>,
    executor: RecursiveDagLiveExecutor<L>,
    enabled: bool,
}

#[allow(dead_code)]
impl<L> RecursiveDagLiveSchedulerDriver<L>
where
    L: RecursiveDagLiveSessionLauncher,
{
    pub(crate) fn new(store: Arc<Mutex<Store>>, executor: RecursiveDagLiveExecutor<L>) -> Self {
        Self {
            store,
            executor,
            enabled: true,
        }
    }

    pub(crate) fn disabled(
        store: Arc<Mutex<Store>>,
        executor: RecursiveDagLiveExecutor<L>,
    ) -> Self {
        Self {
            store,
            executor,
            enabled: false,
        }
    }

    pub(crate) fn enabled(store: Arc<Mutex<Store>>, executor: RecursiveDagLiveExecutor<L>) -> Self {
        Self {
            store,
            executor,
            enabled: true,
        }
    }

    pub(crate) async fn run_request_scoped(
        &mut self,
        request: RecursiveDagLiveSchedulerRunRequest,
    ) -> Result<RecursiveDagLiveSchedulerDriverReport> {
        if !self.enabled {
            return Err(DaemonError::InvalidParam(
                "recursive DAG live scheduler is disabled".to_string(),
            ));
        }

        if request.max_steps == 0 {
            return Err(DaemonError::InvalidParam(
                "recursive live scheduler max_steps must be positive".to_string(),
            ));
        }

        let mut run = {
            let store = self.store.lock().await;
            store.start_recursive_live_scheduler_run_with_lease_policy(
                RecursiveLiveSchedulerRunStart {
                    graph_id: request.graph_id,
                    max_steps: request.max_steps,
                    source: request.source,
                    operator: request.operator.clone(),
                    idempotency_key: request.idempotency_key.clone(),
                    request_fingerprint: request.request_fingerprint.clone(),
                    policy_snapshot: request.policy_snapshot.clone(),
                },
                request.lease_policy.clone(),
            )?
        };
        let mut progress = RecursiveDagLiveSchedulerDriverProgress::default();

        match self
            .run_started_request_scoped(&request, &mut run, &mut progress)
            .await
        {
            Ok(report) => Ok(report),
            Err(error) => {
                let failure_reason = error.to_string();
                if run.status == RecursiveSchedulerRunStatus::Running
                    || run.status == RecursiveSchedulerRunStatus::Cancelling
                {
                    if let Err(mark_failed_error) = self
                        .fail_live_run(run.id, progress.step_count(), failure_reason.clone())
                        .await
                    {
                        tracing::warn!(
                            run_id = %run.id,
                            error = %mark_failed_error,
                            "failed to mark recursive live scheduler run failed after driver error"
                        );
                    }
                }
                Err(error)
            }
        }
    }

    async fn run_started_request_scoped(
        &mut self,
        request: &RecursiveDagLiveSchedulerRunRequest,
        run: &mut RecursiveSchedulerRunSummary,
        progress: &mut RecursiveDagLiveSchedulerDriverProgress,
    ) -> Result<RecursiveDagLiveSchedulerDriverReport> {
        *run = self.heartbeat_run(run).await?;
        if let Some(cancellation_request_id) = self.observe_cancellation(run).await? {
            let cancelled = self
                .cancel_live_run(run.id, cancellation_request_id, progress.step_count())
                .await?;
            return Ok(progress.report(
                cancelled,
                RecursiveSchedulerStopReason::CancellationRequested,
                None,
            ));
        }

        // Live execution still stops after one durable launch boundary.
        // Completed-session output commit is an explicit operator step rather
        // than scheduler-loop reachability.
        #[allow(clippy::never_loop)]
        for step_index in 0..request.max_steps {
            *run = self.heartbeat_run(run).await?;
            if let Some(cancellation_request_id) = self.observe_cancellation(run).await? {
                let cancelled = self
                    .cancel_live_run(run.id, cancellation_request_id, progress.step_count())
                    .await?;
                return Ok(progress.report(
                    cancelled,
                    RecursiveSchedulerStopReason::CancellationRequested,
                    None,
                ));
            }

            let preparation = {
                let store = self.store.lock().await;
                prepare_next_scheduler_step(&store, request.graph_id, Some(run))?
            };
            match preparation {
                RecursiveSchedulerStepPreparation::Runnable(runnable) => {
                    let step = self
                        .execute_selected_live_task(
                            request,
                            run,
                            runnable.task_id,
                            runnable.phase,
                            step_index,
                        )
                        .await?;
                    let (live_attempt, failure_reason) = match step.result {
                        RecursiveDagLiveExecutionResult::Launched { live_attempt, .. } => (
                            live_attempt,
                            LIVE_SCHEDULER_LAUNCH_BOUNDARY_FAILURE.to_string(),
                        ),
                        RecursiveDagLiveExecutionResult::LaunchFailed {
                            live_attempt,
                            failure_reason,
                            ..
                        } => {
                            self.finish_failed_launch_attempt(
                                request.graph_id,
                                step.task_id,
                                step.attempt_id,
                                failure_reason.clone(),
                            )
                            .await?;
                            (
                                live_attempt,
                                format!(
                                    "recursive DAG live scheduler launch failed: {failure_reason}"
                                ),
                            )
                        }
                    };
                    progress.record_launch(step.task_id, live_attempt);
                    let failed = self
                        .fail_live_run(run.id, progress.step_count(), failure_reason.clone())
                        .await?;
                    return Ok(progress.report(
                        failed,
                        RecursiveSchedulerStopReason::ExecutorError,
                        Some(failure_reason),
                    ));
                }
                RecursiveSchedulerStepPreparation::Stopped { stop_reason, .. } => {
                    let stop_reason = RecursiveSchedulerStopReason::from(stop_reason);
                    let completed = self
                        .finish_live_run(run.id, stop_reason, progress.step_count())
                        .await?;
                    return Ok(progress.report(completed, stop_reason, None));
                }
            }
        }

        let stop_reason = {
            let store = self.store.lock().await;
            current_stop_reason(&store, request.graph_id)?
                .map(RecursiveSchedulerStopReason::from)
                .unwrap_or(RecursiveSchedulerStopReason::StepLimitExceeded)
        };
        let completed = self
            .finish_live_run(run.id, stop_reason, progress.step_count())
            .await?;
        Ok(progress.report(completed, stop_reason, None))
    }

    async fn execute_selected_live_task(
        &self,
        request: &RecursiveDagLiveSchedulerRunRequest,
        run: &RecursiveSchedulerRunSummary,
        task_id: RecursiveTaskId,
        phase: RecursiveAttemptPhase,
        _step_index: u32,
    ) -> Result<RecursiveDagLiveSchedulerLaunchedStep> {
        let attempt_id = {
            let store = self.store.lock().await;
            let before = load_graph(&store, request.graph_id)?;
            let attempt_no = next_attempt_no(&before, task_id, phase);
            let attempt_id = deterministic_attempt_id(request.graph_id, task_id, phase, attempt_no);
            store.record_recursive_attempt_start_with_executor_kind(
                request.graph_id,
                task_id,
                phase,
                attempt_id,
                RecursiveExecutionMode::LiveSession,
            )?;
            match phase {
                RecursiveAttemptPhase::Execute => {
                    store.transition_recursive_task_state(
                        request.graph_id,
                        task_id,
                        RecursiveTaskLifecycleState::Planning,
                        Some("recursive DAG live scheduler: planning live execute".to_string()),
                    )?;
                    store.transition_recursive_task_state(
                        request.graph_id,
                        task_id,
                        RecursiveTaskLifecycleState::Running,
                        Some("recursive DAG live scheduler: running live execute".to_string()),
                    )?;
                }
                RecursiveAttemptPhase::Integrate => {
                    store.transition_recursive_task_state(
                        request.graph_id,
                        task_id,
                        RecursiveTaskLifecycleState::Integrating,
                        Some(
                            "recursive DAG live scheduler: launching live integration".to_string(),
                        ),
                    )?;
                }
            }
            attempt_id
        };

        let result = self
            .launch_live_attempt_for_existing_attempt(request, run, task_id, phase, attempt_id)
            .await?;
        Ok(RecursiveDagLiveSchedulerLaunchedStep {
            task_id,
            attempt_id,
            result,
        })
    }

    async fn launch_live_attempt_for_existing_attempt(
        &self,
        request: &RecursiveDagLiveSchedulerRunRequest,
        run: &RecursiveSchedulerRunSummary,
        task_id: RecursiveTaskId,
        phase: RecursiveAttemptPhase,
        attempt_id: RecursiveAttemptId,
    ) -> Result<RecursiveDagLiveExecutionResult> {
        self.executor
            .execute(RecursiveDagLiveExecutionRequest {
                graph_id: request.graph_id,
                task_id,
                scheduler_run_id: run.id,
                attempt_id,
                phase,
                live_attempt_id: None,
                provider: request.provider,
                model: request.model.clone(),
                effort: request.effort.clone(),
                working_dir: request.working_dir.clone(),
                sandbox: request.sandbox.clone(),
                sandbox_worktree_id: request.sandbox_worktree_id.clone(),
                workflow_execution_id: request.workflow_execution_id.clone(),
                topology_workflow_id: request.topology_workflow_id,
                max_wall_time_ms: request.max_wall_time_ms,
                budgets: request.budgets.clone(),
                approval_policy: request.approval_policy.clone(),
                tool_policy: request.tool_policy.clone(),
                sandbox_policy: request.sandbox_policy.clone(),
            })
            .await
    }

    async fn finish_failed_launch_attempt(
        &self,
        graph_id: RecursiveTaskGraphId,
        task_id: RecursiveTaskId,
        attempt_id: RecursiveAttemptId,
        failure_reason: String,
    ) -> Result<()> {
        let store = self.store.lock().await;
        let detail = load_graph(&store, graph_id)?;
        let attempt = detail
            .attempts
            .iter()
            .find(|attempt| attempt.id == attempt_id)
            .ok_or_else(|| {
                DaemonError::Store(format!("recursive attempt not found: {attempt_id}"))
            })?;
        if attempt.status != RecursiveAttemptStatus::Running {
            return Ok(());
        }
        let task = detail
            .nodes
            .iter()
            .find(|node| node.id == task_id)
            .ok_or_else(|| DaemonError::Store(format!("recursive task not found: {task_id}")))?;
        let failed_after_this =
            failed_attempt_count(&detail, task_id, attempt.phase).saturating_add(1);
        let will_retry = failed_after_this <= task.max_retries;
        let next_state = if will_retry {
            retry_state_for_phase(attempt.phase)
        } else {
            RecursiveTaskLifecycleState::Failed
        };
        store.record_recursive_attempt_finish(
            graph_id,
            attempt_id,
            RecursiveAttemptStatus::Failed,
            Some(failure_reason.clone()),
            None,
            Some(next_state),
            Some(format!(
                "recursive DAG live scheduler: launch failed: {failure_reason}"
            )),
        )?;
        Ok(())
    }

    async fn heartbeat_run(
        &self,
        run: &RecursiveSchedulerRunSummary,
    ) -> Result<RecursiveSchedulerRunSummary> {
        let store = self.store.lock().await;
        heartbeat_scheduler_run_lease(&store, run)
    }

    async fn observe_cancellation(
        &self,
        run: &RecursiveSchedulerRunSummary,
    ) -> Result<Option<RecursiveCancellationRequestId>> {
        let store = self.store.lock().await;
        Ok(observe_scheduler_cancellation(&store, run)?.map(|request| request.id))
    }

    async fn finish_live_run(
        &self,
        run_id: RecursiveSchedulerRunId,
        stop_reason: RecursiveSchedulerStopReason,
        step_count: u32,
    ) -> Result<RecursiveSchedulerRunSummary> {
        let store = self.store.lock().await;
        store.finish_recursive_live_scheduler_run(run_id, stop_reason, step_count, None)
    }

    async fn cancel_live_run(
        &self,
        run_id: RecursiveSchedulerRunId,
        cancellation_request_id: RecursiveCancellationRequestId,
        step_count: u32,
    ) -> Result<RecursiveSchedulerRunSummary> {
        let store = self.store.lock().await;
        store.cancel_recursive_live_scheduler_run(run_id, cancellation_request_id, step_count, None)
    }

    async fn fail_live_run(
        &self,
        run_id: RecursiveSchedulerRunId,
        step_count: u32,
        failure_reason: String,
    ) -> Result<RecursiveSchedulerRunSummary> {
        let store = self.store.lock().await;
        store.fail_recursive_live_scheduler_run(run_id, step_count, failure_reason)
    }
}

/// Session returned by the recursive DAG live launch boundary.
///
/// Contract: constructing this value as a successful launcher result means the
/// session id is known, the `sessions` row is durably persisted, the row is
/// loadable through the store read path, and attaching the id to the live
/// attempt is safe. Test launchers may deliberately violate this contract to
/// exercise the executor's defensive check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecursiveDagLiveLaunchedSession {
    pub session_id: Uuid,
}

impl RecursiveDagLiveLaunchedSession {
    #[must_use]
    pub const fn new(session_id: Uuid) -> Self {
        Self { session_id }
    }
}

#[async_trait]
pub(crate) trait RecursiveDagLiveSessionLauncher: Send + Sync {
    /// Launch a normal RSI session for a recursive DAG live attempt.
    ///
    /// A successful result must satisfy the `RecursiveDagLiveLaunchedSession`
    /// contract: the returned session id is durably present and loadable from
    /// the session store before the live attempt tries to attach it.
    async fn launch_recursive_dag_session(
        &self,
        envelope: RecursiveDagLiveLaunchEnvelope,
        config: LaunchConfig,
    ) -> Result<RecursiveDagLiveLaunchedSession>;
}

#[async_trait]
impl RecursiveDagLiveSessionLauncher for SessionManager {
    async fn launch_recursive_dag_session(
        &self,
        _envelope: RecursiveDagLiveLaunchEnvelope,
        config: LaunchConfig,
    ) -> Result<RecursiveDagLiveLaunchedSession> {
        let session_id = self.launch_session_with_durable_store_row(config).await?;
        Ok(RecursiveDagLiveLaunchedSession::new(session_id))
    }
}

/// Disabled daemon-owned binding for the future production live scheduler.
///
/// The binding packages the normal `SessionManager` launch boundary with its
/// store so daemon code can construct the live executor/driver shape without
/// registering an RPC method, starting a background loop, or enabling launches.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct RecursiveDagLiveSessionManagerBinding {
    manager: Arc<SessionManager>,
    store: Arc<Mutex<Store>>,
}

#[allow(dead_code)]
impl RecursiveDagLiveSessionManagerBinding {
    pub(crate) fn new(manager: Arc<SessionManager>) -> Self {
        let store = manager.recursive_dag_live_store_handle();
        Self { manager, store }
    }

    pub(crate) fn disabled_executor(&self) -> RecursiveDagLiveExecutor<Self> {
        RecursiveDagLiveExecutor::disabled(Arc::clone(&self.store), self.clone())
    }

    pub(crate) fn enabled_executor(&self) -> RecursiveDagLiveExecutor<Self> {
        RecursiveDagLiveExecutor::enabled(
            Arc::clone(&self.store),
            self.clone(),
            Arc::clone(&self.manager) as Arc<dyn RecursiveDagLiveSessionInterrupter>,
        )
    }

    pub(crate) fn disabled_driver(&self) -> RecursiveDagLiveSchedulerDriver<Self> {
        RecursiveDagLiveSchedulerDriver::disabled(Arc::clone(&self.store), self.disabled_executor())
    }

    pub(crate) fn enabled_driver(&self) -> RecursiveDagLiveSchedulerDriver<Self> {
        RecursiveDagLiveSchedulerDriver::enabled(Arc::clone(&self.store), self.enabled_executor())
    }

    pub(crate) async fn commit_completed_live_attempt_output(
        &self,
        live_attempt_id: RecursiveLiveAttemptId,
    ) -> Result<RecursiveDagLiveOutputCommitResult> {
        commit_completed_live_attempt_output(
            Arc::clone(&self.store),
            self.manager.as_ref(),
            live_attempt_id,
        )
        .await
    }
}

#[async_trait]
impl RecursiveDagLiveSessionLauncher for RecursiveDagLiveSessionManagerBinding {
    async fn launch_recursive_dag_session(
        &self,
        _envelope: RecursiveDagLiveLaunchEnvelope,
        config: LaunchConfig,
    ) -> Result<RecursiveDagLiveLaunchedSession> {
        let session_id = self
            .manager
            .launch_session_with_durable_store_row(config)
            .await?;
        Ok(RecursiveDagLiveLaunchedSession::new(session_id))
    }
}

#[derive(Debug, Clone)]
struct RecursiveDagLiveSessionSnapshot {
    session: Session,
    conversation: Vec<ConversationEvent>,
}

#[async_trait]
trait RecursiveDagLiveSessionOutputReader: Send + Sync {
    async fn load_recursive_dag_live_session(
        &self,
        session_id: Uuid,
    ) -> Result<Option<RecursiveDagLiveSessionSnapshot>>;
}

#[async_trait]
impl RecursiveDagLiveSessionOutputReader for SessionManager {
    async fn load_recursive_dag_live_session(
        &self,
        session_id: Uuid,
    ) -> Result<Option<RecursiveDagLiveSessionSnapshot>> {
        let Some(session) = self.get_session(session_id).await else {
            return Ok(None);
        };
        let conversation = match self.get_conversation(session_id).await {
            Ok(events) => events,
            Err(DaemonError::SessionNotFound(missing_id)) if missing_id == session_id => Vec::new(),
            Err(error) => return Err(error),
        };
        Ok(Some(RecursiveDagLiveSessionSnapshot {
            session,
            conversation,
        }))
    }
}

#[derive(Debug, Clone)]
struct CapturedRecursiveDagLiveOutput {
    raw_output: String,
    parser_source: RecursiveLiveOutputParserSource,
    event_id: Option<i64>,
    sequence: Option<i32>,
    extraction: &'static str,
}

async fn commit_completed_live_attempt_output<R>(
    store: Arc<Mutex<Store>>,
    reader: &R,
    live_attempt_id: RecursiveLiveAttemptId,
) -> Result<RecursiveDagLiveOutputCommitResult>
where
    R: RecursiveDagLiveSessionOutputReader + ?Sized,
{
    let live_attempt = {
        let store = store.lock().await;
        store
            .load_recursive_live_attempt(live_attempt_id)?
            .ok_or_else(|| {
                DaemonError::Store(format!(
                    "recursive live attempt not found: {live_attempt_id}"
                ))
            })?
    };

    let Some(session_id) = live_attempt.summary.session_id else {
        return Ok(RecursiveDagLiveOutputCommitResult::NotReady {
            live_attempt,
            reason: RecursiveDagLiveOutputNotReadyReason::LiveAttemptMissingSession,
        });
    };

    let Some(snapshot) = reader.load_recursive_dag_live_session(session_id).await? else {
        return Ok(RecursiveDagLiveOutputCommitResult::NotReady {
            live_attempt,
            reason: RecursiveDagLiveOutputNotReadyReason::SessionNotFound,
        });
    };

    if snapshot.session.status != SessionStatus::Completed {
        return Ok(RecursiveDagLiveOutputCommitResult::NotReady {
            live_attempt,
            reason: RecursiveDagLiveOutputNotReadyReason::SessionNotCompleted,
        });
    }

    let captured = capture_final_assistant_live_output(&snapshot.conversation);
    let result = {
        let store = store.lock().await;
        let commit = build_live_output_validation_commit(&store, &live_attempt, &captured)?;
        store.commit_recursive_live_output_validation(commit)?
    };
    Ok(RecursiveDagLiveOutputCommitResult::Committed { result })
}

pub(crate) async fn commit_recoverable_completed_live_attempt_outputs_after_restart(
    store: Arc<Mutex<Store>>,
    reader: &SessionManager,
    budget: RecursiveRecoveryBudget,
) -> Result<(usize, usize, usize)> {
    if budget.max_graphs == 0 {
        return Err(DaemonError::InvalidParam(
            "recursive live output recovery budget max_graphs must be positive".to_string(),
        ));
    }

    let candidates = {
        let store = store.lock().await;
        store
            .list_stale_recursive_live_attempts_for_recovery()?
            .into_iter()
            .filter(|attempt| attempt.summary.status == RecursiveLiveAttemptStatus::RecoveryPending)
            .collect::<Vec<_>>()
    };
    let started = Instant::now();
    let time_budget = budget.time_budget_ms.map(StdDuration::from_millis);
    let mut checked = 0_usize;
    let mut committed = 0_usize;
    let mut deferred = 0_usize;

    for (index, candidate) in candidates.iter().enumerate() {
        if checked >= budget.max_graphs as usize {
            deferred = candidates.len().saturating_sub(index);
            break;
        }
        if time_budget.is_some_and(|budget| started.elapsed() >= budget) {
            deferred = candidates.len().saturating_sub(index);
            break;
        }
        checked = checked.saturating_add(1);

        match commit_completed_live_attempt_output(Arc::clone(&store), reader, candidate.summary.id)
            .await?
        {
            RecursiveDagLiveOutputCommitResult::Committed { .. } => {
                committed = committed.saturating_add(1);
            }
            RecursiveDagLiveOutputCommitResult::NotReady { .. } => {}
        }
    }

    Ok((checked, committed, deferred))
}

fn capture_final_assistant_live_output(
    conversation: &[ConversationEvent],
) -> CapturedRecursiveDagLiveOutput {
    let Some(event) = select_last_terminal_output(conversation, |event| {
        event.event_type == EventType::Message
            && event.role == Some(Role::Assistant)
            && !event.content.trim().is_empty()
    }) else {
        return CapturedRecursiveDagLiveOutput {
            raw_output: String::new(),
            parser_source: RecursiveLiveOutputParserSource::Inline {
                description: Some(
                    "completed live session had no non-empty assistant message".to_string(),
                ),
            },
            event_id: None,
            sequence: None,
            extraction: "missing_assistant_output",
        };
    };

    let content = event.content.trim();
    if let Some((raw_output, block_index)) = final_json_object_block(content) {
        return CapturedRecursiveDagLiveOutput {
            raw_output,
            parser_source: RecursiveLiveOutputParserSource::FinalJsonBlock {
                event_id: Some(event.id),
                block_index: Some(block_index),
            },
            event_id: Some(event.id),
            sequence: Some(event.sequence),
            extraction: "final_json_object",
        };
    }

    CapturedRecursiveDagLiveOutput {
        raw_output: content.to_string(),
        parser_source: RecursiveLiveOutputParserSource::ConversationEvent {
            event_id: event.id,
            sequence: Some(i64::from(event.sequence)),
        },
        event_id: Some(event.id),
        sequence: Some(event.sequence),
        extraction: "final_assistant_message",
    }
}

fn final_json_object_block(content: &str) -> Option<(String, u32)> {
    let mut found = None;
    let mut block_index = 0_u32;
    for (start, end) in json_object_spans(content) {
        let candidate = content[start..end].trim();
        if serde_json::from_str::<serde_json::Value>(candidate).is_ok_and(|value| value.is_object())
        {
            found = Some((candidate.to_string(), block_index));
            block_index = block_index.saturating_add(1);
        }
    }
    found
}

fn json_object_spans(content: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut depth = 0_usize;
    let mut start = None;
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in content.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth = depth.saturating_add(1);
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = start.take() {
                        spans.push((start, index + ch.len_utf8()));
                    }
                }
            }
            _ => {}
        }
    }

    spans
}

fn build_live_output_validation_commit(
    store: &Store,
    live_attempt: &RecursiveLiveAttemptDetail,
    captured: &CapturedRecursiveDagLiveOutput,
) -> Result<RecursiveLiveOutputValidationCommit> {
    let detail = load_graph(store, live_attempt.summary.graph_id)?;
    let task = detail
        .nodes
        .iter()
        .find(|task| task.id == live_attempt.summary.task_id)
        .ok_or_else(|| {
            DaemonError::Store(format!(
                "recursive task not found: {}",
                live_attempt.summary.task_id
            ))
        })?;
    let attempt = detail
        .attempts
        .iter()
        .find(|attempt| attempt.id == live_attempt.summary.attempt_id)
        .ok_or_else(|| {
            DaemonError::Store(format!(
                "recursive attempt not found: {}",
                live_attempt.summary.attempt_id
            ))
        })?;
    let remaining_task_retries = task.max_retries.saturating_sub(attempt.retry_count);
    let mut context = RecursiveLiveOutputValidationContext::new(
        task,
        attempt,
        live_attempt.summary.id,
        live_attempt.summary.scheduler_run_id,
        live_attempt.summary.session_id,
    )
    .with_graph(&detail);
    context.parser_source = Some(captured.parser_source.clone());
    context.retry_policy = RecursiveLiveOutputRepairRetryPolicy {
        remaining_output_repair_attempts: 0,
        // Store retry_count is assigned from prior Failed | Interrupted attempts;
        // keep validation retry metadata aligned with live output DAG-state mapping.
        remaining_task_retries: Some(remaining_task_retries),
    };

    let (validation_input, session_id_enriched) = enrich_missing_live_output_session_id(
        &captured.raw_output,
        live_attempt.summary.session_id,
    )?;
    let mut validation = validate_recursive_live_output_json(&validation_input, &context);
    validation.metadata = serde_json::json!({
        "captured_by": "recursive_dag_live_output_committer",
        "extraction": captured.extraction,
        "session_id": live_attempt.summary.session_id.map(|id| id.to_string()),
        "conversation_event_id": captured.event_id,
        "conversation_sequence": captured.sequence,
        "session_id_enriched": session_id_enriched,
    });
    let validated_output = validation.normalized_output.clone();
    let parser_source = serde_json::to_value(&captured.parser_source).map_err(DaemonError::Json)?;

    Ok(RecursiveLiveOutputValidationCommit {
        live_attempt_id: live_attempt.summary.id,
        graph_id: live_attempt.summary.graph_id,
        task_id: live_attempt.summary.task_id,
        scheduler_run_id: live_attempt.summary.scheduler_run_id,
        attempt_id: live_attempt.summary.attempt_id,
        validated_output,
        validation_result: validation,
        raw_output_artifact: Some(RecursiveExecutionArtifactCreate {
            task_id: live_attempt.summary.task_id,
            attempt_id: Some(live_attempt.summary.attempt_id),
            kind: RecursiveExecutionArtifactKind::Inline,
            label: "live-session-final-output".to_string(),
            content: Some(captured.raw_output.clone()),
            uri: None,
            metadata: serde_json::json!({
                "source": "session_conversation",
                "parser_source": parser_source,
                "extraction": captured.extraction,
                "session_id": live_attempt.summary.session_id.map(|id| id.to_string()),
                "conversation_event_id": captured.event_id,
                "conversation_sequence": captured.sequence,
                "session_id_enriched": session_id_enriched,
            }),
        }),
        produced_artifacts: Vec::new(),
        test_artifacts: Vec::new(),
        diff_artifacts: Vec::new(),
        scheduler_event: Some(RecursiveLiveOutputSchedulerEventCreate {
            event_type: "live_output_validation_committed".to_string(),
            message: Some("recursive DAG live output captured from completed session".to_string()),
            metadata: serde_json::json!({
                "captured_by": "recursive_dag_live_output_committer",
                "extraction": captured.extraction,
                "conversation_event_id": captured.event_id,
                "conversation_sequence": captured.sequence,
                "session_id_enriched": session_id_enriched,
            }),
        }),
    })
}

fn enrich_missing_live_output_session_id(
    raw_output: &str,
    session_id: Option<Uuid>,
) -> Result<(String, bool)> {
    let Some(session_id) = session_id else {
        return Ok((raw_output.to_string(), false));
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(raw_output) else {
        return Ok((raw_output.to_string(), false));
    };
    let Some(correlation) = value
        .get_mut("correlation")
        .and_then(|value| value.as_object_mut())
    else {
        return Ok((raw_output.to_string(), false));
    };
    if correlation
        .get("session_id")
        .is_some_and(|value| !value.is_null())
    {
        return Ok((raw_output.to_string(), false));
    }
    correlation.insert(
        "session_id".to_string(),
        serde_json::Value::String(session_id.to_string()),
    );
    Ok((
        serde_json::to_string(&value).map_err(DaemonError::Json)?,
        true,
    ))
}

#[async_trait]
pub(crate) trait RecursiveDagLiveSessionInterrupter: Send + Sync {
    /// Request graceful interruption through the normal session lifecycle path.
    async fn interrupt_recursive_dag_session(&self, session_id: Uuid) -> Result<()>;
}

#[async_trait]
impl RecursiveDagLiveSessionInterrupter for SessionManager {
    async fn interrupt_recursive_dag_session(&self, session_id: Uuid) -> Result<()> {
        self.interrupt_session(session_id).await
    }
}

struct MissingRecursiveDagLiveSessionInterrupter;

#[async_trait]
impl RecursiveDagLiveSessionInterrupter for MissingRecursiveDagLiveSessionInterrupter {
    async fn interrupt_recursive_dag_session(&self, _session_id: Uuid) -> Result<()> {
        Err(DaemonError::InvalidParam(
            "recursive DAG live session interrupter is not configured".to_string(),
        ))
    }
}

#[allow(dead_code)]
pub(crate) struct RecursiveDagLiveExecutor<L> {
    store: Arc<Mutex<Store>>,
    launcher: L,
    interrupter: Arc<dyn RecursiveDagLiveSessionInterrupter>,
    enabled: bool,
}

#[allow(dead_code)]
impl<L> RecursiveDagLiveExecutor<L>
where
    L: RecursiveDagLiveSessionLauncher,
{
    pub(crate) fn disabled(store: Arc<Mutex<Store>>, launcher: L) -> Self {
        Self {
            store,
            launcher,
            interrupter: Arc::new(MissingRecursiveDagLiveSessionInterrupter),
            enabled: false,
        }
    }

    pub(crate) fn enabled(
        store: Arc<Mutex<Store>>,
        launcher: L,
        interrupter: Arc<dyn RecursiveDagLiveSessionInterrupter>,
    ) -> Self {
        Self {
            store,
            launcher,
            interrupter,
            enabled: true,
        }
    }

    #[cfg(test)]
    fn enabled_for_test(store: Arc<Mutex<Store>>, launcher: L) -> Self {
        Self {
            store,
            launcher,
            interrupter: Arc::new(MissingRecursiveDagLiveSessionInterrupter),
            enabled: true,
        }
    }

    #[cfg(test)]
    fn enabled_for_test_with_interrupter(
        store: Arc<Mutex<Store>>,
        launcher: L,
        interrupter: Arc<dyn RecursiveDagLiveSessionInterrupter>,
    ) -> Self {
        Self {
            store,
            launcher,
            interrupter,
            enabled: true,
        }
    }

    pub(crate) async fn execute(
        &self,
        request: RecursiveDagLiveExecutionRequest,
    ) -> Result<RecursiveDagLiveExecutionResult> {
        if !self.enabled {
            return Err(DaemonError::InvalidParam(
                "recursive DAG live execution is disabled".to_string(),
            ));
        }

        let prepared = self.prepare_launch(request).await?;
        if let Some(session_id) = prepared.existing_session_id {
            self.verify_launched_session_is_loadable(session_id).await?;
            let live_attempt = {
                let store = self.store.lock().await;
                store
                    .load_recursive_live_attempt(prepared.live_attempt_id)?
                    .ok_or_else(|| {
                        DaemonError::Store(format!(
                            "recursive live attempt missing after idempotent load: {}",
                            prepared.live_attempt_id
                        ))
                    })?
            };
            return Ok(RecursiveDagLiveExecutionResult::Launched {
                live_attempt,
                envelope: prepared.envelope,
                session_id,
            });
        }

        let launch_result = self
            .launcher
            .launch_recursive_dag_session(prepared.envelope.clone(), prepared.launch_config.clone())
            .await;

        match launch_result {
            Ok(launched_session) => {
                let session_id = launched_session.session_id;
                let attach_result = match self.verify_launched_session_is_loadable(session_id).await
                {
                    Ok(()) => {
                        self.attach_launched_session(prepared.live_attempt_id, session_id)
                            .await
                    }
                    Err(error) => Err(error),
                };
                match attach_result {
                    Ok(live_attempt) => Ok(RecursiveDagLiveExecutionResult::Launched {
                        live_attempt,
                        envelope: prepared.envelope,
                        session_id,
                    }),
                    Err(error) => {
                        let failure_reason = error.to_string();
                        let live_attempt = self
                            .mark_live_attempt_failed(
                                prepared.live_attempt_id,
                                failure_reason.clone(),
                            )
                            .await?;
                        Ok(RecursiveDagLiveExecutionResult::LaunchFailed {
                            live_attempt,
                            envelope: prepared.envelope,
                            session_id: Some(session_id),
                            failure_reason,
                        })
                    }
                }
            }
            Err(error) => {
                let failure_reason = error.to_string();
                let live_attempt = self
                    .mark_live_attempt_failed(prepared.live_attempt_id, failure_reason.clone())
                    .await?;
                Ok(RecursiveDagLiveExecutionResult::LaunchFailed {
                    live_attempt,
                    envelope: prepared.envelope,
                    session_id: None,
                    failure_reason,
                })
            }
        }
    }

    pub(crate) async fn request_interrupt(
        &self,
        request: RecursiveDagLiveInterruptRequest,
    ) -> Result<RecursiveLiveInterruptSummary> {
        if !self.enabled {
            return Err(DaemonError::InvalidParam(
                "recursive DAG live execution is disabled".to_string(),
            ));
        }

        let requested = {
            let store = self.store.lock().await;
            store.request_recursive_live_interrupt(RecursiveLiveInterruptCreate {
                id: RecursiveLiveInterruptId::new(),
                live_attempt_id: request.live_attempt_id,
                cancellation_request_id: request.cancellation_request_id,
                reason: request.reason,
            })?
        };
        let interrupt = requested.interrupt;
        if !requested.inserted || interrupt.status != RecursiveLiveInterruptStatus::Requested {
            return Ok(interrupt);
        }

        let Some(session_id) = interrupt.session_id else {
            return Ok(interrupt);
        };

        let sent = {
            let store = self.store.lock().await;
            store.mark_recursive_live_interrupt_sent(interrupt.id)?
        };
        if sent.status != RecursiveLiveInterruptStatus::Sent {
            return Ok(sent);
        }

        match self
            .interrupter
            .interrupt_recursive_dag_session(session_id)
            .await
        {
            Ok(()) => {
                let store = self.store.lock().await;
                store.mark_recursive_live_interrupt_interrupted(sent.id)
            }
            Err(error) => {
                let failure_reason = error.to_string();
                let store = self.store.lock().await;
                store.mark_recursive_live_interrupt_failed(sent.id, failure_reason)
            }
        }
    }

    pub(crate) async fn start_heartbeat(
        &self,
        request: RecursiveDagLiveHeartbeatStartRequest,
    ) -> Result<RecursiveLiveAttemptHeartbeatState> {
        if !self.enabled {
            return Err(DaemonError::InvalidParam(
                "recursive DAG live execution is disabled".to_string(),
            ));
        }

        let store = self.store.lock().await;
        store.start_recursive_live_attempt_heartbeat(RecursiveLiveAttemptHeartbeatStart {
            live_attempt_id: request.live_attempt_id,
            heartbeat_owner: request.heartbeat_owner,
            heartbeat_ttl_seconds: request.heartbeat_ttl_seconds,
        })
    }

    pub(crate) async fn heartbeat(
        &self,
        request: RecursiveDagLiveHeartbeatRequest,
    ) -> Result<RecursiveLiveAttemptHeartbeatState> {
        if !self.enabled {
            return Err(DaemonError::InvalidParam(
                "recursive DAG live execution is disabled".to_string(),
            ));
        }

        let store = self.store.lock().await;
        store.heartbeat_recursive_live_attempt(RecursiveLiveAttemptHeartbeatRenew {
            live_attempt_id: request.live_attempt_id,
            heartbeat_token: request.heartbeat_token,
            heartbeat_ttl_seconds: request.heartbeat_ttl_seconds,
        })
    }

    pub(crate) async fn release_heartbeat(
        &self,
        request: RecursiveDagLiveHeartbeatReleaseRequest,
    ) -> Result<RecursiveLiveAttemptHeartbeatState> {
        if !self.enabled {
            return Err(DaemonError::InvalidParam(
                "recursive DAG live execution is disabled".to_string(),
            ));
        }

        let store = self.store.lock().await;
        store.release_recursive_live_attempt_heartbeat(RecursiveLiveAttemptHeartbeatRelease {
            live_attempt_id: request.live_attempt_id,
            heartbeat_token: request.heartbeat_token,
        })
    }

    async fn prepare_launch(
        &self,
        request: RecursiveDagLiveExecutionRequest,
    ) -> Result<PreparedRecursiveDagLiveLaunch> {
        let prepared = {
            let store = self.store.lock().await;
            let detail = store
                .get_recursive_task_graph(request.graph_id)?
                .ok_or_else(|| {
                    DaemonError::Store(format!(
                        "recursive DAG graph not found: {}",
                        request.graph_id
                    ))
                })?;
            let run = store
                .load_recursive_scheduler_run(request.scheduler_run_id)?
                .ok_or_else(|| {
                    DaemonError::Store(format!(
                        "recursive scheduler run not found: {}",
                        request.scheduler_run_id
                    ))
                })?;
            if run.graph_id != request.graph_id {
                return Err(DaemonError::Store(format!(
                    "recursive scheduler run {} belongs to graph {}, not {}",
                    request.scheduler_run_id, run.graph_id, request.graph_id
                )));
            }
            let task = detail
                .nodes
                .iter()
                .find(|node| node.id == request.task_id)
                .cloned()
                .ok_or_else(|| {
                    DaemonError::Store(format!("recursive task not found: {}", request.task_id))
                })?;
            let attempt = detail
                .attempts
                .iter()
                .find(|attempt| attempt.id == request.attempt_id)
                .cloned()
                .ok_or_else(|| {
                    DaemonError::Store(format!(
                        "recursive attempt not found: {}",
                        request.attempt_id
                    ))
                })?;
            if attempt.task_id != request.task_id {
                return Err(DaemonError::Store(format!(
                    "recursive attempt {} belongs to task {}, not {}",
                    request.attempt_id, attempt.task_id, request.task_id
                )));
            }
            if attempt.phase != request.phase {
                return Err(DaemonError::Store(format!(
                    "recursive attempt {} phase {:?} does not match request phase {:?}",
                    request.attempt_id, attempt.phase, request.phase
                )));
            }
            if attempt.status != RecursiveAttemptStatus::Running {
                return Err(DaemonError::Store(format!(
                    "recursive attempt {} is not running",
                    request.attempt_id
                )));
            }

            let parent_session = detail
                .graph
                .parent_session_id
                .map(|session_id| store.get_session(session_id))
                .transpose()?
                .flatten();
            let provider = request
                .provider
                .or_else(|| parent_session.as_ref().map(|session| session.provider));
            let model = request.model.clone().or_else(|| {
                parent_session
                    .as_ref()
                    .and_then(|session| session.model.clone())
            });
            let working_dir = request.working_dir.clone().or_else(|| {
                parent_session
                    .as_ref()
                    .map(|session| session.working_dir.clone())
            });
            let requested_sandbox_kind = request.sandbox.as_ref().and_then(|spec| spec.kind);
            let requested_sandbox_branch = request
                .sandbox
                .as_ref()
                .and_then(|spec| spec.branch.clone());
            let sandbox_kind = requested_sandbox_kind.or_else(|| {
                parent_session
                    .as_ref()
                    .and_then(|session| session.sandbox_kind)
            });
            let sandbox_root = parent_session
                .as_ref()
                .and_then(|session| session.sandbox_root.clone());
            let sandbox_branch = requested_sandbox_branch.clone().or_else(|| {
                parent_session
                    .as_ref()
                    .and_then(|session| session.sandbox_branch.clone())
            });
            let mut sandbox_policy = request.sandbox_policy.clone();
            sandbox_policy.requested_kind =
                sandbox_policy.requested_kind.or(requested_sandbox_kind);
            sandbox_policy.requested_branch = sandbox_policy
                .requested_branch
                .or_else(|| requested_sandbox_branch.clone());

            let live_attempt_id = request
                .live_attempt_id
                .unwrap_or_else(RecursiveLiveAttemptId::new);
            let live_attempt =
                store.create_or_load_recursive_live_attempt(RecursiveLiveAttemptCreate {
                    id: live_attempt_id,
                    graph_id: request.graph_id,
                    task_id: request.task_id,
                    scheduler_run_id: request.scheduler_run_id,
                    attempt_id: request.attempt_id,
                    execution_mode: RecursiveExecutionMode::LiveSession,
                    provider,
                    model: model.clone(),
                    sandbox_kind,
                    sandbox_root: sandbox_root.clone(),
                    sandbox_branch: sandbox_branch.clone(),
                    sandbox_worktree_id: request.sandbox_worktree_id.clone(),
                    workflow_execution_id: request.workflow_execution_id.clone(),
                    topology_workflow_id: request.topology_workflow_id,
                    max_wall_time_ms: request
                        .max_wall_time_ms
                        .or(request.budgets.max_wall_time_ms),
                })?;
            let live_attempt = if live_attempt.live_attempt.summary.session_id.is_some() {
                live_attempt.live_attempt
            } else if live_attempt.live_attempt.summary.status.is_terminal() {
                return Err(DaemonError::Store(format!(
                    "recursive live attempt {} is terminal and cannot be relaunched",
                    live_attempt.live_attempt.summary.id
                )));
            } else {
                store.update_recursive_live_attempt_status(
                    live_attempt.live_attempt.summary.id,
                    RecursiveLiveAttemptStatusUpdate {
                        status: RecursiveLiveAttemptStatus::Launching,
                        failure_reason: None,
                        interruption_reason: None,
                        cancellation_reason: None,
                        recovery_reason: None,
                        error: None,
                    },
                )?
            };

            let mut budgets = request.budgets.clone();
            budgets.max_wall_time_ms = budgets.max_wall_time_ms.or(request.max_wall_time_ms);
            let envelope = build_launch_envelope(
                &detail,
                &task,
                &attempt,
                &live_attempt,
                provider,
                model,
                request.effort,
                working_dir,
                requested_sandbox_kind,
                requested_sandbox_branch,
                sandbox_root,
                sandbox_branch,
                request.sandbox_worktree_id,
                request.workflow_execution_id,
                request.topology_workflow_id,
                budgets,
                request.approval_policy,
                request.tool_policy,
                sandbox_policy,
            );
            let launch_config = launch_config_from_envelope(&envelope);
            PreparedRecursiveDagLiveLaunch {
                live_attempt_id: live_attempt.summary.id,
                existing_session_id: live_attempt.summary.session_id,
                envelope,
                launch_config,
            }
        };
        Ok(prepared)
    }

    async fn verify_launched_session_is_loadable(&self, session_id: Uuid) -> Result<()> {
        let store = self.store.lock().await;
        if store.get_session(session_id)?.is_some() {
            return Ok(());
        }
        Err(DaemonError::Store(format!(
            "recursive DAG live launcher contract violated: session {session_id} was returned before its session row was durably persisted and loadable"
        )))
    }

    async fn attach_launched_session(
        &self,
        live_attempt_id: RecursiveLiveAttemptId,
        session_id: Uuid,
    ) -> Result<RecursiveLiveAttemptDetail> {
        let store = self.store.lock().await;
        store.attach_recursive_live_attempt_session(live_attempt_id, session_id)
    }

    async fn mark_live_attempt_failed(
        &self,
        live_attempt_id: RecursiveLiveAttemptId,
        failure_reason: String,
    ) -> Result<RecursiveLiveAttemptDetail> {
        let store = self.store.lock().await;
        store.update_recursive_live_attempt_status(
            live_attempt_id,
            RecursiveLiveAttemptStatusUpdate {
                status: RecursiveLiveAttemptStatus::Failed,
                failure_reason: Some(failure_reason.clone()),
                interruption_reason: None,
                cancellation_reason: None,
                recovery_reason: None,
                error: Some(failure_reason),
            },
        )
    }
}

#[derive(Debug, Clone)]
struct PreparedRecursiveDagLiveLaunch {
    live_attempt_id: RecursiveLiveAttemptId,
    existing_session_id: Option<Uuid>,
    envelope: RecursiveDagLiveLaunchEnvelope,
    launch_config: LaunchConfig,
}

#[allow(clippy::too_many_arguments)]
fn build_launch_envelope(
    detail: &RecursiveTaskGraphDetail,
    task: &rsi_common::recursive_dag::RecursiveTaskNode,
    attempt: &rsi_common::recursive_dag::RecursiveTaskAttempt,
    live_attempt: &RecursiveLiveAttemptDetail,
    provider: Option<SessionProvider>,
    model: Option<String>,
    effort: Option<String>,
    working_dir: Option<PathBuf>,
    requested_sandbox_kind: Option<SandboxKind>,
    requested_sandbox_branch: Option<String>,
    sandbox_root: Option<PathBuf>,
    sandbox_branch: Option<String>,
    sandbox_worktree_id: Option<String>,
    workflow_execution_id: Option<String>,
    topology_workflow_id: Option<Uuid>,
    budgets: RecursiveDagLiveBudgetPlaceholders,
    approval_policy: RecursiveDagLiveApprovalPolicy,
    tool_policy: RecursiveDagLiveToolPolicy,
    sandbox_policy: RecursiveDagLiveSandboxPolicy,
) -> RecursiveDagLiveLaunchEnvelope {
    let dependency_task_ids = dependency_task_ids(detail, task.id);
    let parent_artifacts = task
        .parent_task_id
        .map(|parent_task_id| artifact_refs_for_tasks(detail, &[parent_task_id]))
        .unwrap_or_default();
    let dependency_artifacts = artifact_refs_for_tasks(detail, &dependency_task_ids);

    let mut envelope = RecursiveDagLiveLaunchEnvelope {
        graph_id: detail.graph.id,
        graph_title: detail.graph.title.clone(),
        graph_objective: detail.graph.objective.clone(),
        task_id: task.id,
        task_title: task.title.clone(),
        task_objective: task.objective.clone(),
        task_scope: task.scope.clone(),
        acceptance_criteria: task.acceptance_criteria.clone(),
        task_depth: task.depth,
        task_scope_units: task.scope_units,
        task_max_retries: task.max_retries,
        phase: attempt.phase,
        attempt_id: attempt.id,
        attempt_no: attempt.attempt_no,
        scheduler_run_id: live_attempt.summary.scheduler_run_id,
        recursive_live_attempt_id: live_attempt.summary.id,
        provider,
        model,
        effort,
        working_dir,
        requested_sandbox_kind,
        requested_sandbox_branch,
        sandbox_root,
        sandbox_branch,
        sandbox_worktree_id,
        execution_mode: RecursiveExecutionMode::LiveSession,
        project_id: detail.graph.project_id,
        workflow_id: detail.graph.workflow_id,
        topology_id: detail.graph.topology_id,
        parent_session_id: detail.graph.parent_session_id,
        source_execution_id: detail.graph.source_execution_id.clone(),
        workflow_execution_id,
        topology_workflow_id,
        budgets,
        approval_policy,
        tool_policy,
        sandbox_policy,
        parent_task_id: task.parent_task_id,
        dependency_task_ids,
        parent_artifacts,
        dependency_artifacts,
        launch_query: String::new(),
        system_prompt: String::new(),
    };
    envelope.launch_query = render_launch_query(&envelope);
    envelope.system_prompt = render_system_prompt(&envelope);
    envelope
}

fn dependency_task_ids(
    detail: &RecursiveTaskGraphDetail,
    task_id: RecursiveTaskId,
) -> Vec<RecursiveTaskId> {
    detail
        .edges
        .iter()
        .filter(|edge| edge.kind == RecursiveTaskEdgeKind::Dependency && edge.to_task_id == task_id)
        .map(|edge| edge.from_task_id)
        .collect()
}

fn artifact_refs_for_tasks(
    detail: &RecursiveTaskGraphDetail,
    task_ids: &[RecursiveTaskId],
) -> Vec<RecursiveDagLiveArtifactReference> {
    detail
        .artifacts
        .iter()
        .filter(|artifact| task_ids.contains(&artifact.task_id))
        .map(|artifact| RecursiveDagLiveArtifactReference {
            artifact_id: artifact.id,
            task_id: artifact.task_id,
            attempt_id: artifact.attempt_id,
            kind: artifact.kind,
            label: artifact.label.clone(),
            uri: artifact.uri.clone(),
            metadata: artifact.metadata.clone(),
        })
        .collect()
}

fn render_launch_query(envelope: &RecursiveDagLiveLaunchEnvelope) -> String {
    let criteria = if envelope.acceptance_criteria.is_empty() {
        "- No explicit acceptance criteria recorded.".to_string()
    } else {
        envelope
            .acceptance_criteria
            .iter()
            .map(|criterion| format!("- {criterion}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Master implement recursive DAG task.\n\nYou are already the live provider session for recursive live attempt {live_attempt_id}. Do not create, copy, inspect, or regenerate a separate recursive DAG fixture database.\n\nGraph: {graph_title}\nGraph objective: {graph_objective}\nTask: {task_title}\nTask objective: {task_objective}\nScope: {task_scope}\nPhase: {phase:?}\nGraph id: {graph_id}\nTask id: {task_id}\nAttempt id: {attempt_id}\nScheduler run id: {run_id}\nRecursive live attempt id: {live_attempt_id}\n\nAcceptance criteria:\n{criteria}\n\nFinal response contract: emit only one valid RecursiveLiveOutputEnvelope JSON object. For a success outcome, include one acceptance entry for each acceptance criterion above, with the criterion text copied exactly, and include a non-empty tests array: either real test evidence, or one explicit not_run record of the form {{\"status\":\"not_run\",\"required\":false,\"reason\":\"<why no test was run>\"}}.",
        graph_title = envelope.graph_title,
        graph_objective = envelope.graph_objective,
        task_title = envelope.task_title,
        task_objective = envelope.task_objective,
        task_scope = envelope.task_scope,
        phase = envelope.phase,
        graph_id = envelope.graph_id,
        task_id = envelope.task_id,
        attempt_id = envelope.attempt_id,
        run_id = envelope.scheduler_run_id,
        live_attempt_id = envelope.recursive_live_attempt_id,
    )
}

fn render_system_prompt(envelope: &RecursiveDagLiveLaunchEnvelope) -> String {
    format!(
        "You are executing one selected recursive DAG task through the normal RSI session launch path. You are already the live session for the recursive live attempt id below. Preserve the graph/task/attempt/run/live-attempt correlation ids in durable artifacts and in the final live output JSON. Do not generate, copy, inspect, or repair a separate fixture database. Your final assistant message must contain only one valid RecursiveLiveOutputEnvelope JSON object; do not wrap it in Markdown or prose. For a success outcome, include one acceptance entry for every listed acceptance criterion and copy each criterion string exactly, and include a non-empty tests array carrying either real test evidence or one explicit not_run record ({{\"status\":\"not_run\",\"required\":false,\"reason\":\"<why no test was run>\"}}). Background heartbeat loops, cancellation recovery, crash recovery, and scheduler-loop validation are not enabled in this phase.\n\nGraph id: {}\nTask id: {}\nAttempt id: {}\nScheduler run id: {}\nRecursive live attempt id: {}",
        envelope.graph_id,
        envelope.task_id,
        envelope.attempt_id,
        envelope.scheduler_run_id,
        envelope.recursive_live_attempt_id
    )
}

fn launch_config_from_envelope(envelope: &RecursiveDagLiveLaunchEnvelope) -> LaunchConfig {
    let sandbox = envelope
        .requested_sandbox_kind
        .or(envelope
            .requested_sandbox_branch
            .as_ref()
            .map(|_| SandboxKind::GitWorktree))
        .map(|kind| SandboxSpec {
            kind: Some(kind),
            branch: envelope.requested_sandbox_branch.clone(),
        });
    LaunchConfig {
        query: envelope.launch_query.clone(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        working_dir: envelope.working_dir.clone(),
        provider: envelope.provider,
        model: envelope.model.clone(),
        configured_context_window: None,
        max_turns: None,
        system_prompt: Some(envelope.system_prompt.clone()),
        resume_session_id: None,
        session_kind: Some(SessionKind::Task),
        project_id: envelope.project_id,
        rsi_session_id: None,
        rsi_socket: None,
        rsi_session_token: None,
        continued_from: None,
        openai_base_url: None,
        openai_api_key: None,
        conversation_history: None,
        workflow_id: envelope.workflow_id,
        workflow_id_override: None,
        max_retries: None,
        skip_project_model_default: false,
        model_invocation_purpose:
            rsi_common::model_control::ModelInvocationPurpose::RecursiveLiveTask,
        // `group_id` is a label FK ("organize related sessions"), NOT hierarchy; the
        // canonical Epic spawn (`spawn_coordinator`) likewise leaves it `None`.
        group_id: None,
        // P1-1 / TND#29: parent the live-DAG child to the graph's owning Epic/Group so
        // it is not orphaned. The id originates from
        // `recursive_task_graphs.parent_session_id`, carried here via the envelope by
        // `build_launch_envelope`, and is written to `Session.parent_id` in
        // `launch_session`.
        parent_id: envelope.parent_session_id,
        effort: envelope.effort.clone(),
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        model_invocation_owner: Some(rsi_common::model_control::InvocationOwner {
            recursive_graph_id: Some(envelope.graph_id.to_string()),
            recursive_task_id: Some(envelope.task_id.to_string()),
            recursive_attempt_id: Some(envelope.attempt_id.to_string()),
            operator: Some("recursive-live".to_string()),
            ..Default::default()
        }),
        model_invocation_dedup_key: Some(format!(
            "recursive.live.task:{}",
            envelope.recursive_live_attempt_id
        )),
        model_invocation_request_fingerprint: Some(hash_request_fingerprint(&[
            rsi_common::model_control::ModelInvocationPurpose::RecursiveLiveTask.as_str(),
            &envelope.graph_id.to_string(),
            &envelope.task_id.to_string(),
            &envelope.attempt_id.to_string(),
            envelope.model.as_deref().unwrap_or(""),
        ])),
        sandbox,
        cargo_target_dir: None,
        execution_scratch: None,
        is_eval: false,
        skip_context_pipeline: false,
        capability_class: None,
        tags: vec!["recursive-dag".to_string(), "live-attempt".to_string()],
        topology_node_id: None,
        topology_iteration: 0,
        closure_selector: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::recursive_dag::{
        RecursiveDagFakeBehavior, RecursiveDagFakeExecutor, RecursiveDagScheduler,
    };
    use crate::store::recursive_dag::{
        RecursiveCancellationRequestCreate, RecursiveChildTaskCreate,
        RecursiveDecompositionBatchCreate, RecursiveExecutionArtifactCreate,
        RecursiveLiveSchedulerRunStart, RecursiveRootTaskCreate, RecursiveTaskGraphCreate,
    };
    use chrono::Utc;
    use rsi_common::recursive_dag::{
        RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION, RecursiveCancellationRequestSource,
        RecursiveExecutionArtifactKind, RecursiveLiveAcceptanceCheck,
        RecursiveLiveAcceptanceStatus, RecursiveLiveAttemptHeartbeatStatus,
        RecursiveLiveOutputCorrelation, RecursiveLiveOutputEnvelope, RecursiveLiveOutputOutcome,
        RecursiveLiveOutputRetryDecisionKind, RecursiveLiveOutputValidationStatus,
        RecursiveLiveSuccessPayload, RecursiveLiveTestResultSummary, RecursiveLiveTestStatus,
        RecursiveRecoverySource, RecursiveSchedulerRunSource,
    };
    use rsi_common::types::{
        ContextUsageConfidence, EventType, Role, SandboxCleanupState, Session, SessionStatus,
    };
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct FakeLauncher {
        state: Arc<FakeLauncherState>,
    }

    struct FakeLauncherState {
        store: Arc<Mutex<Store>>,
        result: StdMutex<FakeLauncherResult>,
        calls: StdMutex<Vec<(RecursiveDagLiveLaunchEnvelope, LaunchConfig)>>,
        model_call_count: AtomicUsize,
    }

    #[derive(Clone)]
    enum FakeLauncherResult {
        Succeed {
            session_id: Uuid,
            provider: SessionProvider,
            model: Option<String>,
        },
        SucceedExistingSession {
            session_id: Uuid,
        },
        Fail(String),
    }

    impl FakeLauncher {
        fn new(store: Arc<Mutex<Store>>, result: FakeLauncherResult) -> Self {
            Self {
                state: Arc::new(FakeLauncherState {
                    store,
                    result: StdMutex::new(result),
                    calls: StdMutex::new(Vec::new()),
                    model_call_count: AtomicUsize::new(0),
                }),
            }
        }

        fn calls(&self) -> Vec<(RecursiveDagLiveLaunchEnvelope, LaunchConfig)> {
            self.state.calls.lock().expect("calls lock").clone()
        }

        fn model_call_count(&self) -> usize {
            self.state.model_call_count.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl RecursiveDagLiveSessionLauncher for FakeLauncher {
        async fn launch_recursive_dag_session(
            &self,
            envelope: RecursiveDagLiveLaunchEnvelope,
            config: LaunchConfig,
        ) -> Result<RecursiveDagLiveLaunchedSession> {
            {
                let mut calls = self.state.calls.lock().expect("calls lock");
                calls.push((envelope.clone(), config));
            }
            {
                let store = self.state.store.lock().await;
                let live = store
                    .load_recursive_live_attempt(envelope.recursive_live_attempt_id)?
                    .expect("live attempt exists while launching");
                assert_eq!(live.summary.status, RecursiveLiveAttemptStatus::Launching);
            }
            let result = self.state.result.lock().expect("result lock").clone();
            match result {
                FakeLauncherResult::Succeed {
                    session_id,
                    provider,
                    model,
                } => {
                    let store = self.state.store.lock().await;
                    store.insert_session(&test_session(session_id, provider, model))?;
                    Ok(RecursiveDagLiveLaunchedSession::new(session_id))
                }
                FakeLauncherResult::SucceedExistingSession { session_id } => {
                    Ok(RecursiveDagLiveLaunchedSession::new(session_id))
                }
                FakeLauncherResult::Fail(reason) => Err(DaemonError::Process(reason)),
            }
        }
    }

    #[derive(Clone)]
    struct FakeInterrupter {
        state: Arc<FakeInterrupterState>,
    }

    struct FakeInterrupterState {
        result: StdMutex<Result<()>>,
        calls: StdMutex<Vec<Uuid>>,
    }

    impl FakeInterrupter {
        fn succeed() -> Self {
            Self::new(Ok(()))
        }

        fn fail(reason: impl Into<String>) -> Self {
            Self::new(Err(DaemonError::Process(reason.into())))
        }

        fn new(result: Result<()>) -> Self {
            Self {
                state: Arc::new(FakeInterrupterState {
                    result: StdMutex::new(result),
                    calls: StdMutex::new(Vec::new()),
                }),
            }
        }

        fn calls(&self) -> Vec<Uuid> {
            self.state.calls.lock().expect("calls lock").clone()
        }
    }

    #[async_trait]
    impl RecursiveDagLiveSessionInterrupter for FakeInterrupter {
        async fn interrupt_recursive_dag_session(&self, session_id: Uuid) -> Result<()> {
            self.state
                .calls
                .lock()
                .expect("calls lock")
                .push(session_id);
            self.state
                .result
                .lock()
                .expect("result lock")
                .as_ref()
                .map(|_| ())
                .map_err(|error| DaemonError::Process(error.to_string()))
        }
    }

    struct LiveFixture {
        _dir: tempfile::TempDir,
        store: Arc<Mutex<Store>>,
        graph_id: RecursiveTaskGraphId,
        root_id: RecursiveTaskId,
        dependency_id: RecursiveTaskId,
        task_id: RecursiveTaskId,
        run_id: RecursiveSchedulerRunId,
        attempt_id: RecursiveAttemptId,
    }

    fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let db_path = dir.path().join("rsi.db");
        let store = Store::open(&db_path).expect("open store");
        (dir, store)
    }

    fn session_manager_for_live_binding()
    -> (Arc<SessionManager>, tempfile::TempDir, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("temp db dir");
        let sandbox_base = tempfile::TempDir::new().expect("temp sandbox dir");
        let db_path = dir.path().join("rsi.db");
        let store = Store::open(&db_path).expect("open store");
        let mut config = Config::from_env();
        config.retry_max_backoff_ms = 1;
        let runtime_config = RuntimeConfig::from_config(&config);
        let manager = SessionManager::new(
            Arc::new(EventBus::new(16)),
            store,
            false,
            dir.path().join("daemon.sock"),
            None,
            Vec::new(),
            runtime_config,
            sandbox_base.path().to_path_buf(),
        )
        .expect("session manager");
        (Arc::new(manager), dir, sandbox_base)
    }

    fn table_count(store: &Store, table: &str) -> i64 {
        store
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or_else(|error| panic!("count {table}: {error}"))
    }

    fn live_fixture() -> LiveFixture {
        live_fixture_with_parent(None)
    }

    fn live_fixture_with_parent(parent_session_id: Option<Uuid>) -> LiveFixture {
        let (dir, store) = test_store();
        // `recursive_task_graphs.parent_session_id` REFERENCES `sessions(id)` with FK
        // enforcement ON, so the owning Epic/Group row must exist before graph create.
        if let Some(parent_id) = parent_session_id {
            let mut parent = test_session(
                parent_id,
                SessionProvider::Codex,
                Some("gpt-5-codex".to_string()),
            );
            parent.session_kind = SessionKind::Epic;
            store
                .insert_session(&parent)
                .expect("insert epic parent session");
        }
        let graph_id = RecursiveTaskGraphId::new();
        let root_id = RecursiveTaskId::new();
        let dependency_id = RecursiveTaskId::new();
        let task_id = RecursiveTaskId::new();
        store
            .create_recursive_live_task_graph(RecursiveTaskGraphCreate {
                graph_id,
                title: "Live graph".to_string(),
                objective: "Ship a recursive live adapter".to_string(),
                root_task: RecursiveRootTaskCreate {
                    task_id: root_id,
                    title: "Root".to_string(),
                    objective: "Coordinate children".to_string(),
                    scope: "Repo".to_string(),
                    acceptance_criteria: vec!["Children complete".to_string()],
                    scope_units: 3,
                    max_retries: 1,
                },
                project_id: None,
                workflow_id: None,
                topology_id: None,
                parent_session_id,
                source_execution_id: None,
                source_eval_id: None,
                max_depth: 3,
                max_fanout: 4,
                max_descendants: 8,
                step_limit: 10,
            })
            .expect("create graph");
        store
            .insert_recursive_decomposition_batch(
                graph_id,
                RecursiveDecompositionBatchCreate {
                    batch_id: rsi_common::recursive_dag::RecursiveInjectionBatchId::new(),
                    parent_task_id: root_id,
                    attempt_id: None,
                    reason_for_decomposition: "split".to_string(),
                    children: vec![
                        RecursiveChildTaskCreate {
                            task_id: dependency_id,
                            title: "Dependency".to_string(),
                            objective: "Produce input".to_string(),
                            scope: "Dependency scope".to_string(),
                            acceptance_criteria: vec!["Input exists".to_string()],
                            scope_units: 1,
                            max_retries: 1,
                            dependencies: Vec::new(),
                        },
                        RecursiveChildTaskCreate {
                            task_id,
                            title: "Selected task".to_string(),
                            objective: "Use dependency input".to_string(),
                            scope: "Selected scope".to_string(),
                            acceptance_criteria: vec![
                                "Launch envelope is inspectable".to_string(),
                                "Session correlation is recorded".to_string(),
                            ],
                            scope_units: 2,
                            max_retries: 2,
                            dependencies: vec![dependency_id],
                        },
                    ],
                    integration_strategy: "integrate children".to_string(),
                    verification_strategy: "verify children".to_string(),
                },
            )
            .expect("insert decomposition");
        store
            .record_recursive_execution_artifacts(
                graph_id,
                vec![
                    RecursiveExecutionArtifactCreate {
                        task_id: root_id,
                        attempt_id: None,
                        kind: RecursiveExecutionArtifactKind::Inline,
                        label: "parent-context".to_string(),
                        content: Some("parent artifact".to_string()),
                        uri: None,
                        metadata: serde_json::json!({"source": "parent"}),
                    },
                    RecursiveExecutionArtifactCreate {
                        task_id: dependency_id,
                        attempt_id: None,
                        kind: RecursiveExecutionArtifactKind::Inline,
                        label: "dependency-output".to_string(),
                        content: Some("dependency artifact".to_string()),
                        uri: None,
                        metadata: serde_json::json!({"source": "dependency"}),
                    },
                ],
            )
            .expect("record artifacts");
        let run = store
            .start_recursive_live_scheduler_run(RecursiveLiveSchedulerRunStart {
                graph_id,
                max_steps: 3,
                source: RecursiveSchedulerRunSource::TestHarness,
                operator: Some("test".to_string()),
                idempotency_key: None,
                request_fingerprint: None,
                policy_snapshot: serde_json::json!({ "test": "live_fixture" }),
            })
            .expect("start scheduler run");
        let attempt_id = RecursiveAttemptId::new();
        store
            .record_recursive_attempt_start_with_executor_kind(
                graph_id,
                task_id,
                RecursiveAttemptPhase::Execute,
                attempt_id,
                RecursiveExecutionMode::LiveSession,
            )
            .expect("start attempt");
        LiveFixture {
            _dir: dir,
            store: Arc::new(Mutex::new(store)),
            graph_id,
            root_id,
            dependency_id,
            task_id,
            run_id: run.id,
            attempt_id,
        }
    }

    fn live_request(fixture: &LiveFixture) -> RecursiveDagLiveExecutionRequest {
        RecursiveDagLiveExecutionRequest {
            graph_id: fixture.graph_id,
            task_id: fixture.task_id,
            scheduler_run_id: fixture.run_id,
            attempt_id: fixture.attempt_id,
            phase: RecursiveAttemptPhase::Execute,
            live_attempt_id: Some(RecursiveLiveAttemptId::new()),
            provider: Some(SessionProvider::Codex),
            model: Some("gpt-5-codex".to_string()),
            effort: None,
            working_dir: Some(PathBuf::from("/tmp/rsi-recursive-live")),
            sandbox: Some(SandboxSpec {
                kind: Some(SandboxKind::GitWorktree),
                branch: Some("rsi/live-adapter".to_string()),
            }),
            sandbox_worktree_id: Some("live-worktree".to_string()),
            workflow_execution_id: Some("workflow-execution".to_string()),
            topology_workflow_id: None,
            max_wall_time_ms: Some(60_000),
            budgets: RecursiveDagLiveBudgetPlaceholders {
                max_wall_time_ms: None,
                token_budget: Some(10_000),
                tool_call_budget: Some(32),
                artifact_bytes: Some(1_000_000),
            },
            approval_policy: RecursiveDagLiveApprovalPolicy {
                policy_name: Some("inherit".to_string()),
                require_operator_approval: Some(false),
            },
            tool_policy: RecursiveDagLiveToolPolicy {
                allowed_tools: vec!["inherit".to_string()],
                denied_tools: Vec::new(),
            },
            sandbox_policy: RecursiveDagLiveSandboxPolicy {
                requested_kind: None,
                requested_branch: None,
                preserve_on_failure: Some(true),
                allowed_write_roots: vec![PathBuf::from("/tmp/rsi-recursive-live")],
            },
        }
    }

    fn test_session(session_id: Uuid, provider: SessionProvider, model: Option<String>) -> Session {
        Session {
            context_fill_pct: None,
            id: session_id,
            provider,
            claude_session_id: None,
            query: "recursive DAG live test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            pending_question: None,
            pending_archive: false,
            working_dir: PathBuf::from("/tmp/rsi-recursive-live"),
            git_branch: Some("main".to_string()),
            status: SessionStatus::Running,
            project_id: None,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            session_kind: SessionKind::Task,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            model,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            continued_from: None,
            context_usage_confidence: ContextUsageConfidence::Missing,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: Some(0),
            work_time_ms: None,
            approval_started_at: None,
            sandbox_kind: Some(SandboxKind::GitWorktree),
            sandbox_root: Some(PathBuf::from("/tmp/rsi-recursive-live-worktree")),
            sandbox_branch: Some("rsi/live-adapter".to_string()),
            sandbox_cleanup_state: Some(SandboxCleanupState::Live),
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    async fn create_attached_live_attempt(fixture: &LiveFixture) -> (RecursiveLiveAttemptId, Uuid) {
        let session_id = Uuid::new_v4();
        let store = fixture.store.lock().await;
        let live = store
            .create_recursive_live_attempt_placeholder(RecursiveLiveAttemptCreate {
                id: RecursiveLiveAttemptId::new(),
                graph_id: fixture.graph_id,
                task_id: fixture.task_id,
                scheduler_run_id: fixture.run_id,
                attempt_id: fixture.attempt_id,
                execution_mode: RecursiveExecutionMode::LiveSession,
                provider: Some(SessionProvider::Codex),
                model: Some("gpt-5-codex".to_string()),
                sandbox_kind: Some(SandboxKind::GitWorktree),
                sandbox_root: Some(PathBuf::from("/tmp/rsi-recursive-live-worktree")),
                sandbox_branch: Some("rsi/live-adapter".to_string()),
                sandbox_worktree_id: Some("live-worktree".to_string()),
                workflow_execution_id: None,
                topology_workflow_id: None,
                max_wall_time_ms: Some(60_000),
            })
            .expect("create live attempt");
        store
            .insert_session(&test_session(
                session_id,
                SessionProvider::Codex,
                Some("gpt-5-codex".to_string()),
            ))
            .expect("insert session");
        let live = store
            .attach_recursive_live_attempt_session(live.summary.id, session_id)
            .expect("attach session");
        assert_eq!(live.summary.status, RecursiveLiveAttemptStatus::Running);
        (live.summary.id, session_id)
    }

    async fn request_run_cancellation(fixture: &LiveFixture) -> RecursiveCancellationRequestId {
        let store = fixture.store.lock().await;
        store
            .request_recursive_scheduler_run_cancellation(
                fixture.run_id,
                RecursiveCancellationRequestCreate {
                    source: RecursiveCancellationRequestSource::TestHarness,
                    reason: "operator cancelled live run".to_string(),
                    requested_by: Some("test".to_string()),
                },
            )
            .expect("request run cancellation")
            .id
    }

    fn interrupt_request(
        live_attempt_id: RecursiveLiveAttemptId,
        cancellation_request_id: RecursiveCancellationRequestId,
    ) -> RecursiveDagLiveInterruptRequest {
        RecursiveDagLiveInterruptRequest {
            live_attempt_id,
            cancellation_request_id: Some(cancellation_request_id),
            reason: "operator requested recursive live interrupt".to_string(),
        }
    }

    fn assistant_event(session_id: Uuid, sequence: i32, content: String) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id,
            sequence,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            created_at: Utc::now(),
            content,
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    fn complete_session_with_assistant_output(
        store: &Store,
        session_id: Uuid,
        output: String,
    ) -> i64 {
        store
            .update_session_status(session_id, SessionStatus::Completed)
            .expect("complete session");
        let event = assistant_event(session_id, 1, output);
        store.insert_event(&event).expect("insert assistant event")
    }

    fn live_success_output(live: &RecursiveLiveAttemptDetail) -> RecursiveLiveOutputEnvelope {
        RecursiveLiveOutputEnvelope {
            schema_version: RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION,
            correlation: RecursiveLiveOutputCorrelation {
                graph_id: live.summary.graph_id,
                task_id: live.summary.task_id,
                attempt_id: live.summary.attempt_id,
                live_attempt_id: live.summary.id,
                scheduler_run_id: live.summary.scheduler_run_id,
                session_id: live.summary.session_id,
            },
            summary: "completed live task".to_string(),
            artifacts: Vec::new(),
            tests: vec![RecursiveLiveTestResultSummary {
                command: None,
                status: RecursiveLiveTestStatus::NotRun,
                exit_code: None,
                duration_ms: None,
                output_artifact: None,
                required: true,
                reason: Some("live validation committer test".to_string()),
                metadata: serde_json::Value::Null,
            }],
            diffs: Vec::new(),
            dependency_outputs: Vec::new(),
            notes: Vec::new(),
            metadata: serde_json::Value::Null,
            confidence: None,
            outcome: RecursiveLiveOutputOutcome::Success(RecursiveLiveSuccessPayload {
                result_summary: "all acceptance criteria met".to_string(),
                acceptance: vec![RecursiveLiveAcceptanceCheck {
                    criterion: "Session correlation is recorded".to_string(),
                    status: RecursiveLiveAcceptanceStatus::Met,
                    evidence: vec!["validated from completed session output".to_string()],
                    artifact_ids: Vec::new(),
                    notes: None,
                }],
            }),
        }
    }

    fn create_live_output_attempt_in_store(store: &Store) -> RecursiveLiveAttemptDetail {
        let (graph_id, task_id, scheduler_run_id) = create_live_output_graph_and_run(store, 1);
        let attempt_id = RecursiveAttemptId::new();
        store
            .record_recursive_attempt_start_with_executor_kind(
                graph_id,
                task_id,
                RecursiveAttemptPhase::Execute,
                attempt_id,
                RecursiveExecutionMode::LiveSession,
            )
            .expect("start live output attempt");
        create_live_output_attempt_placeholder(
            store,
            graph_id,
            task_id,
            scheduler_run_id,
            attempt_id,
        )
    }

    fn create_live_output_attempt_after_interrupted_retry_in_store(
        store: &Store,
    ) -> RecursiveLiveAttemptDetail {
        let (graph_id, task_id, scheduler_run_id) = create_live_output_graph_and_run(store, 1);
        let interrupted_attempt_id = RecursiveAttemptId::new();
        let interrupted_attempt = store
            .record_recursive_attempt_start_with_executor_kind(
                graph_id,
                task_id,
                RecursiveAttemptPhase::Execute,
                interrupted_attempt_id,
                RecursiveExecutionMode::LiveSession,
            )
            .expect("start interrupted live output attempt");
        assert_eq!(interrupted_attempt.retry_count, 0);
        let interrupted_live = create_live_output_attempt_placeholder(
            store,
            graph_id,
            task_id,
            scheduler_run_id,
            interrupted_attempt_id,
        );
        store
            .update_recursive_live_attempt_status(
                interrupted_live.summary.id,
                RecursiveLiveAttemptStatusUpdate {
                    status: RecursiveLiveAttemptStatus::Interrupted,
                    failure_reason: None,
                    interruption_reason: Some(
                        "operator interrupted first live attempt".to_string(),
                    ),
                    cancellation_reason: None,
                    recovery_reason: None,
                    error: None,
                },
            )
            .expect("mark first live attempt interrupted");
        store
            .record_recursive_attempt_finish(
                graph_id,
                interrupted_attempt_id,
                RecursiveAttemptStatus::Interrupted,
                Some("operator interrupted first live attempt".to_string()),
                None,
                Some(RecursiveTaskLifecycleState::Ready),
                Some("retry after interrupted live attempt".to_string()),
            )
            .expect("finish interrupted recursive attempt");

        let attempt_id = RecursiveAttemptId::new();
        let retry_attempt = store
            .record_recursive_attempt_start_with_executor_kind(
                graph_id,
                task_id,
                RecursiveAttemptPhase::Execute,
                attempt_id,
                RecursiveExecutionMode::LiveSession,
            )
            .expect("start retry live output attempt");
        assert_eq!(retry_attempt.retry_count, 1);
        create_live_output_attempt_placeholder(
            store,
            graph_id,
            task_id,
            scheduler_run_id,
            attempt_id,
        )
    }

    fn create_live_output_graph_and_run(
        store: &Store,
        max_retries: u32,
    ) -> (
        RecursiveTaskGraphId,
        RecursiveTaskId,
        RecursiveSchedulerRunId,
    ) {
        let graph_id = RecursiveTaskGraphId::new();
        let task_id = RecursiveTaskId::new();
        store
            .create_recursive_live_task_graph(RecursiveTaskGraphCreate {
                graph_id,
                title: "Live output graph".to_string(),
                objective: "Commit completed session output".to_string(),
                root_task: RecursiveRootTaskCreate {
                    task_id,
                    title: "Live output task".to_string(),
                    objective: "Emit recursive live output JSON".to_string(),
                    scope: "One completed live session".to_string(),
                    acceptance_criteria: vec!["Session correlation is recorded".to_string()],
                    scope_units: 1,
                    max_retries,
                },
                project_id: None,
                workflow_id: None,
                topology_id: None,
                parent_session_id: None,
                source_execution_id: None,
                source_eval_id: None,
                max_depth: 2,
                max_fanout: 2,
                max_descendants: 2,
                step_limit: 2,
            })
            .expect("create live output graph");
        let run = store
            .start_recursive_live_scheduler_run(RecursiveLiveSchedulerRunStart {
                graph_id,
                max_steps: 1,
                source: RecursiveSchedulerRunSource::TestHarness,
                operator: Some("live-output-test".to_string()),
                idempotency_key: None,
                request_fingerprint: None,
                policy_snapshot: serde_json::json!({ "test": "live_output_commit" }),
            })
            .expect("start live output scheduler run");
        (graph_id, task_id, run.id)
    }

    fn create_live_output_attempt_placeholder(
        store: &Store,
        graph_id: RecursiveTaskGraphId,
        task_id: RecursiveTaskId,
        scheduler_run_id: RecursiveSchedulerRunId,
        attempt_id: RecursiveAttemptId,
    ) -> RecursiveLiveAttemptDetail {
        store
            .create_recursive_live_attempt_placeholder(RecursiveLiveAttemptCreate {
                id: RecursiveLiveAttemptId::new(),
                graph_id,
                task_id,
                scheduler_run_id,
                attempt_id,
                execution_mode: RecursiveExecutionMode::LiveSession,
                provider: Some(SessionProvider::Codex),
                model: Some("gpt-5-codex".to_string()),
                sandbox_kind: None,
                sandbox_root: None,
                sandbox_branch: None,
                sandbox_worktree_id: None,
                workflow_execution_id: None,
                topology_workflow_id: None,
                max_wall_time_ms: Some(60_000),
            })
            .expect("create live output attempt")
    }

    fn attach_session_to_live_output_attempt(
        store: &Store,
        live_attempt_id: RecursiveLiveAttemptId,
        status: SessionStatus,
    ) -> RecursiveLiveAttemptDetail {
        let session_id = Uuid::new_v4();
        let mut session = test_session(
            session_id,
            SessionProvider::Codex,
            Some("gpt-5-codex".to_string()),
        );
        session.status = status;
        store.insert_session(&session).expect("insert live session");
        store
            .attach_recursive_live_attempt_session(live_attempt_id, session_id)
            .expect("attach live session")
    }

    async fn validation_count_for_live(
        store: &Arc<Mutex<Store>>,
        live_attempt_id: RecursiveLiveAttemptId,
    ) -> i64 {
        let store = store.lock().await;
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM recursive_live_output_validations WHERE live_attempt_id = ?1",
                rusqlite::params![live_attempt_id.to_string()],
                |row| row.get(0),
            )
            .expect("validation count")
    }

    #[tokio::test]
    async fn live_executor_enabled_fake_launch_creates_correlation_and_attaches_session() {
        let fixture = live_fixture();
        let session_id = Uuid::new_v4();
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Succeed {
                session_id,
                provider: SessionProvider::Codex,
                model: Some("gpt-5-codex".to_string()),
            },
        );
        let executor = RecursiveDagLiveExecutor::enabled_for_test(
            Arc::clone(&fixture.store),
            launcher.clone(),
        );

        let result = executor
            .execute(live_request(&fixture))
            .await
            .expect("execute live adapter");

        let RecursiveDagLiveExecutionResult::Launched {
            live_attempt,
            envelope,
            session_id: returned_session_id,
        } = result
        else {
            panic!("expected launched result");
        };
        assert_eq!(returned_session_id, session_id);
        assert_eq!(live_attempt.summary.session_id, Some(session_id));
        assert_eq!(
            live_attempt.summary.status,
            RecursiveLiveAttemptStatus::Running
        );
        assert_eq!(live_attempt.summary.provider, Some(SessionProvider::Codex));
        assert_eq!(live_attempt.summary.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(envelope.graph_id, fixture.graph_id);
        assert_eq!(envelope.task_id, fixture.task_id);
        assert_eq!(envelope.scheduler_run_id, fixture.run_id);
        assert_eq!(envelope.attempt_id, fixture.attempt_id);
        assert_eq!(envelope.task_objective, "Use dependency input");
        assert_eq!(envelope.task_scope, "Selected scope");
        assert_eq!(
            envelope.acceptance_criteria,
            vec![
                "Launch envelope is inspectable".to_string(),
                "Session correlation is recorded".to_string(),
            ]
        );
        assert_eq!(envelope.execution_mode, RecursiveExecutionMode::LiveSession);
        assert_eq!(envelope.provider, Some(SessionProvider::Codex));
        assert_eq!(envelope.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(
            envelope.working_dir,
            Some(PathBuf::from("/tmp/rsi-recursive-live"))
        );
        assert_eq!(
            envelope.requested_sandbox_kind,
            Some(SandboxKind::GitWorktree)
        );
        assert_eq!(
            envelope.requested_sandbox_branch.as_deref(),
            Some("rsi/live-adapter")
        );
        assert_eq!(
            envelope.sandbox_worktree_id.as_deref(),
            Some("live-worktree")
        );
        assert_eq!(envelope.parent_task_id, Some(fixture.root_id));
        assert_eq!(envelope.dependency_task_ids, vec![fixture.dependency_id]);
        assert_eq!(envelope.parent_artifacts.len(), 1);
        assert_eq!(envelope.parent_artifacts[0].label, "parent-context");
        assert_eq!(
            envelope.parent_artifacts[0].metadata,
            serde_json::json!({"source": "parent"})
        );
        assert_eq!(envelope.dependency_artifacts.len(), 1);
        assert_eq!(envelope.dependency_artifacts[0].label, "dependency-output");
        assert_eq!(
            envelope.dependency_artifacts[0].metadata,
            serde_json::json!({"source": "dependency"})
        );
        assert_eq!(envelope.budgets.max_wall_time_ms, Some(60_000));
        assert_eq!(envelope.budgets.token_budget, Some(10_000));
        assert_eq!(envelope.budgets.tool_call_budget, Some(32));
        assert_eq!(envelope.budgets.artifact_bytes, Some(1_000_000));
        assert_eq!(
            envelope.approval_policy.policy_name.as_deref(),
            Some("inherit")
        );
        assert_eq!(
            envelope.approval_policy.require_operator_approval,
            Some(false)
        );
        assert_eq!(
            envelope.tool_policy.allowed_tools,
            vec!["inherit".to_string()]
        );
        assert_eq!(
            envelope.sandbox_policy.requested_kind,
            Some(SandboxKind::GitWorktree)
        );
        assert_eq!(
            envelope.sandbox_policy.requested_branch.as_deref(),
            Some("rsi/live-adapter")
        );
        assert_eq!(envelope.sandbox_policy.preserve_on_failure, Some(true));
        assert!(
            envelope
                .launch_query
                .contains("Master implement recursive DAG task")
        );
        assert!(
            envelope
                .launch_query
                .contains("- Launch envelope is inspectable")
        );
        assert!(
            envelope
                .system_prompt
                .contains("normal RSI session launch path")
        );
        assert!(
            envelope
                .launch_query
                .contains("already the live provider session")
        );
        assert!(envelope.launch_query.contains(
            "Final response contract: emit only one valid RecursiveLiveOutputEnvelope JSON object"
        ));
        assert!(
            envelope
                .system_prompt
                .contains("Do not generate, copy, inspect, or repair a separate fixture database")
        );

        let calls = launcher.calls();
        assert_eq!(calls.len(), 1);
        let (called_envelope, called_config) = &calls[0];
        assert_eq!(called_envelope, &envelope);
        assert_eq!(called_config.query, envelope.launch_query);
        assert_eq!(called_config.provider, Some(SessionProvider::Codex));
        assert_eq!(called_config.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(called_config.session_kind, Some(SessionKind::Task));
        assert_eq!(called_config.workflow_id_override, None);
        assert_eq!(launcher.model_call_count(), 0);

        let store = fixture.store.lock().await;
        let attempts = store
            .list_recursive_live_attempts_for_graph(fixture.graph_id)
            .expect("list live attempts");
        assert_eq!(
            attempts
                .iter()
                .filter(|attempt| attempt.summary.session_id == Some(session_id))
                .count(),
            1,
            "launched session id must be attached to exactly one live attempt"
        );
    }

    #[tokio::test]
    async fn live_dag_child_is_parented_to_owning_epic_group() {
        // P1-1 / TND#29 regression: a live recursive-DAG child must attach under the
        // graph's owning Epic/Group, not spawn orphaned with `parent_id == None`.
        let epic_id = Uuid::new_v4();
        let fixture = live_fixture_with_parent(Some(epic_id));
        let session_id = Uuid::new_v4();
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Succeed {
                session_id,
                provider: SessionProvider::Codex,
                model: Some("gpt-5-codex".to_string()),
            },
        );
        let executor = RecursiveDagLiveExecutor::enabled_for_test(
            Arc::clone(&fixture.store),
            launcher.clone(),
        );

        executor
            .execute(live_request(&fixture))
            .await
            .expect("execute live adapter");

        let calls = launcher.calls();
        assert_eq!(calls.len(), 1, "exactly one child launch recorded");
        let (envelope, config) = &calls[0];
        // The owning Epic/Group id rides the launch envelope...
        assert_eq!(
            envelope.parent_session_id,
            Some(epic_id),
            "graph parent_session_id must reach the launch envelope"
        );
        // ...and lands as the spawned child's hierarchy parent (the fix).
        assert_eq!(
            config.parent_id,
            Some(epic_id),
            "live-DAG child must be parented to its Epic/Group, not orphaned"
        );
        // `group_id` is a label FK, not hierarchy — it must stay None (no leakage).
        assert_eq!(
            config.group_id, None,
            "group_id is a label FK, not the parent"
        );
    }

    #[tokio::test]
    async fn live_executor_launch_failure_marks_live_attempt_failed_with_reason() {
        let fixture = live_fixture();
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launch boundary failed".to_string()),
        );
        let executor = RecursiveDagLiveExecutor::enabled_for_test(
            Arc::clone(&fixture.store),
            launcher.clone(),
        );

        let result = executor
            .execute(live_request(&fixture))
            .await
            .expect("launch failure is recorded result");

        let RecursiveDagLiveExecutionResult::LaunchFailed {
            live_attempt,
            session_id,
            failure_reason,
            ..
        } = result
        else {
            panic!("expected launch failure result");
        };
        assert_eq!(session_id, None);
        assert!(failure_reason.contains("launch boundary failed"));
        assert_eq!(
            live_attempt.summary.status,
            RecursiveLiveAttemptStatus::Failed
        );
        assert_eq!(
            live_attempt.failure_reason.as_deref(),
            Some("Process error: launch boundary failed")
        );
        assert_eq!(live_attempt.summary.session_id, None);
        let store = fixture.store.lock().await;
        let live_attempts = store
            .list_recursive_live_attempts_for_graph(fixture.graph_id)
            .expect("list live attempts");
        assert_eq!(live_attempts.len(), 1);
        assert_eq!(live_attempts[0].summary.session_id, None);
        assert_eq!(launcher.calls().len(), 1);
        assert_eq!(launcher.model_call_count(), 0);
    }

    #[tokio::test]
    async fn live_executor_missing_persisted_session_fails_without_attach() {
        let fixture = live_fixture();
        let session_id = Uuid::new_v4();
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::SucceedExistingSession { session_id },
        );
        let executor = RecursiveDagLiveExecutor::enabled_for_test(
            Arc::clone(&fixture.store),
            launcher.clone(),
        );

        let result = executor
            .execute(live_request(&fixture))
            .await
            .expect("missing session row is recorded as launch failure");

        let RecursiveDagLiveExecutionResult::LaunchFailed {
            live_attempt,
            session_id: returned_session_id,
            failure_reason,
            ..
        } = result
        else {
            panic!("expected launch failure result");
        };
        assert_eq!(returned_session_id, Some(session_id));
        assert!(failure_reason.contains("launcher contract violated"));
        assert!(failure_reason.contains("durably persisted and loadable"));
        assert_eq!(
            live_attempt.summary.status,
            RecursiveLiveAttemptStatus::Failed
        );
        assert_eq!(live_attempt.summary.session_id, None);
        assert!(
            live_attempt
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("durably persisted and loadable"))
        );

        let store = fixture.store.lock().await;
        assert!(
            store
                .get_session(session_id)
                .expect("session lookup succeeds")
                .is_none()
        );
        assert!(
            store
                .load_recursive_live_attempt_by_session_id(session_id)
                .expect("load by session")
                .is_none()
        );
        let attempts = store
            .list_recursive_live_attempts_for_graph(fixture.graph_id)
            .expect("list live attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].summary.session_id, None);
        assert_eq!(
            attempts[0].summary.status,
            RecursiveLiveAttemptStatus::Failed
        );
        assert_eq!(launcher.calls().len(), 1);
        assert_eq!(launcher.model_call_count(), 0);
    }

    #[tokio::test]
    async fn live_executor_duplicate_session_attach_is_recorded_without_second_link() {
        let fixture = live_fixture();
        let session_id = Uuid::new_v4();
        {
            let store = fixture.store.lock().await;
            let existing_attempt_id = RecursiveAttemptId::new();
            store
                .record_recursive_attempt_start_with_executor_kind(
                    fixture.graph_id,
                    fixture.root_id,
                    RecursiveAttemptPhase::Execute,
                    existing_attempt_id,
                    RecursiveExecutionMode::LiveSession,
                )
                .expect("start existing attempt");
            let existing_live = store
                .create_recursive_live_attempt_placeholder(RecursiveLiveAttemptCreate {
                    id: RecursiveLiveAttemptId::new(),
                    graph_id: fixture.graph_id,
                    task_id: fixture.root_id,
                    scheduler_run_id: fixture.run_id,
                    attempt_id: existing_attempt_id,
                    execution_mode: RecursiveExecutionMode::LiveSession,
                    provider: Some(SessionProvider::Codex),
                    model: Some("gpt-5-codex".to_string()),
                    sandbox_kind: Some(SandboxKind::GitWorktree),
                    sandbox_root: Some(PathBuf::from("/tmp/rsi-recursive-live-worktree")),
                    sandbox_branch: Some("rsi/live-adapter".to_string()),
                    sandbox_worktree_id: Some("existing-worktree".to_string()),
                    workflow_execution_id: None,
                    topology_workflow_id: None,
                    max_wall_time_ms: Some(60_000),
                })
                .expect("create existing live attempt");
            store
                .insert_session(&test_session(
                    session_id,
                    SessionProvider::Codex,
                    Some("gpt-5-codex".to_string()),
                ))
                .expect("insert linked session");
            let existing_live = store
                .attach_recursive_live_attempt_session(existing_live.summary.id, session_id)
                .expect("attach existing live attempt");
            assert_eq!(
                existing_live.summary.status,
                RecursiveLiveAttemptStatus::Running
            );
        }

        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::SucceedExistingSession { session_id },
        );
        let executor = RecursiveDagLiveExecutor::enabled_for_test(
            Arc::clone(&fixture.store),
            launcher.clone(),
        );

        let result = executor
            .execute(live_request(&fixture))
            .await
            .expect("duplicate correlation is recorded as launch failure");

        let RecursiveDagLiveExecutionResult::LaunchFailed {
            live_attempt,
            session_id: returned_session_id,
            failure_reason,
            ..
        } = result
        else {
            panic!("expected launch failure result");
        };
        assert_eq!(returned_session_id, Some(session_id));
        assert!(failure_reason.contains("already linked"));
        assert_eq!(
            live_attempt.summary.status,
            RecursiveLiveAttemptStatus::Failed
        );
        assert_eq!(live_attempt.summary.session_id, None);
        assert!(
            live_attempt
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("already linked"))
        );

        let store = fixture.store.lock().await;
        let by_session = store
            .load_recursive_live_attempt_by_session_id(session_id)
            .expect("load by session")
            .expect("existing live attempt remains linked");
        assert_eq!(by_session.summary.task_id, fixture.root_id);
        let attempts = store
            .list_recursive_live_attempts_for_graph(fixture.graph_id)
            .expect("list live attempts");
        assert_eq!(attempts.len(), 2);
        let new_attempt = attempts
            .iter()
            .find(|attempt| attempt.summary.task_id == fixture.task_id)
            .expect("new live attempt");
        assert_eq!(
            new_attempt.summary.status,
            RecursiveLiveAttemptStatus::Failed
        );
        assert_eq!(new_attempt.summary.session_id, None);
        assert_eq!(launcher.calls().len(), 1);
        assert_eq!(launcher.model_call_count(), 0);
    }

    #[tokio::test]
    async fn live_interrupt_attached_attempt_calls_fake_interrupter_once() {
        let fixture = live_fixture();
        let (live_attempt_id, session_id) = create_attached_live_attempt(&fixture).await;
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launcher must not run".to_string()),
        );
        let interrupter = FakeInterrupter::succeed();
        let executor = RecursiveDagLiveExecutor::enabled_for_test_with_interrupter(
            Arc::clone(&fixture.store),
            launcher.clone(),
            Arc::new(interrupter.clone()),
        );
        let cancellation_request_id = request_run_cancellation(&fixture).await;

        let interrupt = executor
            .request_interrupt(interrupt_request(live_attempt_id, cancellation_request_id))
            .await
            .expect("request interrupt");

        assert_eq!(interrupter.calls(), vec![session_id]);
        assert_eq!(interrupt.status, RecursiveLiveInterruptStatus::Interrupted);
        assert_eq!(interrupt.session_id, Some(session_id));
        assert!(interrupt.sent_at.is_some());
        assert!(interrupt.completed_at.is_some());
        assert_eq!(launcher.calls().len(), 0);

        let store = fixture.store.lock().await;
        let live = store
            .load_recursive_live_attempt(live_attempt_id)
            .expect("load live attempt")
            .expect("live attempt");
        assert_eq!(live.summary.status, RecursiveLiveAttemptStatus::Interrupted);
        assert_eq!(
            live.interruption_reason.as_deref(),
            Some("operator requested recursive live interrupt")
        );
        let interrupts = store
            .list_recursive_live_interrupts_for_live_attempt(live_attempt_id)
            .expect("list interrupts");
        assert_eq!(interrupts.len(), 1);
        assert_eq!(interrupts[0].id, interrupt.id);
    }

    #[tokio::test]
    async fn live_interrupt_duplicate_request_is_idempotent_without_second_call() {
        let fixture = live_fixture();
        let (live_attempt_id, session_id) = create_attached_live_attempt(&fixture).await;
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launcher must not run".to_string()),
        );
        let interrupter = FakeInterrupter::succeed();
        let executor = RecursiveDagLiveExecutor::enabled_for_test_with_interrupter(
            Arc::clone(&fixture.store),
            launcher,
            Arc::new(interrupter.clone()),
        );
        let request = interrupt_request(live_attempt_id, request_run_cancellation(&fixture).await);

        let first = executor
            .request_interrupt(request.clone())
            .await
            .expect("first interrupt");
        let second = executor
            .request_interrupt(request)
            .await
            .expect("duplicate interrupt");

        assert_eq!(first.id, second.id);
        assert_eq!(second.status, RecursiveLiveInterruptStatus::Interrupted);
        assert_eq!(interrupter.calls(), vec![session_id]);
        let store = fixture.store.lock().await;
        let interrupts = store
            .list_recursive_live_interrupts_for_live_attempt(live_attempt_id)
            .expect("list interrupts");
        assert_eq!(interrupts.len(), 1);
    }

    #[tokio::test]
    async fn live_interrupt_missing_cancellation_request_id_rejects_before_mutation_or_fake_call() {
        let fixture = live_fixture();
        let (live_attempt_id, _) = create_attached_live_attempt(&fixture).await;
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launcher must not run".to_string()),
        );
        let interrupter = FakeInterrupter::succeed();
        let executor = RecursiveDagLiveExecutor::enabled_for_test_with_interrupter(
            Arc::clone(&fixture.store),
            launcher,
            Arc::new(interrupter.clone()),
        );

        let error = executor
            .request_interrupt(RecursiveDagLiveInterruptRequest {
                live_attempt_id,
                cancellation_request_id: None,
                reason: "operator requested recursive live interrupt".to_string(),
            })
            .await
            .expect_err("missing cancellation request id rejects");

        assert!(error.to_string().contains("cancellation_request_id"));
        assert!(interrupter.calls().is_empty());
        let store = fixture.store.lock().await;
        assert!(
            store
                .list_recursive_live_interrupts_for_live_attempt(live_attempt_id)
                .expect("list interrupts")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn live_interrupt_missing_session_records_failure_without_fake_call() {
        let fixture = live_fixture();
        let store = fixture.store.lock().await;
        let live = store
            .create_recursive_live_attempt_placeholder(RecursiveLiveAttemptCreate {
                id: RecursiveLiveAttemptId::new(),
                graph_id: fixture.graph_id,
                task_id: fixture.task_id,
                scheduler_run_id: fixture.run_id,
                attempt_id: fixture.attempt_id,
                execution_mode: RecursiveExecutionMode::LiveSession,
                provider: Some(SessionProvider::Codex),
                model: Some("gpt-5-codex".to_string()),
                sandbox_kind: Some(SandboxKind::GitWorktree),
                sandbox_root: None,
                sandbox_branch: Some("rsi/live-adapter".to_string()),
                sandbox_worktree_id: None,
                workflow_execution_id: None,
                topology_workflow_id: None,
                max_wall_time_ms: Some(60_000),
            })
            .expect("create live attempt without session");
        drop(store);

        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launcher must not run".to_string()),
        );
        let interrupter = FakeInterrupter::succeed();
        let executor = RecursiveDagLiveExecutor::enabled_for_test_with_interrupter(
            Arc::clone(&fixture.store),
            launcher,
            Arc::new(interrupter.clone()),
        );
        let cancellation_request_id = request_run_cancellation(&fixture).await;

        let interrupt = executor
            .request_interrupt(interrupt_request(live.summary.id, cancellation_request_id))
            .await
            .expect("missing session interrupt is durable failure");

        assert_eq!(interrupt.status, RecursiveLiveInterruptStatus::Failed);
        assert_eq!(interrupt.session_id, None);
        assert!(
            interrupt
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no attached session id"))
        );
        assert!(interrupter.calls().is_empty());
    }

    #[tokio::test]
    async fn live_interrupt_terminal_attempt_is_rejected_without_fake_call() {
        let fixture = live_fixture();
        let (live_attempt_id, _) = create_attached_live_attempt(&fixture).await;
        {
            let store = fixture.store.lock().await;
            store
                .update_recursive_live_attempt_status(
                    live_attempt_id,
                    RecursiveLiveAttemptStatusUpdate {
                        status: RecursiveLiveAttemptStatus::Succeeded,
                        failure_reason: None,
                        interruption_reason: None,
                        cancellation_reason: None,
                        recovery_reason: None,
                        error: None,
                    },
                )
                .expect("mark terminal");
        }

        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launcher must not run".to_string()),
        );
        let interrupter = FakeInterrupter::succeed();
        let executor = RecursiveDagLiveExecutor::enabled_for_test_with_interrupter(
            Arc::clone(&fixture.store),
            launcher,
            Arc::new(interrupter.clone()),
        );
        let cancellation_request_id = request_run_cancellation(&fixture).await;

        let interrupt = executor
            .request_interrupt(interrupt_request(live_attempt_id, cancellation_request_id))
            .await
            .expect("terminal interrupt is rejected durably");

        assert_eq!(interrupt.status, RecursiveLiveInterruptStatus::Rejected);
        assert!(
            interrupt
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("terminal recursive live attempt"))
        );
        assert!(interrupter.calls().is_empty());
        let store = fixture.store.lock().await;
        let live = store
            .load_recursive_live_attempt(live_attempt_id)
            .expect("load live")
            .expect("live attempt");
        assert_eq!(live.summary.status, RecursiveLiveAttemptStatus::Succeeded);
    }

    #[tokio::test]
    async fn live_interrupt_failure_records_durable_failed_interrupt() {
        let fixture = live_fixture();
        let (live_attempt_id, session_id) = create_attached_live_attempt(&fixture).await;
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launcher must not run".to_string()),
        );
        let interrupter = FakeInterrupter::fail("interrupt boundary failed");
        let executor = RecursiveDagLiveExecutor::enabled_for_test_with_interrupter(
            Arc::clone(&fixture.store),
            launcher,
            Arc::new(interrupter.clone()),
        );
        let cancellation_request_id = request_run_cancellation(&fixture).await;

        let interrupt = executor
            .request_interrupt(interrupt_request(live_attempt_id, cancellation_request_id))
            .await
            .expect("failed interrupt is recorded");

        assert_eq!(interrupter.calls(), vec![session_id]);
        assert_eq!(interrupt.status, RecursiveLiveInterruptStatus::Failed);
        assert!(
            interrupt
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("interrupt boundary failed"))
        );
        assert!(interrupt.sent_at.is_some());
        assert!(interrupt.completed_at.is_some());

        let store = fixture.store.lock().await;
        let live = store
            .load_recursive_live_attempt(live_attempt_id)
            .expect("load live")
            .expect("live attempt");
        assert_eq!(live.summary.status, RecursiveLiveAttemptStatus::Running);
        assert!(
            live.error
                .as_deref()
                .is_some_and(|reason| reason.contains("interrupt boundary failed"))
        );
    }

    #[tokio::test]
    async fn live_interrupt_duplicate_after_failed_interrupt_returns_existing_without_second_call()
    {
        let fixture = live_fixture();
        let (live_attempt_id, session_id) = create_attached_live_attempt(&fixture).await;
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launcher must not run".to_string()),
        );
        let interrupter = FakeInterrupter::fail("interrupt boundary failed");
        let executor = RecursiveDagLiveExecutor::enabled_for_test_with_interrupter(
            Arc::clone(&fixture.store),
            launcher,
            Arc::new(interrupter.clone()),
        );
        let request = interrupt_request(live_attempt_id, request_run_cancellation(&fixture).await);

        let first = executor
            .request_interrupt(request.clone())
            .await
            .expect("first interrupt fails durably");
        let second = executor
            .request_interrupt(request)
            .await
            .expect("duplicate failed interrupt returns existing handle");

        assert_eq!(first.id, second.id);
        assert_eq!(second.status, RecursiveLiveInterruptStatus::Failed);
        assert_eq!(second.failure_reason, first.failure_reason);
        assert_eq!(interrupter.calls(), vec![session_id]);

        let store = fixture.store.lock().await;
        let live = store
            .load_recursive_live_attempt(live_attempt_id)
            .expect("load live")
            .expect("live attempt");
        assert_eq!(live.summary.status, RecursiveLiveAttemptStatus::Running);
    }

    #[tokio::test]
    async fn live_interrupt_links_cancellation_request_id() {
        let fixture = live_fixture();
        let (live_attempt_id, session_id) = create_attached_live_attempt(&fixture).await;
        let cancellation = {
            let store = fixture.store.lock().await;
            store
                .request_recursive_scheduler_run_cancellation(
                    fixture.run_id,
                    crate::store::recursive_dag::RecursiveCancellationRequestCreate {
                        source: rsi_common::recursive_dag::RecursiveCancellationRequestSource::TestHarness,
                        reason: "operator cancelled live run".to_string(),
                        requested_by: Some("test".to_string()),
                    },
                )
                .expect("request cancellation")
        };
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launcher must not run".to_string()),
        );
        let interrupter = FakeInterrupter::succeed();
        let executor = RecursiveDagLiveExecutor::enabled_for_test_with_interrupter(
            Arc::clone(&fixture.store),
            launcher,
            Arc::new(interrupter.clone()),
        );

        let interrupt = executor
            .request_interrupt(RecursiveDagLiveInterruptRequest {
                live_attempt_id,
                cancellation_request_id: Some(cancellation.id),
                reason: "applying recursive cancellation request".to_string(),
            })
            .await
            .expect("interrupt cancellation");

        assert_eq!(interrupter.calls(), vec![session_id]);
        assert_eq!(interrupt.status, RecursiveLiveInterruptStatus::Interrupted);
        assert_eq!(interrupt.cancellation_request_id, Some(cancellation.id));

        let store = fixture.store.lock().await;
        let live = store
            .load_recursive_live_attempt(live_attempt_id)
            .expect("load live")
            .expect("live attempt");
        assert_eq!(live.cancellation_request_id, Some(cancellation.id));
        let cancellation = store
            .load_recursive_cancellation_request(cancellation.id)
            .expect("load cancellation")
            .expect("cancellation");
        assert_eq!(
            cancellation.status,
            rsi_common::recursive_dag::RecursiveCancellationRequestStatus::Observed
        );
    }

    #[tokio::test]
    async fn live_interrupt_rejects_task_scoped_cancellation_without_fake_call() {
        let fixture = live_fixture();
        let (live_attempt_id, _) = create_attached_live_attempt(&fixture).await;
        let cancellation = {
            let store = fixture.store.lock().await;
            store
                .request_recursive_task_cancellation(
                    fixture.graph_id,
                    fixture.task_id,
                    RecursiveCancellationRequestCreate {
                        source: RecursiveCancellationRequestSource::TestHarness,
                        reason: "task-scoped cancellation is modeled only".to_string(),
                        requested_by: Some("test".to_string()),
                    },
                )
                .expect("request task cancellation")
        };
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launcher must not run".to_string()),
        );
        let interrupter = FakeInterrupter::succeed();
        let executor = RecursiveDagLiveExecutor::enabled_for_test_with_interrupter(
            Arc::clone(&fixture.store),
            launcher,
            Arc::new(interrupter.clone()),
        );

        let error = executor
            .request_interrupt(RecursiveDagLiveInterruptRequest {
                live_attempt_id,
                cancellation_request_id: Some(cancellation.id),
                reason: "applying recursive task cancellation request".to_string(),
            })
            .await
            .expect_err("task-scoped cancellation must not interrupt live sessions");

        assert!(error.to_string().contains("task-scoped cancellation"));
        assert!(interrupter.calls().is_empty());
        let store = fixture.store.lock().await;
        assert!(
            store
                .list_recursive_live_interrupts_for_live_attempt(live_attempt_id)
                .expect("list interrupts")
                .is_empty()
        );
        let cancellation = store
            .load_recursive_cancellation_request(cancellation.id)
            .expect("load cancellation")
            .expect("cancellation");
        assert_eq!(
            cancellation.status,
            rsi_common::recursive_dag::RecursiveCancellationRequestStatus::Requested
        );
    }

    #[tokio::test]
    async fn disabled_live_executor_rejects_interrupt_before_mutation_or_fake_call() {
        let fixture = live_fixture();
        let (live_attempt_id, _) = create_attached_live_attempt(&fixture).await;
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launcher must not run".to_string()),
        );
        let interrupter = FakeInterrupter::succeed();
        let executor =
            RecursiveDagLiveExecutor::disabled(Arc::clone(&fixture.store), launcher.clone());
        let cancellation_request_id = request_run_cancellation(&fixture).await;

        let error = executor
            .request_interrupt(interrupt_request(live_attempt_id, cancellation_request_id))
            .await
            .expect_err("disabled adapter rejects interrupt");

        assert!(error.to_string().contains("live execution is disabled"));
        assert!(interrupter.calls().is_empty());
        assert!(launcher.calls().is_empty());
        let store = fixture.store.lock().await;
        assert!(
            store
                .list_recursive_live_interrupts_for_live_attempt(live_attempt_id)
                .expect("list interrupts")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn live_executor_heartbeat_methods_wrap_store_without_launching() {
        let fixture = live_fixture();
        let (live_attempt_id, _) = create_attached_live_attempt(&fixture).await;
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launcher must not run".to_string()),
        );
        let executor = RecursiveDagLiveExecutor::enabled_for_test(
            Arc::clone(&fixture.store),
            launcher.clone(),
        );

        let started = executor
            .start_heartbeat(RecursiveDagLiveHeartbeatStartRequest {
                live_attempt_id,
                heartbeat_owner: "rsid-recursive-dag-live:test-adapter".to_string(),
                heartbeat_ttl_seconds: 60,
            })
            .await
            .expect("start heartbeat");
        let token = started.heartbeat_token.clone().expect("heartbeat token");
        assert_eq!(
            started.heartbeat_status,
            RecursiveLiveAttemptHeartbeatStatus::Active
        );

        let renewed = executor
            .heartbeat(RecursiveDagLiveHeartbeatRequest {
                live_attempt_id,
                heartbeat_token: token.clone(),
                heartbeat_ttl_seconds: 60,
            })
            .await
            .expect("heartbeat");
        assert_eq!(renewed.heartbeat_token.as_deref(), Some(token.as_str()));
        assert_eq!(
            renewed.heartbeat_status,
            RecursiveLiveAttemptHeartbeatStatus::Active
        );

        {
            let store = fixture.store.lock().await;
            store
                .update_recursive_live_attempt_status(
                    live_attempt_id,
                    RecursiveLiveAttemptStatusUpdate {
                        status: RecursiveLiveAttemptStatus::Succeeded,
                        failure_reason: None,
                        interruption_reason: None,
                        cancellation_reason: None,
                        recovery_reason: None,
                        error: None,
                    },
                )
                .expect("mark terminal");
        }
        let released = executor
            .release_heartbeat(RecursiveDagLiveHeartbeatReleaseRequest {
                live_attempt_id,
                heartbeat_token: token,
            })
            .await
            .expect("release terminal heartbeat");

        assert_eq!(
            released.heartbeat_status,
            RecursiveLiveAttemptHeartbeatStatus::Released
        );
        assert_eq!(released.heartbeat_token, None);
        assert!(launcher.calls().is_empty());
    }

    #[tokio::test]
    async fn disabled_live_executor_rejects_heartbeat_before_mutation_or_fake_call() {
        let fixture = live_fixture();
        let (live_attempt_id, _) = create_attached_live_attempt(&fixture).await;
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("launcher must not run".to_string()),
        );
        let executor =
            RecursiveDagLiveExecutor::disabled(Arc::clone(&fixture.store), launcher.clone());

        let error = executor
            .start_heartbeat(RecursiveDagLiveHeartbeatStartRequest {
                live_attempt_id,
                heartbeat_owner: "rsid-recursive-dag-live:test-adapter".to_string(),
                heartbeat_ttl_seconds: 60,
            })
            .await
            .expect_err("disabled heartbeat rejects");

        assert!(error.to_string().contains("live execution is disabled"));
        assert!(launcher.calls().is_empty());
        let store = fixture.store.lock().await;
        let heartbeat = store
            .load_recursive_live_attempt_heartbeat(live_attempt_id)
            .expect("load heartbeat")
            .expect("heartbeat state");
        assert_eq!(
            heartbeat.heartbeat_status,
            RecursiveLiveAttemptHeartbeatStatus::Missing
        );
    }

    #[tokio::test]
    async fn disabled_live_executor_rejects_before_mutation_or_launch() {
        let fixture = live_fixture();
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("should not be called".to_string()),
        );
        let executor =
            RecursiveDagLiveExecutor::disabled(Arc::clone(&fixture.store), launcher.clone());

        let error = executor
            .execute(live_request(&fixture))
            .await
            .expect_err("disabled adapter rejects");
        assert!(error.to_string().contains("live execution is disabled"));
        assert!(launcher.calls().is_empty());
        let store = fixture.store.lock().await;
        assert!(
            store
                .list_recursive_live_attempts_for_graph(fixture.graph_id)
                .expect("list live attempts")
                .is_empty()
        );
        assert!(store.load_sessions().expect("load sessions").is_empty());
        assert_eq!(launcher.model_call_count(), 0);
    }

    #[tokio::test]
    async fn session_manager_binding_builds_disabled_driver_and_rejects_before_mutation() {
        let (manager, _db_dir, _sandbox_base) = session_manager_for_live_binding();
        let binding = RecursiveDagLiveSessionManagerBinding::new(Arc::clone(&manager));
        let executor = binding.disabled_executor();
        let mut driver = binding.disabled_driver();
        let fixture = live_fixture();
        let request = live_request(&fixture);
        let graph_id = request.graph_id;

        let error = executor
            .execute(request)
            .await
            .expect_err("disabled production binding rejects");

        assert!(error.to_string().contains("live execution is disabled"));
        let scheduler_fixture = live_scheduler_fixture();
        let driver_error = driver
            .run_request_scoped(live_scheduler_request(&scheduler_fixture))
            .await
            .expect_err("disabled production driver rejects");
        assert!(
            driver_error
                .to_string()
                .contains("live scheduler is disabled")
        );
        let store = binding.store.lock().await;
        for table in [
            "sessions",
            "recursive_scheduler_runs",
            "recursive_live_attempts",
            "recursive_live_interrupts",
            "recursive_live_output_validations",
            "recursive_execution_artifacts",
        ] {
            assert_eq!(table_count(&store, table), 0, "{table} should not mutate");
        }
        assert!(
            store
                .list_recursive_live_attempts_for_graph(graph_id)
                .expect("list live attempts")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn recursive_dag_live_output_committer_captures_final_json_and_commits_state() {
        let (manager, _db_dir, _sandbox_base) = session_manager_for_live_binding();
        let binding = RecursiveDagLiveSessionManagerBinding::new(Arc::clone(&manager));
        let (live_attempt_id, session_id, raw_output) = {
            let store = binding.store.lock().await;
            let live = create_live_output_attempt_in_store(&store);
            let live = attach_session_to_live_output_attempt(
                &store,
                live.summary.id,
                SessionStatus::Running,
            );
            let output = serde_json::to_string_pretty(&live_success_output(&live))
                .expect("serialize live output");
            let wrapped =
                format!("Completed the task.\n\nFinal recursive live output:\n{output}\n\nDone.");
            complete_session_with_assistant_output(
                &store,
                live.summary.session_id.expect("session id"),
                wrapped,
            );
            (
                live.summary.id,
                live.summary.session_id.expect("session id"),
                output,
            )
        };

        let result = binding
            .commit_completed_live_attempt_output(live_attempt_id)
            .await
            .expect("commit completed live output");

        let RecursiveDagLiveOutputCommitResult::Committed { result } = result else {
            panic!("completed session should commit");
        };
        assert_eq!(
            result.validation_result.summary.status,
            RecursiveLiveOutputValidationStatus::Valid
        );
        assert_eq!(
            result.live_attempt.summary.status,
            RecursiveLiveAttemptStatus::Succeeded
        );
        assert_eq!(
            result.recursive_attempt.status,
            RecursiveAttemptStatus::Succeeded
        );
        assert_eq!(result.task.status, RecursiveTaskLifecycleState::Succeeded);
        assert!(result.normalized_output_artifact.is_some());
        assert_eq!(
            result
                .raw_output_artifact
                .as_ref()
                .and_then(|artifact| artifact.content.as_deref()),
            Some(raw_output.as_str())
        );
        assert_eq!(
            result.validation_result.summary.session_id,
            Some(session_id)
        );
        assert!(matches!(
            result.validation_result.parser_source,
            Some(RecursiveLiveOutputParserSource::FinalJsonBlock { .. })
        ));
        assert_eq!(
            validation_count_for_live(&binding.store, live_attempt_id).await,
            1
        );
    }

    #[tokio::test]
    async fn recursive_dag_live_output_committer_does_not_rewrite_wrong_session_id() {
        let (manager, _db_dir, _sandbox_base) = session_manager_for_live_binding();
        let binding = RecursiveDagLiveSessionManagerBinding::new(Arc::clone(&manager));
        let live_attempt_id = {
            let store = binding.store.lock().await;
            let live = create_live_output_attempt_in_store(&store);
            let live = attach_session_to_live_output_attempt(
                &store,
                live.summary.id,
                SessionStatus::Running,
            );
            let mut output = live_success_output(&live);
            let wrong_session_id = Uuid::new_v4();
            assert_ne!(Some(wrong_session_id), live.summary.session_id);
            output.correlation.session_id = Some(wrong_session_id);
            complete_session_with_assistant_output(
                &store,
                live.summary.session_id.expect("session id"),
                serde_json::to_string(&output).expect("serialize output"),
            );
            live.summary.id
        };

        let result = binding
            .commit_completed_live_attempt_output(live_attempt_id)
            .await
            .expect("commit wrong-session live output");

        let RecursiveDagLiveOutputCommitResult::Committed { result } = result else {
            panic!("completed wrong-session output should persist validation");
        };
        assert_eq!(
            result.validation_result.summary.status,
            RecursiveLiveOutputValidationStatus::Invalid
        );
        assert_eq!(
            result
                .validation_result
                .metadata
                .get("session_id_enriched")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
        assert!(result.validation_result.issues.iter().any(|issue| {
            issue.message.contains("session_id does not match")
                && matches!(
                    &issue.location,
                    Some(rsi_common::recursive_dag::RecursiveLiveValidationIssueLocation::OutputPath { path })
                        if path == "/correlation/session_id"
                )
        }));
        assert_eq!(
            result.live_attempt.summary.status,
            RecursiveLiveAttemptStatus::Failed
        );
        assert_eq!(
            validation_count_for_live(&binding.store, live_attempt_id).await,
            1
        );
    }

    #[tokio::test]
    async fn recursive_dag_live_restart_output_recovery_commits_recovery_pending_completed_session()
    {
        let (manager, _db_dir, _sandbox_base) = session_manager_for_live_binding();
        let binding = RecursiveDagLiveSessionManagerBinding::new(Arc::clone(&manager));
        let live_attempt_id = {
            let store = binding.store.lock().await;
            let live = create_live_output_attempt_in_store(&store);
            let live = attach_session_to_live_output_attempt(
                &store,
                live.summary.id,
                SessionStatus::Running,
            );
            let output =
                serde_json::to_string(&live_success_output(&live)).expect("serialize output");
            complete_session_with_assistant_output(
                &store,
                live.summary.session_id.expect("session id"),
                output,
            );
            let recovered = store
                .recover_recursive_live_attempt(live.summary.id)
                .expect("recover completed live attempt");
            assert_eq!(
                recovered.after_status,
                RecursiveLiveAttemptStatus::RecoveryPending
            );
            live.summary.id
        };

        let (checked, committed, deferred) = manager
            .commit_recoverable_recursive_live_outputs_after_restart(RecursiveRecoveryBudget {
                max_graphs: 10,
                time_budget_ms: None,
                source: RecursiveRecoverySource::Startup,
            })
            .await
            .expect("commit recoverable live output");

        assert_eq!(checked, 1);
        assert_eq!(committed, 1);
        assert_eq!(deferred, 0);
        let store = binding.store.lock().await;
        let live = store
            .load_recursive_live_attempt(live_attempt_id)
            .expect("load live")
            .expect("live");
        assert_eq!(live.summary.status, RecursiveLiveAttemptStatus::Succeeded);
        let attempt = store
            .load_recursive_task_attempts(live.summary.graph_id)
            .expect("load attempts")
            .into_iter()
            .find(|attempt| attempt.id == live.summary.attempt_id)
            .expect("recursive attempt");
        assert_eq!(attempt.status, RecursiveAttemptStatus::Succeeded);
    }

    #[tokio::test]
    async fn recursive_dag_live_output_committer_persists_malformed_output_as_failed_attempt() {
        let (manager, _db_dir, _sandbox_base) = session_manager_for_live_binding();
        let binding = RecursiveDagLiveSessionManagerBinding::new(Arc::clone(&manager));
        let live_attempt_id = {
            let store = binding.store.lock().await;
            let live = create_live_output_attempt_in_store(&store);
            let live = attach_session_to_live_output_attempt(
                &store,
                live.summary.id,
                SessionStatus::Running,
            );
            complete_session_with_assistant_output(
                &store,
                live.summary.session_id.expect("session id"),
                "I completed the task but did not emit JSON.".to_string(),
            );
            live.summary.id
        };

        let result = binding
            .commit_completed_live_attempt_output(live_attempt_id)
            .await
            .expect("commit malformed live output");

        let RecursiveDagLiveOutputCommitResult::Committed { result } = result else {
            panic!("completed malformed session should persist validation");
        };
        assert_eq!(
            result.validation_result.summary.status,
            RecursiveLiveOutputValidationStatus::Invalid
        );
        assert_eq!(
            result.live_attempt.summary.status,
            RecursiveLiveAttemptStatus::Failed
        );
        assert_eq!(
            result.recursive_attempt.status,
            RecursiveAttemptStatus::Failed
        );
        assert!(result.normalized_output_artifact.is_none());
        assert_eq!(
            validation_count_for_live(&binding.store, live_attempt_id).await,
            1
        );
    }

    #[tokio::test]
    async fn recursive_dag_live_output_committer_counts_prior_interrupted_retry() {
        let (manager, _db_dir, _sandbox_base) = session_manager_for_live_binding();
        let binding = RecursiveDagLiveSessionManagerBinding::new(Arc::clone(&manager));
        let live_attempt_id = {
            let store = binding.store.lock().await;
            let live = create_live_output_attempt_after_interrupted_retry_in_store(&store);
            let live = attach_session_to_live_output_attempt(
                &store,
                live.summary.id,
                SessionStatus::Running,
            );
            complete_session_with_assistant_output(
                &store,
                live.summary.session_id.expect("session id"),
                "The task is done, but this is not JSON.".to_string(),
            );
            live.summary.id
        };

        let result = binding
            .commit_completed_live_attempt_output(live_attempt_id)
            .await
            .expect("commit malformed live output after interrupted retry");

        let RecursiveDagLiveOutputCommitResult::Committed { result } = result else {
            panic!("completed malformed retry session should persist validation");
        };
        assert_eq!(
            result.validation_result.summary.status,
            RecursiveLiveOutputValidationStatus::Invalid
        );
        let retry_decision = result
            .validation_result
            .summary
            .retry_decision
            .as_ref()
            .expect("retry decision");
        assert_eq!(
            retry_decision.decision,
            RecursiveLiveOutputRetryDecisionKind::NoRetry
        );
        assert_eq!(retry_decision.remaining_task_retries, Some(0));
        assert_eq!(result.recursive_attempt.retry_count, 1);
        assert_eq!(
            result.recursive_attempt.status,
            RecursiveAttemptStatus::Failed
        );
        assert_eq!(result.task.status, RecursiveTaskLifecycleState::Failed);
        assert_eq!(
            result.live_attempt.summary.status,
            RecursiveLiveAttemptStatus::Failed
        );
        assert_eq!(
            validation_count_for_live(&binding.store, live_attempt_id).await,
            1
        );
    }

    #[tokio::test]
    async fn recursive_dag_live_output_committer_rejects_duplicate_commit_without_new_rows() {
        let (manager, _db_dir, _sandbox_base) = session_manager_for_live_binding();
        let binding = RecursiveDagLiveSessionManagerBinding::new(Arc::clone(&manager));
        let live_attempt_id = {
            let store = binding.store.lock().await;
            let live = create_live_output_attempt_in_store(&store);
            let live = attach_session_to_live_output_attempt(
                &store,
                live.summary.id,
                SessionStatus::Running,
            );
            let output =
                serde_json::to_string(&live_success_output(&live)).expect("serialize output");
            complete_session_with_assistant_output(
                &store,
                live.summary.session_id.expect("session id"),
                output,
            );
            live.summary.id
        };

        binding
            .commit_completed_live_attempt_output(live_attempt_id)
            .await
            .expect("first commit");
        let count_after_first = validation_count_for_live(&binding.store, live_attempt_id).await;
        let error = binding
            .commit_completed_live_attempt_output(live_attempt_id)
            .await
            .expect_err("duplicate commit rejects");

        assert!(error.to_string().contains("already committed"));
        assert_eq!(
            validation_count_for_live(&binding.store, live_attempt_id).await,
            count_after_first
        );
    }

    #[tokio::test]
    async fn recursive_dag_live_output_committer_reports_not_ready_for_missing_or_running_session()
    {
        let (manager, _db_dir, _sandbox_base) = session_manager_for_live_binding();
        let binding = RecursiveDagLiveSessionManagerBinding::new(Arc::clone(&manager));
        let (missing_session_live_id, running_session_live_id) = {
            let store = binding.store.lock().await;
            let missing_session_live = create_live_output_attempt_in_store(&store);
            let running_session_live = create_live_output_attempt_in_store(&store);
            let running_session_live = attach_session_to_live_output_attempt(
                &store,
                running_session_live.summary.id,
                SessionStatus::Running,
            );
            (
                missing_session_live.summary.id,
                running_session_live.summary.id,
            )
        };

        let missing = binding
            .commit_completed_live_attempt_output(missing_session_live_id)
            .await
            .expect("missing session is not-ready");
        let RecursiveDagLiveOutputCommitResult::NotReady { reason, .. } = missing else {
            panic!("missing session should be not-ready");
        };
        assert_eq!(
            reason,
            RecursiveDagLiveOutputNotReadyReason::LiveAttemptMissingSession
        );

        let running = binding
            .commit_completed_live_attempt_output(running_session_live_id)
            .await
            .expect("running session is not-ready");
        let RecursiveDagLiveOutputCommitResult::NotReady { reason, .. } = running else {
            panic!("running session should be not-ready");
        };
        assert_eq!(
            reason,
            RecursiveDagLiveOutputNotReadyReason::SessionNotCompleted
        );
        assert_eq!(
            validation_count_for_live(&binding.store, missing_session_live_id).await,
            0
        );
        assert_eq!(
            validation_count_for_live(&binding.store, running_session_live_id).await,
            0
        );
    }

    struct LiveSchedulerFixture {
        _dir: tempfile::TempDir,
        store: Arc<Mutex<Store>>,
        graph_id: RecursiveTaskGraphId,
        root_id: RecursiveTaskId,
    }

    fn live_scheduler_fixture() -> LiveSchedulerFixture {
        let (dir, store) = test_store();
        let graph_id = RecursiveTaskGraphId::new();
        let root_id = RecursiveTaskId::new();
        store
            .create_recursive_live_task_graph(RecursiveTaskGraphCreate {
                graph_id,
                title: "Live scheduler graph".to_string(),
                objective: "Exercise the internal live scheduler driver".to_string(),
                root_task: RecursiveRootTaskCreate {
                    task_id: root_id,
                    title: "Root live task".to_string(),
                    objective: "Launch a fake durable session".to_string(),
                    scope: "One live task".to_string(),
                    acceptance_criteria: vec!["Session correlation is durable".to_string()],
                    scope_units: 1,
                    max_retries: 0,
                },
                project_id: None,
                workflow_id: None,
                topology_id: None,
                parent_session_id: None,
                source_execution_id: None,
                source_eval_id: None,
                max_depth: 2,
                max_fanout: 2,
                max_descendants: 2,
                step_limit: 2,
            })
            .expect("create live graph");
        LiveSchedulerFixture {
            _dir: dir,
            store: Arc::new(Mutex::new(store)),
            graph_id,
            root_id,
        }
    }

    fn live_scheduler_request(
        fixture: &LiveSchedulerFixture,
    ) -> RecursiveDagLiveSchedulerRunRequest {
        RecursiveDagLiveSchedulerRunRequest {
            graph_id: fixture.graph_id,
            max_steps: 1,
            source: RecursiveSchedulerRunSource::TestHarness,
            operator: Some("live-driver-test".to_string()),
            idempotency_key: Some("slice-4-driver".to_string()),
            request_fingerprint: Some("slice-4-driver-fingerprint".to_string()),
            policy_snapshot: serde_json::json!({
                "slice": 5,
                "mode": "internal_test_only",
                "output_validation": "internal_commit_only"
            }),
            provider: Some(SessionProvider::Codex),
            model: Some("gpt-5-codex".to_string()),
            effort: None,
            working_dir: Some(PathBuf::from("/tmp/rsi-recursive-live-driver")),
            sandbox: Some(SandboxSpec {
                kind: Some(SandboxKind::GitWorktree),
                branch: Some("rsi/live-driver".to_string()),
            }),
            sandbox_worktree_id: Some("live-driver-worktree".to_string()),
            workflow_execution_id: Some("live-driver-workflow".to_string()),
            topology_workflow_id: None,
            max_wall_time_ms: Some(30_000),
            budgets: RecursiveDagLiveBudgetPlaceholders {
                max_wall_time_ms: None,
                token_budget: Some(1_000),
                tool_call_budget: Some(8),
                artifact_bytes: Some(64 * 1024),
            },
            approval_policy: RecursiveDagLiveApprovalPolicy {
                policy_name: Some("test".to_string()),
                require_operator_approval: Some(false),
            },
            tool_policy: RecursiveDagLiveToolPolicy {
                allowed_tools: vec!["fake".to_string()],
                denied_tools: Vec::new(),
            },
            sandbox_policy: RecursiveDagLiveSandboxPolicy {
                requested_kind: None,
                requested_branch: None,
                preserve_on_failure: Some(true),
                allowed_write_roots: vec![PathBuf::from("/tmp/rsi-recursive-live-driver")],
            },
            lease_policy: RecursiveSchedulerLeasePolicy {
                lease_owner: "rsid-recursive-dag-live-driver-test".to_string(),
                lease_ttl_seconds: 60,
                max_active_runs: 4,
            },
        }
    }

    #[tokio::test]
    async fn recursive_dag_live_scheduler_driver_creates_live_run_and_attaches_fake_session() {
        let fixture = live_scheduler_fixture();
        let session_id = Uuid::new_v4();
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Succeed {
                session_id,
                provider: SessionProvider::Codex,
                model: Some("gpt-5-codex".to_string()),
            },
        );
        let executor = RecursiveDagLiveExecutor::enabled_for_test(
            Arc::clone(&fixture.store),
            launcher.clone(),
        );
        let mut driver = RecursiveDagLiveSchedulerDriver::new(Arc::clone(&fixture.store), executor);

        let report = driver
            .run_request_scoped(live_scheduler_request(&fixture))
            .await
            .expect("run live scheduler driver");

        assert_eq!(
            report.scheduler_run.executor_mode,
            RecursiveExecutionMode::LiveSession
        );
        assert_eq!(
            report.scheduler_run.status,
            RecursiveSchedulerRunStatus::Failed
        );
        assert_eq!(
            report.scheduler_run.stop_reason,
            Some(RecursiveSchedulerStopReason::ExecutorError)
        );
        assert_eq!(report.step_count, 1);
        assert_eq!(report.selected_task_order, vec![fixture.root_id]);
        assert!(
            report
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("CommitRecursiveLiveAttemptOutput"))
        );
        assert_eq!(report.live_attempts.len(), 1);
        let live_attempt = &report.live_attempts[0];
        assert_eq!(live_attempt.summary.graph_id, fixture.graph_id);
        assert_eq!(live_attempt.summary.task_id, fixture.root_id);
        assert_eq!(
            live_attempt.summary.execution_mode,
            RecursiveExecutionMode::LiveSession
        );
        assert_eq!(
            live_attempt.summary.status,
            RecursiveLiveAttemptStatus::Running
        );
        assert_eq!(live_attempt.summary.session_id, Some(session_id));

        let calls = launcher.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.scheduler_run_id, report.scheduler_run.id);
        assert_eq!(calls[0].0.task_id, fixture.root_id);
        assert_eq!(
            calls[0].0.execution_mode,
            RecursiveExecutionMode::LiveSession
        );
        assert_eq!(launcher.model_call_count(), 0);

        let store = fixture.store.lock().await;
        let run = store
            .load_recursive_scheduler_run(report.scheduler_run.id)
            .expect("load run")
            .expect("run exists");
        assert_eq!(run.status, RecursiveSchedulerRunStatus::Failed);
        let metadata = store
            .load_recursive_scheduler_run_live_metadata(report.scheduler_run.id)
            .expect("load live metadata")
            .expect("live metadata");
        assert_eq!(metadata.idempotency_key.as_deref(), Some("slice-4-driver"));
        assert_eq!(metadata.policy_snapshot["slice"], 5);
        let attempts = store
            .load_recursive_task_attempts(fixture.graph_id)
            .expect("load recursive attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(
            attempts[0].executor_kind,
            RecursiveExecutionMode::LiveSession
        );
        assert_eq!(attempts[0].status, RecursiveAttemptStatus::Running);
        assert_eq!(
            attempts[0].session_id, None,
            "live launch attaches only through recursive_live_attempts"
        );
        assert_eq!(
            store
                .list_recursive_live_attempts_for_graph(fixture.graph_id)
                .expect("list live attempts")
                .len(),
            1
        );
        assert_eq!(store.load_sessions().expect("load sessions").len(), 1);
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM topologies", [], |row| row
                    .get::<_, i64>(0))
                .expect("topology count"),
            0
        );
    }

    #[tokio::test]
    async fn recursive_dag_live_scheduler_driver_replays_existing_live_attempt_without_relaunch() {
        let fixture = live_scheduler_fixture();
        let session_id = Uuid::new_v4();
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Succeed {
                session_id,
                provider: SessionProvider::Codex,
                model: Some("gpt-5-codex".to_string()),
            },
        );
        let executor = RecursiveDagLiveExecutor::enabled_for_test(
            Arc::clone(&fixture.store),
            launcher.clone(),
        );
        let mut driver = RecursiveDagLiveSchedulerDriver::new(Arc::clone(&fixture.store), executor);
        let request = live_scheduler_request(&fixture);
        let report = driver
            .run_request_scoped(request.clone())
            .await
            .expect("initial run");
        let live_attempt = report.live_attempts[0].clone();
        assert_eq!(launcher.calls().len(), 1);

        let replay = driver
            .launch_live_attempt_for_existing_attempt(
                &request,
                &report.scheduler_run,
                fixture.root_id,
                RecursiveAttemptPhase::Execute,
                live_attempt.summary.attempt_id,
            )
            .await
            .expect("idempotent replay loads existing live attempt");

        let RecursiveDagLiveExecutionResult::Launched {
            live_attempt: replayed,
            session_id: replayed_session_id,
            ..
        } = replay
        else {
            panic!("expected replayed live launch correlation");
        };
        assert_eq!(replayed.summary.id, live_attempt.summary.id);
        assert_eq!(replayed.summary.session_id, Some(session_id));
        assert_eq!(replayed_session_id, session_id);
        assert_eq!(
            launcher.calls().len(),
            1,
            "idempotent live attempt load must not launch a second session"
        );

        let store = fixture.store.lock().await;
        assert_eq!(
            store
                .list_recursive_live_attempts_for_graph(fixture.graph_id)
                .expect("list live attempts")
                .len(),
            1
        );
        assert_eq!(store.load_sessions().expect("load sessions").len(), 1);
    }

    #[tokio::test]
    async fn recursive_dag_live_scheduler_driver_launch_failure_is_durable() {
        let fixture = live_scheduler_fixture();
        let launcher = FakeLauncher::new(
            Arc::clone(&fixture.store),
            FakeLauncherResult::Fail("fake launcher failed".to_string()),
        );
        let executor = RecursiveDagLiveExecutor::enabled_for_test(
            Arc::clone(&fixture.store),
            launcher.clone(),
        );
        let mut driver = RecursiveDagLiveSchedulerDriver::new(Arc::clone(&fixture.store), executor);

        let report = driver
            .run_request_scoped(live_scheduler_request(&fixture))
            .await
            .expect("launch failure is durable driver result");

        assert_eq!(
            report.scheduler_run.status,
            RecursiveSchedulerRunStatus::Failed
        );
        assert_eq!(
            report.scheduler_run.stop_reason,
            Some(RecursiveSchedulerStopReason::ExecutorError)
        );
        assert_eq!(report.step_count, 1);
        assert!(
            report
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("fake launcher failed"))
        );
        assert_eq!(report.live_attempts.len(), 1);
        assert_eq!(
            report.live_attempts[0].summary.status,
            RecursiveLiveAttemptStatus::Failed
        );
        assert_eq!(report.live_attempts[0].summary.session_id, None);
        assert_eq!(launcher.calls().len(), 1);
        assert_eq!(launcher.model_call_count(), 0);

        let store = fixture.store.lock().await;
        let run = store
            .load_recursive_scheduler_run(report.scheduler_run.id)
            .expect("load run")
            .expect("run exists");
        assert_eq!(run.status, RecursiveSchedulerRunStatus::Failed);
        assert!(
            run.failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("fake launcher failed"))
        );
        let attempts = store
            .load_recursive_task_attempts(fixture.graph_id)
            .expect("load attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, RecursiveAttemptStatus::Failed);
        assert_eq!(
            attempts[0].executor_kind,
            RecursiveExecutionMode::LiveSession
        );
        let task = store
            .load_recursive_task(fixture.graph_id, fixture.root_id)
            .expect("load task")
            .expect("task exists");
        assert_eq!(task.status, RecursiveTaskLifecycleState::Failed);
        assert_eq!(
            store
                .list_recursive_live_attempts_for_graph(fixture.graph_id)
                .expect("list live attempts")
                .len(),
            1
        );
        assert!(store.load_sessions().expect("load sessions").is_empty());
    }

    #[test]
    fn live_scheduler_driver_is_internal_and_not_session_manager_wired() {
        let source = include_str!("live.rs");
        let start = source
            .find("pub(crate) struct RecursiveDagLiveSchedulerDriver")
            .expect("driver source");
        let end = source[start..]
            .find("/// Session returned by the recursive DAG live launch boundary.")
            .map(|offset| start + offset)
            .expect("launcher contract follows driver");
        let driver_source = &source[start..end];

        assert!(driver_source.contains("prepare_next_scheduler_step"));
        assert!(driver_source.contains("start_recursive_live_scheduler_run_with_lease_policy"));
        assert!(!driver_source.contains("SessionManager"));
        assert!(!driver_source.contains("launch_session_with_durable_store_row"));
        assert!(!driver_source.contains("Command::new"));
        assert!(!driver_source.contains("tokio::process"));
    }

    #[test]
    fn fake_scheduler_source_does_not_reference_live_executor() {
        let source = concat!(
            include_str!("../recursive_dag.rs"),
            include_str!("scheduler_core.rs")
        );
        assert!(!source.contains("RecursiveDagLiveExecutor"));
        assert!(!source.contains("RecursiveDagLiveSessionLauncher"));
        assert!(!source.contains("RecursiveDagLiveSessionInterrupter"));
        assert!(!source.contains("launch_recursive_dag_session"));
        assert!(!source.contains("interrupt_recursive_dag_session"));
        assert!(!source.contains("request_interrupt"));
        assert!(!source.contains("RecursiveDagLiveHeartbeat"));
        assert!(!source.contains("start_recursive_live_attempt_heartbeat"));
        assert!(!source.contains("heartbeat_recursive_live_attempt"));
        assert!(!source.contains("release_recursive_live_attempt_heartbeat"));
        assert!(!source.contains("recover_recursive_live_attempt"));
        assert!(!source.contains("live::"));
    }

    #[test]
    fn production_launcher_contract_is_documented_and_enforced_without_launching_subprocesses() {
        let source = include_str!("live.rs");
        let start = source
            .find("impl RecursiveDagLiveSessionLauncher for SessionManager")
            .expect("production launcher impl");
        let end = source[start..]
            .find("#[allow(dead_code)]\npub(crate) struct RecursiveDagLiveExecutor")
            .map(|offset| start + offset)
            .expect("executor follows production launcher impl");
        let impl_source = &source[start..end];
        let launch_source = include_str!("../session/launch.rs");

        assert!(source.contains("the `sessions` row is durably persisted"));
        assert!(source.contains("loadable through the store read path"));
        assert!(impl_source.contains("self.launch_session_with_durable_store_row(config).await"));
        assert!(launch_source.contains("launch_session_with_durable_store_row"));
        assert!(launch_source.contains("wait_for_durable_session_row"));
        assert!(launch_source.contains("did not become durably persisted and loadable"));
        for forbidden in [
            "Command::new",
            "tokio::process",
            "std::process",
            "ClaudeClient",
            "CodexClient",
            "OpenAiClient",
            ".launch(&config)",
        ] {
            assert!(
                !impl_source.contains(forbidden),
                "production launcher wrapper must not reference {forbidden}"
            );
        }
    }

    #[test]
    fn production_session_manager_binding_builds_only_disabled_live_components() {
        let source = include_str!("live.rs");
        let start = source
            .find("pub(crate) struct RecursiveDagLiveSessionManagerBinding")
            .expect("production binding struct");
        let end = source[start..]
            .find("#[async_trait]\npub(crate) trait RecursiveDagLiveSessionInterrupter")
            .map(|offset| start + offset)
            .expect("interrupter trait follows production binding");
        let binding_source = &source[start..end];

        assert!(binding_source.contains("Arc<SessionManager>"));
        assert!(binding_source.contains("recursive_dag_live_store_handle"));
        assert!(binding_source.contains("RecursiveDagLiveExecutor::disabled"));
        assert!(binding_source.contains("RecursiveDagLiveSchedulerDriver::disabled"));
        assert!(binding_source.contains("launch_session_with_durable_store_row(config)"));
        for forbidden in [
            "Command::new",
            "tokio::process",
            "std::process",
            "ClaudeClient",
            "CodexClient",
            "OpenAiClient",
            ".launch(&config)",
        ] {
            assert!(
                !binding_source.contains(forbidden),
                "production binding must not reference {forbidden}"
            );
        }
    }

    #[test]
    fn production_interrupter_delegates_to_existing_session_interrupt_path() {
        let source = include_str!("live.rs");
        let start = source
            .find("impl RecursiveDagLiveSessionInterrupter for SessionManager")
            .expect("production interrupter impl");
        let end = source[start..]
            .find("struct MissingRecursiveDagLiveSessionInterrupter")
            .map(|offset| start + offset)
            .expect("missing interrupter follows production impl");
        let impl_source = &source[start..end];

        assert!(impl_source.contains("self.interrupt_session(session_id).await"));
        for forbidden in [
            "Command::new",
            "tokio::process",
            "std::process",
            "ProviderProcess",
            ".kill(",
            "SIGKILL",
        ] {
            assert!(
                !impl_source.contains(forbidden),
                "production interrupter wrapper must not reference {forbidden}"
            );
        }
    }

    #[test]
    fn rpc_dispatch_does_not_expose_recursive_live_interrupt() {
        let source = include_str!("../rpc.rs");
        assert!(!source.contains("RecursiveDagLiveInterrupt"));
        assert!(!source.contains("request_interrupt("));
        assert!(!source.contains("request_recursive_live_interrupt"));
        assert!(!source.contains("interrupt_recursive_dag_session"));
        assert!(!source.contains("RecursiveDagLiveHeartbeat"));
        assert!(!source.contains("start_heartbeat("));
        assert!(!source.contains("heartbeat_recursive_live_attempt"));
        assert!(!source.contains("start_recursive_live_attempt_heartbeat"));
        assert!(!source.contains("release_recursive_live_attempt_heartbeat"));
        assert!(!source.contains("recover_recursive_live_attempt"));
        assert!(!source.contains("RecursiveLiveAttemptRecovery"));
    }

    #[test]
    fn fake_scheduler_remains_fake_only_and_does_not_create_live_attempts() {
        let (_dir, store) = test_store();
        let graph_id = RecursiveTaskGraphId::new();
        let root_id = RecursiveTaskId::new();
        store
            .create_recursive_task_graph(RecursiveTaskGraphCreate {
                graph_id,
                title: "Fake graph".to_string(),
                objective: "Stay fake".to_string(),
                root_task: RecursiveRootTaskCreate {
                    task_id: root_id,
                    title: "Root".to_string(),
                    objective: "Run fake".to_string(),
                    scope: "Fake".to_string(),
                    acceptance_criteria: vec!["Fake root succeeds".to_string()],
                    scope_units: 1,
                    max_retries: 0,
                },
                project_id: None,
                workflow_id: None,
                topology_id: None,
                parent_session_id: None,
                source_execution_id: None,
                source_eval_id: None,
                max_depth: 2,
                max_fanout: 2,
                max_descendants: 2,
                step_limit: 2,
            })
            .expect("create graph");
        let executor = RecursiveDagFakeExecutor::new().on_execute(
            root_id,
            RecursiveDagFakeBehavior::direct_success("root-output"),
        );
        let mut scheduler = RecursiveDagScheduler::new(executor);
        let report = scheduler
            .run_until_idle_with_limit(&store, graph_id, 2)
            .expect("run fake scheduler");
        assert_eq!(report.step_count, 1);
        let runs = store
            .list_recursive_scheduler_runs_for_graph(graph_id)
            .expect("list scheduler runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].executor_mode, RecursiveExecutionMode::Fake);
        assert!(
            store
                .list_recursive_live_attempts_for_graph(graph_id)
                .expect("list live attempts")
                .is_empty()
        );
        assert!(store.load_sessions().expect("load sessions").is_empty());
    }
}
