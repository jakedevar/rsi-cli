//! Durable, step-at-a-time topology reconciler (#634, plan §2.3–§2.5).
//!
//! `advance` reads durable rows, reserves effects in one store transaction
//! (attempt rows with a pre-minted session id and deterministic dedup key),
//! performs effects outside the store lock, and records results. It is
//! idempotent: running it again after a crash takes the same next step, and a
//! node launch is never repeated for a dedup key that already reached model
//! admission (the key is UNIQUE in `model_invocations`).

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rsi_common::agent_contract::{PipelineStatusV2, parse_pipeline_handoff_v2};
use rsi_common::types::{
    CatalogOp, EdgeWhen, FailurePolicy, GraphExecutionUpdate, SessionStatus, TopologyStep,
    UntilCondition,
};
use rsi_graph::data::{NodeData, Value as GraphValue};
use rsi_graph::format::{EdgeDef, NodeDef};
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::topology::catalog::{CommandPoll, CommandRunner};
use crate::topology::custody::{
    self, PinSelector, TopologyCustody, TopologyForkSource, node_pin_ref, preserved_ref,
};
use crate::topology::graph::{
    GraphShape, InstanceState, MAX_ATTEMPTS_PER_NODE, RegionDecision, RegionProgress,
};
use crate::topology::launch::NodeLaunchBuilder;
use crate::topology::steps::WorkflowSteps;
use crate::topology::store::{
    self as rows, AttemptRow, AttemptStatus, ExecutionRow, ExecutionStatus, NewAttempt, Settlement,
    failure,
};

/// Wall-time bound for one session node attempt (plan §2.5, fixes F-4).
pub(crate) const SESSION_WALL_TIME: chrono::TimeDelta = chrono::TimeDelta::hours(4);
/// Periodic reconciliation tick (plan §2.3).
pub(crate) const EXECUTOR_TICK: Duration = Duration::from_secs(30);
/// Bounded inner rounds per `advance` call; a step that keeps changing state
/// yields back to the driver after this many rounds.
const ADVANCE_ROUNDS: usize = 32;
/// Settled executions cleaned per settlement-cleanup pass.
pub(crate) const CLEANUP_BATCH: usize = 64;

/// What the daemon observes about a node session.
#[derive(Clone, Debug)]
pub(crate) struct SessionObservation {
    pub(crate) status: SessionStatus,
    pub(crate) sandbox_root: Option<PathBuf>,
}

/// One node launch: the reserved attempt's identity plus its launch config.
pub(crate) struct LaunchRequest {
    pub(crate) session_id: Uuid,
    pub(crate) config: crate::claude::LaunchConfig,
    pub(crate) fork: TopologyForkSource,
}

/// The executor's only effect boundary. Production drives the session
/// manager; tests substitute a provider-free fake.
pub(crate) trait NodeEffects: Send + Sync + 'static {
    fn enabled(&self) -> bool;
    fn launch(&self, request: LaunchRequest) -> impl Future<Output = Result<Uuid>> + Send;
    fn session(&self, session_id: Uuid) -> impl Future<Output = Option<SessionObservation>> + Send;
    fn output(&self, session_id: Uuid) -> impl Future<Output = Result<NodeData>> + Send;
    fn interrupt(&self, session_id: Uuid) -> impl Future<Output = ()> + Send;
    fn reclaim(&self, session_id: Uuid) -> impl Future<Output = ()> + Send;
    fn release_sandbox(&self, session_id: Uuid) -> impl Future<Output = Result<()>> + Send;
    fn predicate_met(
        &self,
        predicate: &str,
        project_id: Option<Uuid>,
    ) -> impl Future<Output = bool> + Send;
    fn publish(&self, update: GraphExecutionUpdate);
    /// Suspension point inside the reservation window, after the ready set
    /// is decided and before the reservation transaction. Production: no-op;
    /// tests use it to force two advancers into a real race.
    fn before_reserve(&self, _execution_id: Uuid) -> impl Future<Output = ()> + Send {
        async {}
    }
    /// Command nodes (#635): a fresh sandbox at the fork commit, keyed by the
    /// attempt's pre-minted session id (adopted if it already exists).
    fn allocate_command_sandbox(
        &self,
        session_id: Uuid,
        fork: TopologyForkSource,
    ) -> impl Future<Output = Result<PathBuf>> + Send;
    /// Start one catalog op; returns its first process group id.
    fn start_command(
        &self,
        attempt_id: Uuid,
        sandbox: PathBuf,
        op: CatalogOp,
    ) -> impl Future<Output = Result<Option<i32>>> + Send;
    /// `None`: this incarnation has no run for the attempt.
    fn poll_command(&self, attempt_id: Uuid) -> Option<CommandPoll>;
    fn cancel_command(&self, attempt_id: Uuid);
    fn forget_command(&self, attempt_id: Uuid);
    /// Kill a process group left by an earlier incarnation (plan §2.4).
    fn kill_stale_group(&self, pgid: i32);
    /// Operator setting `topology_max_concurrent_build_nodes`.
    fn build_node_cap(&self) -> u32;
    /// Suspension point between a resolution's key pre-check and its
    /// recording transaction. Production: no-op; tests force a key race.
    fn before_resolution_record(&self, _execution_id: Uuid) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// Result of one `advance` call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Step {
    /// In-flight work remains; wake on a session change or the tick.
    Wait,
    /// Nothing left for this driver (settled, or blocked with no in-flight).
    Done,
    /// The kill switch is off; no effect was taken.
    Paused,
}

pub(crate) struct Executor<E: NodeEffects> {
    pub(crate) store: Arc<Mutex<Store>>,
    pub(crate) effects: Arc<E>,
    pub(crate) boot_id: Uuid,
    /// Shared by every handle in one daemon incarnation: drivers and the
    /// per-execution advance lock.
    pub(crate) registry: Arc<DriverRegistry>,
}

impl<E: NodeEffects> Clone for Executor<E> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            effects: Arc::clone(&self.effects),
            boot_id: self.boot_id,
            registry: Arc::clone(&self.registry),
        }
    }
}

/// Latest attempt per `(node, iteration)` plus per-instance attempt lists.
struct AttemptIndex<'a> {
    by_instance: HashMap<(String, u32), Vec<&'a AttemptRow>>,
    /// Attempts charged against the execution cap (infrastructure losses
    /// excluded; they have their own per-instance bound).
    charged: usize,
}

impl<'a> AttemptIndex<'a> {
    fn new(attempts: &'a [AttemptRow]) -> Self {
        let mut by_instance: HashMap<(String, u32), Vec<&AttemptRow>> = HashMap::new();
        for attempt in attempts {
            by_instance
                .entry((attempt.node_id.clone(), attempt.iteration))
                .or_default()
                .push(attempt);
        }
        for list in by_instance.values_mut() {
            list.sort_by_key(|attempt| attempt.attempt_no);
        }
        Self {
            by_instance,
            charged: attempts
                .iter()
                .filter(|attempt| {
                    !rows::failure::is_infrastructure(attempt.failure_class.as_deref())
                })
                .count(),
        }
    }

    fn instance(&self, node: &str, iteration: u32) -> &[&'a AttemptRow] {
        self.by_instance
            .get(&(node.to_owned(), iteration))
            .map_or(&[], Vec::as_slice)
    }

    fn latest(&self, node: &str, iteration: u32) -> Option<&'a AttemptRow> {
        self.instance(node, iteration).last().copied()
    }

    /// The succeeded attempt's recorded result for one instance.
    fn result(&self, node: &str, iteration: u32) -> Option<&'a AttemptRow> {
        self.latest(node, iteration)
            .filter(|attempt| attempt.status == AttemptStatus::Succeeded)
    }

    /// The node's final recorded result across iterations.
    fn latest_result(&self, node: &str) -> Option<&'a AttemptRow> {
        self.by_instance
            .iter()
            .filter(|((id, _), _)| id == node)
            .filter_map(|((_, iteration), list)| {
                list.last()
                    .filter(|attempt| attempt.status == AttemptStatus::Succeeded)
                    .map(|attempt| (*iteration, *attempt))
            })
            .max_by_key(|(iteration, _)| *iteration)
            .map(|(_, attempt)| attempt)
    }
}

/// Whether a terminally failed latest attempt earns another attempt.
fn retry_due(
    shape: &GraphShape,
    execution: &ExecutionRow,
    index: &AttemptIndex<'_>,
    latest: &AttemptRow,
) -> bool {
    if latest.resolution.is_some()
        || !matches!(
            latest.status,
            AttemptStatus::Failed | AttemptStatus::Interrupted | AttemptStatus::Lost
        )
        || index.charged >= shape.attempt_cap(execution.max_node_attempts) as usize
    {
        return false;
    }
    let count = |classes: &[&str]| {
        index
            .instance(&latest.node_id, latest.iteration)
            .iter()
            .filter(|attempt| {
                attempt
                    .failure_class
                    .as_deref()
                    .is_some_and(|class| classes.contains(&class))
            })
            .count()
    };
    match latest.failure_class.as_deref() {
        // Infrastructure losses are not the node's fault and never consume
        // the legacy retry budget; plan §2.5 bounds them at
        // `MAX_ATTEMPTS_PER_NODE` per instance on top of it.
        Some(failure::LOST_BEFORE_SESSION | failure::LOST_AFTER_SESSION | failure::INTERRUPTED) => {
            count(&failure::INFRASTRUCTURE) < MAX_ATTEMPTS_PER_NODE as usize
        }
        // Node failures keep the legacy `FailurePolicy::Retry` budget
        // exactly (`repeat_policy.max_iterations`, default 1 retry): the
        // kill switch must not change an ordinary workflow's behaviour.
        Some(
            failure::SESSION_FAILED
            | failure::TIMEOUT
            | failure::HANDOFF_INVALID
            | failure::EXIT_NONZERO,
        ) => {
            shape.failure_policy(&latest.node_id) == Some(FailurePolicy::Retry)
                && count(&[
                    failure::SESSION_FAILED,
                    failure::TIMEOUT,
                    failure::HANDOFF_INVALID,
                    failure::EXIT_NONZERO,
                ]) <= shape.retry_budget(&latest.node_id) as usize
        }
        _ => false,
    }
}

fn instance_state(
    shape: &GraphShape,
    execution: &ExecutionRow,
    index: &AttemptIndex<'_>,
    node: &str,
    iteration: u32,
) -> InstanceState {
    let Some(latest) = index.latest(node, iteration) else {
        return InstanceState::Pending;
    };
    match latest.status {
        status if status.in_flight() => InstanceState::InFlight,
        AttemptStatus::Succeeded => InstanceState::Complete,
        AttemptStatus::Blocked => InstanceState::Blocked,
        AttemptStatus::Skipped => InstanceState::Dead,
        _ if retry_due(shape, execution, index, latest) => InstanceState::InFlight,
        // A cancelled attempt is never routed.
        AttemptStatus::Failed | AttemptStatus::Interrupted | AttemptStatus::Lost
            if shape.steps().routes_failure(node) =>
        {
            InstanceState::Routed
        }
        _ if shape.failure_policy(node) == Some(FailurePolicy::Skip) => InstanceState::Skipped,
        _ => InstanceState::Failed,
    }
}

/// Whether `edge` is taken for its source instance at `at` (plan §3).
fn edge_taken(
    shape: &GraphShape,
    execution: &ExecutionRow,
    index: &AttemptIndex<'_>,
    edge: &EdgeDef,
    at: u32,
) -> bool {
    let when = shape.steps().when(&edge.source, &edge.target);
    match instance_state(shape, execution, index, &edge.source, at) {
        InstanceState::Complete => match when {
            EdgeWhen::Success | EdgeWhen::Completed => true,
            EdgeWhen::GateTrue | EdgeWhen::GateFalse => {
                index
                    .result(&edge.source, at)
                    .and_then(|attempt| attempt.output.as_ref())
                    .and_then(|output| output.pointer("/fields/value"))
                    .and_then(Value::as_bool)
                    == Some(when == EdgeWhen::GateTrue)
            }
            EdgeWhen::Failure | EdgeWhen::VerdictAccepted | EdgeWhen::VerdictChangesRequested => {
                false
            }
        },
        // Legacy `FailurePolicy::Skip`: downstream still runs.
        InstanceState::Skipped => when == EdgeWhen::Success,
        InstanceState::Routed => matches!(when, EdgeWhen::Failure | EdgeWhen::Completed),
        _ => false,
    }
}

/// `(node_kind, catalog_op, effect_class)` of a node: daemon-derived.
pub(crate) fn attempt_kind(
    steps: &WorkflowSteps,
    node: &str,
) -> (&'static str, Option<&'static str>, Option<&'static str>) {
    match steps.step(node) {
        Some(TopologyStep::Command { op }) => (
            "command",
            Some(op.name()),
            Some(crate::topology::catalog::effect_class(op)),
        ),
        Some(step) => (step.kind_name(), None, None),
        None => ("session", None, None),
    }
}

/// Render one upstream node's typed output for a downstream prompt. The
/// full last message (`content`) is included only when the consumer sets
/// `pass_content: true`; `content_tail` and internal `_` keys never are.
fn render_upstream(output: &NodeData, pass_content: bool) -> String {
    let mut lines = Vec::new();
    for (key, value) in output.iter() {
        if key.starts_with('_')
            || key == "content"
            || key == "content_tail"
            || key == crate::session::graph_runner::PIPELINE_ENTRY_CONTEXT_KEY
        {
            continue;
        }
        match value {
            GraphValue::String(text) if text.contains('\n') => {
                lines.push(format!("{key}:\n```\n{}\n```", text.trim_end()));
            }
            value => lines.push(format!("{key}: {value}")),
        }
    }
    if pass_content && let Some(GraphValue::String(content)) = output.get("content") {
        lines.push(format!("content:\n{content}"));
    }
    lines.join("\n")
}

/// Build a `NodeData` from a JSON object (typed command/gate outputs).
pub(crate) fn node_data(value: &Value) -> NodeData {
    let mut data = NodeData::new();
    if let Some(object) = value.as_object() {
        for (key, field) in object {
            if let Ok(field) = serde_json::from_value::<GraphValue>(field.clone()) {
                data.insert(key.clone(), field);
            }
        }
    }
    data
}

/// Marker for an output stored as path@commit (plan §2.1, >64 KiB).
pub(crate) const EXTERNAL_OUTPUT_KEY: &str = "_rsi_external_output";

/// A node's recorded output; an external output is read back from Git and
/// digest-verified, so a missing or altered blob refuses instead of feeding
/// an empty result downstream.
pub(crate) fn node_output(repo: &std::path::Path, attempt: &AttemptRow) -> Result<NodeData> {
    let Some(output) = attempt.output.as_ref() else {
        return Ok(NodeData::new());
    };
    let Some(external) = output.get(EXTERNAL_OUTPUT_KEY) else {
        return Ok(serde_json::from_value(output.clone())?);
    };
    let commit = external
        .get("commit")
        .and_then(Value::as_str)
        .ok_or_else(|| DaemonError::Store("external output has no commit".into()))?;
    let text = custody::read_output(repo, commit)?;
    if external.get("digest").and_then(Value::as_str) != Some(rows::digest(&text).as_str()) {
        return Err(DaemonError::Store(format!(
            "external output of node {} failed digest verification",
            attempt.node_id
        )));
    }
    Ok(serde_json::from_str(&text)?)
}

fn output_mentions_halt(repo: &std::path::Path, attempt: &AttemptRow) -> Result<bool> {
    Ok(match node_output(repo, attempt)?.get("content") {
        Some(GraphValue::String(content)) => {
            crate::session::types::HALT_DIRECTIVE_RE.is_match(content)
        }
        _ => false,
    })
}

impl<E: NodeEffects> Executor<E> {
    pub(crate) const fn new(
        store: Arc<Mutex<Store>>,
        effects: Arc<E>,
        boot_id: Uuid,
        registry: Arc<DriverRegistry>,
    ) -> Self {
        Self {
            store,
            effects,
            boot_id,
            registry,
        }
    }

    pub(crate) fn publish(&self, updates: impl IntoIterator<Item = GraphExecutionUpdate>) {
        for update in updates {
            self.effects.publish(update);
        }
    }

    async fn load(&self, execution_id: Uuid) -> Result<(ExecutionRow, Vec<AttemptRow>)> {
        let store = self.store.lock().await;
        let execution = rows::load_execution(&store, execution_id)?.ok_or_else(|| {
            DaemonError::InvalidParam(format!("topology execution not found: {execution_id}"))
        })?;
        let attempts = rows::load_attempts(&store, execution_id)?;
        Ok((execution, attempts))
    }

    /// Reconcile one execution until it must wait for an external change.
    pub(crate) async fn advance(&self, execution_id: Uuid) -> Result<Step> {
        if !self.effects.enabled() {
            return Ok(Step::Paused);
        }
        // One advancer per execution in this incarnation (driver, recovery
        // pass and supervisor alike); the instance guard fences other boots.
        let serial = self.registry.advance_lock(execution_id);
        let _serial = serial.lock().await;
        for _ in 0..ADVANCE_ROUNDS {
            if let Some(step) = self.round(execution_id).await? {
                return Ok(step);
            }
        }
        Ok(Step::Wait)
    }

    /// One reconciliation round; `None` means durable state changed and the
    /// next round must re-read it.
    async fn round(&self, execution_id: Uuid) -> Result<Option<Step>> {
        let (execution, attempts) = self.load(execution_id).await?;
        if execution.status.is_final() {
            return Ok(Some(Step::Done));
        }
        // 1. Effects owed by in-flight attempts: launch, adopt, settle. A
        // blocked execution takes no new effect: reserved attempts wait for
        // the resolution; launched sessions are still observed.
        let in_flight: Vec<&AttemptRow> = attempts
            .iter()
            .filter(|attempt| attempt.status.in_flight())
            .filter(|attempt| {
                execution.status != ExecutionStatus::Blocked
                    || attempt.status == AttemptStatus::Running
            })
            .collect();
        let mut changed = false;
        for attempt in &in_flight {
            changed |= self.drive_attempt(&execution, attempt).await?;
        }
        if changed {
            return Ok(None);
        }
        let waiting = if in_flight.is_empty() {
            Step::Done
        } else {
            Step::Wait
        };
        match execution.status {
            ExecutionStatus::Accepted => {
                self.transition(
                    &execution,
                    &[ExecutionStatus::Accepted],
                    ExecutionStatus::Running,
                    None,
                    None,
                    None,
                )
                .await?;
                Ok(None)
            }
            ExecutionStatus::Cancelling if waiting == Step::Wait => Ok(Some(Step::Wait)),
            ExecutionStatus::Cancelling => {
                self.transition(
                    &execution,
                    &[ExecutionStatus::Cancelling],
                    ExecutionStatus::Cancelled,
                    Some("workflow execution cancelled".into()),
                    None,
                    None,
                )
                .await?;
                crate::topology::recovery::settlement_cleanup(self, CLEANUP_BATCH).await;
                Ok(Some(Step::Done))
            }
            ExecutionStatus::Blocked => Ok(Some(waiting)),
            _ => self.schedule(&execution, &attempts, waiting).await,
        }
    }

    /// Phases 2–7 for a running execution.
    async fn schedule(
        &self,
        execution: &ExecutionRow,
        attempts: &[AttemptRow],
        waiting: Step,
    ) -> Result<Option<Step>> {
        let shape = GraphShape::from_workflow(&execution.definition)?;
        let index = AttemptIndex::new(attempts);
        let progress = rows::load_region_progress(
            &*self.store.lock().await,
            execution.id,
            shape.region_count(),
        )?;
        let state =
            |node: &str, iteration: u32| instance_state(&shape, execution, &index, node, iteration);
        let latest = |attempt: &&AttemptRow| {
            index
                .latest(&attempt.node_id, attempt.iteration)
                .map(|a| a.id)
                == Some(attempt.id)
        };

        // 2. A preserved-work block stops new effects until resolved.
        if let Some(blocked) = attempts
            .iter()
            .filter(latest)
            .find(|attempt| attempt.status == AttemptStatus::Blocked)
        {
            self.block(execution, blocked).await?;
            return Ok(Some(waiting));
        }

        // 3. Bounded retries: always a new attempt_no, never in place.
        let retries: Vec<NewAttempt> = attempts
            .iter()
            .filter(latest)
            .filter(|attempt| retry_due(&shape, execution, &index, attempt))
            .map(|attempt| {
                let (node_kind, catalog_op, effect_class) =
                    attempt_kind(shape.steps(), &attempt.node_id);
                NewAttempt {
                    node_id: attempt.node_id.clone(),
                    iteration: attempt.iteration,
                    attempt_no: attempt.attempt_no + 1,
                    base_commit: attempt.base_commit.clone(),
                    query: attempt.query().to_owned(),
                    node_kind,
                    catalog_op,
                    effect_class,
                }
            })
            .collect();
        if !retries.is_empty() {
            self.reserve(execution, &retries).await?;
            return Ok(None);
        }

        // 4. A terminal node failure fails the execution once drained.
        if let Some(failed) = attempts
            .iter()
            .filter(latest)
            .find(|attempt| state(&attempt.node_id, attempt.iteration) == InstanceState::Failed)
        {
            if waiting == Step::Wait {
                return Ok(Some(Step::Wait));
            }
            let error = format!(
                "node '{}' failed: {}",
                failed.node_id,
                failed
                    .error
                    .as_deref()
                    .or(failed.failure_class.as_deref())
                    .unwrap_or("failed")
            );
            self.transition(
                execution,
                &[ExecutionStatus::Running],
                ExecutionStatus::Failed,
                Some(error),
                None,
                None,
            )
            .await?;
            return Ok(Some(Step::Done));
        }

        // 5. Loop regions decide their next iteration durably.
        let decisions = shape.regions_awaiting_decision(&state, &progress);
        if !decisions.is_empty() {
            for (region, iteration) in decisions {
                self.decide_region(execution, &shape, &index, region, iteration)
                    .await?;
            }
            return Ok(None);
        }

        // 6. Completion.
        if shape.is_complete(&state, &progress) {
            self.complete(execution, &shape, &index, &progress).await?;
            return Ok(Some(Step::Done));
        }

        // 7. Reserve every ready instance in one transaction; record gate
        // results and dead-path skips without an effect.
        let taken = |edge: &EdgeDef, at: u32| edge_taken(&shape, execution, &index, edge, at);
        let ready = shape.ready_instances(&state, &progress, &taken);
        if ready.is_empty() {
            return Ok(Some(waiting));
        }
        self.reserve_ready(execution, &shape, &index, &progress, ready)
            .await?;
        Ok(None)
    }

    /// Record `blocked(preserved_work)` (or a blocked handoff) naming the
    /// attempt to resolve.
    async fn block(&self, execution: &ExecutionRow, blocked: &AttemptRow) -> Result<()> {
        let kind = if blocked.preserved_commit.is_some() {
            failure::PRESERVED_WORK
        } else {
            blocked
                .failure_class
                .as_deref()
                .unwrap_or(failure::PRESERVED_WORK)
        };
        let reason = serde_json::json!({
            "kind": kind,
            "failure_class": blocked.failure_class,
            "attempt_id": blocked.id,
            "node_id": blocked.node_id,
            "iteration": blocked.iteration,
            "preserved_commit": blocked.preserved_commit,
            "evidence": blocked.error,
        });
        self.transition(
            execution,
            &[ExecutionStatus::Running],
            ExecutionStatus::Blocked,
            None,
            None,
            Some(&reason),
        )
        .await
    }

    async fn reserve_ready(
        &self,
        execution: &ExecutionRow,
        shape: &GraphShape,
        index: &AttemptIndex<'_>,
        progress: &[RegionProgress],
        ready: Vec<(String, u32, bool)>,
    ) -> Result<()> {
        let mut reservations = Vec::with_capacity(ready.len());
        let mut settled = Vec::new();
        let mut refused = Vec::new();
        for (node, iteration, live) in ready {
            if !live {
                let (node_kind, catalog_op, effect_class) = attempt_kind(shape.steps(), &node);
                settled.push((
                    NewAttempt {
                        node_id: node,
                        iteration,
                        attempt_no: 1,
                        base_commit: execution.base_commit.clone(),
                        query: String::new(),
                        node_kind,
                        catalog_op,
                        effect_class,
                    },
                    AttemptStatus::Skipped,
                    Settlement {
                        failure_class: Some(failure::DEAD_PATH),
                        error: Some("every incoming edge was untaken".into()),
                        ..Settlement::default()
                    },
                ));
                continue;
            }
            match Self::plan_instance(execution, shape, index, progress, &node, iteration) {
                Ok(reservation) => match shape.steps().step(&node) {
                    Some(TopologyStep::Gate { condition }) => {
                        let (status, settlement) = Self::evaluate_gate(
                            execution,
                            shape,
                            index,
                            progress,
                            &reservation,
                            condition,
                        );
                        settled.push((reservation, status, settlement));
                    }
                    _ => reservations.push(reservation),
                },
                Err(error) => refused.push((node, iteration, error.to_string())),
            }
        }
        if !refused.is_empty() {
            self.refuse_instances(execution, refused).await?;
        }
        if !settled.is_empty() {
            let updates = {
                let store = self.store.lock().await;
                rows::record_settled_attempts(&store, execution.id, &settled)
            };
            match updates {
                Ok(updates) => self.publish(updates),
                Err(DaemonError::PolicyDenied(message)) => {
                    return self
                        .transition(
                            execution,
                            &[ExecutionStatus::Running],
                            ExecutionStatus::Failed,
                            Some(message),
                            None,
                            None,
                        )
                        .await;
                }
                Err(error) => return Err(error),
            }
        }
        self.reserve(execution, &reservations).await?;
        Ok(())
    }

    /// A gate is pure: evaluate its condition over the typed outputs of the
    /// ancestors it names (plan §3). A type error or missing path fails it.
    fn evaluate_gate(
        execution: &ExecutionRow,
        shape: &GraphShape,
        index: &AttemptIndex<'_>,
        progress: &[RegionProgress],
        reservation: &NewAttempt,
        condition: &rsi_common::types::GateCondition,
    ) -> (AttemptStatus, Settlement) {
        let region = shape.region_of(&reservation.node_id);
        let mut nodes = serde_json::Map::new();
        let mut error = None;
        for path in condition.paths() {
            let Ok(node) = rsi_common::types::gate_path_node(path) else {
                continue;
            };
            if nodes.contains_key(node) {
                continue;
            }
            let at = shape.source_iteration(node, region, reservation.iteration, progress);
            let source_edge = shape
                .forward_edges(&reservation.node_id)
                .iter()
                .find(|edge| {
                    edge.source == node
                        && shape.steps().when(node, &reservation.node_id)
                            == rsi_common::types::EdgeWhen::Completed
                });
            let source_attempt = index.result(node, at).or_else(|| {
                source_edge
                    .filter(|_| {
                        instance_state(shape, execution, index, node, at) == InstanceState::Routed
                    })
                    .and_then(|_| index.latest(node, at))
            });
            if let Some(attempt) = source_attempt {
                match node_output(&execution.repo_root, attempt)
                    .and_then(|data| Ok(serde_json::to_value(&data)?))
                {
                    Ok(mut value) => {
                        let fields = value["fields"].as_object_mut();
                        if attempt.status != AttemptStatus::Succeeded
                            && let Some(fields) = fields
                        {
                            fields.insert(
                                "failure_class".into(),
                                serde_json::Value::String(
                                    attempt.failure_class.clone().unwrap_or_default(),
                                ),
                            );
                            fields.insert(
                                "error".into(),
                                serde_json::Value::String(
                                    attempt.error.clone().unwrap_or_default(),
                                ),
                            );
                        }
                        nodes.insert(node.to_owned(), value["fields"].clone());
                    }
                    Err(read) => error = Some(read.to_string()),
                }
            }
        }
        let result = error.map_or_else(|| crate::topology::gate::evaluate(condition, &nodes), Err);
        match result {
            Ok(value) => {
                let output = serde_json::json!({
                    "value": value,
                    "condition_digest": crate::topology::gate::condition_digest(condition),
                });
                (
                    AttemptStatus::Succeeded,
                    Settlement {
                        // A gate passes its fork commit through unchanged.
                        result_commit: Some(reservation.base_commit.clone()),
                        output: serde_json::to_value(node_data(&output)).ok(),
                        ..Settlement::default()
                    },
                )
            }
            Err(error) => (AttemptStatus::Failed, failed(failure::GATE_ERROR, &error)),
        }
    }

    async fn transition(
        &self,
        execution: &ExecutionRow,
        from: &[ExecutionStatus],
        to: ExecutionStatus,
        error: Option<String>,
        output: Option<&Value>,
        blocked_reason: Option<&Value>,
    ) -> Result<()> {
        let update = {
            let store = self.store.lock().await;
            rows::transition_execution(
                &store,
                execution.id,
                from,
                to,
                error,
                output,
                blocked_reason,
            )?
        };
        self.publish(update);
        Ok(())
    }

    async fn reserve(&self, execution: &ExecutionRow, attempts: &[NewAttempt]) -> Result<()> {
        if attempts.is_empty() {
            return Ok(());
        }
        self.effects.before_reserve(execution.id).await;
        let reserved = {
            let store = self.store.lock().await;
            rows::reserve_attempts(&store, execution.id, attempts)
        };
        match reserved {
            Ok(updates) => {
                self.publish(updates);
                Ok(())
            }
            Err(DaemonError::PolicyDenied(message)) => {
                self.transition(
                    execution,
                    &[ExecutionStatus::Running],
                    ExecutionStatus::Failed,
                    Some(message),
                    None,
                    None,
                )
                .await
            }
            Err(error) => Err(error),
        }
    }

    /// Record instances whose custody cannot resolve (a missing pin refuses
    /// the launch) as failed attempts with zero effects.
    async fn refuse_instances(
        &self,
        execution: &ExecutionRow,
        refused: Vec<(String, u32, String)>,
    ) -> Result<()> {
        let steps = WorkflowSteps::from_workflow(&execution.definition).unwrap_or_default();
        let refused: Vec<(NewAttempt, String)> = refused
            .into_iter()
            .map(|(node_id, iteration, error)| {
                let (node_kind, catalog_op, effect_class) = attempt_kind(&steps, &node_id);
                (
                    NewAttempt {
                        node_id,
                        iteration,
                        attempt_no: 1,
                        base_commit: execution.base_commit.clone(),
                        query: String::new(),
                        node_kind,
                        catalog_op,
                        effect_class,
                    },
                    error,
                )
            })
            .collect();
        let updates = {
            let store = self.store.lock().await;
            rows::refuse_attempts(&store, execution.id, &refused)?
        };
        self.publish(updates);
        Ok(())
    }

    /// Resolve the fork commit and rendered query for one ready instance.
    fn plan_instance(
        execution: &ExecutionRow,
        shape: &GraphShape,
        index: &AttemptIndex<'_>,
        progress: &[RegionProgress],
        node_id: &str,
        iteration: u32,
    ) -> Result<NewAttempt> {
        let node = node_def(execution, node_id)?;
        let plan = execution.custody_plan.node(node_id)?;
        let base_commit = match plan.durable_source(iteration) {
            None => execution.base_commit.clone(),
            Some((source, selector)) => {
                let result = match selector {
                    PinSelector::Iteration(at) => index.result(source, at),
                    PinSelector::Latest => index.latest_result(source),
                };
                // A `failure` route forks from the commit the failed source
                // itself forked from; its work produced no result.
                let routed = || {
                    let at = match selector {
                        PinSelector::Iteration(at) => at,
                        PinSelector::Latest => shape.source_iteration(
                            source,
                            shape.region_of(node_id),
                            iteration,
                            progress,
                        ),
                    };
                    (instance_state(shape, execution, index, source, at) == InstanceState::Routed)
                        .then(|| index.latest(source, at).map(|a| a.base_commit.clone()))
                        .flatten()
                };
                result
                    .and_then(|attempt| attempt.result_commit.clone())
                    .or_else(routed)
                    .ok_or_else(|| {
                        DaemonError::InvalidParam(format!(
                            "custody source node {source} has no committed result for {selector:?}"
                        ))
                    })?
            }
        };
        let input = if shape.is_source(node_id) {
            let mut input = execution
                .input
                .clone()
                .and_then(|value| serde_json::from_value::<NodeData>(value).ok())
                .unwrap_or_default();
            input.remove("base_commit");
            input
        } else {
            // Typed downstream rendering (plan §3): one section per taken
            // upstream edge; the full last message only with `pass_content`.
            let region = shape.region_of(node_id);
            let pass_content = shape.steps().pass_content(node_id);
            let mut input = NodeData::new();
            for edge in shape.forward_edges(node_id) {
                let at = shape.source_iteration(&edge.source, region, iteration, progress);
                if !edge_taken(shape, execution, index, edge, at) {
                    continue;
                }
                let output = match index.result(&edge.source, at) {
                    Some(attempt) => node_output(&execution.repo_root, attempt)?,
                    // A routed failure retains both its typed recorded output
                    // and the executor's terminal failure details.
                    None => match index.latest(&edge.source, at) {
                        Some(a) => {
                            let mut output = node_output(&execution.repo_root, a)?;
                            output.insert(
                                "failure_class",
                                GraphValue::String(a.failure_class.clone().unwrap_or_default()),
                            );
                            output.insert(
                                "error",
                                GraphValue::String(a.error.clone().unwrap_or_default()),
                            );
                            output
                        }
                        None => NodeData::new(),
                    },
                };
                let output = crate::session::graph_runner::apply_edge_filter(&output, edge);
                if let Some(context) =
                    output.get(crate::session::graph_runner::PIPELINE_ENTRY_CONTEXT_KEY)
                {
                    input.insert(
                        crate::session::graph_runner::PIPELINE_ENTRY_CONTEXT_KEY,
                        context.clone(),
                    );
                }
                input.insert(
                    edge.source.clone(),
                    GraphValue::String(render_upstream(&output, pass_content)),
                );
            }
            input
        };
        let (node_kind, catalog_op, effect_class) = attempt_kind(shape.steps(), node_id);
        Ok(NewAttempt {
            node_id: node_id.to_owned(),
            iteration,
            attempt_no: 1,
            base_commit,
            query: crate::session::graph_runner::build_node_query(node, &input),
            node_kind,
            catalog_op,
            effect_class,
        })
    }

    async fn decide_region(
        &self,
        execution: &ExecutionRow,
        shape: &GraphShape,
        index: &AttemptIndex<'_>,
        region: usize,
        iteration: u32,
    ) -> Result<()> {
        let mut lead_halted = false;
        for attempt in shape
            .region_nodes(region)
            .iter()
            .filter_map(|node| index.result(node, iteration))
        {
            lead_halted |= output_mentions_halt(&execution.repo_root, attempt)?;
        }
        let predicate_met = match shape.until() {
            Some(UntilCondition::Predicate(expr)) => {
                self.effects.predicate_met(expr, execution.project_id).await
            }
            _ => false,
        };
        let decision = match shape.decide_region(region, iteration, lead_halted, predicate_met) {
            RegionDecision::Continue => "continue".to_owned(),
            RegionDecision::Halt(reason) => format!("halt:{reason}"),
        };
        let update = {
            let store = self.store.lock().await;
            rows::record_region_decision(&store, execution.id, region, iteration, &decision)?
        };
        self.publish([update]);
        Ok(())
    }

    async fn complete(
        &self,
        execution: &ExecutionRow,
        shape: &GraphShape,
        index: &AttemptIndex<'_>,
        progress: &[RegionProgress],
    ) -> Result<()> {
        let mut output = NodeData::new();
        for sink in shape.sinks() {
            let at = shape.source_iteration(sink, None, 0, progress);
            if let Some(attempt) = index.result(sink, at) {
                output.merge(node_output(&execution.repo_root, attempt)?);
            }
        }
        let output = serde_json::to_value(&output)?;
        self.transition(
            execution,
            &[ExecutionStatus::Running],
            ExecutionStatus::Succeeded,
            None,
            Some(&output),
            None,
        )
        .await?;
        // Pins are released and node sessions archived at settlement.
        crate::topology::recovery::settlement_cleanup(self, CLEANUP_BATCH).await;
        Ok(())
    }

    pub(crate) async fn settle(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        status: AttemptStatus,
        settlement: Settlement,
    ) -> Result<()> {
        let update = {
            let store = self.store.lock().await;
            rows::settle_attempt(&store, execution.id, attempt, status, &settlement)?
        };
        self.publish([update]);
        Ok(())
    }

    /// Launch, adopt, or settle one in-flight attempt. Returns `true` when a
    /// durable row changed.
    async fn drive_attempt(&self, execution: &ExecutionRow, attempt: &AttemptRow) -> Result<bool> {
        if attempt.node_kind == "command" {
            return self.drive_command(execution, attempt).await;
        }
        match attempt.status {
            AttemptStatus::Reserved | AttemptStatus::Launching => {
                if execution.status == ExecutionStatus::Cancelling {
                    // Never launch into a cancelling execution.
                    if self.effects.session(attempt.session_id).await.is_none() {
                        self.settle(
                            execution,
                            attempt,
                            AttemptStatus::Cancelled,
                            Settlement {
                                failure_class: Some(failure::CANCELLED),
                                ..Settlement::default()
                            },
                        )
                        .await?;
                        return Ok(true);
                    }
                }
                self.launch_attempt(execution, attempt).await
            }
            AttemptStatus::Running => self.observe_attempt(execution, attempt).await,
            _ => Ok(false),
        }
    }

    /// Plan §2.4: the same attempt (same session id and dedup key) launches
    /// only while no admission exists; an admitted launch without a session
    /// row settles `lost` and is replaced uncharged; a session row is adopted.
    async fn launch_attempt(&self, execution: &ExecutionRow, attempt: &AttemptRow) -> Result<bool> {
        if let Some(observed) = self.effects.session(attempt.session_id).await {
            self.adopt(execution, attempt, observed).await?;
            return Ok(true);
        }
        {
            let store = self.store.lock().await;
            if rows::invocation_admitted(&store, &attempt.dedup_key)? {
                drop(store);
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Lost,
                    Settlement {
                        failure_class: Some(failure::LOST_BEFORE_SESSION),
                        error: Some("admitted launch produced no session".into()),
                        ..Settlement::default()
                    },
                )
                .await?;
                return Ok(true);
            }
            if !rows::mark_launching(&store, attempt.id, self.boot_id)? {
                return Ok(false);
            }
        }
        let request = match Self::launch_request(execution, attempt) {
            Ok(request) => request,
            Err(error) => {
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Failed,
                    Settlement {
                        failure_class: Some(failure::CUSTODY_REFUSED),
                        error: Some(error.to_string()),
                        ..Settlement::default()
                    },
                )
                .await?;
                return Ok(true);
            }
        };
        match self.effects.launch(request).await {
            Ok(_) => {
                let update = {
                    let store = self.store.lock().await;
                    rows::mark_running(&store, execution.id, attempt, None)?
                };
                self.publish([update]);
            }
            Err(error) => {
                if let Some(observed) = self.effects.session(attempt.session_id).await {
                    self.adopt(execution, attempt, observed).await?;
                } else {
                    self.settle(
                        execution,
                        attempt,
                        AttemptStatus::Failed,
                        Settlement {
                            failure_class: Some(failure::LAUNCH_REFUSED),
                            error: Some(error.to_string()),
                            ..Settlement::default()
                        },
                    )
                    .await?;
                }
            }
        }
        Ok(true)
    }

    async fn adopt(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        observed: SessionObservation,
    ) -> Result<()> {
        let update = {
            let store = self.store.lock().await;
            if let Some(root) = &observed.sandbox_root {
                rows::record_sandbox_root(&store, attempt.id, root)?;
            }
            if !observed.status.is_terminal() {
                rows::stamp_boot(&store, attempt.id, self.boot_id)?;
            }
            rows::mark_running(&store, execution.id, attempt, None)?
        };
        self.publish([update]);
        Ok(())
    }

    fn launch_request(execution: &ExecutionRow, attempt: &AttemptRow) -> Result<LaunchRequest> {
        let node = node_def(execution, &attempt.node_id)?;
        let custody = TopologyCustody::new(
            execution.repo_root.clone(),
            execution.base_commit.clone(),
            execution.id,
        );
        let builder = NodeLaunchBuilder {
            custody: &custody,
            is_topology: execution
                .definition
                .metadata
                .contains_key("source_topology_id"),
            workflow_id: execution.workflow_id,
            project_id: execution.project_id,
            parent_id: execution.parent_session_id,
        };
        let config = builder.build_attempt(
            node,
            attempt.query().to_owned(),
            attempt.iteration,
            attempt.attempt_no,
            attempt.dedup_key.clone(),
        )?;
        let fork = TopologyForkSource::verified(&execution.repo_root, &attempt.base_commit)?;
        Ok(LaunchRequest {
            session_id: attempt.session_id,
            config,
            fork,
        })
    }

    async fn observe_attempt(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        let Some(observed) = self.effects.session(attempt.session_id).await else {
            let settlement = failed(failure::LOST_AFTER_SESSION, "node session row disappeared");
            self.settle(execution, attempt, AttemptStatus::Lost, settlement)
                .await?;
            return Ok(true);
        };
        // Launched (or last seen alive) by an earlier daemon incarnation.
        let previous_boot = attempt.boot_id != Some(self.boot_id);
        {
            let store = self.store.lock().await;
            if let Some(root) = &observed.sandbox_root
                && attempt.sandbox_root.is_none()
            {
                rows::record_sandbox_root(&store, attempt.id, root)?;
            }
            if previous_boot && !observed.status.is_terminal() {
                rows::stamp_boot(&store, attempt.id, self.boot_id)?;
            }
        }
        let sandbox = observed
            .sandbox_root
            .or_else(|| attempt.sandbox_root.clone());
        let (status, settlement) = match observed.status {
            status if !status.is_terminal() => return self.observe_live(execution, attempt).await,
            SessionStatus::Completed => self.observe_completed(execution, attempt, sandbox).await?,
            SessionStatus::Failed if !previous_boot => (
                AttemptStatus::Failed,
                failed(failure::SESSION_FAILED, "node session failed"),
            ),
            _ if execution.status == ExecutionStatus::Cancelling => (
                AttemptStatus::Cancelled,
                Settlement {
                    failure_class: Some(failure::CANCELLED),
                    ..Settlement::default()
                },
            ),
            // Startup restore marks every provider that was live when the
            // daemon died `Failed`; for an attempt this incarnation never saw
            // alive that is the restart interrupting it (plan §2.4 → §3.4).
            SessionStatus::Failed => {
                Self::interrupted_outcome(execution, attempt, sandbox, SessionStatus::Interrupted)
            }
            // Interrupted, Archived or Deleted before completing.
            ended => Self::interrupted_outcome(execution, attempt, sandbox, ended),
        };
        self.settle(execution, attempt, status, settlement).await?;
        // Interrupt-path and terminal reclaim alike: only this attempt's cache.
        self.effects.reclaim(attempt.session_id).await;
        Ok(true)
    }

    /// A live session: interrupt it while cancelling, or bound its wall time.
    async fn observe_live(&self, execution: &ExecutionRow, attempt: &AttemptRow) -> Result<bool> {
        if execution.status == ExecutionStatus::Cancelling {
            self.effects.interrupt(attempt.session_id).await;
            return Ok(false);
        }
        let started = attempt.started_at.unwrap_or(execution.created_at);
        let wall_deadline = started + SESSION_WALL_TIME;
        let deadline = execution
            .deadline_at
            .map_or(wall_deadline, |deadline| deadline.min(wall_deadline));
        if Utc::now() < deadline {
            return Ok(false);
        }
        self.effects.interrupt(attempt.session_id).await;
        let settlement = failed(
            failure::TIMEOUT,
            "node session exceeded its wall-time limit",
        );
        self.settle(execution, attempt, AttemptStatus::Failed, settlement)
            .await?;
        Ok(true)
    }

    /// Plan §4 rule 3: observe git, never trust a reported SHA.
    async fn observe_completed(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        sandbox: Option<PathBuf>,
    ) -> Result<(AttemptStatus, Settlement)> {
        let Some(sandbox) = sandbox else {
            return Ok((
                AttemptStatus::Failed,
                Settlement {
                    failure_class: Some(failure::CUSTODY_REFUSED),
                    error: Some("topology session has no sandbox".into()),
                    ..Settlement::default()
                },
            ));
        };
        let observed = match custody::observe_sandbox(&sandbox) {
            Ok(observed) => observed,
            Err(error) => {
                return Ok((
                    AttemptStatus::Failed,
                    Settlement {
                        failure_class: Some(failure::CUSTODY_REFUSED),
                        error: Some(error.to_string()),
                        ..Settlement::default()
                    },
                ));
            }
        };
        if observed.dirty {
            // T1's `uncommitted_work` is preserved work, never discarded.
            return Ok(Self::preserve(execution, attempt, &sandbox));
        }
        let node = node_def(execution, &attempt.node_id)?;
        let steps = WorkflowSteps::from_workflow(&execution.definition)
            .map_err(DaemonError::InvalidParam)?;
        let mut output = self.effects.output(attempt.session_id).await?;
        let content = match output.get("content") {
            Some(GraphValue::String(content)) => content.clone(),
            _ => String::new(),
        };
        let handoff = match parse_pipeline_handoff_v2(&content, "") {
            Ok(handoff) => handoff,
            Err(error) => {
                return Ok((
                    AttemptStatus::Failed,
                    Settlement {
                        failure_class: Some(failure::HANDOFF_INVALID),
                        error: Some(error.to_string()),
                        ..Settlement::default()
                    },
                ));
            }
        };
        if handoff.strict_status != PipelineStatusV2::Complete {
            return Ok((
                AttemptStatus::Blocked,
                Settlement {
                    failure_class: Some(failure::HANDOFF_BLOCKED),
                    error: Some(format!(
                        "status={:?}; class={:?}; evidence={}",
                        handoff.strict_status,
                        handoff.blocker_class,
                        handoff.blocker_evidence.as_deref().unwrap_or_default()
                    )),
                    ..Settlement::default()
                },
            ));
        }
        if steps.expects_commit(&attempt.node_id) && observed.head == attempt.base_commit {
            return Ok((
                AttemptStatus::Failed,
                Settlement {
                    failure_class: Some(failure::HANDOFF_INVALID),
                    error: Some("node handoff requires a new commit".into()),
                    ..Settlement::default()
                },
            ));
        }
        let pin = node_pin_ref(execution.id, &attempt.node_id, attempt.iteration);
        if let Err(error) = custody::pin_commit(&sandbox, &pin, &observed.head) {
            return Ok((
                AttemptStatus::Failed,
                Settlement {
                    failure_class: Some(failure::CUSTODY_REFUSED),
                    error: Some(error.to_string()),
                    ..Settlement::default()
                },
            ));
        }
        let mut typed_handoff = std::collections::BTreeMap::new();
        typed_handoff.insert(
            "stage".into(),
            GraphValue::String(handoff.handoff.stage.as_str().to_ascii_lowercase()),
        );
        typed_handoff.insert("status".into(), GraphValue::String("complete".into()));
        // `artifacts[path@commit]`: the handoff doc when it lives in the
        // sandbox, addressed by the pinned result commit.
        let artifacts: Vec<GraphValue> = std::path::Path::new(&handoff.handoff.doc_path)
            .strip_prefix(&sandbox)
            .ok()
            .map(|relative| GraphValue::String(format!("{}@{}", relative.display(), observed.head)))
            .into_iter()
            .collect();
        typed_handoff.insert(
            "doc_path".into(),
            GraphValue::String(handoff.handoff.doc_path),
        );
        output.insert("handoff", GraphValue::Map(typed_handoff));
        output.insert("artifacts", GraphValue::List(artifacts));
        output.insert(
            "base_commit",
            GraphValue::String(attempt.base_commit.clone()),
        );
        output.insert("result_commit", GraphValue::String(observed.head.clone()));
        output.insert(
            "changed",
            GraphValue::Bool(observed.head != attempt.base_commit),
        );
        output.insert(
            "content_tail",
            GraphValue::String(
                content
                    .chars()
                    .rev()
                    .take(8192)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect(),
            ),
        );
        // The full message stays recorded (large outputs go to Git) for
        // `pass_content` consumers and `/halt` detection; rendering decides
        // what flows downstream.
        crate::session::graph_runner::attach_pipeline_entry_context(node, &mut output);
        let text = serde_json::to_string(&output)?;
        let output = if text.len() > rows::OUTPUT_INLINE_LIMIT {
            let name = custody::output_ref(
                execution.id,
                &attempt.node_id,
                attempt.iteration,
                attempt.attempt_no,
            );
            let commit = custody::store_output(&execution.repo_root, &name, &text)?;
            serde_json::json!({ EXTERNAL_OUTPUT_KEY: {
                "ref": name,
                "commit": commit,
                "path": custody::OUTPUT_PATH,
                "digest": rows::digest(&text),
                "size": text.len(),
            }})
        } else {
            serde_json::to_value(&output)?
        };
        Ok((
            AttemptStatus::Succeeded,
            Settlement {
                result_commit: Some(observed.head),
                pin_ref: Some(pin),
                output: Some(output),
                ..Settlement::default()
            },
        ))
    }

    /// An interrupted or lost session: diverged work is preserved and blocks;
    /// an unchanged sandbox is retryable.
    fn interrupted_outcome(
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        sandbox: Option<PathBuf>,
        status: SessionStatus,
    ) -> (AttemptStatus, Settlement) {
        let clean = || {
            if status == SessionStatus::Interrupted {
                (
                    AttemptStatus::Interrupted,
                    Settlement {
                        failure_class: Some(failure::INTERRUPTED),
                        ..Settlement::default()
                    },
                )
            } else {
                (
                    AttemptStatus::Lost,
                    Settlement {
                        failure_class: Some(failure::LOST_AFTER_SESSION),
                        ..Settlement::default()
                    },
                )
            }
        };
        match sandbox {
            Some(sandbox) if sandbox.exists() => match custody::observe_sandbox(&sandbox) {
                Ok(observed) if !observed.dirty && observed.head == attempt.base_commit => clean(),
                Ok(_) => Self::preserve(execution, attempt, &sandbox),
                Err(error) => (
                    AttemptStatus::Failed,
                    Settlement {
                        failure_class: Some(failure::CUSTODY_REFUSED),
                        error: Some(error.to_string()),
                        ..Settlement::default()
                    },
                ),
            },
            _ => clean(),
        }
    }

    /// Plan §3.4: record a verified preservation point before blocking.
    pub(crate) fn preserve(
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        sandbox: &std::path::Path,
    ) -> (AttemptStatus, Settlement) {
        let name = preserved_ref(
            execution.id,
            &attempt.node_id,
            attempt.iteration,
            attempt.attempt_no,
        );
        match custody::preserve_sandbox(sandbox, &name, attempt.id, &attempt.base_commit) {
            Ok(Some(preservation)) => (
                AttemptStatus::Blocked,
                Settlement {
                    failure_class: Some(failure::PRESERVED_WORK),
                    error: Some(
                        "diverged sandbox preserved; resolve with ResolveTopologyAttempt".into(),
                    ),
                    preserved_ref: Some(preservation.ref_name),
                    preserved_commit: Some(preservation.commit),
                    preserved_paths_digest: preservation.paths_digest,
                    ..Settlement::default()
                },
            ),
            Ok(None) => (
                AttemptStatus::Interrupted,
                Settlement {
                    failure_class: Some(failure::INTERRUPTED),
                    ..Settlement::default()
                },
            ),
            // Never retry over work that could not be preserved.
            Err(error) => (
                AttemptStatus::Failed,
                Settlement {
                    failure_class: Some(failure::CUSTODY_REFUSED),
                    error: Some(format!("preservation failed: {error}")),
                    ..Settlement::default()
                },
            ),
        }
    }

    /// Request interruption durably (plan §2.5): `cancelling` first, then
    /// each in-flight session is interrupted by the driver.
    pub(crate) async fn request_interrupt(
        &self,
        execution_id: Uuid,
    ) -> Result<Option<ExecutionStatus>> {
        let execution = {
            let store = self.store.lock().await;
            rows::load_execution(&store, execution_id)?
        };
        let Some(execution) = execution else {
            return Ok(None);
        };
        self.transition(
            &execution,
            &[
                ExecutionStatus::Accepted,
                ExecutionStatus::Running,
                ExecutionStatus::Blocked,
            ],
            ExecutionStatus::Cancelling,
            None,
            None,
            None,
        )
        .await?;
        Ok(Some(execution.status))
    }
}

pub(crate) fn failed(class: &'static str, error: &str) -> Settlement {
    Settlement {
        failure_class: Some(class),
        error: Some(error.to_owned()),
        ..Settlement::default()
    }
}

pub(crate) fn node_def<'a>(execution: &'a ExecutionRow, node_id: &str) -> Result<&'a NodeDef> {
    execution
        .definition
        .nodes
        .iter()
        .find(|node| node.id == node_id)
        .ok_or_else(|| {
            DaemonError::InvalidParam(format!("node '{node_id}' not found in workflow definition"))
        })
}

/// One live driver per execution inside this daemon incarnation.
#[derive(Default)]
pub(crate) struct DriverRegistry {
    drivers: std::sync::Mutex<HashMap<Uuid, Arc<tokio::sync::Notify>>>,
    advancing: std::sync::Mutex<HashMap<Uuid, std::sync::Weak<Mutex<()>>>>,
    /// Catalog-op runs of this incarnation (#635); lost on restart by design.
    pub(crate) commands: CommandRunner,
}

impl DriverRegistry {
    /// The per-execution advance lock; entries die with their last holder.
    fn advance_lock(&self, execution_id: Uuid) -> Arc<Mutex<()>> {
        let mut advancing = self
            .advancing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(lock) = advancing
            .get(&execution_id)
            .and_then(std::sync::Weak::upgrade)
        {
            return lock;
        }
        advancing.retain(|_, lock| lock.strong_count() > 0);
        let lock = Arc::new(Mutex::new(()));
        advancing.insert(execution_id, Arc::downgrade(&lock));
        lock
    }

    /// Wake an existing driver; `false` when none is registered.
    pub(crate) fn wake(&self, execution_id: Uuid) -> bool {
        let drivers = self
            .drivers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drivers
            .get(&execution_id)
            .map(|notify| notify.notify_one())
            .is_some()
    }

    fn register(&self, execution_id: Uuid) -> Option<Arc<tokio::sync::Notify>> {
        let mut drivers = self
            .drivers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if drivers.contains_key(&execution_id) {
            return None;
        }
        let notify = Arc::new(tokio::sync::Notify::new());
        drivers.insert(execution_id, Arc::clone(&notify));
        Some(notify)
    }

    fn unregister(&self, execution_id: Uuid) {
        self.drivers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&execution_id);
    }

    pub(crate) fn is_driving(&self, execution_id: Uuid) -> bool {
        self.drivers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&execution_id)
    }
}

/// Spawn (or wake) the single driver task of one execution. The driver wakes
/// on any session status change, an explicit wake, or the 30 s tick; a lagged
/// bus simply re-reads durable state.
pub(crate) fn spawn_driver<E: NodeEffects>(
    executor: Executor<E>,
    bus: Arc<crate::bus::EventBus>,
    execution_id: Uuid,
) {
    let registry = Arc::clone(&executor.registry);
    let Some(notify) = registry.register(execution_id) else {
        registry.wake(execution_id);
        return;
    };
    tokio::spawn(async move {
        let mut events = bus.subscribe();
        {
            let store = executor.store.lock().await;
            if let Err(error) = rows::claim_lease(&store, execution_id, executor.boot_id) {
                tracing::warn!(%execution_id, %error, "topology lease claim failed");
            }
        }
        loop {
            match executor.advance(execution_id).await {
                Ok(Step::Done | Step::Paused) => break,
                Ok(Step::Wait) => {}
                Err(error) => {
                    tracing::warn!(%execution_id, %error, "topology advance failed; retrying on tick");
                }
            }
            let tick = tokio::time::sleep(EXECUTOR_TICK);
            tokio::pin!(tick);
            loop {
                tokio::select! {
                    () = notify.notified() => break,
                    () = &mut tick => break,
                    event = events.recv() => match event {
                        Ok(event) => {
                            if matches!(event.as_ref(), crate::bus::DaemonEvent::SessionStatusChanged { .. }) {
                                break;
                            }
                        }
                        // Lagged: re-read durable state instead of waiting.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tick.as_mut().await;
                            break;
                        }
                    },
                }
            }
        }
        bus.unsubscribe();
        registry.unregister(execution_id);
    });
}
