use super::{
    RecursiveDagSchedulerExecutedStep, RecursiveDagSchedulerReport, RecursiveDagSchedulerStopReason,
};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::recursive_dag::{
    DEFAULT_RECURSIVE_SCHEDULER_LEASE_TTL_SECONDS, RecursiveExecutionArtifactCreate,
};
use chrono::Utc;
use rsi_common::recursive_dag::{
    RecursiveAttemptId, RecursiveAttemptPhase, RecursiveAttemptStatus,
    RecursiveCancellationRequestSummary, RecursiveCancellationScope, RecursiveExecutionArtifact,
    RecursiveExecutionArtifactKind, RecursiveGraphStatus, RecursiveInjectionBatchId,
    RecursiveSchedulerRunSource, RecursiveSchedulerRunSummary, RecursiveTaskGraphDetail,
    RecursiveTaskGraphId, RecursiveTaskId, RecursiveTaskLifecycleState, RecursiveTaskNode,
};
use serde_json::json;
use std::collections::BTreeMap;
use uuid::Uuid;

const SCHEDULER_NAMESPACE: Uuid = uuid::uuid!("14572e31-32f7-43e5-a7a0-11c3a8fbc2ef");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RecursiveSchedulerRunnableTask {
    pub(super) task_id: RecursiveTaskId,
    pub(super) phase: RecursiveAttemptPhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RecursiveSchedulerStepPreparation {
    Runnable(RecursiveSchedulerRunnableTask),
    Stopped {
        graph_id: RecursiveTaskGraphId,
        stop_reason: RecursiveDagSchedulerStopReason,
    },
}

#[derive(Debug, Default)]
pub(super) struct RecursiveSchedulerRunProgress {
    selected_task_order: Vec<RecursiveTaskId>,
    task_outcomes: Vec<RecursiveDagSchedulerExecutedStep>,
}

impl RecursiveSchedulerRunProgress {
    pub(super) fn record_executed_step(&mut self, step: RecursiveDagSchedulerExecutedStep) {
        self.selected_task_order.push(step.task_id);
        self.task_outcomes.push(step);
    }

    pub(super) fn step_count(&self) -> u32 {
        saturating_usize_to_u32(self.task_outcomes.len())
    }

    pub(super) fn build_report(
        &self,
        run: &RecursiveSchedulerRunSummary,
        stop_reason: RecursiveDagSchedulerStopReason,
        cancellation_request_id: Option<rsi_common::RecursiveCancellationRequestId>,
    ) -> RecursiveDagSchedulerReport {
        RecursiveDagSchedulerReport {
            run_id: run.id,
            graph_id: run.graph_id,
            started_at: run.started_at,
            completed_at: Utc::now(),
            max_steps: run.max_steps,
            source: run.source,
            operator: run.operator.clone(),
            stop_reason,
            cancellation_request_id,
            step_count: self.step_count(),
            selected_task_order: self.selected_task_order.clone(),
            task_outcomes: self.task_outcomes.clone(),
        }
    }
}

pub(super) fn load_graph(
    store: &Store,
    graph_id: RecursiveTaskGraphId,
) -> Result<RecursiveTaskGraphDetail> {
    store
        .get_recursive_task_graph(graph_id)?
        .ok_or_else(|| DaemonError::Store(format!("recursive DAG graph not found: {graph_id}")))
}

pub(super) fn prepare_next_scheduler_step(
    store: &Store,
    graph_id: RecursiveTaskGraphId,
    run: Option<&RecursiveSchedulerRunSummary>,
) -> Result<RecursiveSchedulerStepPreparation> {
    let detail = promote_lifecycle_until_stable(store, graph_id)?;
    if is_quarantined(&detail) {
        return Ok(RecursiveSchedulerStepPreparation::Stopped {
            graph_id,
            stop_reason: RecursiveDagSchedulerStopReason::Quarantined,
        });
    }
    if let Some(stop_reason) = terminal_stop_reason(&detail) {
        return Ok(RecursiveSchedulerStepPreparation::Stopped {
            graph_id,
            stop_reason,
        });
    }
    if let Some(run) = run
        && observe_scheduler_cancellation(store, run)?.is_some()
    {
        return Ok(RecursiveSchedulerStepPreparation::Stopped {
            graph_id,
            stop_reason: RecursiveDagSchedulerStopReason::CancellationRequested,
        });
    }

    let Some(runnable) = select_next_runnable(&detail) else {
        return Ok(RecursiveSchedulerStepPreparation::Stopped {
            graph_id,
            stop_reason: RecursiveDagSchedulerStopReason::IdleNoRunnable,
        });
    };

    Ok(RecursiveSchedulerStepPreparation::Runnable(runnable))
}

pub(super) fn current_stop_reason(
    store: &Store,
    graph_id: RecursiveTaskGraphId,
) -> Result<Option<RecursiveDagSchedulerStopReason>> {
    let detail = promote_lifecycle_until_stable(store, graph_id)?;
    if is_quarantined(&detail) {
        return Ok(Some(RecursiveDagSchedulerStopReason::Quarantined));
    }
    if let Some(stop_reason) = terminal_stop_reason(&detail) {
        return Ok(Some(stop_reason));
    }
    Ok(select_next_runnable(&detail)
        .is_none()
        .then_some(RecursiveDagSchedulerStopReason::IdleNoRunnable))
}

pub(super) fn heartbeat_scheduler_run_lease(
    store: &Store,
    run: &RecursiveSchedulerRunSummary,
) -> Result<RecursiveSchedulerRunSummary> {
    let lease_token = run.lease_token.as_deref().ok_or_else(|| {
        DaemonError::Store(format!(
            "recursive scheduler run {} is missing a lease token",
            run.id
        ))
    })?;
    store.heartbeat_recursive_scheduler_run_lease(
        run.id,
        lease_token,
        DEFAULT_RECURSIVE_SCHEDULER_LEASE_TTL_SECONDS,
    )
}

pub(super) fn observe_scheduler_cancellation(
    store: &Store,
    run: &RecursiveSchedulerRunSummary,
) -> Result<Option<RecursiveCancellationRequestSummary>> {
    let Some(request) = store.next_pending_recursive_cancellation_for_run(run.graph_id, run.id)?
    else {
        return Ok(None);
    };
    let observed = store.observe_recursive_cancellation_request(request.id, Some(run.id))?;
    if observed.scope == RecursiveCancellationScope::Graph {
        store.apply_recursive_graph_cancellation(observed.id)?;
    }
    Ok(Some(observed))
}

pub(super) fn failed_attempt_count(
    detail: &RecursiveTaskGraphDetail,
    task_id: RecursiveTaskId,
    phase: RecursiveAttemptPhase,
) -> u32 {
    saturating_usize_to_u32(
        detail
            .attempts
            .iter()
            .filter(|attempt| {
                attempt.task_id == task_id
                    && attempt.phase == phase
                    && attempt.status == RecursiveAttemptStatus::Failed
            })
            .count(),
    )
}

pub(super) fn next_attempt_no(
    detail: &RecursiveTaskGraphDetail,
    task_id: RecursiveTaskId,
    phase: RecursiveAttemptPhase,
) -> u32 {
    saturating_usize_to_u32(
        detail
            .attempts
            .iter()
            .filter(|attempt| attempt.task_id == task_id && attempt.phase == phase)
            .count()
            .saturating_add(1),
    )
}

pub(super) fn retry_state_for_phase(phase: RecursiveAttemptPhase) -> RecursiveTaskLifecycleState {
    match phase {
        RecursiveAttemptPhase::Execute => RecursiveTaskLifecycleState::Ready,
        RecursiveAttemptPhase::Integrate => RecursiveTaskLifecycleState::BlockedOnChildren,
    }
}

pub(super) fn deterministic_attempt_id(
    graph_id: RecursiveTaskGraphId,
    task_id: RecursiveTaskId,
    phase: RecursiveAttemptPhase,
    attempt_no: u32,
) -> RecursiveAttemptId {
    RecursiveAttemptId(Uuid::new_v5(
        &SCHEDULER_NAMESPACE,
        format!(
            "attempt:{graph_id}:{task_id}:{}:{attempt_no}",
            phase_key(phase)
        )
        .as_bytes(),
    ))
}

pub(super) fn deterministic_batch_id(
    graph_id: RecursiveTaskGraphId,
    task_id: RecursiveTaskId,
    batch_no: u32,
) -> RecursiveInjectionBatchId {
    RecursiveInjectionBatchId(Uuid::new_v5(
        &SCHEDULER_NAMESPACE,
        format!("batch:{graph_id}:{task_id}:{batch_no}").as_bytes(),
    ))
}

pub(super) fn finish_fake_scheduler_run_with_report(
    store: &Store,
    run: &RecursiveSchedulerRunSummary,
    report: &RecursiveDagSchedulerReport,
) -> Result<()> {
    let report_artifact = persist_scheduler_report(store, report)?;
    if report.stop_reason == RecursiveDagSchedulerStopReason::CancellationRequested {
        let cancellation_request_id = report.cancellation_request_id.ok_or_else(|| {
            DaemonError::Store(format!(
                "recursive scheduler run {} stopped for cancellation without request id",
                run.id
            ))
        })?;
        store.cancel_recursive_scheduler_run(
            run.id,
            cancellation_request_id,
            report.step_count,
            report_artifact.map(|artifact| artifact.id),
        )?;
    } else {
        store.finish_recursive_scheduler_run(
            run.id,
            report.stop_reason.into(),
            report.step_count,
            report_artifact.map(|artifact| artifact.id),
        )?;
    }
    Ok(())
}

pub(super) fn phase_key(phase: RecursiveAttemptPhase) -> &'static str {
    match phase {
        RecursiveAttemptPhase::Execute => "execute",
        RecursiveAttemptPhase::Integrate => "integrate",
    }
}

pub(super) fn saturating_usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn promote_lifecycle_until_stable(
    store: &Store,
    graph_id: RecursiveTaskGraphId,
) -> Result<RecursiveTaskGraphDetail> {
    loop {
        let detail = load_graph(store, graph_id)?;
        if is_quarantined(&detail) {
            return Ok(detail);
        }
        let Some(promotion) = next_lifecycle_promotion(&detail) else {
            return Ok(detail);
        };
        store.transition_recursive_task_state(
            graph_id,
            promotion.task_id,
            promotion.to_status,
            Some(promotion.reason),
        )?;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LifecyclePromotion {
    task_id: RecursiveTaskId,
    to_status: RecursiveTaskLifecycleState,
    reason: String,
}

fn next_lifecycle_promotion(detail: &RecursiveTaskGraphDetail) -> Option<LifecyclePromotion> {
    let statuses: BTreeMap<_, _> = detail
        .nodes
        .iter()
        .map(|node| (node.id, node.status))
        .collect();
    // No persisted per-node upstream-content baseline exists in the model yet;
    // an empty baseline keeps `dependencies_satisfied` equivalent to the prior
    // status-only gate (see its docs). Threaded here so content-staleness lands
    // in this exact satisfaction path once a baseline is available.
    let content_baseline: BTreeMap<RecursiveTaskId, String> = BTreeMap::new();

    for node in &detail.nodes {
        match node.status {
            RecursiveTaskLifecycleState::Pending | RecursiveTaskLifecycleState::Ready => {
                if let Some((dependency_id, dependency_status)) =
                    failed_dependency(&statuses, detail, node.id)
                {
                    return Some(LifecyclePromotion {
                        task_id: node.id,
                        to_status: RecursiveTaskLifecycleState::Blocked,
                        reason: format!(
                            "recursive DAG scheduler: dependency {dependency_id} is {dependency_status:?}"
                        ),
                    });
                }
                if node.status == RecursiveTaskLifecycleState::Pending
                    && dependencies_satisfied(&statuses, detail, node.id, &content_baseline)
                {
                    return Some(LifecyclePromotion {
                        task_id: node.id,
                        to_status: RecursiveTaskLifecycleState::Ready,
                        reason: "recursive DAG scheduler: dependencies satisfied".to_string(),
                    });
                }
                if retry_budget_exhausted(detail, node.id, RecursiveAttemptPhase::Execute)
                    && !integrate_from_ready(detail, node)
                {
                    return Some(LifecyclePromotion {
                        task_id: node.id,
                        to_status: RecursiveTaskLifecycleState::Failed,
                        reason: "recursive DAG scheduler: execute retry budget exhausted"
                            .to_string(),
                    });
                }
            }
            RecursiveTaskLifecycleState::Decomposed => {
                return Some(LifecyclePromotion {
                    task_id: node.id,
                    to_status: RecursiveTaskLifecycleState::BlockedOnChildren,
                    reason: "recursive DAG scheduler: waiting on decomposed children".to_string(),
                });
            }
            RecursiveTaskLifecycleState::BlockedOnChildren => {
                if retry_budget_exhausted(detail, node.id, RecursiveAttemptPhase::Integrate) {
                    return Some(LifecyclePromotion {
                        task_id: node.id,
                        to_status: RecursiveTaskLifecycleState::Failed,
                        reason: "recursive DAG scheduler: integrate retry budget exhausted"
                            .to_string(),
                    });
                }
                if let Some(promotion) = blocked_on_children_promotion(&statuses, detail, node.id) {
                    return Some(promotion);
                }
            }
            _ => {}
        }
    }

    None
}

fn blocked_on_children_promotion(
    statuses: &BTreeMap<RecursiveTaskId, RecursiveTaskLifecycleState>,
    detail: &RecursiveTaskGraphDetail,
    task_id: RecursiveTaskId,
) -> Option<LifecyclePromotion> {
    let child_ids = child_ids(detail, task_id);
    if child_ids.is_empty() {
        return Some(LifecyclePromotion {
            task_id,
            to_status: RecursiveTaskLifecycleState::Ready,
            reason: "recursive DAG scheduler: decomposed parent has no children".to_string(),
        });
    }

    for child_id in &child_ids {
        if statuses.get(child_id).copied() == Some(RecursiveTaskLifecycleState::Failed) {
            return Some(LifecyclePromotion {
                task_id,
                to_status: RecursiveTaskLifecycleState::Failed,
                reason: format!("recursive DAG scheduler: child {child_id} failed"),
            });
        }
    }
    for child_id in &child_ids {
        if statuses.get(child_id).copied() == Some(RecursiveTaskLifecycleState::Blocked) {
            return Some(LifecyclePromotion {
                task_id,
                to_status: RecursiveTaskLifecycleState::Blocked,
                reason: format!("recursive DAG scheduler: child {child_id} blocked"),
            });
        }
    }
    if child_ids.iter().all(|child_id| {
        statuses.get(child_id).copied() == Some(RecursiveTaskLifecycleState::Cancelled)
    }) {
        return Some(LifecyclePromotion {
            task_id,
            to_status: RecursiveTaskLifecycleState::Cancelled,
            reason: "recursive DAG scheduler: all children cancelled".to_string(),
        });
    }
    for child_id in &child_ids {
        if statuses.get(child_id).copied() == Some(RecursiveTaskLifecycleState::Cancelled) {
            return Some(LifecyclePromotion {
                task_id,
                to_status: RecursiveTaskLifecycleState::Blocked,
                reason: format!("recursive DAG scheduler: child {child_id} cancelled"),
            });
        }
    }

    None
}

fn select_next_runnable(
    detail: &RecursiveTaskGraphDetail,
) -> Option<RecursiveSchedulerRunnableTask> {
    let statuses: BTreeMap<_, _> = detail
        .nodes
        .iter()
        .map(|node| (node.id, node.status))
        .collect();
    // Empty baseline == prior status-only behavior (see `dependencies_satisfied`).
    let content_baseline: BTreeMap<RecursiveTaskId, String> = BTreeMap::new();
    let mut candidates = Vec::new();

    for node in &detail.nodes {
        if !dependencies_satisfied(&statuses, detail, node.id, &content_baseline) {
            continue;
        }
        match node.status {
            RecursiveTaskLifecycleState::Ready if integrate_from_ready(detail, node) => {
                if !retry_budget_exhausted(detail, node.id, RecursiveAttemptPhase::Integrate) {
                    candidates.push((node.depth, node.id, RecursiveAttemptPhase::Integrate));
                }
            }
            RecursiveTaskLifecycleState::Ready => {
                if !retry_budget_exhausted(detail, node.id, RecursiveAttemptPhase::Execute) {
                    candidates.push((node.depth, node.id, RecursiveAttemptPhase::Execute));
                }
            }
            RecursiveTaskLifecycleState::BlockedOnChildren
                if children_succeeded(&statuses, detail, node.id) =>
            {
                if !retry_budget_exhausted(detail, node.id, RecursiveAttemptPhase::Integrate) {
                    candidates.push((node.depth, node.id, RecursiveAttemptPhase::Integrate));
                }
            }
            _ => {}
        }
    }

    candidates.sort_by_key(|(depth, task_id, phase)| (*depth, *task_id, phase_order(*phase)));
    candidates
        .into_iter()
        .next()
        .map(|(_, task_id, phase)| RecursiveSchedulerRunnableTask { task_id, phase })
}

fn integrate_from_ready(detail: &RecursiveTaskGraphDetail, node: &RecursiveTaskNode) -> bool {
    node.decomposed_once
        && !child_ids(detail, node.id).is_empty()
        && children_succeeded(
            &detail
                .nodes
                .iter()
                .map(|task| (task.id, task.status))
                .collect(),
            detail,
            node.id,
        )
}

fn terminal_stop_reason(
    detail: &RecursiveTaskGraphDetail,
) -> Option<RecursiveDagSchedulerStopReason> {
    match detail.graph.status {
        RecursiveGraphStatus::Terminal => Some(RecursiveDagSchedulerStopReason::GraphTerminal),
        RecursiveGraphStatus::Failed
        | RecursiveGraphStatus::Blocked
        | RecursiveGraphStatus::Cancelled => Some(RecursiveDagSchedulerStopReason::PartialFailure),
        RecursiveGraphStatus::Malformed if is_quarantined(detail) => {
            Some(RecursiveDagSchedulerStopReason::Quarantined)
        }
        RecursiveGraphStatus::Active | RecursiveGraphStatus::Malformed => None,
    }
}

fn is_quarantined(detail: &RecursiveTaskGraphDetail) -> bool {
    detail.graph.status == RecursiveGraphStatus::Malformed || detail.graph.quarantined_at.is_some()
}

fn dependencies_succeeded(
    statuses: &BTreeMap<RecursiveTaskId, RecursiveTaskLifecycleState>,
    detail: &RecursiveTaskGraphDetail,
    task_id: RecursiveTaskId,
) -> bool {
    dependency_ids(detail, task_id).iter().all(|dependency_id| {
        statuses.get(dependency_id).copied() == Some(RecursiveTaskLifecycleState::Succeeded)
    })
}

/// D6 content-staleness — full dependency-satisfaction check.
///
/// Historically the scheduler gated a node purely on upstream *status*
/// (`dependencies_succeeded`). This adds a content dimension: a node is only
/// satisfied when every upstream dependency has both `Succeeded` AND unchanged
/// resolved content relative to `content_baseline` (the upstream content hashes
/// captured when this node last ran). A changed upstream hash marks the
/// downstream node stale — it must re-run even though the upstream is
/// `Succeeded`.
///
/// The check is scoped per node: only nodes whose own upstreams changed are
/// stale, so a content edit re-runs downstream nodes without disturbing
/// siblings whose inputs are untouched.
///
/// A missing baseline entry (or an empty baseline) is treated as fresh, so the
/// production call sites — which do not yet persist a per-node upstream-content
/// baseline in the `recursive_dag` model — preserve the exact prior status-only
/// behavior (no spurious re-runs). Persisting that baseline is a follow-up that
/// requires an `rsi_common` model field, out of scope for this slice; the
/// predicate below is complete and exercised by the tests.
fn dependencies_satisfied(
    statuses: &BTreeMap<RecursiveTaskId, RecursiveTaskLifecycleState>,
    detail: &RecursiveTaskGraphDetail,
    task_id: RecursiveTaskId,
    content_baseline: &BTreeMap<RecursiveTaskId, String>,
) -> bool {
    dependencies_succeeded(statuses, detail, task_id)
        && !upstream_content_stale(detail, task_id, content_baseline)
}

/// Content hash of a node's RESOLVED input-bearing fields.
///
/// Uses the std `DefaultHasher` (the same hash facility `rsi_graph::cache` uses
/// for `CacheKey` — no new dependency). Only fields that materially change the
/// node's produced output are folded in.
fn node_content_hash(node: &RecursiveTaskNode) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    node.title.hash(&mut hasher);
    node.objective.hash(&mut hasher);
    node.scope.hash(&mut hasher);
    node.acceptance_criteria.hash(&mut hasher);
    node.integration_strategy.hash(&mut hasher);
    node.verification_strategy.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// True when any upstream dependency's current content hash differs from the
/// recorded `baseline` — i.e. an input was edited since this node last ran, so
/// the node is content-stale and must re-run. Upstreams absent from `baseline`
/// are treated as fresh (see `dependencies_satisfied`).
fn upstream_content_stale(
    detail: &RecursiveTaskGraphDetail,
    task_id: RecursiveTaskId,
    baseline: &BTreeMap<RecursiveTaskId, String>,
) -> bool {
    dependency_ids(detail, task_id)
        .into_iter()
        .any(|dependency_id| {
            let Some(expected) = baseline.get(&dependency_id) else {
                return false;
            };
            match detail.nodes.iter().find(|node| node.id == dependency_id) {
                Some(node) => &node_content_hash(node) != expected,
                None => false,
            }
        })
}

fn failed_dependency(
    statuses: &BTreeMap<RecursiveTaskId, RecursiveTaskLifecycleState>,
    detail: &RecursiveTaskGraphDetail,
    task_id: RecursiveTaskId,
) -> Option<(RecursiveTaskId, RecursiveTaskLifecycleState)> {
    dependency_ids(detail, task_id)
        .into_iter()
        .find_map(|dependency_id| {
            let status = statuses.get(&dependency_id).copied()?;
            matches!(
                status,
                RecursiveTaskLifecycleState::Failed
                    | RecursiveTaskLifecycleState::Blocked
                    | RecursiveTaskLifecycleState::Cancelled
            )
            .then_some((dependency_id, status))
        })
}

fn children_succeeded(
    statuses: &BTreeMap<RecursiveTaskId, RecursiveTaskLifecycleState>,
    detail: &RecursiveTaskGraphDetail,
    task_id: RecursiveTaskId,
) -> bool {
    let children = child_ids(detail, task_id);
    !children.is_empty()
        && children.iter().all(|child_id| {
            statuses.get(child_id).copied() == Some(RecursiveTaskLifecycleState::Succeeded)
        })
}

fn dependency_ids(
    detail: &RecursiveTaskGraphDetail,
    task_id: RecursiveTaskId,
) -> Vec<RecursiveTaskId> {
    detail
        .edges
        .iter()
        .filter(|edge| {
            edge.kind == rsi_common::RecursiveTaskEdgeKind::Dependency && edge.to_task_id == task_id
        })
        .map(|edge| edge.from_task_id)
        .collect()
}

fn child_ids(detail: &RecursiveTaskGraphDetail, task_id: RecursiveTaskId) -> Vec<RecursiveTaskId> {
    detail
        .edges
        .iter()
        .filter(|edge| {
            edge.kind == rsi_common::RecursiveTaskEdgeKind::ParentChild
                && edge.from_task_id == task_id
        })
        .map(|edge| edge.to_task_id)
        .collect()
}

fn retry_budget_exhausted(
    detail: &RecursiveTaskGraphDetail,
    task_id: RecursiveTaskId,
    phase: RecursiveAttemptPhase,
) -> bool {
    let Some(task) = detail.nodes.iter().find(|node| node.id == task_id) else {
        return true;
    };
    failed_attempt_count(detail, task_id, phase) > task.max_retries
}

fn persist_scheduler_report(
    store: &Store,
    report: &RecursiveDagSchedulerReport,
) -> Result<Option<RecursiveExecutionArtifact>> {
    if report.stop_reason == RecursiveDagSchedulerStopReason::Quarantined {
        return Ok(None);
    }
    store.record_recursive_scheduler_stop(
        report.graph_id,
        report.stop_reason.as_str().to_string(),
    )?;
    let detail = load_graph(store, report.graph_id)?;
    let report_json = serde_json::to_string(report).map_err(DaemonError::Json)?;
    let artifacts = store.record_recursive_execution_artifacts(
        report.graph_id,
        vec![RecursiveExecutionArtifactCreate {
            task_id: detail.graph.root_task_id,
            attempt_id: None,
            kind: RecursiveExecutionArtifactKind::Inline,
            label: "scheduler-report".to_string(),
            content: Some(report_json),
            uri: None,
            metadata: json!({
                "artifact": "recursive_dag_scheduler_report",
                "run_id": report.run_id.to_string(),
                "max_steps": report.max_steps,
                "source": scheduler_run_source_key(report.source),
                "operator": report.operator.as_deref(),
                "stop_reason": report.stop_reason.as_str(),
                "cancellation_request_id": report.cancellation_request_id.map(|id| id.to_string()),
                "step_count": report.step_count,
            }),
        }],
    )?;
    Ok(artifacts.into_iter().next())
}

fn phase_order(phase: RecursiveAttemptPhase) -> u8 {
    match phase {
        RecursiveAttemptPhase::Execute => 0,
        RecursiveAttemptPhase::Integrate => 1,
    }
}

fn scheduler_run_source_key(source: RecursiveSchedulerRunSource) -> &'static str {
    match source {
        RecursiveSchedulerRunSource::TestHarness => "test_harness",
        RecursiveSchedulerRunSource::ManualRpc => "manual_rpc",
        RecursiveSchedulerRunSource::StartupRecovery => "startup_recovery",
        RecursiveSchedulerRunSource::FutureDaemonLoop => "future_daemon_loop",
    }
}

#[cfg(test)]
mod content_staleness_tests {
    use super::*;
    use rsi_common::recursive_dag::{
        RecursiveExecutionMode, RecursiveTaskEdge, RecursiveTaskEdgeKind, RecursiveTaskGraphDetail,
        RecursiveTaskGraphSummary,
    };

    fn graph_id() -> RecursiveTaskGraphId {
        RecursiveTaskGraphId::new()
    }

    fn node(
        id: RecursiveTaskId,
        gid: RecursiveTaskGraphId,
        objective: &str,
        status: RecursiveTaskLifecycleState,
    ) -> RecursiveTaskNode {
        let now = Utc::now();
        RecursiveTaskNode {
            id,
            graph_id: gid,
            parent_task_id: None,
            title: "task".to_string(),
            objective: objective.to_string(),
            scope: "scope".to_string(),
            acceptance_criteria: vec!["ac".to_string()],
            depth: 0,
            scope_units: 1,
            max_retries: 3,
            status,
            decomposed_once: false,
            integration_strategy: None,
            verification_strategy: None,
            blocked_reason: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn dep_edge(
        gid: RecursiveTaskGraphId,
        from: RecursiveTaskId,
        to: RecursiveTaskId,
    ) -> RecursiveTaskEdge {
        RecursiveTaskEdge {
            id: 0,
            graph_id: gid,
            from_task_id: from,
            to_task_id: to,
            kind: RecursiveTaskEdgeKind::Dependency,
            injection_batch_id: None,
            created_at: Utc::now(),
        }
    }

    fn detail(
        gid: RecursiveTaskGraphId,
        nodes: Vec<RecursiveTaskNode>,
        edges: Vec<RecursiveTaskEdge>,
    ) -> RecursiveTaskGraphDetail {
        let now = Utc::now();
        RecursiveTaskGraphDetail {
            graph: RecursiveTaskGraphSummary {
                id: gid,
                root_task_id: nodes[0].id,
                title: "g".to_string(),
                objective: "o".to_string(),
                status: RecursiveGraphStatus::Active,
                project_id: None,
                workflow_id: None,
                topology_id: None,
                parent_session_id: None,
                source_execution_id: None,
                source_eval_id: None,
                execution_mode: RecursiveExecutionMode::Fake,
                max_depth: 4,
                max_fanout: 4,
                max_descendants: 16,
                step_limit: 64,
                last_stop_reason: None,
                malformed_reason: None,
                created_at: now,
                updated_at: now,
                recovered_at: None,
                quarantined_at: None,
                quarantine_reason: None,
                recovery_checked_at: None,
            },
            nodes,
            edges,
            attempts: Vec::new(),
            injection_batches: Vec::new(),
            lifecycle_events: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    #[test]
    fn node_content_hash_changes_with_content() {
        let gid = graph_id();
        let id = RecursiveTaskId::new();
        let before = node(
            id,
            gid,
            "objective A",
            RecursiveTaskLifecycleState::Succeeded,
        );
        let after = node(
            id,
            gid,
            "objective B",
            RecursiveTaskLifecycleState::Succeeded,
        );
        assert_ne!(node_content_hash(&before), node_content_hash(&after));
    }

    /// Pinning gate (scheduler level): changing an upstream input's content
    /// marks ONLY its downstream node stale (must re-run) while a sibling whose
    /// upstream is unchanged stays fresh (hits the cache) — downstream-only.
    #[test]
    fn stale_upstream_forces_only_its_downstream_to_rerun() {
        let gid = graph_id();
        // Two independent chains: U1 -> D1 and U2 -> D2.
        let u1 = RecursiveTaskId::new();
        let u2 = RecursiveTaskId::new();
        let d1 = RecursiveTaskId::new();
        let d2 = RecursiveTaskId::new();

        // Baseline: capture upstream content hashes as they were when D1/D2 ran.
        let u1_before = node(
            u1,
            gid,
            "upstream one",
            RecursiveTaskLifecycleState::Succeeded,
        );
        let u2_before = node(
            u2,
            gid,
            "upstream two",
            RecursiveTaskLifecycleState::Succeeded,
        );
        let mut baseline: BTreeMap<RecursiveTaskId, String> = BTreeMap::new();
        baseline.insert(u1, node_content_hash(&u1_before));
        baseline.insert(u2, node_content_hash(&u2_before));

        // Now U1's content is edited; U2 is untouched (reuse the same node).
        let u1_after = node(
            u1,
            gid,
            "upstream one EDITED",
            RecursiveTaskLifecycleState::Succeeded,
        );
        let u2_after = u2_before;
        let d1_node = node(
            d1,
            gid,
            "downstream one",
            RecursiveTaskLifecycleState::Pending,
        );
        let d2_node = node(
            d2,
            gid,
            "downstream two",
            RecursiveTaskLifecycleState::Pending,
        );

        let det = detail(
            gid,
            vec![u1_after, u2_after, d1_node, d2_node],
            vec![dep_edge(gid, u1, d1), dep_edge(gid, u2, d2)],
        );

        // Detection primitive: D1 stale, D2 fresh (downstream-only).
        assert!(
            upstream_content_stale(&det, d1, &baseline),
            "D1 must be stale after its upstream U1 content changed"
        );
        assert!(
            !upstream_content_stale(&det, d2, &baseline),
            "D2 must stay fresh — its upstream U2 is unchanged"
        );

        // Wired gate: statuses show all upstreams Succeeded, so satisfaction is
        // decided purely by content. D2 (unchanged) is satisfied → hits cache;
        // D1 (stale) is NOT satisfied → must re-run.
        let statuses: BTreeMap<_, _> = det.nodes.iter().map(|n| (n.id, n.status)).collect();
        assert!(
            dependencies_satisfied(&statuses, &det, d2, &baseline),
            "unchanged input hits the cache (satisfied)"
        );
        assert!(
            !dependencies_satisfied(&statuses, &det, d1, &baseline),
            "changed input forces re-run (not satisfied on stale content)"
        );

        // Empty baseline == prior status-only behavior: both satisfied.
        let empty: BTreeMap<RecursiveTaskId, String> = BTreeMap::new();
        assert!(dependencies_satisfied(&statuses, &det, d1, &empty));
        assert!(dependencies_satisfied(&statuses, &det, d2, &empty));
    }

    #[test]
    fn missing_or_unchanged_baseline_is_fresh() {
        let gid = graph_id();
        let u = RecursiveTaskId::new();
        let d = RecursiveTaskId::new();
        let u_node = node(u, gid, "same", RecursiveTaskLifecycleState::Succeeded);
        let d_node = node(d, gid, "downstream", RecursiveTaskLifecycleState::Pending);
        let det = detail(gid, vec![u_node.clone(), d_node], vec![dep_edge(gid, u, d)]);

        // Missing baseline entry => fresh.
        let empty: BTreeMap<RecursiveTaskId, String> = BTreeMap::new();
        assert!(!upstream_content_stale(&det, d, &empty));

        // Matching baseline entry => fresh.
        let mut baseline: BTreeMap<RecursiveTaskId, String> = BTreeMap::new();
        baseline.insert(u, node_content_hash(&u_node));
        assert!(!upstream_content_stale(&det, d, &baseline));
    }
}
