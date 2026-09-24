//! Deterministic recursive-DAG capability gate for master-implement workflows.
//!
//! This module intentionally does not call an LLM and does not drive daemon
//! sessions. It proves the graph, lifecycle, decomposition, retry, and attempt
//! invariants needed before recursive master-implement execution can be wired
//! into production.

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use thiserror::Error;
use uuid::Uuid;

const SIM_NAMESPACE: Uuid = uuid::uuid!("7a4aeb5f-21d6-44c7-9e7e-69865f7f78f7");
const START_TIME_SECS: i64 = 1_735_689_600; // 2025-01-01T00:00:00Z

/// Stable id for a task node in the recursive task graph.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

impl TaskId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Deterministic id for simulation fixtures and scripted fake executors.
    #[must_use]
    pub fn stable(name: &str) -> Self {
        Self(Uuid::new_v5(&SIM_NAMESPACE, name.as_bytes()))
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable id for an inspectable execution attempt.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct AttemptId(pub Uuid);

impl AttemptId {
    #[must_use]
    fn from_sequence(sequence: u64) -> Self {
        Self(Uuid::new_v5(
            &SIM_NAMESPACE,
            format!("attempt:{sequence}").as_bytes(),
        ))
    }
}

/// Stable id for a committed child-injection transaction.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct InjectionBatchId(pub Uuid);

impl InjectionBatchId {
    #[must_use]
    fn from_sequence(sequence: u64) -> Self {
        Self(Uuid::new_v5(
            &SIM_NAMESPACE,
            format!("injection:{sequence}").as_bytes(),
        ))
    }
}

/// Conceptual lifecycle for recursive task execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLifecycleState {
    Pending,
    Planning,
    Ready,
    Running,
    Decomposed,
    BlockedOnChildren,
    Integrating,
    Verifying,
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}

impl TaskLifecycleState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Blocked | Self::Cancelled
        )
    }
}

/// Directed edge category. The graph must remain acyclic across all edge kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskEdgeKind {
    ParentChild,
    Dependency,
}

/// Durable directed task edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEdge {
    pub from: TaskId,
    pub to: TaskId,
    pub kind: TaskEdgeKind,
}

/// Retry and sizing budget carried by each task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBudget {
    pub scope_units: u32,
    pub max_retries: u32,
}

impl TaskBudget {
    #[must_use]
    pub const fn new(scope_units: u32, max_retries: u32) -> Self {
        Self {
            scope_units,
            max_retries,
        }
    }
}

/// Durable task/node representation used by the dry-run scheduler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: TaskId,
    #[serde(default)]
    pub parent_id: Option<TaskId>,
    pub title: String,
    pub objective: String,
    pub scope: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    pub depth: u32,
    pub budget: TaskBudget,
    pub status: TaskLifecycleState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub decomposed_once: bool,
    #[serde(default)]
    pub integration_strategy: Option<String>,
    #[serde(default)]
    pub verification_strategy: Option<String>,
    #[serde(default)]
    pub blocked_reason: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<ExecutionArtifact>,
}

impl TaskNode {
    #[must_use]
    pub fn from_spec(
        spec: TaskSpec,
        parent_id: Option<TaskId>,
        depth: u32,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            id: spec.id,
            parent_id,
            title: spec.title,
            objective: spec.objective,
            scope: spec.scope,
            acceptance_criteria: spec.acceptance_criteria,
            depth,
            budget: spec.budget,
            status: TaskLifecycleState::Pending,
            created_at: now,
            updated_at: now,
            decomposed_once: false,
            integration_strategy: None,
            verification_strategy: None,
            blocked_reason: None,
            artifacts: Vec::new(),
        }
    }
}

/// Input spec for root and child task creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub id: TaskId,
    pub title: String,
    pub objective: String,
    pub scope: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    pub budget: TaskBudget,
}

impl TaskSpec {
    #[must_use]
    pub fn new(
        id: TaskId,
        title: impl Into<String>,
        objective: impl Into<String>,
        scope: impl Into<String>,
        scope_units: u32,
        max_retries: u32,
    ) -> Self {
        Self {
            id,
            title: title.into(),
            objective: objective.into(),
            scope: scope.into(),
            acceptance_criteria: vec!["done".to_string()],
            budget: TaskBudget::new(scope_units, max_retries),
        }
    }
}

/// One child returned by a structured decomposition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildTaskSpec {
    pub task: TaskSpec,
    /// Prerequisite task ids that must succeed before this child can run.
    #[serde(default)]
    pub dependencies: Vec<TaskId>,
}

/// Structured decomposition result emitted by the fake master executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecompositionResult {
    pub parent_task_id: TaskId,
    pub reason_for_decomposition: String,
    pub children: Vec<ChildTaskSpec>,
    pub integration_strategy: String,
    pub verification_strategy: String,
}

/// Bounded decomposition constraints enforced transactionally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecompositionLimits {
    pub max_depth: u32,
    pub max_fanout: usize,
    pub max_descendants: usize,
}

impl Default for DecompositionLimits {
    fn default() -> Self {
        Self {
            max_depth: 4,
            max_fanout: 4,
            max_descendants: 16,
        }
    }
}

/// Artifact emitted by a deterministic attempt. Future production storage can
/// map this to session events, files, or daemon execution records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionArtifact {
    pub label: String,
    pub content: String,
}

impl ExecutionArtifact {
    #[must_use]
    pub fn new(label: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptPhase {
    Execute,
    Integrate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Running,
    Succeeded,
    Decomposed,
    Failed,
    Blocked,
    Cancelled,
}

/// Dependency states captured at attempt start for post-run inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencySnapshot {
    pub task_id: TaskId,
    pub status: TaskLifecycleState,
}

/// Inspectable execution attempt record. Failed attempts are never overwritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionAttempt {
    pub id: AttemptId,
    pub task_id: TaskId,
    pub phase: AttemptPhase,
    pub attempt_no: u32,
    /// Number of previous failed attempts for the same task and phase.
    pub retry_count: u32,
    pub status: AttemptStatus,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub block_reason: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<ExecutionArtifact>,
    #[serde(default)]
    pub dependency_snapshot: Vec<DependencySnapshot>,
}

/// Committed injection batch, proving all child rows and edges landed together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InjectionRecord {
    pub id: InjectionBatchId,
    pub parent_task_id: TaskId,
    pub child_task_ids: Vec<TaskId>,
    pub edge_count: usize,
    pub committed_at: DateTime<Utc>,
}

/// Lifecycle transition audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleEvent {
    pub task_id: TaskId,
    pub from: TaskLifecycleState,
    pub to: TaskLifecycleState,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Serializable in-memory graph used by the capability gate.
#[derive(Debug, Clone, Serialize)]
pub struct DynamicTaskGraph {
    tasks: BTreeMap<TaskId, TaskNode>,
    edges: Vec<TaskEdge>,
    attempts: Vec<ExecutionAttempt>,
    injection_records: Vec<InjectionRecord>,
    lifecycle_events: Vec<LifecycleEvent>,
    next_attempt_sequence: u64,
    next_injection_sequence: u64,
    clock_tick: i64,
}

#[derive(Deserialize)]
struct DynamicTaskGraphWire {
    pub tasks: BTreeMap<TaskId, TaskNode>,
    pub edges: Vec<TaskEdge>,
    pub attempts: Vec<ExecutionAttempt>,
    pub injection_records: Vec<InjectionRecord>,
    pub lifecycle_events: Vec<LifecycleEvent>,
    #[serde(default)]
    next_attempt_sequence: u64,
    #[serde(default)]
    next_injection_sequence: u64,
    #[serde(default)]
    clock_tick: i64,
}

impl<'de> Deserialize<'de> for DynamicTaskGraph {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = DynamicTaskGraphWire::deserialize(deserializer)?;
        let attempt_len = wire.attempts.len();
        let injection_len = wire.injection_records.len();
        let mut graph = Self {
            tasks: wire.tasks,
            edges: wire.edges,
            attempts: wire.attempts,
            injection_records: wire.injection_records,
            lifecycle_events: wire.lifecycle_events,
            next_attempt_sequence: wire
                .next_attempt_sequence
                .max(saturating_usize_to_u64(attempt_len.saturating_add(1))),
            next_injection_sequence: wire
                .next_injection_sequence
                .max(saturating_usize_to_u64(injection_len.saturating_add(1))),
            clock_tick: wire.clock_tick,
        };
        graph.skip_existing_generated_ids();
        graph
            .validate_integrity()
            .map_err(serde::de::Error::custom)?;
        Ok(graph)
    }
}

impl DynamicTaskGraph {
    #[must_use]
    pub fn with_root(root: TaskSpec) -> Self {
        let mut graph = Self {
            tasks: BTreeMap::new(),
            edges: Vec::new(),
            attempts: Vec::new(),
            injection_records: Vec::new(),
            lifecycle_events: Vec::new(),
            next_attempt_sequence: 1,
            next_injection_sequence: 1,
            clock_tick: 0,
        };
        let now = graph.next_time();
        graph
            .tasks
            .insert(root.id, TaskNode::from_spec(root, None, 0, now));
        graph
    }

    #[must_use]
    pub fn task(&self, task_id: TaskId) -> Option<&TaskNode> {
        self.tasks.get(&task_id)
    }

    #[must_use]
    pub fn attempts_for_task(&self, task_id: TaskId) -> Vec<&ExecutionAttempt> {
        self.attempts
            .iter()
            .filter(|attempt| attempt.task_id == task_id)
            .collect()
    }

    #[must_use]
    pub fn attempts_for_task_phase(
        &self,
        task_id: TaskId,
        phase: AttemptPhase,
    ) -> Vec<&ExecutionAttempt> {
        self.attempts
            .iter()
            .filter(|attempt| attempt.task_id == task_id && attempt.phase == phase)
            .collect()
    }

    #[must_use]
    pub fn dependency_ids(&self, task_id: TaskId) -> Vec<TaskId> {
        self.edges
            .iter()
            .filter(|edge| edge.kind == TaskEdgeKind::Dependency && edge.to == task_id)
            .map(|edge| edge.from)
            .collect()
    }

    #[must_use]
    pub fn child_ids(&self, parent_id: TaskId) -> Vec<TaskId> {
        self.edges
            .iter()
            .filter(|edge| edge.kind == TaskEdgeKind::ParentChild && edge.from == parent_id)
            .map(|edge| edge.to)
            .collect()
    }

    #[must_use]
    pub fn all_terminal(&self) -> bool {
        self.tasks.values().all(|task| task.status.is_terminal())
    }

    #[must_use]
    pub fn is_acyclic(&self) -> bool {
        validate_edges_acyclic(self.tasks.keys().copied(), &self.edges).is_ok()
    }

    /// Transactionally inject children from a structured decomposition result.
    ///
    /// # Errors
    ///
    /// Returns [`GateError`] if the parent is missing or already decomposed, the
    /// proposed children violate size/depth/fanout/descendant bounds, any
    /// dependency is invalid, or the candidate graph would become cyclic.
    #[allow(clippy::too_many_lines)]
    pub fn inject_decomposition(
        &mut self,
        result: DecompositionResult,
        limits: DecompositionLimits,
    ) -> Result<InjectionRecord, GateError> {
        let parent = self
            .tasks
            .get(&result.parent_task_id)
            .ok_or(GateError::TaskNotFound(result.parent_task_id))?;

        if parent.decomposed_once {
            return Err(GateError::AlreadyDecomposed(result.parent_task_id));
        }

        if result.children.is_empty() {
            return Err(GateError::EmptyDecomposition(result.parent_task_id));
        }

        if result.children.len() > limits.max_fanout {
            return Err(GateError::FanoutExceeded {
                parent_task_id: result.parent_task_id,
                actual: result.children.len(),
                max: limits.max_fanout,
            });
        }

        let child_depth = parent.depth.saturating_add(1);
        if child_depth > limits.max_depth {
            return Err(GateError::DepthExceeded {
                parent_task_id: result.parent_task_id,
                child_depth,
                max: limits.max_depth,
            });
        }

        let existing_descendants = self.descendant_count(result.parent_task_id);
        if existing_descendants + result.children.len() > limits.max_descendants {
            return Err(GateError::DescendantLimitExceeded {
                parent_task_id: result.parent_task_id,
                actual: existing_descendants + result.children.len(),
                max: limits.max_descendants,
            });
        }

        let mut candidate_tasks = self.tasks.clone();
        let mut candidate_edges = self.edges.clone();
        let mut child_ids = Vec::with_capacity(result.children.len());
        let mut seen_child_ids = BTreeSet::new();
        let existing_ids: BTreeSet<TaskId> = self.tasks.keys().copied().collect();

        for child in &result.children {
            validate_child_smaller(parent, child)?;
            validate_child_fields(child)?;

            let child_id = child.task.id;
            if existing_ids.contains(&child_id) || !seen_child_ids.insert(child_id) {
                return Err(GateError::DuplicateTaskId(child_id));
            }

            let now = self.peek_time();
            candidate_tasks.insert(
                child_id,
                TaskNode::from_spec(
                    child.task.clone(),
                    Some(result.parent_task_id),
                    child_depth,
                    now,
                ),
            );
            candidate_edges.push(TaskEdge {
                from: result.parent_task_id,
                to: child_id,
                kind: TaskEdgeKind::ParentChild,
            });
            child_ids.push(child_id);
        }

        let candidate_ids: BTreeSet<TaskId> = candidate_tasks.keys().copied().collect();
        for child in &result.children {
            for dependency in &child.dependencies {
                if *dependency == result.parent_task_id {
                    return Err(GateError::ParentDependencyRejected {
                        child_task_id: child.task.id,
                        parent_task_id: result.parent_task_id,
                    });
                }
                if !candidate_ids.contains(dependency) {
                    return Err(GateError::UnknownDependency {
                        child_task_id: child.task.id,
                        dependency: *dependency,
                    });
                }
                candidate_edges.push(TaskEdge {
                    from: *dependency,
                    to: child.task.id,
                    kind: TaskEdgeKind::Dependency,
                });
            }
        }

        validate_edges_reference_existing_nodes(candidate_tasks.keys().copied(), &candidate_edges)?;
        validate_edges_acyclic(candidate_tasks.keys().copied(), &candidate_edges)?;

        for bounded_parent_id in self.ancestor_chain_including(result.parent_task_id) {
            let actual = descendant_count_in_edges(&candidate_edges, bounded_parent_id);
            if actual > limits.max_descendants {
                return Err(GateError::DescendantLimitExceeded {
                    parent_task_id: bounded_parent_id,
                    actual,
                    max: limits.max_descendants,
                });
            }
        }

        self.tasks = candidate_tasks;
        self.edges = candidate_edges;
        let committed_at = self.next_time();
        if let Some(parent) = self.tasks.get_mut(&result.parent_task_id) {
            parent.decomposed_once = true;
            parent.integration_strategy = Some(result.integration_strategy);
            parent.verification_strategy = Some(result.verification_strategy);
            parent.updated_at = committed_at;
        }

        let record = InjectionRecord {
            id: self.next_injection_id(),
            parent_task_id: result.parent_task_id,
            child_task_ids: child_ids,
            edge_count: result.children.len()
                + result
                    .children
                    .iter()
                    .map(|child| child.dependencies.len())
                    .sum::<usize>(),
            committed_at,
        };
        self.injection_records.push(record.clone());
        Ok(record)
    }

    /// Explicitly reopen a parent for a future decomposition pass.
    ///
    /// The dry-run scheduler never calls this automatically; repeated
    /// decomposition is impossible unless the caller makes this state change.
    ///
    /// # Errors
    ///
    /// Returns [`GateError::TaskNotFound`] if `task_id` does not exist.
    pub fn reopen_for_decomposition(
        &mut self,
        task_id: TaskId,
        reason: impl Into<String>,
    ) -> Result<(), GateError> {
        let reason = reason.into();
        let task = self
            .tasks
            .get_mut(&task_id)
            .ok_or(GateError::TaskNotFound(task_id))?;
        task.decomposed_once = false;
        task.blocked_reason = None;
        self.transition_task(task_id, TaskLifecycleState::Ready, Some(reason))
    }

    fn promote_unblocked(&mut self) -> Result<(), GateError> {
        let task_ids: Vec<TaskId> = self.tasks.keys().copied().collect();
        for task_id in task_ids {
            let status = self
                .tasks
                .get(&task_id)
                .ok_or(GateError::TaskNotFound(task_id))?
                .status;

            match status {
                TaskLifecycleState::Pending => {
                    if self.dependencies_succeeded(task_id) {
                        self.transition_task(task_id, TaskLifecycleState::Ready, None)?;
                    } else if let Some(failed_dependency) = self.failed_dependency(task_id) {
                        self.transition_task(
                            task_id,
                            TaskLifecycleState::Blocked,
                            Some(format!("dependency {failed_dependency} did not succeed")),
                        )?;
                    }
                }
                TaskLifecycleState::Decomposed => {
                    self.transition_task(task_id, TaskLifecycleState::BlockedOnChildren, None)?;
                }
                TaskLifecycleState::BlockedOnChildren => {
                    let children = self.child_ids(task_id);
                    if children.is_empty() {
                        continue;
                    }

                    if children.iter().all(|child_id| {
                        self.tasks
                            .get(child_id)
                            .is_some_and(|task| task.status == TaskLifecycleState::Succeeded)
                    }) {
                        self.transition_task(task_id, TaskLifecycleState::Integrating, None)?;
                    } else if let Some(failed_child) = children.iter().find(|child_id| {
                        self.tasks.get(child_id).is_some_and(|task| {
                            matches!(
                                task.status,
                                TaskLifecycleState::Failed
                                    | TaskLifecycleState::Blocked
                                    | TaskLifecycleState::Cancelled
                            )
                        })
                    }) {
                        self.transition_task(
                            task_id,
                            TaskLifecycleState::Blocked,
                            Some(format!("child {failed_child} did not succeed")),
                        )?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn next_runnable(&self) -> Option<(TaskId, AttemptPhase)> {
        self.tasks
            .iter()
            .find_map(|(task_id, task)| match task.status {
                TaskLifecycleState::Ready
                    if self.dependencies_succeeded(*task_id)
                        && (self.child_ids(*task_id).is_empty()
                            || (!task.decomposed_once && self.children_succeeded(*task_id))) =>
                {
                    Some((*task_id, AttemptPhase::Execute))
                }
                TaskLifecycleState::Integrating
                    if self.dependencies_succeeded(*task_id)
                        && self.children_succeeded(*task_id) =>
                {
                    Some((*task_id, AttemptPhase::Integrate))
                }
                _ => None,
            })
    }

    fn start_attempt(
        &mut self,
        task_id: TaskId,
        phase: AttemptPhase,
    ) -> Result<AttemptId, GateError> {
        if !self.tasks.contains_key(&task_id) {
            return Err(GateError::TaskNotFound(task_id));
        }

        let attempt_no = saturating_usize_to_u32(
            self.attempts
                .iter()
                .filter(|attempt| attempt.task_id == task_id && attempt.phase == phase)
                .count()
                .saturating_add(1),
        );
        let retry_count = saturating_usize_to_u32(
            self.attempts
                .iter()
                .filter(|attempt| {
                    attempt.task_id == task_id
                        && attempt.phase == phase
                        && attempt.status == AttemptStatus::Failed
                })
                .count(),
        );
        let dependency_snapshot = self
            .dependency_ids(task_id)
            .into_iter()
            .filter_map(|dep_id| {
                self.tasks.get(&dep_id).map(|task| DependencySnapshot {
                    task_id: dep_id,
                    status: task.status,
                })
            })
            .collect();
        let attempt_id = self.next_attempt_id();
        let started_at = self.next_time();
        self.attempts.push(ExecutionAttempt {
            id: attempt_id,
            task_id,
            phase,
            attempt_no,
            retry_count,
            status: AttemptStatus::Running,
            started_at,
            finished_at: None,
            failure_reason: None,
            block_reason: None,
            artifacts: Vec::new(),
            dependency_snapshot,
        });
        Ok(attempt_id)
    }

    fn finish_attempt(
        &mut self,
        attempt_id: AttemptId,
        status: AttemptStatus,
        reason: Option<String>,
        artifacts: Vec<ExecutionArtifact>,
    ) -> Result<(), GateError> {
        let finished_at = self.next_time();
        let attempt = self
            .attempts
            .iter_mut()
            .find(|attempt| attempt.id == attempt_id)
            .ok_or(GateError::AttemptNotFound(attempt_id))?;
        attempt.status = status;
        attempt.finished_at = Some(finished_at);
        match status {
            AttemptStatus::Failed => attempt.failure_reason = reason,
            AttemptStatus::Blocked => attempt.block_reason = reason,
            _ => {}
        }
        attempt.artifacts = artifacts;
        Ok(())
    }

    fn transition_task(
        &mut self,
        task_id: TaskId,
        to: TaskLifecycleState,
        reason: Option<String>,
    ) -> Result<(), GateError> {
        let at = self.next_time();
        let task = self
            .tasks
            .get_mut(&task_id)
            .ok_or(GateError::TaskNotFound(task_id))?;
        let from = task.status;
        if from == to {
            return Ok(());
        }
        task.status = to;
        task.updated_at = at;
        if matches!(to, TaskLifecycleState::Blocked | TaskLifecycleState::Failed) {
            task.blocked_reason.clone_from(&reason);
        }
        self.lifecycle_events.push(LifecycleEvent {
            task_id,
            from,
            to,
            at,
            reason,
        });
        Ok(())
    }

    fn attach_artifacts(
        &mut self,
        task_id: TaskId,
        artifacts: &[ExecutionArtifact],
    ) -> Result<(), GateError> {
        let task = self
            .tasks
            .get_mut(&task_id)
            .ok_or(GateError::TaskNotFound(task_id))?;
        task.artifacts.extend(artifacts.iter().cloned());
        Ok(())
    }

    fn dependencies_succeeded(&self, task_id: TaskId) -> bool {
        self.dependency_ids(task_id).iter().all(|dep_id| {
            self.tasks
                .get(dep_id)
                .is_some_and(|task| task.status == TaskLifecycleState::Succeeded)
        })
    }

    fn failed_dependency(&self, task_id: TaskId) -> Option<TaskId> {
        self.dependency_ids(task_id).into_iter().find(|dep_id| {
            self.tasks.get(dep_id).is_some_and(|task| {
                matches!(
                    task.status,
                    TaskLifecycleState::Failed
                        | TaskLifecycleState::Blocked
                        | TaskLifecycleState::Cancelled
                )
            })
        })
    }

    fn children_succeeded(&self, task_id: TaskId) -> bool {
        let children = self.child_ids(task_id);
        !children.is_empty()
            && children.iter().all(|child_id| {
                self.tasks
                    .get(child_id)
                    .is_some_and(|task| task.status == TaskLifecycleState::Succeeded)
            })
    }

    fn descendant_count(&self, parent_id: TaskId) -> usize {
        descendant_count_in_edges(&self.edges, parent_id)
    }

    fn ancestor_chain_including(&self, task_id: TaskId) -> Vec<TaskId> {
        let mut ids = Vec::new();
        let mut current = Some(task_id);
        while let Some(current_id) = current {
            ids.push(current_id);
            current = self.tasks.get(&current_id).and_then(|task| task.parent_id);
        }
        ids
    }

    fn next_attempt_id(&mut self) -> AttemptId {
        loop {
            let id = AttemptId::from_sequence(self.next_attempt_sequence);
            self.next_attempt_sequence += 1;
            if !self.attempts.iter().any(|attempt| attempt.id == id) {
                return id;
            }
        }
    }

    fn next_injection_id(&mut self) -> InjectionBatchId {
        loop {
            let id = InjectionBatchId::from_sequence(self.next_injection_sequence);
            self.next_injection_sequence += 1;
            if !self.injection_records.iter().any(|record| record.id == id) {
                return id;
            }
        }
    }

    fn skip_existing_generated_ids(&mut self) {
        while self
            .attempts
            .iter()
            .any(|attempt| attempt.id == AttemptId::from_sequence(self.next_attempt_sequence))
        {
            self.next_attempt_sequence += 1;
        }
        while self.injection_records.iter().any(|record| {
            record.id == InjectionBatchId::from_sequence(self.next_injection_sequence)
        }) {
            self.next_injection_sequence += 1;
        }
    }

    #[allow(clippy::too_many_lines)]
    fn validate_integrity(&self) -> Result<(), GateError> {
        validate_edges_reference_existing_nodes(self.tasks.keys().copied(), &self.edges)?;
        validate_edges_acyclic(self.tasks.keys().copied(), &self.edges)?;

        let mut edge_keys = BTreeSet::new();
        for edge in &self.edges {
            let edge_kind = match edge.kind {
                TaskEdgeKind::ParentChild => 0u8,
                TaskEdgeKind::Dependency => 1u8,
            };
            if !edge_keys.insert((edge.from, edge.to, edge_kind)) {
                return Err(GateError::DuplicateEdge {
                    from: edge.from,
                    to: edge.to,
                    kind: edge.kind,
                });
            }
        }

        let root_count = self
            .tasks
            .values()
            .filter(|task| task.parent_id.is_none())
            .count();
        if root_count != 1 {
            return Err(GateError::InvalidRootCount(root_count));
        }

        for task in self.tasks.values() {
            if let Some(parent_id) = task.parent_id {
                let parent = self
                    .tasks
                    .get(&parent_id)
                    .ok_or(GateError::TaskNotFound(parent_id))?;
                if task.depth != parent.depth.saturating_add(1) {
                    return Err(GateError::InvalidTaskDepth {
                        task_id: task.id,
                        expected: parent.depth.saturating_add(1),
                        actual: task.depth,
                    });
                }
                if !self.edges.iter().any(|edge| {
                    edge.kind == TaskEdgeKind::ParentChild
                        && edge.from == parent_id
                        && edge.to == task.id
                }) {
                    return Err(GateError::MissingParentChildEdge {
                        parent_task_id: parent_id,
                        child_task_id: task.id,
                    });
                }
            }
        }

        for edge in &self.edges {
            if edge.kind == TaskEdgeKind::ParentChild {
                let child = self
                    .tasks
                    .get(&edge.to)
                    .ok_or(GateError::TaskNotFound(edge.to))?;
                if child.parent_id != Some(edge.from) {
                    return Err(GateError::InvalidParentLink {
                        parent_task_id: edge.from,
                        child_task_id: edge.to,
                    });
                }
            }
        }

        let mut attempt_ids = BTreeSet::new();
        for attempt in &self.attempts {
            if !self.tasks.contains_key(&attempt.task_id) {
                return Err(GateError::TaskNotFound(attempt.task_id));
            }
            if !attempt_ids.insert(attempt.id) {
                return Err(GateError::DuplicateAttemptId(attempt.id));
            }
            match attempt.status {
                AttemptStatus::Running if attempt.finished_at.is_some() => {
                    return Err(GateError::InvalidAttemptState(attempt.id));
                }
                AttemptStatus::Running => {}
                _ if attempt.finished_at.is_none() => {
                    return Err(GateError::InvalidAttemptState(attempt.id));
                }
                _ => {}
            }
        }

        let mut injection_ids = BTreeSet::new();
        for record in &self.injection_records {
            if !self.tasks.contains_key(&record.parent_task_id) {
                return Err(GateError::TaskNotFound(record.parent_task_id));
            }
            if !injection_ids.insert(record.id) {
                return Err(GateError::DuplicateInjectionBatchId(record.id));
            }
            if let Some(missing) = record
                .child_task_ids
                .iter()
                .find(|child_id| !self.tasks.contains_key(child_id))
                .copied()
            {
                return Err(GateError::TaskNotFound(missing));
            }
            let parent_child_edges = record
                .child_task_ids
                .iter()
                .filter(|child_id| {
                    self.edges.iter().any(|edge| {
                        edge.kind == TaskEdgeKind::ParentChild
                            && edge.from == record.parent_task_id
                            && edge.to == **child_id
                    })
                })
                .count();
            if parent_child_edges != record.child_task_ids.len()
                || record.edge_count < record.child_task_ids.len()
            {
                return Err(GateError::InvalidInjectionRecord(record.id));
            }
        }

        Ok(())
    }

    fn next_time(&mut self) -> DateTime<Utc> {
        let base = Utc
            .timestamp_opt(START_TIME_SECS, 0)
            .single()
            .unwrap_or_else(Utc::now);
        let at = base + Duration::seconds(self.clock_tick);
        self.clock_tick += 1;
        at
    }

    fn peek_time(&self) -> DateTime<Utc> {
        let base = Utc
            .timestamp_opt(START_TIME_SECS, 0)
            .single()
            .unwrap_or_else(Utc::now);
        base + Duration::seconds(self.clock_tick)
    }
}

fn descendant_count_in_edges(edges: &[TaskEdge], parent_id: TaskId) -> usize {
    let mut count = 0usize;
    let mut queue: VecDeque<TaskId> = edges
        .iter()
        .filter(|edge| edge.kind == TaskEdgeKind::ParentChild && edge.from == parent_id)
        .map(|edge| edge.to)
        .collect();
    while let Some(task_id) = queue.pop_front() {
        count += 1;
        for child_id in edges
            .iter()
            .filter(|edge| edge.kind == TaskEdgeKind::ParentChild && edge.from == task_id)
            .map(|edge| edge.to)
        {
            queue.push_back(child_id);
        }
    }
    count
}

/// Dry-run scheduler configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub decomposition_limits: DecompositionLimits,
    pub max_steps: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            decomposition_limits: DecompositionLimits::default(),
            max_steps: 128,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerStopReason {
    GraphTerminal,
    IdleNoRunnable,
    StepLimitExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerRun {
    pub stop_reason: SchedulerStopReason,
    pub steps: usize,
}

/// Deterministic scheduler over an acyclic dynamic task graph.
pub struct DryRunScheduler<E> {
    executor: E,
    config: SchedulerConfig,
}

impl<E> DryRunScheduler<E>
where
    E: MasterImplementExecutor,
{
    #[must_use]
    pub const fn new(executor: E, config: SchedulerConfig) -> Self {
        Self { executor, config }
    }

    /// Run the deterministic scheduler until the graph is terminal, no work is
    /// runnable, or `max_steps` is reached.
    ///
    /// # Errors
    ///
    /// Returns [`GateError`] when lifecycle transitions, attempt recording, or
    /// transactional child injection encounter invalid graph state.
    pub fn run(&mut self, graph: &mut DynamicTaskGraph) -> Result<SchedulerRun, GateError> {
        for step in 0..self.config.max_steps {
            graph.promote_unblocked()?;

            if graph.all_terminal() {
                return Ok(SchedulerRun {
                    stop_reason: SchedulerStopReason::GraphTerminal,
                    steps: step,
                });
            }

            let Some((task_id, phase)) = graph.next_runnable() else {
                return Ok(SchedulerRun {
                    stop_reason: SchedulerStopReason::IdleNoRunnable,
                    steps: step,
                });
            };

            self.run_one(graph, task_id, phase)?;
        }

        Ok(SchedulerRun {
            stop_reason: SchedulerStopReason::StepLimitExceeded,
            steps: self.config.max_steps,
        })
    }

    fn run_one(
        &mut self,
        graph: &mut DynamicTaskGraph,
        task_id: TaskId,
        phase: AttemptPhase,
    ) -> Result<(), GateError> {
        match phase {
            AttemptPhase::Execute => {
                graph.transition_task(task_id, TaskLifecycleState::Planning, None)?;
                graph.transition_task(task_id, TaskLifecycleState::Running, None)?;
            }
            AttemptPhase::Integrate => {
                graph.transition_task(task_id, TaskLifecycleState::Integrating, None)?;
            }
        }

        let attempt_id = graph.start_attempt(task_id, phase)?;
        let outcome = self.executor.execute(graph, task_id, phase);

        match outcome {
            ExecutorOutcome::Succeeded { artifacts } => {
                graph.finish_attempt(
                    attempt_id,
                    AttemptStatus::Succeeded,
                    None,
                    artifacts.clone(),
                )?;
                graph.attach_artifacts(task_id, &artifacts)?;
                graph.transition_task(task_id, TaskLifecycleState::Verifying, None)?;
                graph.transition_task(task_id, TaskLifecycleState::Succeeded, None)?;
            }
            ExecutorOutcome::Decomposed(result) => {
                match graph.inject_decomposition(result, self.config.decomposition_limits) {
                    Ok(_) => {
                        graph.finish_attempt(
                            attempt_id,
                            AttemptStatus::Decomposed,
                            None,
                            vec![],
                        )?;
                        graph.transition_task(task_id, TaskLifecycleState::Decomposed, None)?;
                        graph.transition_task(
                            task_id,
                            TaskLifecycleState::BlockedOnChildren,
                            None,
                        )?;
                    }
                    Err(err) => {
                        let reason = err.to_string();
                        graph.finish_attempt(
                            attempt_id,
                            AttemptStatus::Failed,
                            Some(reason.clone()),
                            vec![],
                        )?;
                        graph.transition_task(task_id, TaskLifecycleState::Failed, Some(reason))?;
                    }
                }
            }
            ExecutorOutcome::Failed { reason, retryable } => {
                graph.finish_attempt(
                    attempt_id,
                    AttemptStatus::Failed,
                    Some(reason.clone()),
                    vec![],
                )?;
                let failed_count = saturating_usize_to_u32(
                    graph
                        .attempts_for_task_phase(task_id, phase)
                        .into_iter()
                        .filter(|attempt| attempt.status == AttemptStatus::Failed)
                        .count(),
                );
                let max_retries = graph
                    .task(task_id)
                    .ok_or(GateError::TaskNotFound(task_id))?
                    .budget
                    .max_retries;
                if retryable && failed_count <= max_retries {
                    let retry_state = match phase {
                        AttemptPhase::Execute => TaskLifecycleState::Ready,
                        AttemptPhase::Integrate => TaskLifecycleState::Integrating,
                    };
                    graph.transition_task(task_id, retry_state, Some(reason))?;
                } else {
                    graph.transition_task(task_id, TaskLifecycleState::Failed, Some(reason))?;
                }
            }
            ExecutorOutcome::Blocked { reason } => {
                graph.finish_attempt(
                    attempt_id,
                    AttemptStatus::Blocked,
                    Some(reason.clone()),
                    vec![],
                )?;
                graph.transition_task(task_id, TaskLifecycleState::Blocked, Some(reason))?;
            }
            ExecutorOutcome::Cancelled { reason } => {
                graph.finish_attempt(
                    attempt_id,
                    AttemptStatus::Cancelled,
                    Some(reason.clone()),
                    vec![],
                )?;
                graph.transition_task(task_id, TaskLifecycleState::Cancelled, Some(reason))?;
            }
        }

        Ok(())
    }
}

/// Fake executor contract used by the dry-run scheduler.
pub trait MasterImplementExecutor {
    fn execute(
        &mut self,
        graph: &DynamicTaskGraph,
        task_id: TaskId,
        phase: AttemptPhase,
    ) -> ExecutorOutcome;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutorOutcome {
    Succeeded { artifacts: Vec<ExecutionArtifact> },
    Decomposed(DecompositionResult),
    Failed { reason: String, retryable: bool },
    Blocked { reason: String },
    Cancelled { reason: String },
}

/// Deterministic fake behavior for one executor phase.
#[derive(Debug, Clone)]
pub enum FakeBehavior {
    DirectSuccess {
        artifact_label: String,
    },
    Decompose(DecompositionResult),
    FailOnceThenSucceed {
        failure_reason: String,
        artifact_label: String,
    },
    FailPermanently {
        reason: String,
    },
    Cancel {
        reason: String,
    },
    Block {
        reason: String,
    },
}

impl FakeBehavior {
    #[must_use]
    pub fn direct(label: impl Into<String>) -> Self {
        Self::DirectSuccess {
            artifact_label: label.into(),
        }
    }
}

/// Scripted fake executor. Missing integrate behavior defaults to direct success;
/// missing execute behavior blocks visibly instead of succeeding silently.
#[derive(Debug, Clone, Default)]
pub struct FakeMasterImplementExecutor {
    execute_behaviors: BTreeMap<TaskId, FakeBehavior>,
    integrate_behaviors: BTreeMap<TaskId, FakeBehavior>,
    call_counts: HashMap<(TaskId, AttemptPhase), u32>,
}

impl FakeMasterImplementExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn on_execute(mut self, task_id: TaskId, behavior: FakeBehavior) -> Self {
        self.execute_behaviors.insert(task_id, behavior);
        self
    }

    #[must_use]
    pub fn on_integrate(mut self, task_id: TaskId, behavior: FakeBehavior) -> Self {
        self.integrate_behaviors.insert(task_id, behavior);
        self
    }
}

impl MasterImplementExecutor for FakeMasterImplementExecutor {
    fn execute(
        &mut self,
        _graph: &DynamicTaskGraph,
        task_id: TaskId,
        phase: AttemptPhase,
    ) -> ExecutorOutcome {
        let count = self.call_counts.entry((task_id, phase)).or_insert(0);
        *count += 1;

        let behavior = match phase {
            AttemptPhase::Execute => self.execute_behaviors.get(&task_id),
            AttemptPhase::Integrate => self.integrate_behaviors.get(&task_id),
        };

        let default_integrate = FakeBehavior::direct(format!("integrated:{task_id}"));
        let missing_execute = FakeBehavior::Block {
            reason: format!("no fake execute behavior for task {task_id}"),
        };
        let selected = match (phase, behavior) {
            (_, Some(behavior)) => behavior,
            (AttemptPhase::Integrate, None) => &default_integrate,
            (AttemptPhase::Execute, None) => &missing_execute,
        };

        match selected {
            FakeBehavior::DirectSuccess { artifact_label } => ExecutorOutcome::Succeeded {
                artifacts: vec![ExecutionArtifact::new(
                    artifact_label.clone(),
                    format!("task {task_id} {phase:?} succeeded"),
                )],
            },
            FakeBehavior::Decompose(result) => ExecutorOutcome::Decomposed(result.clone()),
            FakeBehavior::FailOnceThenSucceed {
                failure_reason,
                artifact_label,
            } => {
                if *count == 1 {
                    ExecutorOutcome::Failed {
                        reason: failure_reason.clone(),
                        retryable: true,
                    }
                } else {
                    ExecutorOutcome::Succeeded {
                        artifacts: vec![ExecutionArtifact::new(
                            artifact_label.clone(),
                            format!("task {task_id} retry succeeded"),
                        )],
                    }
                }
            }
            FakeBehavior::FailPermanently { reason } => ExecutorOutcome::Failed {
                reason: reason.clone(),
                retryable: true,
            },
            FakeBehavior::Cancel { reason } => ExecutorOutcome::Cancelled {
                reason: reason.clone(),
            },
            FakeBehavior::Block { reason } => ExecutorOutcome::Blocked {
                reason: reason.clone(),
            },
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GateError {
    #[error("task not found: {0}")]
    TaskNotFound(TaskId),
    #[error("attempt not found: {0:?}")]
    AttemptNotFound(AttemptId),
    #[error("duplicate task id: {0}")]
    DuplicateTaskId(TaskId),
    #[error("decomposition for parent {0} must include at least one child")]
    EmptyDecomposition(TaskId),
    #[error("parent {parent_task_id} fanout {actual} exceeds max {max}")]
    FanoutExceeded {
        parent_task_id: TaskId,
        actual: usize,
        max: usize,
    },
    #[error("parent {parent_task_id} child depth {child_depth} exceeds max {max}")]
    DepthExceeded {
        parent_task_id: TaskId,
        child_depth: u32,
        max: u32,
    },
    #[error("parent {parent_task_id} descendant count {actual} exceeds max {max}")]
    DescendantLimitExceeded {
        parent_task_id: TaskId,
        actual: usize,
        max: usize,
    },
    #[error("child {child_task_id} scope must be smaller than parent {parent_task_id}")]
    ChildNotSmaller {
        parent_task_id: TaskId,
        child_task_id: TaskId,
    },
    #[error("child {child_task_id} missing required field: {field}")]
    MissingChildField {
        child_task_id: TaskId,
        field: &'static str,
    },
    #[error("child {child_task_id} depends on unknown task {dependency}")]
    UnknownDependency {
        child_task_id: TaskId,
        dependency: TaskId,
    },
    #[error("child {child_task_id} may not depend on its parent {parent_task_id}")]
    ParentDependencyRejected {
        child_task_id: TaskId,
        parent_task_id: TaskId,
    },
    #[error("edge references unknown task {0}")]
    EdgeReferencesUnknownTask(TaskId),
    #[error("task graph would contain a cycle")]
    CycleDetected,
    #[error("parent {0} already decomposed; explicit reopen required")]
    AlreadyDecomposed(TaskId),
    #[error("duplicate edge {kind:?} {from} -> {to}")]
    DuplicateEdge {
        from: TaskId,
        to: TaskId,
        kind: TaskEdgeKind,
    },
    #[error("serialized graph must contain exactly one root, found {0}")]
    InvalidRootCount(usize),
    #[error("task {task_id} depth {actual} did not match expected {expected}")]
    InvalidTaskDepth {
        task_id: TaskId,
        expected: u32,
        actual: u32,
    },
    #[error("task {child_task_id} is missing parent-child edge from {parent_task_id}")]
    MissingParentChildEdge {
        parent_task_id: TaskId,
        child_task_id: TaskId,
    },
    #[error("parent-child edge {parent_task_id} -> {child_task_id} does not match child parent_id")]
    InvalidParentLink {
        parent_task_id: TaskId,
        child_task_id: TaskId,
    },
    #[error("duplicate attempt id: {0:?}")]
    DuplicateAttemptId(AttemptId),
    #[error("invalid serialized attempt state: {0:?}")]
    InvalidAttemptState(AttemptId),
    #[error("duplicate injection batch id: {0:?}")]
    DuplicateInjectionBatchId(InjectionBatchId),
    #[error("invalid serialized injection record: {0:?}")]
    InvalidInjectionRecord(InjectionBatchId),
}

const fn validate_child_smaller(parent: &TaskNode, child: &ChildTaskSpec) -> Result<(), GateError> {
    if child.task.budget.scope_units >= parent.budget.scope_units {
        return Err(GateError::ChildNotSmaller {
            parent_task_id: parent.id,
            child_task_id: child.task.id,
        });
    }
    Ok(())
}

fn validate_child_fields(child: &ChildTaskSpec) -> Result<(), GateError> {
    for (field, value) in [
        ("title", child.task.title.as_str()),
        ("objective", child.task.objective.as_str()),
        ("scope", child.task.scope.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(GateError::MissingChildField {
                child_task_id: child.task.id,
                field,
            });
        }
    }
    if child.task.acceptance_criteria.is_empty() {
        return Err(GateError::MissingChildField {
            child_task_id: child.task.id,
            field: "acceptance_criteria",
        });
    }
    Ok(())
}

fn validate_edges_reference_existing_nodes(
    task_ids: impl IntoIterator<Item = TaskId>,
    edges: &[TaskEdge],
) -> Result<(), GateError> {
    let known: BTreeSet<TaskId> = task_ids.into_iter().collect();
    for edge in edges {
        if !known.contains(&edge.from) {
            return Err(GateError::EdgeReferencesUnknownTask(edge.from));
        }
        if !known.contains(&edge.to) {
            return Err(GateError::EdgeReferencesUnknownTask(edge.to));
        }
    }
    Ok(())
}

fn validate_edges_acyclic(
    task_ids: impl IntoIterator<Item = TaskId>,
    edges: &[TaskEdge],
) -> Result<(), GateError> {
    let task_ids: Vec<TaskId> = task_ids.into_iter().collect();
    let known: BTreeSet<TaskId> = task_ids.iter().copied().collect();
    let mut in_degree: BTreeMap<TaskId, usize> =
        task_ids.iter().copied().map(|id| (id, 0usize)).collect();
    let mut successors: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();

    for edge in edges {
        if edge.from == edge.to {
            return Err(GateError::CycleDetected);
        }
        if !known.contains(&edge.from) {
            return Err(GateError::EdgeReferencesUnknownTask(edge.from));
        }
        if !known.contains(&edge.to) {
            return Err(GateError::EdgeReferencesUnknownTask(edge.to));
        }
        successors.entry(edge.from).or_default().push(edge.to);
        *in_degree.entry(edge.to).or_insert(0) += 1;
    }

    let mut queue: VecDeque<TaskId> = in_degree
        .iter()
        .filter_map(|(task_id, degree)| (*degree == 0).then_some(*task_id))
        .collect();
    let mut visited = 0usize;

    while let Some(task_id) = queue.pop_front() {
        visited += 1;
        if let Some(next_ids) = successors.get(&task_id) {
            for next_id in next_ids {
                let Some(degree) = in_degree.get_mut(next_id) else {
                    return Err(GateError::EdgeReferencesUnknownTask(*next_id));
                };
                *degree = degree.saturating_sub(1);
                if *degree == 0 {
                    queue.push_back(*next_id);
                }
            }
        }
    }

    if visited != task_ids.len() {
        return Err(GateError::CycleDetected);
    }
    Ok(())
}

fn saturating_usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn saturating_usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::similar_names, clippy::too_many_lines)]

    use super::*;

    fn spec(name: &str, scope_units: u32, max_retries: u32) -> TaskSpec {
        TaskSpec::new(
            TaskId::stable(name),
            name,
            format!("implement {name}"),
            format!("scope for {name}"),
            scope_units,
            max_retries,
        )
    }

    fn child(name: &str, scope_units: u32, max_retries: u32, deps: Vec<TaskId>) -> ChildTaskSpec {
        ChildTaskSpec {
            task: spec(name, scope_units, max_retries),
            dependencies: deps,
        }
    }

    fn decomposition(
        parent: TaskId,
        children: Vec<ChildTaskSpec>,
        reason: &str,
    ) -> DecompositionResult {
        DecompositionResult {
            parent_task_id: parent,
            reason_for_decomposition: reason.to_string(),
            children,
            integration_strategy: "integrate child artifacts in dependency order".to_string(),
            verification_strategy: "verify acceptance criteria after integration".to_string(),
        }
    }

    fn status_time(
        graph: &DynamicTaskGraph,
        task_id: TaskId,
        status: TaskLifecycleState,
    ) -> DateTime<Utc> {
        graph
            .lifecycle_events
            .iter()
            .find(|event| event.task_id == task_id && event.to == status)
            .map(|event| event.at)
            .expect("status transition exists")
    }

    fn deserialize_error_for(graph: &DynamicTaskGraph) -> String {
        serde_json::from_str::<DynamicTaskGraph>(
            &serde_json::to_string(graph).expect("serializes test graph"),
        )
        .expect_err("corrupt graph must fail deserialization")
        .to_string()
    }

    fn finished_attempt(task_id: TaskId) -> ExecutionAttempt {
        ExecutionAttempt {
            id: AttemptId::from_sequence(10_000),
            task_id,
            phase: AttemptPhase::Execute,
            attempt_no: 1,
            retry_count: 0,
            status: AttemptStatus::Succeeded,
            started_at: Utc
                .timestamp_opt(START_TIME_SECS, 0)
                .single()
                .expect("valid test timestamp"),
            finished_at: Some(
                Utc.timestamp_opt(START_TIME_SECS + 1, 0)
                    .single()
                    .expect("valid test timestamp"),
            ),
            failure_reason: None,
            block_reason: None,
            artifacts: Vec::new(),
            dependency_snapshot: Vec::new(),
        }
    }

    struct ReopenExecutor {
        root: TaskId,
        first: DecompositionResult,
        second: DecompositionResult,
        root_execute_count: u32,
    }

    impl MasterImplementExecutor for ReopenExecutor {
        fn execute(
            &mut self,
            _graph: &DynamicTaskGraph,
            task_id: TaskId,
            phase: AttemptPhase,
        ) -> ExecutorOutcome {
            if task_id == self.root && phase == AttemptPhase::Execute {
                self.root_execute_count += 1;
                if self.root_execute_count == 1 {
                    ExecutorOutcome::Decomposed(self.first.clone())
                } else {
                    ExecutorOutcome::Decomposed(self.second.clone())
                }
            } else {
                ExecutorOutcome::Succeeded {
                    artifacts: vec![ExecutionArtifact::new(
                        format!("artifact:{task_id}"),
                        "done",
                    )],
                }
            }
        }
    }

    #[test]
    fn transactional_injection_rejects_cycles_without_mutating_graph() {
        let root = TaskId::stable("root-cycle");
        let a = TaskId::stable("cycle-a");
        let b = TaskId::stable("cycle-b");
        let mut graph = DynamicTaskGraph::with_root(spec("root-cycle", 100, 0));
        let before_tasks = graph.tasks.len();
        let before_edges = graph.edges.len();

        let result = decomposition(
            root,
            vec![
                child("cycle-a", 20, 0, vec![b]),
                child("cycle-b", 20, 0, vec![a]),
            ],
            "invalid cyclic split",
        );

        let err = graph
            .inject_decomposition(result, DecompositionLimits::default())
            .expect_err("cycle must be rejected");
        assert_eq!(err, GateError::CycleDetected);
        assert_eq!(graph.tasks.len(), before_tasks);
        assert_eq!(graph.edges.len(), before_edges);
        assert!(graph.injection_records.is_empty());
        assert!(graph.is_acyclic());
    }

    #[test]
    fn repeated_decomposition_requires_explicit_reopen() {
        let root = TaskId::stable("root-repeat");
        let mut graph = DynamicTaskGraph::with_root(spec("root-repeat", 100, 0));

        graph
            .inject_decomposition(
                decomposition(root, vec![child("repeat-a", 20, 0, vec![])], "first"),
                DecompositionLimits::default(),
            )
            .expect("first decomposition commits");

        let err = graph
            .inject_decomposition(
                decomposition(root, vec![child("repeat-b", 20, 0, vec![])], "second"),
                DecompositionLimits::default(),
            )
            .expect_err("second decomposition must be gated");
        assert_eq!(err, GateError::AlreadyDecomposed(root));

        graph
            .reopen_for_decomposition(root, "operator reopened parent")
            .expect("explicit reopen succeeds");
        graph
            .inject_decomposition(
                decomposition(root, vec![child("repeat-b", 20, 0, vec![])], "second"),
                DecompositionLimits {
                    max_descendants: 4,
                    ..DecompositionLimits::default()
                },
            )
            .expect("reopened parent can decompose again");
    }

    #[test]
    fn decomposition_bounds_are_enforced() {
        let root = TaskId::stable("root-bounds");
        let mut graph = DynamicTaskGraph::with_root(spec("root-bounds", 100, 0));
        let fanout_err = graph
            .inject_decomposition(
                decomposition(
                    root,
                    vec![
                        child("bounds-a", 10, 0, vec![]),
                        child("bounds-b", 10, 0, vec![]),
                    ],
                    "fanout too large",
                ),
                DecompositionLimits {
                    max_fanout: 1,
                    ..DecompositionLimits::default()
                },
            )
            .expect_err("fanout cap rejects");
        assert!(matches!(fanout_err, GateError::FanoutExceeded { .. }));

        let depth_err = graph
            .inject_decomposition(
                decomposition(root, vec![child("bounds-c", 10, 0, vec![])], "too deep"),
                DecompositionLimits {
                    max_depth: 0,
                    ..DecompositionLimits::default()
                },
            )
            .expect_err("depth cap rejects");
        assert!(matches!(depth_err, GateError::DepthExceeded { .. }));

        let scope_err = graph
            .inject_decomposition(
                decomposition(root, vec![child("bounds-d", 100, 0, vec![])], "not smaller"),
                DecompositionLimits::default(),
            )
            .expect_err("child scope must be smaller");
        assert!(matches!(scope_err, GateError::ChildNotSmaller { .. }));
        assert_eq!(graph.tasks.len(), 1, "failed transactions must not mutate");
    }

    #[test]
    fn descendant_limit_applies_to_ancestors_transactionally() {
        let root = TaskId::stable("root-descendants");
        let a = TaskId::stable("desc-a");
        let mut graph = DynamicTaskGraph::with_root(spec("root-descendants", 100, 0));
        graph
            .inject_decomposition(
                decomposition(
                    root,
                    vec![
                        child("desc-a", 40, 0, vec![]),
                        child("desc-b", 40, 0, vec![]),
                    ],
                    "root split",
                ),
                DecompositionLimits {
                    max_descendants: 2,
                    ..DecompositionLimits::default()
                },
            )
            .expect("root reaches descendant cap");
        let before_tasks = graph.tasks.len();
        let before_edges = graph.edges.len();

        let err = graph
            .inject_decomposition(
                decomposition(
                    a,
                    vec![child("desc-a1", 10, 0, vec![])],
                    "would exceed root",
                ),
                DecompositionLimits {
                    max_descendants: 2,
                    ..DecompositionLimits::default()
                },
            )
            .expect_err("ancestor descendant cap rejects");

        assert_eq!(
            err,
            GateError::DescendantLimitExceeded {
                parent_task_id: root,
                actual: 3,
                max: 2,
            }
        );
        assert_eq!(graph.tasks.len(), before_tasks);
        assert_eq!(graph.edges.len(), before_edges);
    }

    #[test]
    fn dependency_failure_blocks_dependents_and_parents_reach_terminal_state() {
        let root = TaskId::stable("root-dependency-failure");
        let a = TaskId::stable("dep-fail-a");
        let b = TaskId::stable("dep-fail-b");
        let root_decomposition = decomposition(
            root,
            vec![
                child("dep-fail-a", 20, 0, vec![]),
                child("dep-fail-b", 20, 0, vec![a]),
            ],
            "root split with dependency",
        );
        let fake = FakeMasterImplementExecutor::new()
            .on_execute(root, FakeBehavior::Decompose(root_decomposition))
            .on_execute(
                a,
                FakeBehavior::FailPermanently {
                    reason: "A cannot complete".to_string(),
                },
            )
            .on_execute(b, FakeBehavior::direct("B must not run"));
        let mut graph = DynamicTaskGraph::with_root(spec("root-dependency-failure", 100, 0));
        let mut scheduler = DryRunScheduler::new(
            fake,
            SchedulerConfig {
                decomposition_limits: DecompositionLimits::default(),
                max_steps: 16,
            },
        );

        let run = scheduler.run(&mut graph).expect("scheduler run succeeds");

        assert_eq!(run.stop_reason, SchedulerStopReason::GraphTerminal);
        assert_eq!(
            graph.task(a).expect("A exists").status,
            TaskLifecycleState::Failed
        );
        assert_eq!(
            graph.task(b).expect("B exists").status,
            TaskLifecycleState::Blocked
        );
        assert_eq!(
            graph.task(root).expect("root exists").status,
            TaskLifecycleState::Blocked
        );
        assert!(
            graph
                .attempts_for_task_phase(b, AttemptPhase::Execute)
                .is_empty(),
            "dependent task must not execute after prerequisite failure"
        );
        assert!(graph.all_terminal());
    }

    #[test]
    fn deserialization_validates_graph_integrity() {
        let root = TaskId::stable("root-serde");
        let a = TaskId::stable("serde-a");
        let mut graph = DynamicTaskGraph::with_root(spec("root-serde", 100, 0));
        graph
            .inject_decomposition(
                decomposition(root, vec![child("serde-a", 20, 0, vec![])], "root split"),
                DecompositionLimits::default(),
            )
            .expect("valid graph commits");

        let roundtrip = serde_json::from_str::<DynamicTaskGraph>(
            &serde_json::to_string(&graph).expect("serializes"),
        )
        .expect("valid graph deserializes");
        assert!(roundtrip.is_acyclic());

        graph.edges.push(TaskEdge {
            from: a,
            to: root,
            kind: TaskEdgeKind::Dependency,
        });
        let err = serde_json::from_str::<DynamicTaskGraph>(
            &serde_json::to_string(&graph).expect("serializes invalid shape"),
        )
        .expect_err("cycle is rejected during deserialization");
        assert!(err.to_string().contains("cycle"));
    }

    #[test]
    fn deserialization_rejects_malformed_parent_links_and_dangling_references() {
        let root = TaskId::stable("root-serde-links");
        let a = TaskId::stable("serde-link-a");
        let mut base = DynamicTaskGraph::with_root(spec("root-serde-links", 100, 0));
        base.inject_decomposition(
            decomposition(
                root,
                vec![child("serde-link-a", 20, 0, vec![])],
                "root split",
            ),
            DecompositionLimits::default(),
        )
        .expect("valid graph commits");

        let mut missing_parent_edge = base.clone();
        missing_parent_edge.edges.clear();
        assert!(
            deserialize_error_for(&missing_parent_edge).contains("missing parent-child edge"),
            "missing parent-child edge must be rejected"
        );

        let mut dangling_edge = base.clone();
        dangling_edge.edges.push(TaskEdge {
            from: TaskId::stable("serde-link-missing"),
            to: a,
            kind: TaskEdgeKind::Dependency,
        });
        assert!(
            deserialize_error_for(&dangling_edge).contains("edge references unknown task"),
            "dangling edge endpoint must be rejected"
        );

        let mut dangling_attempt = base;
        let mut attempt = finished_attempt(TaskId::stable("serde-link-missing-attempt"));
        attempt.id = AttemptId::from_sequence(10_001);
        dangling_attempt.attempts.push(attempt);
        assert!(
            deserialize_error_for(&dangling_attempt).contains("task not found"),
            "attempt for unknown task must be rejected"
        );
    }

    #[test]
    fn deserialization_rejects_duplicate_ids_bad_attempts_and_invalid_injections() {
        let root = TaskId::stable("root-serde-records");
        let a = TaskId::stable("serde-record-a");
        let mut base = DynamicTaskGraph::with_root(spec("root-serde-records", 100, 0));
        base.inject_decomposition(
            decomposition(
                root,
                vec![child("serde-record-a", 20, 0, vec![])],
                "root split",
            ),
            DecompositionLimits::default(),
        )
        .expect("valid graph commits");

        let mut duplicate_attempts = base.clone();
        let attempt = finished_attempt(root);
        duplicate_attempts.attempts.push(attempt.clone());
        duplicate_attempts.attempts.push(attempt);
        assert!(
            deserialize_error_for(&duplicate_attempts).contains("duplicate attempt id"),
            "duplicate attempt ids must be rejected"
        );

        let mut bad_attempt = base.clone();
        let mut attempt = finished_attempt(root);
        attempt.id = AttemptId::from_sequence(10_002);
        attempt.finished_at = None;
        bad_attempt.attempts.push(attempt);
        assert!(
            deserialize_error_for(&bad_attempt).contains("invalid serialized attempt state"),
            "finished attempt without finished_at must be rejected"
        );

        let mut duplicate_injections = base.clone();
        let injection = duplicate_injections.injection_records[0].clone();
        duplicate_injections.injection_records.push(injection);
        assert!(
            deserialize_error_for(&duplicate_injections).contains("duplicate injection batch id"),
            "duplicate injection ids must be rejected"
        );

        let mut invalid_injection = base.clone();
        invalid_injection.injection_records[0].edge_count = 0;
        assert!(
            deserialize_error_for(&invalid_injection)
                .contains("invalid serialized injection record"),
            "injection record edge count below child count must be rejected"
        );

        let mut dangling_injection = base;
        dangling_injection.injection_records[0]
            .child_task_ids
            .push(TaskId::stable("serde-record-missing-child"));
        assert!(
            deserialize_error_for(&dangling_injection).contains("task not found"),
            "injection record child ids must reference known tasks"
        );

        assert_eq!(
            a,
            TaskId::stable("serde-record-a"),
            "fixture id sanity check"
        );
    }

    #[test]
    fn integration_failure_propagates_to_dependents_and_ancestors() {
        let root = TaskId::stable("root-integration-fail");
        let parent = TaskId::stable("integration-fail-parent");
        let dependent = TaskId::stable("integration-fail-dependent");
        let child_id = TaskId::stable("integration-fail-child");
        let root_decomposition = decomposition(
            root,
            vec![
                child("integration-fail-parent", 50, 0, vec![]),
                child("integration-fail-dependent", 20, 0, vec![parent]),
            ],
            "root split",
        );
        let parent_decomposition = decomposition(
            parent,
            vec![child("integration-fail-child", 10, 0, vec![])],
            "parent split",
        );
        let fake = FakeMasterImplementExecutor::new()
            .on_execute(root, FakeBehavior::Decompose(root_decomposition))
            .on_execute(parent, FakeBehavior::Decompose(parent_decomposition))
            .on_integrate(
                parent,
                FakeBehavior::FailPermanently {
                    reason: "integration failed".to_string(),
                },
            )
            .on_execute(child_id, FakeBehavior::direct("child-artifact"))
            .on_execute(dependent, FakeBehavior::direct("dependent-must-not-run"));
        let mut graph = DynamicTaskGraph::with_root(spec("root-integration-fail", 100, 0));
        let mut scheduler = DryRunScheduler::new(
            fake,
            SchedulerConfig {
                decomposition_limits: DecompositionLimits::default(),
                max_steps: 32,
            },
        );

        let run = scheduler.run(&mut graph).expect("scheduler run succeeds");

        assert_eq!(run.stop_reason, SchedulerStopReason::GraphTerminal);
        assert_eq!(
            graph.task(child_id).expect("child exists").status,
            TaskLifecycleState::Succeeded
        );
        assert_eq!(
            graph.task(parent).expect("parent exists").status,
            TaskLifecycleState::Failed
        );
        assert_eq!(
            graph.task(dependent).expect("dependent exists").status,
            TaskLifecycleState::Blocked
        );
        assert_eq!(
            graph.task(root).expect("root exists").status,
            TaskLifecycleState::Blocked
        );
        assert!(
            graph
                .attempts_for_task_phase(dependent, AttemptPhase::Execute)
                .is_empty(),
            "dependent must not run after integration prerequisite failure"
        );
        assert!(graph.all_terminal());
    }

    #[test]
    fn integration_cancel_propagates_to_dependents_and_ancestors() {
        let root = TaskId::stable("root-integration-cancel");
        let parent = TaskId::stable("integration-cancel-parent");
        let dependent = TaskId::stable("integration-cancel-dependent");
        let child_id = TaskId::stable("integration-cancel-child");
        let root_decomposition = decomposition(
            root,
            vec![
                child("integration-cancel-parent", 50, 0, vec![]),
                child("integration-cancel-dependent", 20, 0, vec![parent]),
            ],
            "root split",
        );
        let parent_decomposition = decomposition(
            parent,
            vec![child("integration-cancel-child", 10, 0, vec![])],
            "parent split",
        );
        let fake = FakeMasterImplementExecutor::new()
            .on_execute(root, FakeBehavior::Decompose(root_decomposition))
            .on_execute(parent, FakeBehavior::Decompose(parent_decomposition))
            .on_integrate(
                parent,
                FakeBehavior::Cancel {
                    reason: "operator cancelled integration".to_string(),
                },
            )
            .on_execute(child_id, FakeBehavior::direct("child-artifact"))
            .on_execute(dependent, FakeBehavior::direct("dependent-must-not-run"));
        let mut graph = DynamicTaskGraph::with_root(spec("root-integration-cancel", 100, 0));
        let mut scheduler = DryRunScheduler::new(
            fake,
            SchedulerConfig {
                decomposition_limits: DecompositionLimits::default(),
                max_steps: 32,
            },
        );

        let run = scheduler.run(&mut graph).expect("scheduler run succeeds");

        assert_eq!(run.stop_reason, SchedulerStopReason::GraphTerminal);
        assert_eq!(
            graph.task(child_id).expect("child exists").status,
            TaskLifecycleState::Succeeded
        );
        assert_eq!(
            graph.task(parent).expect("parent exists").status,
            TaskLifecycleState::Cancelled
        );
        assert_eq!(
            graph.task(dependent).expect("dependent exists").status,
            TaskLifecycleState::Blocked
        );
        assert_eq!(
            graph.task(root).expect("root exists").status,
            TaskLifecycleState::Blocked
        );
        assert!(graph.all_terminal());
    }

    #[test]
    fn scheduler_step_limit_preserves_inspectable_nonterminal_graph() {
        let root = TaskId::stable("root-step-limit");
        let fake = FakeMasterImplementExecutor::new().on_execute(
            root,
            FakeBehavior::FailPermanently {
                reason: "retry forever within budget".to_string(),
            },
        );
        let mut graph = DynamicTaskGraph::with_root(spec("root-step-limit", 100, 100));
        let mut scheduler = DryRunScheduler::new(
            fake,
            SchedulerConfig {
                decomposition_limits: DecompositionLimits::default(),
                max_steps: 3,
            },
        );

        let run = scheduler.run(&mut graph).expect("scheduler run succeeds");

        assert_eq!(run.stop_reason, SchedulerStopReason::StepLimitExceeded);
        assert_eq!(run.steps, 3);
        assert_eq!(
            graph.task(root).expect("root exists").status,
            TaskLifecycleState::Ready
        );
        assert_eq!(
            graph
                .attempts_for_task_phase(root, AttemptPhase::Execute)
                .len(),
            3
        );
        assert!(!graph.all_terminal());
        assert!(graph.is_acyclic());
    }

    #[test]
    fn explicit_reopen_allows_second_scheduler_decomposition_only_after_reopen() {
        let root = TaskId::stable("root-reopen-scheduler");
        let a = TaskId::stable("reopen-scheduler-a");
        let b = TaskId::stable("reopen-scheduler-b");
        let first = decomposition(
            root,
            vec![child("reopen-scheduler-a", 20, 0, vec![])],
            "first split",
        );
        let second = decomposition(
            root,
            vec![child("reopen-scheduler-b", 20, 0, vec![])],
            "second split",
        );
        let mut graph = DynamicTaskGraph::with_root(spec("root-reopen-scheduler", 100, 0));
        let mut scheduler = DryRunScheduler::new(
            ReopenExecutor {
                root,
                first,
                second: second.clone(),
                root_execute_count: 0,
            },
            SchedulerConfig {
                decomposition_limits: DecompositionLimits {
                    max_descendants: 4,
                    ..DecompositionLimits::default()
                },
                max_steps: 16,
            },
        );

        let first_run = scheduler.run(&mut graph).expect("first run succeeds");
        assert_eq!(first_run.stop_reason, SchedulerStopReason::GraphTerminal);
        assert_eq!(
            graph.task(root).expect("root exists").status,
            TaskLifecycleState::Succeeded
        );
        assert_eq!(graph.child_ids(root), vec![a]);

        let err = graph
            .inject_decomposition(second, DecompositionLimits::default())
            .expect_err("second decomposition is rejected before explicit reopen");
        assert_eq!(err, GateError::AlreadyDecomposed(root));

        graph
            .reopen_for_decomposition(root, "operator requested second pass")
            .expect("explicit reopen succeeds");
        let second_run = scheduler.run(&mut graph).expect("second run succeeds");

        assert_eq!(second_run.stop_reason, SchedulerStopReason::GraphTerminal);
        assert_eq!(
            graph.task(root).expect("root exists").status,
            TaskLifecycleState::Succeeded
        );
        assert_eq!(graph.child_ids(root), vec![a, b]);
        assert_eq!(graph.injection_records.len(), 2);
        assert!(graph.all_terminal());
    }

    #[test]
    fn vertical_slice_decomposes_retries_integrates_and_stops_cleanly() {
        let root = TaskId::stable("root");
        let a = TaskId::stable("A");
        let b = TaskId::stable("B");
        let b1 = TaskId::stable("B1");
        let b2 = TaskId::stable("B2");

        let root_decomposition = decomposition(
            root,
            vec![child("A", 25, 0, vec![]), child("B", 60, 0, vec![a])],
            "root is too broad",
        );
        let b_decomposition = decomposition(
            b,
            vec![child("B1", 20, 0, vec![]), child("B2", 15, 1, vec![b1])],
            "B needs narrower implementation tasks",
        );

        let fake = FakeMasterImplementExecutor::new()
            .on_execute(root, FakeBehavior::Decompose(root_decomposition))
            .on_integrate(root, FakeBehavior::direct("root-integrated"))
            .on_execute(a, FakeBehavior::direct("A-artifact"))
            .on_execute(b, FakeBehavior::Decompose(b_decomposition))
            .on_integrate(b, FakeBehavior::direct("B-integrated"))
            .on_execute(b1, FakeBehavior::direct("B1-artifact"))
            .on_execute(
                b2,
                FakeBehavior::FailOnceThenSucceed {
                    failure_reason: "deterministic first failure".to_string(),
                    artifact_label: "B2-artifact".to_string(),
                },
            );

        let mut graph = DynamicTaskGraph::with_root(spec("root", 100, 0));
        let mut scheduler = DryRunScheduler::new(
            fake,
            SchedulerConfig {
                decomposition_limits: DecompositionLimits {
                    max_depth: 3,
                    max_fanout: 3,
                    max_descendants: 8,
                },
                max_steps: 32,
            },
        );

        let run = scheduler.run(&mut graph).expect("scheduler run succeeds");

        assert_eq!(run.stop_reason, SchedulerStopReason::GraphTerminal);
        assert!(run.steps < 32);
        assert!(graph.is_acyclic(), "dynamic graph must remain acyclic");

        assert_eq!(graph.injection_records.len(), 2);
        assert_eq!(graph.injection_records[0].parent_task_id, root);
        assert_eq!(graph.injection_records[0].child_task_ids, vec![a, b]);
        assert_eq!(graph.injection_records[1].parent_task_id, b);
        assert_eq!(graph.injection_records[1].child_task_ids, vec![b1, b2]);

        assert!(
            graph
                .lifecycle_events
                .iter()
                .any(|event| event.task_id == root
                    && event.to == TaskLifecycleState::BlockedOnChildren),
            "root must pause after decomposition"
        );
        assert!(
            graph.lifecycle_events.iter().any(
                |event| event.task_id == b && event.to == TaskLifecycleState::BlockedOnChildren
            ),
            "B must pause after decomposition"
        );

        for attempt in &graph.attempts {
            assert!(
                attempt
                    .dependency_snapshot
                    .iter()
                    .all(|dep| dep.status == TaskLifecycleState::Succeeded),
                "attempt {:?} ran before dependencies were ready: {:?}",
                attempt.id,
                attempt.dependency_snapshot
            );
        }

        let b_attempt = graph
            .attempts_for_task_phase(b, AttemptPhase::Execute)
            .into_iter()
            .next()
            .expect("B execute attempt recorded");
        assert_eq!(
            b_attempt.dependency_snapshot,
            vec![DependencySnapshot {
                task_id: a,
                status: TaskLifecycleState::Succeeded,
            }]
        );

        let b2_attempts = graph.attempts_for_task_phase(b2, AttemptPhase::Execute);
        assert_eq!(b2_attempts.len(), 2);
        assert_eq!(b2_attempts[0].status, AttemptStatus::Failed);
        assert_eq!(b2_attempts[0].retry_count, 0);
        assert_eq!(b2_attempts[1].status, AttemptStatus::Succeeded);
        assert_eq!(b2_attempts[1].retry_count, 1);
        assert!(
            b2_attempts
                .iter()
                .filter(|attempt| attempt.status == AttemptStatus::Failed)
                .count()
                <= graph.task(b2).expect("B2 exists").budget.max_retries as usize
        );

        for task_id in [a, b, b1, b2, root] {
            let attempts = graph.attempts_for_task(task_id);
            assert!(!attempts.is_empty(), "attempts recorded for {task_id}");
            assert!(
                graph
                    .task(task_id)
                    .expect("task exists")
                    .status
                    .is_terminal(),
                "task {task_id} reached terminal state"
            );
        }

        assert_eq!(
            graph.task(root).expect("root exists").status,
            TaskLifecycleState::Succeeded
        );
        assert_eq!(
            graph.task(a).expect("A exists").status,
            TaskLifecycleState::Succeeded
        );
        assert_eq!(
            graph.task(b).expect("B exists").status,
            TaskLifecycleState::Succeeded
        );
        assert_eq!(
            graph.task(b1).expect("B1 exists").status,
            TaskLifecycleState::Succeeded
        );
        assert_eq!(
            graph.task(b2).expect("B2 exists").status,
            TaskLifecycleState::Succeeded
        );

        assert!(!graph.task(a).expect("A exists").artifacts.is_empty());
        assert!(!graph.task(b).expect("B exists").artifacts.is_empty());
        assert!(!graph.task(root).expect("root exists").artifacts.is_empty());

        let root_blocked_at = status_time(&graph, root, TaskLifecycleState::BlockedOnChildren);
        let a_running_at = status_time(&graph, a, TaskLifecycleState::Running);
        assert!(
            root_blocked_at < a_running_at,
            "parent must pause before children run"
        );

        let b_succeeded_at = status_time(&graph, b, TaskLifecycleState::Succeeded);
        let b1_succeeded_at = status_time(&graph, b1, TaskLifecycleState::Succeeded);
        let b2_succeeded_at = status_time(&graph, b2, TaskLifecycleState::Succeeded);
        assert!(b_succeeded_at > b1_succeeded_at);
        assert!(b_succeeded_at > b2_succeeded_at);

        let root_succeeded_at = status_time(&graph, root, TaskLifecycleState::Succeeded);
        let a_succeeded_at = status_time(&graph, a, TaskLifecycleState::Succeeded);
        assert!(root_succeeded_at > a_succeeded_at);
        assert!(root_succeeded_at > b_succeeded_at);

        let root_attempts = graph.attempts_for_task(root);
        assert_eq!(root_attempts.len(), 2);
        assert_eq!(root_attempts[0].status, AttemptStatus::Decomposed);
        assert_eq!(root_attempts[1].phase, AttemptPhase::Integrate);
        assert_eq!(root_attempts[1].status, AttemptStatus::Succeeded);

        assert!(graph.all_terminal());
    }
}
