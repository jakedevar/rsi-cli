//! Daemon-internal fake scheduler for persisted recursive DAGs.
//!
//! This module deliberately does not spawn provider sessions or call models. It
//! executes one deterministic fake step at a time over the daemon store APIs so
//! Phase 4 can prove persisted recursive scheduling without introducing a
//! background loop or live LLM execution.

pub(crate) mod live;
mod scheduler_core;
#[cfg(feature = "dev-fixtures")]
pub mod smoke_fixture;

use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::recursive_dag::{
    RecursiveChildTaskCreate, RecursiveDecompositionBatchCreate, RecursiveExecutionArtifactCreate,
    RecursiveSchedulerLeasePolicy, RecursiveSchedulerRunStart,
};
use chrono::{DateTime, Utc};
use rsi_common::recursive_dag::{
    RecursiveAttemptId, RecursiveAttemptPhase, RecursiveAttemptStatus,
    RecursiveCancellationRequestId, RecursiveExecutionArtifactKind, RecursiveExecutionMode,
    RecursiveSchedulerRunId, RecursiveSchedulerRunSource, RecursiveSchedulerRunSummary,
    RecursiveSchedulerStopReason, RecursiveTaskGraphDetail, RecursiveTaskGraphId, RecursiveTaskId,
    RecursiveTaskLifecycleState, RecursiveTaskNode,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

use self::scheduler_core::{
    RecursiveSchedulerRunProgress, RecursiveSchedulerStepPreparation, current_stop_reason,
    deterministic_attempt_id, deterministic_batch_id, failed_attempt_count,
    finish_fake_scheduler_run_with_report, heartbeat_scheduler_run_lease, load_graph,
    next_attempt_no, observe_scheduler_cancellation, phase_key, prepare_next_scheduler_step,
    retry_state_for_phase, saturating_usize_to_u32,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveDagSchedulerStopReason {
    GraphTerminal,
    IdleNoRunnable,
    StepLimitExceeded,
    PartialFailure,
    Quarantined,
    CancellationRequested,
}

impl RecursiveDagSchedulerStopReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GraphTerminal => "graph_terminal",
            Self::IdleNoRunnable => "idle_no_runnable",
            Self::StepLimitExceeded => "step_limit_exceeded",
            Self::PartialFailure => "partial_failure",
            Self::Quarantined => "quarantined",
            Self::CancellationRequested => "cancellation_requested",
        }
    }
}

impl From<RecursiveDagSchedulerStopReason> for RecursiveSchedulerStopReason {
    fn from(value: RecursiveDagSchedulerStopReason) -> Self {
        match value {
            RecursiveDagSchedulerStopReason::GraphTerminal => Self::GraphTerminal,
            RecursiveDagSchedulerStopReason::IdleNoRunnable => Self::IdleNoRunnable,
            RecursiveDagSchedulerStopReason::StepLimitExceeded => Self::StepLimitExceeded,
            RecursiveDagSchedulerStopReason::PartialFailure => Self::PartialFailure,
            RecursiveDagSchedulerStopReason::Quarantined => Self::Quarantined,
            RecursiveDagSchedulerStopReason::CancellationRequested => Self::CancellationRequested,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveDagSchedulerReport {
    pub run_id: RecursiveSchedulerRunId,
    pub graph_id: RecursiveTaskGraphId,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub max_steps: u32,
    pub source: RecursiveSchedulerRunSource,
    pub operator: Option<String>,
    pub stop_reason: RecursiveDagSchedulerStopReason,
    #[serde(default)]
    pub cancellation_request_id: Option<RecursiveCancellationRequestId>,
    pub step_count: u32,
    pub selected_task_order: Vec<RecursiveTaskId>,
    pub task_outcomes: Vec<RecursiveDagSchedulerExecutedStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecursiveDagSchedulerStepResult {
    Executed(RecursiveDagSchedulerExecutedStep),
    Stopped {
        graph_id: RecursiveTaskGraphId,
        stop_reason: RecursiveDagSchedulerStopReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveDagSchedulerExecutedStep {
    pub graph_id: RecursiveTaskGraphId,
    pub step_index: u32,
    pub task_id: RecursiveTaskId,
    pub phase: RecursiveAttemptPhase,
    pub attempt_id: RecursiveAttemptId,
    pub outcome: RecursiveDagSchedulerTaskOutcome,
    pub final_task_status: RecursiveTaskLifecycleState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecursiveDagSchedulerTaskOutcome {
    Succeeded {
        artifact_labels: Vec<String>,
    },
    Decomposed {
        child_task_ids: Vec<RecursiveTaskId>,
    },
    Failed {
        reason: String,
        retryable: bool,
        will_retry: bool,
    },
    Blocked {
        reason: String,
    },
    Cancelled {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveDagFakeArtifact {
    pub label: String,
    pub content: String,
}

impl RecursiveDagFakeArtifact {
    #[must_use]
    pub fn new(label: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecursiveDagFakeDecomposition {
    pub reason_for_decomposition: String,
    pub children: Vec<RecursiveChildTaskCreate>,
    pub integration_strategy: String,
    pub verification_strategy: String,
}

impl RecursiveDagFakeDecomposition {
    #[must_use]
    pub fn new(
        reason_for_decomposition: impl Into<String>,
        children: Vec<RecursiveChildTaskCreate>,
        integration_strategy: impl Into<String>,
        verification_strategy: impl Into<String>,
    ) -> Self {
        Self {
            reason_for_decomposition: reason_for_decomposition.into(),
            children,
            integration_strategy: integration_strategy.into(),
            verification_strategy: verification_strategy.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecursiveDagFakeBehavior {
    DirectSuccess {
        artifact_label: String,
    },
    Decompose(RecursiveDagFakeDecomposition),
    RetryableFailure {
        reason: String,
    },
    RetryableFailureThenSuccess {
        failure_reason: String,
        artifact_label: String,
    },
    PermanentFailure {
        reason: String,
    },
    Blocked {
        reason: String,
    },
    Cancelled {
        reason: String,
    },
    IntegrationSuccess {
        artifact_label: String,
    },
    IntegrationFailure {
        reason: String,
        retryable: bool,
    },
}

impl RecursiveDagFakeBehavior {
    #[must_use]
    pub fn direct_success(artifact_label: impl Into<String>) -> Self {
        Self::DirectSuccess {
            artifact_label: artifact_label.into(),
        }
    }

    #[must_use]
    pub fn integration_success(artifact_label: impl Into<String>) -> Self {
        Self::IntegrationSuccess {
            artifact_label: artifact_label.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RecursiveDagFakeExecutor {
    execute_behaviors: BTreeMap<RecursiveTaskId, RecursiveDagFakeBehavior>,
    integrate_behaviors: BTreeMap<RecursiveTaskId, RecursiveDagFakeBehavior>,
}

impl RecursiveDagFakeExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn on_execute(
        mut self,
        task_id: RecursiveTaskId,
        behavior: RecursiveDagFakeBehavior,
    ) -> Self {
        self.execute_behaviors.insert(task_id, behavior);
        self
    }

    #[must_use]
    pub fn on_integrate(
        mut self,
        task_id: RecursiveTaskId,
        behavior: RecursiveDagFakeBehavior,
    ) -> Self {
        self.integrate_behaviors.insert(task_id, behavior);
        self
    }
}

pub trait RecursiveDagExecutor {
    fn execute(
        &mut self,
        detail: &RecursiveTaskGraphDetail,
        task: &RecursiveTaskNode,
        phase: RecursiveAttemptPhase,
    ) -> RecursiveDagExecutorOutcome;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecursiveDagExecutorOutcome {
    Succeeded {
        artifacts: Vec<RecursiveDagFakeArtifact>,
    },
    Decomposed(RecursiveDagFakeDecomposition),
    Failed {
        reason: String,
        retryable: bool,
    },
    Blocked {
        reason: String,
    },
    Cancelled {
        reason: String,
    },
}

impl RecursiveDagExecutor for RecursiveDagFakeExecutor {
    fn execute(
        &mut self,
        detail: &RecursiveTaskGraphDetail,
        task: &RecursiveTaskNode,
        phase: RecursiveAttemptPhase,
    ) -> RecursiveDagExecutorOutcome {
        let behavior = match phase {
            RecursiveAttemptPhase::Execute => self.execute_behaviors.get(&task.id),
            RecursiveAttemptPhase::Integrate => self.integrate_behaviors.get(&task.id),
        };

        let default_integrate = RecursiveDagFakeBehavior::IntegrationSuccess {
            artifact_label: format!("integrated:{}", task.id),
        };
        let missing_execute = RecursiveDagFakeBehavior::Blocked {
            reason: format!("no fake execute behavior for task {}", task.id),
        };
        let selected = match (phase, behavior) {
            (_, Some(behavior)) => behavior,
            (RecursiveAttemptPhase::Integrate, None) => &default_integrate,
            (RecursiveAttemptPhase::Execute, None) => &missing_execute,
        };

        match selected {
            RecursiveDagFakeBehavior::DirectSuccess { artifact_label }
            | RecursiveDagFakeBehavior::IntegrationSuccess { artifact_label } => {
                RecursiveDagExecutorOutcome::Succeeded {
                    artifacts: vec![RecursiveDagFakeArtifact::new(
                        artifact_label.clone(),
                        format!("task {} {:?} succeeded", task.id, phase),
                    )],
                }
            }
            RecursiveDagFakeBehavior::Decompose(decomposition) => {
                RecursiveDagExecutorOutcome::Decomposed(decomposition.clone())
            }
            RecursiveDagFakeBehavior::RetryableFailure { reason } => {
                RecursiveDagExecutorOutcome::Failed {
                    reason: reason.clone(),
                    retryable: true,
                }
            }
            RecursiveDagFakeBehavior::RetryableFailureThenSuccess {
                failure_reason,
                artifact_label,
            } => {
                if failed_attempt_count(detail, task.id, phase) == 0 {
                    RecursiveDagExecutorOutcome::Failed {
                        reason: failure_reason.clone(),
                        retryable: true,
                    }
                } else {
                    RecursiveDagExecutorOutcome::Succeeded {
                        artifacts: vec![RecursiveDagFakeArtifact::new(
                            artifact_label.clone(),
                            format!("task {} retry succeeded", task.id),
                        )],
                    }
                }
            }
            RecursiveDagFakeBehavior::PermanentFailure { reason } => {
                RecursiveDagExecutorOutcome::Failed {
                    reason: reason.clone(),
                    retryable: false,
                }
            }
            RecursiveDagFakeBehavior::Blocked { reason } => RecursiveDagExecutorOutcome::Blocked {
                reason: reason.clone(),
            },
            RecursiveDagFakeBehavior::Cancelled { reason } => {
                RecursiveDagExecutorOutcome::Cancelled {
                    reason: reason.clone(),
                }
            }
            RecursiveDagFakeBehavior::IntegrationFailure { reason, retryable } => {
                RecursiveDagExecutorOutcome::Failed {
                    reason: reason.clone(),
                    retryable: *retryable,
                }
            }
        }
    }
}

pub struct RecursiveDagScheduler<E = RecursiveDagFakeExecutor> {
    executor: E,
}

impl<E> RecursiveDagScheduler<E>
where
    E: RecursiveDagExecutor,
{
    #[must_use]
    pub const fn new(executor: E) -> Self {
        Self { executor }
    }

    pub fn run_one_step(
        &mut self,
        store: &Store,
        graph_id: RecursiveTaskGraphId,
    ) -> Result<RecursiveDagSchedulerStepResult> {
        self.run_one_step_with_index(store, graph_id, 0)
    }

    pub fn run_until_idle(
        &mut self,
        store: &Store,
        graph_id: RecursiveTaskGraphId,
    ) -> Result<RecursiveDagSchedulerReport> {
        let detail = load_graph(store, graph_id)?;
        self.run_until_idle_with_limit(store, graph_id, detail.graph.step_limit)
    }

    pub fn run_until_idle_with_limit(
        &mut self,
        store: &Store,
        graph_id: RecursiveTaskGraphId,
        max_steps: u32,
    ) -> Result<RecursiveDagSchedulerReport> {
        self.run_until_idle_with_options(
            store,
            graph_id,
            max_steps,
            RecursiveSchedulerRunSource::TestHarness,
            Some("test".to_string()),
            RecursiveSchedulerLeasePolicy::default(),
        )
    }

    pub fn run_until_idle_with_options(
        &mut self,
        store: &Store,
        graph_id: RecursiveTaskGraphId,
        max_steps: u32,
        source: RecursiveSchedulerRunSource,
        operator: Option<String>,
        lease_policy: RecursiveSchedulerLeasePolicy,
    ) -> Result<RecursiveDagSchedulerReport> {
        let mut run = store.start_recursive_scheduler_run_with_lease_policy(
            RecursiveSchedulerRunStart {
                graph_id,
                max_steps,
                source,
                operator,
                executor_mode: RecursiveExecutionMode::Fake,
            },
            lease_policy,
        )?;
        let mut progress = RecursiveSchedulerRunProgress::default();

        let result: Result<RecursiveDagSchedulerReport> = (|| {
            run = heartbeat_scheduler_run_lease(store, &run)?;
            if let Some(cancellation) = observe_scheduler_cancellation(store, &run)? {
                let report = progress.build_report(
                    &run,
                    RecursiveDagSchedulerStopReason::CancellationRequested,
                    Some(cancellation.id),
                );
                finish_fake_scheduler_run_with_report(store, &run, &report)?;
                return Ok(report);
            }

            for step_index in 0..max_steps {
                run = heartbeat_scheduler_run_lease(store, &run)?;
                if let Some(cancellation) = observe_scheduler_cancellation(store, &run)? {
                    let report = progress.build_report(
                        &run,
                        RecursiveDagSchedulerStopReason::CancellationRequested,
                        Some(cancellation.id),
                    );
                    finish_fake_scheduler_run_with_report(store, &run, &report)?;
                    return Ok(report);
                }

                match self.run_one_step_for_run_with_index(store, graph_id, &run, step_index)? {
                    RecursiveDagSchedulerStepResult::Executed(step) => {
                        progress.record_executed_step(step);
                        run = store.update_recursive_scheduler_run_step_count(
                            run.id,
                            progress.step_count(),
                        )?;
                        run = heartbeat_scheduler_run_lease(store, &run)?;
                        if let Some(cancellation) = observe_scheduler_cancellation(store, &run)? {
                            let report = progress.build_report(
                                &run,
                                RecursiveDagSchedulerStopReason::CancellationRequested,
                                Some(cancellation.id),
                            );
                            finish_fake_scheduler_run_with_report(store, &run, &report)?;
                            return Ok(report);
                        }
                    }
                    RecursiveDagSchedulerStepResult::Stopped { stop_reason, .. } => {
                        let cancellation_request_id = if stop_reason
                            == RecursiveDagSchedulerStopReason::CancellationRequested
                        {
                            store
                                .next_pending_recursive_cancellation_for_run(graph_id, run.id)?
                                .map(|request| request.id)
                        } else {
                            None
                        };
                        let report =
                            progress.build_report(&run, stop_reason, cancellation_request_id);
                        finish_fake_scheduler_run_with_report(store, &run, &report)?;
                        return Ok(report);
                    }
                }
            }

            let stop_reason = current_stop_reason(store, graph_id)?
                .unwrap_or(RecursiveDagSchedulerStopReason::StepLimitExceeded);
            let report = progress.build_report(&run, stop_reason, None);
            finish_fake_scheduler_run_with_report(store, &run, &report)?;
            Ok(report)
        })();

        match result {
            Ok(report) => Ok(report),
            Err(error) => {
                let failure_reason = error.to_string();
                if let Err(mark_failed_error) = store.fail_recursive_scheduler_run(
                    run.id,
                    progress.step_count(),
                    failure_reason,
                ) {
                    tracing::warn!(
                        run_id = %run.id,
                        error = %mark_failed_error,
                        "failed to mark recursive scheduler run failed after scheduler error"
                    );
                }
                Err(error)
            }
        }
    }

    fn run_one_step_with_index(
        &mut self,
        store: &Store,
        graph_id: RecursiveTaskGraphId,
        step_index: u32,
    ) -> Result<RecursiveDagSchedulerStepResult> {
        self.run_one_step_with_index_inner(store, graph_id, None, step_index)
    }

    fn run_one_step_for_run_with_index(
        &mut self,
        store: &Store,
        graph_id: RecursiveTaskGraphId,
        run: &RecursiveSchedulerRunSummary,
        step_index: u32,
    ) -> Result<RecursiveDagSchedulerStepResult> {
        self.run_one_step_with_index_inner(store, graph_id, Some(run), step_index)
    }

    fn run_one_step_with_index_inner(
        &mut self,
        store: &Store,
        graph_id: RecursiveTaskGraphId,
        run: Option<&RecursiveSchedulerRunSummary>,
        step_index: u32,
    ) -> Result<RecursiveDagSchedulerStepResult> {
        match prepare_next_scheduler_step(store, graph_id, run)? {
            RecursiveSchedulerStepPreparation::Runnable(runnable) => {
                let step = self.execute_selected_task(
                    store,
                    graph_id,
                    runnable.task_id,
                    runnable.phase,
                    step_index,
                )?;
                Ok(RecursiveDagSchedulerStepResult::Executed(step))
            }
            RecursiveSchedulerStepPreparation::Stopped {
                graph_id,
                stop_reason,
            } => Ok(RecursiveDagSchedulerStepResult::Stopped {
                graph_id,
                stop_reason,
            }),
        }
    }

    fn execute_selected_task(
        &mut self,
        store: &Store,
        graph_id: RecursiveTaskGraphId,
        task_id: RecursiveTaskId,
        phase: RecursiveAttemptPhase,
        step_index: u32,
    ) -> Result<RecursiveDagSchedulerExecutedStep> {
        let before = load_graph(store, graph_id)?;
        let attempt_no = next_attempt_no(&before, task_id, phase);
        let attempt_id = deterministic_attempt_id(graph_id, task_id, phase, attempt_no);
        store.record_recursive_attempt_start(graph_id, task_id, phase, attempt_id)?;

        match phase {
            RecursiveAttemptPhase::Execute => {
                store.transition_recursive_task_state(
                    graph_id,
                    task_id,
                    RecursiveTaskLifecycleState::Planning,
                    Some("recursive DAG scheduler: planning fake execute".to_string()),
                )?;
                store.transition_recursive_task_state(
                    graph_id,
                    task_id,
                    RecursiveTaskLifecycleState::Running,
                    Some("recursive DAG scheduler: running fake execute".to_string()),
                )?;
            }
            RecursiveAttemptPhase::Integrate => {
                store.transition_recursive_task_state(
                    graph_id,
                    task_id,
                    RecursiveTaskLifecycleState::Integrating,
                    Some("recursive DAG scheduler: integrating child results".to_string()),
                )?;
            }
        }

        let running_detail = load_graph(store, graph_id)?;
        let task = running_detail
            .nodes
            .iter()
            .find(|node| node.id == task_id)
            .ok_or_else(|| DaemonError::Store(format!("recursive task not found: {task_id}")))?;
        let outcome = self.executor.execute(&running_detail, task, phase);

        let scheduler_outcome = match outcome {
            RecursiveDagExecutorOutcome::Succeeded { artifacts } => {
                store.transition_recursive_task_state(
                    graph_id,
                    task_id,
                    RecursiveTaskLifecycleState::Verifying,
                    Some("recursive DAG scheduler: verifying fake result".to_string()),
                )?;
                store.record_recursive_attempt_finish(
                    graph_id,
                    attempt_id,
                    RecursiveAttemptStatus::Succeeded,
                    None,
                    None,
                    Some(RecursiveTaskLifecycleState::Succeeded),
                    Some("recursive DAG scheduler: fake execution succeeded".to_string()),
                )?;
                let artifact_labels = artifacts
                    .iter()
                    .map(|artifact| artifact.label.clone())
                    .collect::<Vec<_>>();
                write_fake_artifacts(store, graph_id, task_id, attempt_id, phase, artifacts)?;
                RecursiveDagSchedulerTaskOutcome::Succeeded { artifact_labels }
            }
            RecursiveDagExecutorOutcome::Decomposed(decomposition) => {
                let child_task_ids = decomposition
                    .children
                    .iter()
                    .map(|child| child.task_id)
                    .collect::<Vec<_>>();
                let batch_id = deterministic_batch_id(
                    graph_id,
                    task_id,
                    saturating_usize_to_u32(
                        running_detail.injection_batches.len().saturating_add(1),
                    ),
                );
                store.insert_recursive_decomposition_batch(
                    graph_id,
                    RecursiveDecompositionBatchCreate {
                        batch_id,
                        parent_task_id: task_id,
                        attempt_id: Some(attempt_id),
                        reason_for_decomposition: decomposition.reason_for_decomposition,
                        children: decomposition.children,
                        integration_strategy: decomposition.integration_strategy,
                        verification_strategy: decomposition.verification_strategy,
                    },
                )?;
                store.record_recursive_attempt_finish(
                    graph_id,
                    attempt_id,
                    RecursiveAttemptStatus::Decomposed,
                    None,
                    None,
                    Some(RecursiveTaskLifecycleState::Decomposed),
                    Some("recursive DAG scheduler: fake decomposition committed".to_string()),
                )?;
                store.transition_recursive_task_state(
                    graph_id,
                    task_id,
                    RecursiveTaskLifecycleState::BlockedOnChildren,
                    Some("recursive DAG scheduler: waiting on decomposed children".to_string()),
                )?;
                RecursiveDagSchedulerTaskOutcome::Decomposed { child_task_ids }
            }
            RecursiveDagExecutorOutcome::Failed { reason, retryable } => {
                let failed_after_this =
                    failed_attempt_count(&running_detail, task_id, phase).saturating_add(1);
                let max_retries = task.max_retries;
                let will_retry = retryable && failed_after_this <= max_retries;
                let next_state = if will_retry {
                    retry_state_for_phase(phase)
                } else {
                    RecursiveTaskLifecycleState::Failed
                };
                store.record_recursive_attempt_finish(
                    graph_id,
                    attempt_id,
                    RecursiveAttemptStatus::Failed,
                    Some(reason.clone()),
                    None,
                    Some(next_state),
                    Some(reason.clone()),
                )?;
                RecursiveDagSchedulerTaskOutcome::Failed {
                    reason,
                    retryable,
                    will_retry,
                }
            }
            RecursiveDagExecutorOutcome::Blocked { reason } => {
                store.record_recursive_attempt_finish(
                    graph_id,
                    attempt_id,
                    RecursiveAttemptStatus::Blocked,
                    None,
                    Some(reason.clone()),
                    Some(RecursiveTaskLifecycleState::Blocked),
                    Some(reason.clone()),
                )?;
                RecursiveDagSchedulerTaskOutcome::Blocked { reason }
            }
            RecursiveDagExecutorOutcome::Cancelled { reason } => {
                store.record_recursive_attempt_finish(
                    graph_id,
                    attempt_id,
                    RecursiveAttemptStatus::Cancelled,
                    Some(reason.clone()),
                    None,
                    Some(RecursiveTaskLifecycleState::Cancelled),
                    Some(reason.clone()),
                )?;
                RecursiveDagSchedulerTaskOutcome::Cancelled { reason }
            }
        };

        let final_task_status = store
            .load_recursive_task(graph_id, task_id)?
            .ok_or_else(|| DaemonError::Store(format!("recursive task not found: {task_id}")))?
            .status;

        Ok(RecursiveDagSchedulerExecutedStep {
            graph_id,
            step_index,
            task_id,
            phase,
            attempt_id,
            outcome: scheduler_outcome,
            final_task_status,
        })
    }
}

fn write_fake_artifacts(
    store: &Store,
    graph_id: RecursiveTaskGraphId,
    task_id: RecursiveTaskId,
    attempt_id: RecursiveAttemptId,
    phase: RecursiveAttemptPhase,
    artifacts: Vec<RecursiveDagFakeArtifact>,
) -> Result<()> {
    let creates = artifacts
        .into_iter()
        .map(|artifact| RecursiveExecutionArtifactCreate {
            task_id,
            attempt_id: Some(attempt_id),
            kind: RecursiveExecutionArtifactKind::Inline,
            label: artifact.label,
            content: Some(artifact.content),
            uri: None,
            metadata: json!({
                "executor": "recursive_dag_fake",
                "phase": phase_key(phase),
            }),
        })
        .collect();
    store.record_recursive_execution_artifacts(graph_id, creates)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::too_many_lines, clippy::unwrap_used)]

    use super::*;
    use crate::store::recursive_dag::{
        RecursiveCancellationRequestCreate, RecursiveRootTaskCreate, RecursiveTaskGraphCreate,
    };
    use rsi_common::{
        RecursiveCancellationRequestSource, RecursiveCancellationRequestStatus,
        RecursiveExecutionArtifact, RecursiveGraphStatus, RecursiveInjectionBatchId,
        RecursiveSchedulerRunStatus,
    };
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use uuid::Uuid;

    struct TestDb {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl TestDb {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("test.db");
            Self { _dir: dir, path }
        }

        fn open(&self) -> Store {
            Store::open(&self.path).expect("open store")
        }
    }

    fn gid(value: u128) -> RecursiveTaskGraphId {
        RecursiveTaskGraphId(Uuid::from_u128(value))
    }

    fn tid(value: u128) -> RecursiveTaskId {
        RecursiveTaskId(Uuid::from_u128(value))
    }

    fn aid(value: u128) -> RecursiveAttemptId {
        RecursiveAttemptId(Uuid::from_u128(value))
    }

    fn root_create(task_id: RecursiveTaskId, scope_units: u32) -> RecursiveRootTaskCreate {
        RecursiveRootTaskCreate {
            task_id,
            title: format!("Task {task_id}"),
            objective: format!("Implement {task_id}"),
            scope: "root scope".to_string(),
            acceptance_criteria: vec!["done".to_string()],
            scope_units,
            max_retries: 2,
        }
    }

    fn graph_create(
        graph_id: RecursiveTaskGraphId,
        root_id: RecursiveTaskId,
        step_limit: u32,
    ) -> RecursiveTaskGraphCreate {
        RecursiveTaskGraphCreate {
            graph_id,
            title: "Recursive test graph".to_string(),
            objective: "Exercise persisted scheduler".to_string(),
            root_task: root_create(root_id, 100),
            project_id: None,
            workflow_id: None,
            topology_id: None,
            parent_session_id: None,
            source_execution_id: None,
            source_eval_id: None,
            max_depth: 4,
            max_fanout: 8,
            max_descendants: 16,
            step_limit,
        }
    }

    fn create_graph(
        store: &Store,
        graph_id: RecursiveTaskGraphId,
        root_id: RecursiveTaskId,
        step_limit: u32,
    ) {
        store
            .create_recursive_task_graph(graph_create(graph_id, root_id, step_limit))
            .expect("create recursive graph");
    }

    fn child(
        task_id: RecursiveTaskId,
        scope_units: u32,
        max_retries: u32,
        dependencies: Vec<RecursiveTaskId>,
    ) -> RecursiveChildTaskCreate {
        RecursiveChildTaskCreate {
            task_id,
            title: format!("Task {task_id}"),
            objective: format!("Implement {task_id}"),
            scope: format!("Scope for {task_id}"),
            acceptance_criteria: vec!["done".to_string()],
            scope_units,
            max_retries,
            dependencies,
        }
    }

    fn decomp(children: Vec<RecursiveChildTaskCreate>) -> RecursiveDagFakeDecomposition {
        RecursiveDagFakeDecomposition::new(
            "split into smaller fake tasks",
            children,
            "integrate child artifacts in deterministic order",
            "verify child acceptance criteria",
        )
    }

    fn node_status(
        detail: &RecursiveTaskGraphDetail,
        task_id: RecursiveTaskId,
    ) -> RecursiveTaskLifecycleState {
        detail
            .nodes
            .iter()
            .find(|node| node.id == task_id)
            .expect("task exists")
            .status
    }

    fn transition_event_id(
        detail: &RecursiveTaskGraphDetail,
        task_id: RecursiveTaskId,
        status: RecursiveTaskLifecycleState,
    ) -> i64 {
        detail
            .lifecycle_events
            .iter()
            .find(|event| event.task_id == task_id && event.to_status == status)
            .expect("transition event exists")
            .id
    }

    fn scheduler_report_artifact_row(
        detail: &RecursiveTaskGraphDetail,
    ) -> &RecursiveExecutionArtifact {
        detail
            .artifacts
            .iter()
            .rev()
            .find(|artifact| artifact.label == "scheduler-report")
            .expect("scheduler report artifact exists")
    }

    fn scheduler_report_artifact(detail: &RecursiveTaskGraphDetail) -> serde_json::Value {
        let artifact = scheduler_report_artifact_row(detail);
        serde_json::from_str(artifact.content.as_deref().expect("report content"))
            .expect("report JSON")
    }

    struct RequestGraphCancellationAndDecompose {
        db_path: PathBuf,
        decomposition: RecursiveDagFakeDecomposition,
        requested: bool,
    }

    impl RecursiveDagExecutor for RequestGraphCancellationAndDecompose {
        fn execute(
            &mut self,
            detail: &RecursiveTaskGraphDetail,
            _task: &RecursiveTaskNode,
            phase: RecursiveAttemptPhase,
        ) -> RecursiveDagExecutorOutcome {
            assert_eq!(phase, RecursiveAttemptPhase::Execute);
            if !self.requested {
                self.requested = true;
                let store = Store::open(&self.db_path).expect("open store from fake executor");
                store
                    .request_recursive_graph_cancellation(
                        detail.graph.id,
                        RecursiveCancellationRequestCreate {
                            reason: "cancel after committed fake step".to_string(),
                            requested_by: Some("test".to_string()),
                            source: RecursiveCancellationRequestSource::TestHarness,
                        },
                    )
                    .expect("request graph cancellation");
            }
            RecursiveDagExecutorOutcome::Decomposed(self.decomposition.clone())
        }
    }

    struct RequestCurrentRunCancellationAndDecompose {
        db_path: PathBuf,
        decomposition: RecursiveDagFakeDecomposition,
        requested: bool,
    }

    impl RecursiveDagExecutor for RequestCurrentRunCancellationAndDecompose {
        fn execute(
            &mut self,
            detail: &RecursiveTaskGraphDetail,
            _task: &RecursiveTaskNode,
            phase: RecursiveAttemptPhase,
        ) -> RecursiveDagExecutorOutcome {
            assert_eq!(phase, RecursiveAttemptPhase::Execute);
            if !self.requested {
                self.requested = true;
                let store = Store::open(&self.db_path).expect("open store from fake executor");
                let active_run = store
                    .list_recursive_scheduler_runs_for_graph(detail.graph.id)
                    .expect("list scheduler runs")
                    .into_iter()
                    .find(|run| run.status == RecursiveSchedulerRunStatus::Running)
                    .expect("active scheduler run");
                store
                    .request_recursive_scheduler_run_cancellation(
                        active_run.id,
                        RecursiveCancellationRequestCreate {
                            reason: "cancel this run after step".to_string(),
                            requested_by: Some("test".to_string()),
                            source: RecursiveCancellationRequestSource::TestHarness,
                        },
                    )
                    .expect("request run cancellation");
            }
            RecursiveDagExecutorOutcome::Decomposed(self.decomposition.clone())
        }
    }

    #[test]
    fn recursive_dag_persisted_vertical_slice_decomposes_retries_integrates() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(100);
        let root = tid(1);
        let a = tid(2);
        let b = tid(3);
        let b1 = tid(4);
        let b2 = tid(5);
        create_graph(&store, graph, root, 64);

        let fake = RecursiveDagFakeExecutor::new()
            .on_execute(
                root,
                RecursiveDagFakeBehavior::Decompose(decomp(vec![
                    child(a, 25, 0, vec![]),
                    child(b, 60, 0, vec![]),
                ])),
            )
            .on_integrate(
                root,
                RecursiveDagFakeBehavior::integration_success("root-integrated"),
            )
            .on_execute(a, RecursiveDagFakeBehavior::direct_success("A-artifact"))
            .on_execute(
                b,
                RecursiveDagFakeBehavior::Decompose(decomp(vec![
                    child(b1, 20, 0, vec![]),
                    child(b2, 15, 1, vec![]),
                ])),
            )
            .on_integrate(
                b,
                RecursiveDagFakeBehavior::integration_success("B-integrated"),
            )
            .on_execute(b1, RecursiveDagFakeBehavior::direct_success("B1-artifact"))
            .on_execute(
                b2,
                RecursiveDagFakeBehavior::RetryableFailureThenSuccess {
                    failure_reason: "deterministic first failure".to_string(),
                    artifact_label: "B2-artifact".to_string(),
                },
            );
        let mut scheduler = RecursiveDagScheduler::new(fake);

        let report = scheduler
            .run_until_idle(&store, graph)
            .expect("scheduler run");

        assert_eq!(
            report.stop_reason,
            RecursiveDagSchedulerStopReason::GraphTerminal
        );
        assert_eq!(
            report.selected_task_order,
            vec![root, a, b, b1, b2, b2, b, root]
        );

        let detail = store
            .get_recursive_task_graph(graph)
            .expect("read graph")
            .expect("graph exists");
        assert_eq!(detail.graph.status, RecursiveGraphStatus::Terminal);
        assert_eq!(
            detail.graph.last_stop_reason.as_deref(),
            Some("graph_terminal")
        );
        for task_id in [root, a, b, b1, b2] {
            assert_eq!(
                node_status(&detail, task_id),
                RecursiveTaskLifecycleState::Succeeded
            );
        }

        assert_eq!(detail.injection_batches.len(), 2);
        assert_eq!(detail.injection_batches[0].parent_task_id, root);
        assert_eq!(detail.injection_batches[0].child_task_ids, vec![a, b]);
        assert_eq!(detail.injection_batches[1].parent_task_id, b);
        assert_eq!(detail.injection_batches[1].child_task_ids, vec![b1, b2]);

        let b2_attempts: Vec<_> = detail
            .attempts
            .iter()
            .filter(|attempt| {
                attempt.task_id == b2 && attempt.phase == RecursiveAttemptPhase::Execute
            })
            .collect();
        assert_eq!(b2_attempts.len(), 2);
        assert_eq!(b2_attempts[0].status, RecursiveAttemptStatus::Failed);
        assert_eq!(b2_attempts[0].retry_count, 0);
        assert_eq!(b2_attempts[1].status, RecursiveAttemptStatus::Succeeded);
        assert_eq!(b2_attempts[1].retry_count, 1);

        assert!(detail.lifecycle_events.iter().any(|event| {
            event.task_id == root
                && event.to_status == RecursiveTaskLifecycleState::BlockedOnChildren
        }));
        assert!(detail.lifecycle_events.iter().any(|event| {
            event.task_id == b && event.to_status == RecursiveTaskLifecycleState::BlockedOnChildren
        }));

        let b_done = transition_event_id(&detail, b, RecursiveTaskLifecycleState::Succeeded);
        let b1_done = transition_event_id(&detail, b1, RecursiveTaskLifecycleState::Succeeded);
        let b2_done = transition_event_id(&detail, b2, RecursiveTaskLifecycleState::Succeeded);
        assert!(b_done > b1_done);
        assert!(b_done > b2_done);

        let root_done = transition_event_id(&detail, root, RecursiveTaskLifecycleState::Succeeded);
        let a_done = transition_event_id(&detail, a, RecursiveTaskLifecycleState::Succeeded);
        assert!(root_done > a_done);
        assert!(root_done > b_done);

        let labels: BTreeSet<_> = detail
            .artifacts
            .iter()
            .map(|artifact| artifact.label.as_str())
            .collect();
        for label in [
            "A-artifact",
            "B1-artifact",
            "B2-artifact",
            "B-integrated",
            "root-integrated",
            "scheduler-report",
        ] {
            assert!(labels.contains(label), "missing artifact label {label}");
        }

        let report_json = scheduler_report_artifact(&detail);
        assert_eq!(report_json["stop_reason"], "graph_terminal");
        assert_eq!(report_json["step_count"], 8);
        assert_eq!(report_json["max_steps"], 64);
        assert_eq!(report_json["source"], "test_harness");
        let runs = store
            .list_recursive_scheduler_runs_for_graph(graph)
            .expect("list scheduler runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RecursiveSchedulerRunStatus::Completed);
        assert_eq!(
            runs[0].stop_reason,
            Some(RecursiveSchedulerStopReason::GraphTerminal)
        );
        assert_eq!(runs[0].step_count, 8);
        assert_eq!(runs[0].max_steps, 64);
        assert_eq!(runs[0].executor_mode, RecursiveExecutionMode::Fake);
        assert_eq!(
            runs[0].report_artifact_id,
            Some(scheduler_report_artifact_row(&detail).id)
        );
        assert_eq!(report_json["run_id"], runs[0].id.to_string());
        store
            .validate_recursive_task_graph_integrity(graph)
            .expect("read model remains valid");
    }

    #[test]
    fn recursive_dag_scheduler_orders_multiple_runnable_tasks_deterministically() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(101);
        let root = tid(10);
        let later_id = tid(30);
        let earlier_id = tid(20);
        create_graph(&store, graph, root, 32);

        let fake = RecursiveDagFakeExecutor::new()
            .on_execute(
                root,
                RecursiveDagFakeBehavior::Decompose(decomp(vec![
                    child(later_id, 20, 0, vec![]),
                    child(earlier_id, 20, 0, vec![]),
                ])),
            )
            .on_execute(later_id, RecursiveDagFakeBehavior::direct_success("later"))
            .on_execute(
                earlier_id,
                RecursiveDagFakeBehavior::direct_success("earlier"),
            );
        let mut scheduler = RecursiveDagScheduler::new(fake);

        let report = scheduler
            .run_until_idle(&store, graph)
            .expect("scheduler run");

        assert_eq!(
            report.selected_task_order[..3],
            [root, earlier_id, later_id],
            "sibling runnable order should be stable by task id, not child insertion order"
        );
    }

    #[test]
    fn recursive_dag_scheduler_skips_quarantined_graph_without_mutation() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(102);
        let root = tid(40);
        create_graph(&store, graph, root, 8);
        store
            .quarantine_recursive_task_graph(graph, "test quarantine".to_string())
            .expect("quarantine");

        let mut scheduler = RecursiveDagScheduler::new(RecursiveDagFakeExecutor::new());
        let report = scheduler
            .run_until_idle(&store, graph)
            .expect("scheduler run");

        assert_eq!(
            report.stop_reason,
            RecursiveDagSchedulerStopReason::Quarantined
        );
        let detail = store
            .get_recursive_task_graph(graph)
            .expect("read graph")
            .expect("graph exists");
        assert_eq!(detail.graph.status, RecursiveGraphStatus::Malformed);
        assert!(detail.artifacts.is_empty());
        assert_eq!(
            detail.graph.quarantine_reason.as_deref(),
            Some("test quarantine")
        );
        let runs = store
            .list_recursive_scheduler_runs_for_graph(graph)
            .expect("list scheduler runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RecursiveSchedulerRunStatus::Completed);
        assert_eq!(
            runs[0].stop_reason,
            Some(RecursiveSchedulerStopReason::Quarantined)
        );
        assert_eq!(runs[0].step_count, 0);
        assert_eq!(runs[0].report_artifact_id, None);
    }

    #[test]
    fn recursive_dag_scheduler_persists_step_limit_stop_and_readback() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(103);
        let root = tid(50);
        create_graph(&store, graph, root, 100);
        let fake = RecursiveDagFakeExecutor::new().on_execute(
            root,
            RecursiveDagFakeBehavior::RetryableFailure {
                reason: "retryable failure".to_string(),
            },
        );
        let mut scheduler = RecursiveDagScheduler::new(fake);

        let report = scheduler
            .run_until_idle_with_limit(&store, graph, 2)
            .expect("scheduler run");

        assert_eq!(
            report.stop_reason,
            RecursiveDagSchedulerStopReason::StepLimitExceeded
        );
        assert_eq!(report.step_count, 2);
        let detail = store
            .get_recursive_task_graph(graph)
            .expect("read graph")
            .expect("graph exists");
        assert_eq!(
            detail.graph.last_stop_reason.as_deref(),
            Some("step_limit_exceeded")
        );
        assert_eq!(detail.attempts.len(), 2);
        let report_json = scheduler_report_artifact(&detail);
        assert_eq!(report_json["stop_reason"], "step_limit_exceeded");
        assert_eq!(
            report_json["selected_task_order"].as_array().unwrap().len(),
            2
        );
        let runs = store
            .list_recursive_scheduler_runs_for_graph(graph)
            .expect("list scheduler runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RecursiveSchedulerRunStatus::Completed);
        assert_eq!(
            runs[0].stop_reason,
            Some(RecursiveSchedulerStopReason::StepLimitExceeded)
        );
        assert_eq!(runs[0].step_count, 2);
        assert_eq!(runs[0].max_steps, 2);
        assert_eq!(
            runs[0].report_artifact_id,
            Some(scheduler_report_artifact_row(&detail).id)
        );
        store
            .start_recursive_scheduler_run(RecursiveSchedulerRunStart {
                graph_id: graph,
                max_steps: 8,
                source: RecursiveSchedulerRunSource::TestHarness,
                operator: Some("test".to_string()),
                executor_mode: RecursiveExecutionMode::Fake,
            })
            .expect("step-limit scheduler run releases graph lease");
    }

    #[test]
    fn recursive_dag_scheduler_graph_cancellation_before_first_step_creates_no_attempts() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(200);
        let root = tid(201);
        create_graph(&store, graph, root, 16);
        let request = store
            .request_recursive_graph_cancellation(
                graph,
                RecursiveCancellationRequestCreate {
                    reason: "cancel before start".to_string(),
                    requested_by: Some("test".to_string()),
                    source: RecursiveCancellationRequestSource::TestHarness,
                },
            )
            .expect("request graph cancellation");
        let fake = RecursiveDagFakeExecutor::new().on_execute(
            root,
            RecursiveDagFakeBehavior::direct_success("unreachable"),
        );
        let mut scheduler = RecursiveDagScheduler::new(fake);

        let report = scheduler
            .run_until_idle(&store, graph)
            .expect("scheduler run");

        assert_eq!(
            report.stop_reason,
            RecursiveDagSchedulerStopReason::CancellationRequested
        );
        assert_eq!(report.cancellation_request_id, Some(request.id));
        assert_eq!(report.step_count, 0);
        assert!(report.selected_task_order.is_empty());
        let detail = store
            .get_recursive_task_graph(graph)
            .expect("read graph")
            .expect("graph exists");
        assert_eq!(detail.graph.status, RecursiveGraphStatus::Cancelled);
        assert_eq!(
            detail.graph.last_stop_reason.as_deref(),
            Some("cancellation_requested")
        );
        assert_eq!(detail.attempts.len(), 0);
        assert_eq!(
            node_status(&detail, root),
            RecursiveTaskLifecycleState::Cancelled
        );
        let request = store
            .load_recursive_cancellation_request(request.id)
            .expect("load cancellation")
            .expect("request exists");
        assert_eq!(request.status, RecursiveCancellationRequestStatus::Applied);
        let runs = store
            .list_recursive_scheduler_runs_for_graph(graph)
            .expect("list scheduler runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RecursiveSchedulerRunStatus::Cancelled);
        assert_eq!(
            runs[0].stop_reason,
            Some(RecursiveSchedulerStopReason::CancellationRequested)
        );
        assert_eq!(runs[0].cancellation_request_id, Some(request.id));
        assert_eq!(runs[0].step_count, 0);
        let report_json = scheduler_report_artifact(&detail);
        assert_eq!(report_json["stop_reason"], "cancellation_requested");
        assert_eq!(
            report_json["cancellation_request_id"],
            request.id.to_string()
        );
        store
            .start_recursive_scheduler_run(RecursiveSchedulerRunStart {
                graph_id: graph,
                max_steps: 8,
                source: RecursiveSchedulerRunSource::TestHarness,
                operator: Some("test".to_string()),
                executor_mode: RecursiveExecutionMode::Fake,
            })
            .expect("cancelled scheduler run releases graph lease");
    }

    #[test]
    fn recursive_dag_scheduler_graph_cancellation_between_steps_stops_before_next_task() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(202);
        let root = tid(203);
        let a = tid(204);
        let b = tid(205);
        create_graph(&store, graph, root, 16);
        let fake = RequestGraphCancellationAndDecompose {
            db_path: db.path.clone(),
            decomposition: decomp(vec![child(a, 20, 0, vec![]), child(b, 20, 0, vec![])]),
            requested: false,
        };
        let mut scheduler = RecursiveDagScheduler::new(fake);

        let report = scheduler
            .run_until_idle(&store, graph)
            .expect("scheduler run");

        assert_eq!(
            report.stop_reason,
            RecursiveDagSchedulerStopReason::CancellationRequested
        );
        assert_eq!(report.step_count, 1);
        assert_eq!(report.selected_task_order, vec![root]);
        let detail = store
            .get_recursive_task_graph(graph)
            .expect("read graph")
            .expect("graph exists");
        assert_eq!(detail.graph.status, RecursiveGraphStatus::Cancelled);
        assert_eq!(detail.attempts.len(), 1);
        assert_eq!(detail.attempts[0].task_id, root);
        assert_eq!(
            node_status(&detail, root),
            RecursiveTaskLifecycleState::Cancelled
        );
        assert_eq!(
            node_status(&detail, a),
            RecursiveTaskLifecycleState::Cancelled
        );
        assert_eq!(
            node_status(&detail, b),
            RecursiveTaskLifecycleState::Cancelled
        );
        let runs = store
            .list_recursive_scheduler_runs_for_graph(graph)
            .expect("list scheduler runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RecursiveSchedulerRunStatus::Cancelled);
        assert_eq!(runs[0].step_count, 1);
    }

    #[test]
    fn recursive_dag_scheduler_run_cancellation_stops_run_without_cancelling_graph() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(206);
        let root = tid(207);
        let a = tid(208);
        create_graph(&store, graph, root, 16);
        let fake = RequestCurrentRunCancellationAndDecompose {
            db_path: db.path.clone(),
            decomposition: decomp(vec![child(a, 20, 0, vec![])]),
            requested: false,
        };
        let mut scheduler = RecursiveDagScheduler::new(fake);

        let report = scheduler
            .run_until_idle(&store, graph)
            .expect("scheduler run");

        assert_eq!(
            report.stop_reason,
            RecursiveDagSchedulerStopReason::CancellationRequested
        );
        assert_eq!(report.step_count, 1);
        let detail = store
            .get_recursive_task_graph(graph)
            .expect("read graph")
            .expect("graph exists");
        assert_eq!(detail.graph.status, RecursiveGraphStatus::Active);
        assert_eq!(
            node_status(&detail, root),
            RecursiveTaskLifecycleState::BlockedOnChildren
        );
        assert_eq!(
            node_status(&detail, a),
            RecursiveTaskLifecycleState::Pending
        );
        let runs = store
            .list_recursive_scheduler_runs_for_graph(graph)
            .expect("list scheduler runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RecursiveSchedulerRunStatus::Cancelled);
        let cancellation_request_id = runs[0]
            .cancellation_request_id
            .expect("run cancellation request id");
        let request = store
            .load_recursive_cancellation_request(cancellation_request_id)
            .expect("load cancellation")
            .expect("request exists");
        assert_eq!(request.status, RecursiveCancellationRequestStatus::Applied);
        assert_eq!(request.run_id, Some(runs[0].id));
    }

    #[test]
    fn recursive_dag_scheduler_run_cancellation_applies_only_to_matching_run() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(209);
        let root = tid(210);
        let unrelated_graph = gid(1209);
        let unrelated_root = tid(1210);
        create_graph(&store, graph, root, 16);
        create_graph(&store, unrelated_graph, unrelated_root, 16);
        let unrelated_run = store
            .start_recursive_scheduler_run(RecursiveSchedulerRunStart {
                graph_id: unrelated_graph,
                max_steps: 8,
                source: RecursiveSchedulerRunSource::TestHarness,
                operator: Some("test".to_string()),
                executor_mode: RecursiveExecutionMode::Fake,
            })
            .expect("start unrelated run");
        let unrelated_request = store
            .request_recursive_scheduler_run_cancellation(
                unrelated_run.id,
                RecursiveCancellationRequestCreate {
                    reason: "cancel another run".to_string(),
                    requested_by: Some("test".to_string()),
                    source: RecursiveCancellationRequestSource::TestHarness,
                },
            )
            .expect("request unrelated run cancellation");
        let fake = RecursiveDagFakeExecutor::new().on_execute(
            root,
            RecursiveDagFakeBehavior::direct_success("root-artifact"),
        );
        let mut scheduler = RecursiveDagScheduler::new(fake);

        let report = scheduler
            .run_until_idle(&store, graph)
            .expect("scheduler run");

        assert_eq!(
            report.stop_reason,
            RecursiveDagSchedulerStopReason::GraphTerminal
        );
        assert_eq!(report.step_count, 1);
        let unrelated_request = store
            .load_recursive_cancellation_request(unrelated_request.id)
            .expect("load cancellation")
            .expect("request exists");
        assert_eq!(
            unrelated_request.status,
            RecursiveCancellationRequestStatus::Requested
        );
        let runs = store
            .list_recursive_scheduler_runs_for_graph(graph)
            .expect("list scheduler runs");
        let completed_run = runs
            .iter()
            .find(|run| run.status == RecursiveSchedulerRunStatus::Completed)
            .expect("completed run");
        assert_ne!(completed_run.id, unrelated_run.id);
        assert_eq!(
            completed_run.stop_reason,
            Some(RecursiveSchedulerStopReason::GraphTerminal)
        );
    }

    #[test]
    fn recursive_dag_scheduler_task_scoped_cancellation_is_not_executed() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(211);
        let root = tid(212);
        create_graph(&store, graph, root, 16);
        let request = store
            .request_recursive_task_cancellation(
                graph,
                root,
                RecursiveCancellationRequestCreate {
                    reason: "model task cancellation only".to_string(),
                    requested_by: Some("test".to_string()),
                    source: RecursiveCancellationRequestSource::TestHarness,
                },
            )
            .expect("request task cancellation");
        let fake = RecursiveDagFakeExecutor::new().on_execute(
            root,
            RecursiveDagFakeBehavior::direct_success("root-artifact"),
        );
        let mut scheduler = RecursiveDagScheduler::new(fake);

        let report = scheduler
            .run_until_idle(&store, graph)
            .expect("scheduler run");

        assert_eq!(
            report.stop_reason,
            RecursiveDagSchedulerStopReason::GraphTerminal
        );
        let detail = store
            .get_recursive_task_graph(graph)
            .expect("read graph")
            .expect("graph exists");
        assert_eq!(detail.graph.status, RecursiveGraphStatus::Terminal);
        assert_eq!(
            node_status(&detail, root),
            RecursiveTaskLifecycleState::Succeeded
        );
        let request = store
            .load_recursive_cancellation_request(request.id)
            .expect("load cancellation")
            .expect("request exists");
        assert_eq!(
            request.status,
            RecursiveCancellationRequestStatus::Requested
        );
    }

    #[test]
    fn recursive_dag_scheduler_report_after_partial_failure_is_durable() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(104);
        let root = tid(60);
        create_graph(&store, graph, root, 16);
        let fake = RecursiveDagFakeExecutor::new().on_execute(
            root,
            RecursiveDagFakeBehavior::PermanentFailure {
                reason: "permanent failure".to_string(),
            },
        );
        let mut scheduler = RecursiveDagScheduler::new(fake);

        let report = scheduler
            .run_until_idle(&store, graph)
            .expect("scheduler run");

        assert_eq!(
            report.stop_reason,
            RecursiveDagSchedulerStopReason::PartialFailure
        );
        assert_eq!(report.step_count, 1);
        let detail = store
            .get_recursive_task_graph(graph)
            .expect("read graph")
            .expect("graph exists");
        assert_eq!(detail.graph.status, RecursiveGraphStatus::Failed);
        assert_eq!(
            detail.graph.last_stop_reason.as_deref(),
            Some("partial_failure")
        );
        let report_json = scheduler_report_artifact(&detail);
        assert_eq!(report_json["stop_reason"], "partial_failure");
        assert_eq!(report_json["task_outcomes"][0]["outcome"]["kind"], "failed");
        let runs = store
            .list_recursive_scheduler_runs_for_graph(graph)
            .expect("list scheduler runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RecursiveSchedulerRunStatus::Completed);
        assert_eq!(
            runs[0].stop_reason,
            Some(RecursiveSchedulerStopReason::PartialFailure)
        );
        assert_eq!(runs[0].step_count, 1);
        assert_eq!(
            runs[0].report_artifact_id,
            Some(scheduler_report_artifact_row(&detail).id)
        );
    }

    #[test]
    fn recursive_dag_scheduler_error_after_run_start_records_failed_run() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(109);
        let root = tid(110);
        let duplicated_child = tid(111);
        create_graph(&store, graph, root, 16);
        let fake = RecursiveDagFakeExecutor::new().on_execute(
            root,
            RecursiveDagFakeBehavior::Decompose(decomp(vec![
                child(duplicated_child, 20, 0, vec![]),
                child(duplicated_child, 10, 0, vec![]),
            ])),
        );
        let mut scheduler = RecursiveDagScheduler::new(fake);

        let error = scheduler
            .run_until_idle(&store, graph)
            .expect_err("invalid decomposition fails scheduler run");

        assert!(error.to_string().contains("duplicate recursive task id"));
        let runs = store
            .list_recursive_scheduler_runs_for_graph(graph)
            .expect("list scheduler runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RecursiveSchedulerRunStatus::Failed);
        assert_eq!(
            runs[0].stop_reason,
            Some(RecursiveSchedulerStopReason::ExecutorError)
        );
        assert_eq!(runs[0].step_count, 0);
        assert!(runs[0].completed_at.is_some());
        assert_eq!(runs[0].report_artifact_id, None);
        assert!(
            runs[0]
                .failure_reason
                .as_deref()
                .unwrap_or_default()
                .contains("duplicate recursive task id")
        );
        store
            .start_recursive_scheduler_run(RecursiveSchedulerRunStart {
                graph_id: graph,
                max_steps: 8,
                source: RecursiveSchedulerRunSource::TestHarness,
                operator: Some("test".to_string()),
                executor_mode: RecursiveExecutionMode::Fake,
            })
            .expect("failed scheduler run releases graph lease");
    }

    #[test]
    fn recursive_dag_scheduler_persists_fake_artifacts() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(105);
        let root = tid(70);
        create_graph(&store, graph, root, 8);
        let fake = RecursiveDagFakeExecutor::new().on_execute(
            root,
            RecursiveDagFakeBehavior::direct_success("root-artifact"),
        );
        let mut scheduler = RecursiveDagScheduler::new(fake);

        scheduler
            .run_until_idle(&store, graph)
            .expect("scheduler run");

        let detail = store
            .get_recursive_task_graph(graph)
            .expect("read graph")
            .expect("graph exists");
        let artifact = detail
            .artifacts
            .iter()
            .find(|artifact| artifact.label == "root-artifact")
            .expect("root artifact");
        assert_eq!(artifact.kind, RecursiveExecutionArtifactKind::Inline);
        assert!(artifact.attempt_id.is_some());
        assert_eq!(artifact.metadata["executor"], "recursive_dag_fake");
    }

    #[test]
    fn recursive_dag_scheduler_source_does_not_bypass_store_apis() {
        let source = concat!(
            include_str!("recursive_dag.rs"),
            include_str!("recursive_dag/scheduler_core.rs")
        );
        assert!(!source.contains(concat!("rusq", "lite")));
        assert!(!source.contains(concat!(".", "conn")));
        assert!(!source.contains(concat!("execute", "_batch")));
    }

    #[test]
    fn recursive_dag_scheduler_has_no_live_model_execution_path() {
        let source = concat!(
            include_str!("recursive_dag.rs"),
            include_str!("recursive_dag/scheduler_core.rs")
        );
        assert!(!source.contains(concat!("Launch", "Session")));
        assert!(!source.contains(concat!("Continue", "Session")));
        assert!(!source.contains(concat!("Session", "Manager")));
        assert!(!source.contains(concat!("cla", "ude")));
        assert!(!source.contains(concat!("cod", "ex")));
    }

    #[test]
    fn recursive_dag_scheduler_resumes_after_store_reopen() {
        let db = TestDb::new();
        let graph = gid(106);
        let root = tid(80);
        let a = tid(81);
        let completed_run_id;
        {
            let store = db.open();
            create_graph(&store, graph, root, 32);
            let fake = RecursiveDagFakeExecutor::new().on_execute(
                root,
                RecursiveDagFakeBehavior::Decompose(decomp(vec![child(a, 20, 0, vec![])])),
            );
            let mut scheduler = RecursiveDagScheduler::new(fake);
            let step = scheduler.run_one_step(&store, graph).expect("first step");
            assert!(matches!(step, RecursiveDagSchedulerStepResult::Executed(_)));
        }

        {
            let store = db.open();
            let fake = RecursiveDagFakeExecutor::new()
                .on_execute(
                    a,
                    RecursiveDagFakeBehavior::direct_success("A-after-reopen"),
                )
                .on_integrate(
                    root,
                    RecursiveDagFakeBehavior::integration_success("root-after-reopen"),
                );
            let mut scheduler = RecursiveDagScheduler::new(fake);
            let report = scheduler.run_until_idle(&store, graph).expect("resume run");
            assert_eq!(
                report.stop_reason,
                RecursiveDagSchedulerStopReason::GraphTerminal
            );
            let detail = store
                .get_recursive_task_graph(graph)
                .expect("read graph")
                .expect("graph exists");
            assert_eq!(detail.graph.status, RecursiveGraphStatus::Terminal);
            assert_eq!(detail.attempts.len(), 3);
            let attempt_ids: BTreeSet<_> =
                detail.attempts.iter().map(|attempt| attempt.id).collect();
            assert_eq!(attempt_ids.len(), detail.attempts.len());
            let runs = store
                .list_recursive_scheduler_runs_for_graph(graph)
                .expect("list scheduler runs");
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].status, RecursiveSchedulerRunStatus::Completed);
            completed_run_id = runs[0].id;
        }

        {
            let store = db.open();
            let run = store
                .load_recursive_scheduler_run(completed_run_id)
                .expect("load scheduler run")
                .expect("scheduler run survives reopen");
            assert_eq!(run.graph_id, graph);
            assert_eq!(run.status, RecursiveSchedulerRunStatus::Completed);
            assert_eq!(
                run.stop_reason,
                Some(RecursiveSchedulerStopReason::GraphTerminal)
            );
            assert!(run.report_artifact_id.is_some());
        }
    }

    #[test]
    fn recursive_dag_scheduler_mixed_blocked_cancelled_children_block_parent() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(107);
        let root = tid(90);
        let succeeded = tid(91);
        let blocked = tid(92);
        let cancelled = tid(93);
        create_graph(&store, graph, root, 16);
        store
            .insert_recursive_decomposition_batch(
                graph,
                RecursiveDecompositionBatchCreate {
                    batch_id: RecursiveInjectionBatchId(Uuid::from_u128(900)),
                    parent_task_id: root,
                    attempt_id: None,
                    reason_for_decomposition: "manual mixed children".to_string(),
                    children: vec![
                        child(succeeded, 20, 0, vec![]),
                        child(blocked, 20, 0, vec![]),
                        child(cancelled, 20, 0, vec![]),
                    ],
                    integration_strategy: "integrate".to_string(),
                    verification_strategy: "verify".to_string(),
                },
            )
            .expect("insert children");
        store
            .transition_recursive_task_state(
                graph,
                root,
                RecursiveTaskLifecycleState::BlockedOnChildren,
                Some("waiting".to_string()),
            )
            .expect("root waits");
        store
            .transition_recursive_task_state(
                graph,
                succeeded,
                RecursiveTaskLifecycleState::Succeeded,
                Some("done".to_string()),
            )
            .expect("child succeeds");
        store
            .transition_recursive_task_state(
                graph,
                blocked,
                RecursiveTaskLifecycleState::Blocked,
                Some("blocked child".to_string()),
            )
            .expect("child blocks");
        store
            .transition_recursive_task_state(
                graph,
                cancelled,
                RecursiveTaskLifecycleState::Cancelled,
                Some("cancelled child".to_string()),
            )
            .expect("child cancels");

        let mut scheduler = RecursiveDagScheduler::new(RecursiveDagFakeExecutor::new());
        let report = scheduler
            .run_until_idle(&store, graph)
            .expect("scheduler run");

        assert_eq!(
            report.stop_reason,
            RecursiveDagSchedulerStopReason::PartialFailure
        );
        let detail = store
            .get_recursive_task_graph(graph)
            .expect("read graph")
            .expect("graph exists");
        let root_node = detail
            .nodes
            .iter()
            .find(|node| node.id == root)
            .expect("root node");
        assert_eq!(root_node.status, RecursiveTaskLifecycleState::Blocked);
        assert!(
            root_node
                .blocked_reason
                .as_deref()
                .unwrap_or_default()
                .contains(&blocked.to_string())
        );
        assert_eq!(
            node_status(&detail, cancelled),
            RecursiveTaskLifecycleState::Cancelled
        );
    }

    #[test]
    fn recursive_dag_store_artifact_api_rejects_wrong_task_attempt_pair() {
        let db = TestDb::new();
        let store = db.open();
        let graph = gid(108);
        let root = tid(1000);
        let other = tid(1001);
        create_graph(&store, graph, root, 8);
        store
            .insert_recursive_decomposition_batch(
                graph,
                RecursiveDecompositionBatchCreate {
                    batch_id: RecursiveInjectionBatchId(Uuid::from_u128(1002)),
                    parent_task_id: root,
                    attempt_id: None,
                    reason_for_decomposition: "split".to_string(),
                    children: vec![child(other, 20, 0, vec![])],
                    integration_strategy: "integrate".to_string(),
                    verification_strategy: "verify".to_string(),
                },
            )
            .expect("insert child");
        store
            .record_recursive_attempt_start(graph, root, RecursiveAttemptPhase::Execute, aid(1003))
            .expect("start attempt");

        let error = store
            .record_recursive_execution_artifacts(
                graph,
                vec![RecursiveExecutionArtifactCreate {
                    task_id: other,
                    attempt_id: Some(aid(1003)),
                    kind: RecursiveExecutionArtifactKind::Inline,
                    label: "wrong-task".to_string(),
                    content: Some("bad".to_string()),
                    uri: None,
                    metadata: json!({}),
                }],
            )
            .expect_err("wrong task rejects")
            .to_string();
        assert!(error.contains("belongs to task"));
    }
}
