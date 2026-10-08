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

use chrono::{DateTime, Utc};
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
use crate::topology::land::{LandEnqueue, LandRequest, LandStatus};
use crate::topology::launch::NodeLaunchBuilder;
use crate::topology::oncall;
use crate::topology::review::{
    ExtraRound, ExtraRoundRequest, OncallAcceptance, ReviewRequest, ReviewStatus,
};
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

/// The prompt a restart-cut node session continues with: the same words the
/// restart journal's own continuation uses.
const RESTART_RESUME_PROMPT: &str = "The daemon restarted after interrupting your active turn. Continue the task from its durable state.";

/// How a restart resume of a node session ended (#1728).
enum ResumeOutcome {
    Continued,
    Refused,
    Deferred,
}

/// What the daemon observes about a node session.
#[derive(Clone, Debug)]
pub(crate) struct SessionObservation {
    pub(crate) status: SessionStatus,
    pub(crate) sandbox_root: Option<PathBuf>,
    /// When the session entered its current wait on an answer, if the daemon
    /// knows (#1704). `None` when it does not; the wait then starts at the
    /// last moment the attempt was known not to be waiting.
    pub(crate) waiting_since: Option<chrono::DateTime<Utc>>,
    /// #1641 S4a: the provider supports resuming this exact session.
    pub(crate) resumable: bool,
    /// #1641 S4a: the daemon's restart journal still owns this session (a
    /// deploy drain or graceful restart cut its turn and the startup pass has
    /// not continued it yet). It is live, not ended.
    pub(crate) restart_intent_pending: bool,
    /// Why the session stopped, as the daemon recorded it.
    pub(crate) stop_reason: Option<String>,
    /// Cumulative milliseconds the session has spent in waits that already
    /// ended (its own counter, which an answer or resume never clears). A wait
    /// that began and ended between two observations leaves no open wait to
    /// see, only this total (#1704). `None` when the daemon does not know.
    pub(crate) waited_ms: Option<u64>,
    /// #1728: classification, journal ownership and turn cursor read in
    /// one store snapshot. Production recovery defers if this is unavailable.
    pub(crate) restart_cut_fence: Option<crate::store::manager_actions::fence::RestartCutFenceV1>,
}

impl SessionObservation {
    /// #1641 S4a: the session was cut off by a daemon restart rather than
    /// ending on its own. A crash leaves it `Failed` with no recorded cause;
    /// a graceful restart, a shutdown or a deploy drain leaves it
    /// `Interrupted` with the restart cause. A session the restart journal
    /// already tried (and refused) carries `daemon_restart_resume:..` and is
    /// not retried here.
    pub(crate) fn cut_by_restart(&self) -> bool {
        let reason = self.stop_reason.as_deref().unwrap_or("").trim();
        match self.status {
            SessionStatus::Failed => reason.is_empty() || reason.starts_with("interrupted:"),
            SessionStatus::Interrupted => matches!(
                reason,
                "interrupted:daemon_restart"
                    | "interrupted:daemon_shutdown"
                    | "interrupted:deploy_drain"
            ),
            _ => false,
        }
    }
}

/// One answer delivery to a parked node's session (#1715).
#[derive(Clone, Debug)]
pub(crate) struct AnswerContinuation {
    pub(crate) session_id: Uuid,
    pub(crate) prompt: String,
    pub(crate) binding: crate::topology::store::AnswerBinding,
}

/// #1641 S4b: the `admission_hold` kind of a launch refused by the sandbox
/// capacity gate (`launch.rs`, JSON-RPC code -32029), or `None` for any other
/// error. A direct-root limit is `source_root_limit`; a free-space shortfall is
/// `disk_floor`.
pub(crate) fn admission_hold_kind(error: &DaemonError) -> Option<&'static str> {
    let DaemonError::StructuredRpc {
        rpc_code: -32029,
        data,
        ..
    } = error
    else {
        return None;
    };
    Some(match data["code"].as_str() {
        Some("source_root_limit") => "source_root_limit",
        _ => "disk_floor",
    })
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
    /// Kill a process group left by an earlier incarnation (plan §2.4), only
    /// once it is proven to still run inside the attempt's `sandbox` (#1227).
    fn kill_stale_group(&self, pgid: i32, sandbox: Option<&std::path::Path>);
    /// Operator setting `topology_max_concurrent_build_nodes`.
    fn build_node_cap(&self) -> u32;
    /// Suspension point between a resolution's key pre-check and its
    /// recording transaction. Production: no-op; tests force a key race.
    fn before_resolution_record(&self, _execution_id: Uuid) -> impl Future<Output = ()> + Send {
        async {}
    }
    /// Host-load admission (#1417): `true` while the daemon holds this
    /// attempt's launch because the host is too loaded for a new worker. The
    /// attempt stays `Reserved`, nothing is written and the driver asks again
    /// on its next tick. `unattended` is false only for the first launches of
    /// an operator-requested execution (an operator start is immediate); every
    /// later node of any execution is held like any other (#1641 S4b). `since`
    /// (the execution's age) orders held launches oldest-first. Production
    /// forwards to the host-load gate; the default never holds.
    fn launch_held(&self, _unattended: bool, _since: DateTime<Utc>, _attempt: &AttemptRow) -> bool {
        false
    }
    /// #1641 S4b: a command node that runs cargo takes a governor build slot
    /// (which also applies the disk floor, `min_free_disk_gb`) before it
    /// starts. `Some(kind)` (`disk_floor` or `build_slot`) holds the attempt
    /// `Reserved` until the slot is granted; the slot is kept for the attempt
    /// until [`Self::release_build_slot`]. The default never holds.
    fn build_slot_held(&self, _attempt: &AttemptRow) -> Option<&'static str> {
        None
    }
    /// Release the build slot [`Self::build_slot_held`] granted (idempotent).
    fn release_build_slot(&self, _attempt_id: Uuid) {}
    /// Review nodes (#1641): open a review assignment for the commit under
    /// review and return its id. Must be idempotent per `request.attempt_id`
    /// so a crash between the request and its record never opens a second
    /// assignment. The default refuses: a daemon without a review service
    /// blocks the node (`review_unsettled`) instead of waiting forever.
    fn request_review(&self, _request: ReviewRequest) -> impl Future<Output = Result<Uuid>> + Send {
        async {
            Err(DaemonError::PolicyDenied(
                "the review service is not wired to topology review nodes".into(),
            ))
        }
    }
    /// Review nodes: the assignment's current verdict, re-derived from
    /// durable state on every call (never cached).
    fn review_status(&self, _assignment_id: Uuid) -> impl Future<Output = ReviewStatus> + Send {
        async { ReviewStatus::Unsettled("review_service_unavailable".into()) }
    }
    /// Review nodes (#1715): whether one more round of an exhausted review is
    /// policy-valid, re-derived from the review ledger. The default offers the
    /// node's own reviewer; production applies the store's closure-specialist
    /// and round-budget rules.
    fn review_extra_round(
        &self,
        _request: ExtraRoundRequest,
    ) -> impl Future<Output = ExtraRound> + Send {
        async { ExtraRound::Same }
    }
    /// Review nodes (#1740): record the on-call manager's `accept` ruling on an
    /// exhausted review as a review-ledger fact for that assignment and exact
    /// commit, so the land node's admission re-check admits it. Idempotent.
    /// The default refuses: without a review ledger the ruling cannot admit
    /// a landing, so the node blocks instead of routing to a refused land.
    fn record_oncall_acceptance(
        &self,
        _acceptance: OncallAcceptance,
    ) -> impl Future<Output = Result<()>> + Send {
        async {
            Err(DaemonError::PolicyDenied(
                "the review ledger is not wired to topology review nodes".into(),
            ))
        }
    }
    /// Land nodes (#1641 S2): the merge-queue entry an earlier incarnation
    /// already created for this landing (found by its replay key), if any.
    /// A crash between the enqueue and its record must adopt it, never
    /// enqueue or abandon a second.
    fn find_land_entry(&self, _request: &LandRequest) -> impl Future<Output = Option<Uuid>> + Send {
        async { None }
    }
    /// Land nodes: enqueue the accepted commit on the merge queue on behalf
    /// of the execution's owner, re-checking the acceptance and the owner's
    /// authority first. Idempotent per `request.dedup_key`. The default
    /// refuses: a daemon without a merge queue fails the node instead of
    /// waiting forever. An `Err` is transient and asked again next tick.
    fn enqueue_land(
        &self,
        _request: LandRequest,
    ) -> impl Future<Output = Result<LandEnqueue>> + Send {
        async {
            Ok(LandEnqueue::Refused(
                "the merge queue is not wired to topology land nodes".into(),
            ))
        }
    }
    /// Land nodes: the queue entry's current state, re-derived from the
    /// queue on every call (never cached).
    fn land_status(&self, _entry_id: Uuid) -> impl Future<Output = LandStatus> + Send {
        async {
            LandStatus::Refused {
                state: "unavailable".into(),
                reason: "merge_queue_unavailable".into(),
            }
        }
    }
    /// #1641 S3a: one operator attention message (the daemon's system
    /// message bus). Called once per transition into a visible wait, never
    /// per tick. The default drops it.
    fn notify_operator(&self, _level: &str, _message: String) {}

    /// #1641 S3c / #1715: deliver a ruling's answer to the node's own
    /// (finished) session through the continue path, bound to the exact
    /// execution, attempt and decision. The continuation rechecks the binding
    /// under the session's spawn guard and durably claims the delivery before
    /// the provider effect, so cancellation, the deadline or a supersession
    /// stop it and a crash after the effect never repeats it. The default
    /// refuses, so a daemon without a provider path blocks the waiting node
    /// visibly instead of hanging.
    fn continue_with_answer(
        &self,
        _request: AnswerContinuation,
    ) -> impl Future<Output = Result<()>> + Send {
        async {
            Err(DaemonError::InvalidParam(
                "this daemon cannot continue a topology node session".into(),
            ))
        }
    }

    /// #1641 S3c: continue the node's own (finished) session with `prompt`,
    /// the normal continue path. The default refuses, so a daemon without a
    /// provider path blocks the waiting node visibly instead of hanging.
    fn continue_session(
        &self,
        _session_id: Uuid,
        _prompt: String,
    ) -> impl Future<Output = Result<()>> + Send {
        async {
            Err(DaemonError::InvalidParam(
                "this daemon cannot continue a topology node session".into(),
            ))
        }
    }

    /// #1728: continue a node session a daemon restart cut off. Production
    /// takes the same fenced path as every other automated continuation
    /// (published tip, spawn guard, effect claim), so an operator continuation
    /// of the same session that got there first refuses this one with the
    /// typed `continuation_target_busy` instead of racing it. The default is
    /// the plain continuation. `observed_restart_cut` carries the classification
    /// and cursor from the same snapshot: the
    /// continuation refuses `continuation_turn_changed` under the spawn guard
    /// if the session's conversation moved since (a competing continuation
    /// started and finished in between).
    fn resume_cut_session(
        &self,
        session_id: Uuid,
        prompt: String,
        _observed_restart_cut: Option<crate::store::manager_actions::fence::RestartCutFenceV1>,
    ) -> impl Future<Output = Result<()>> + Send {
        self.continue_session(session_id, prompt)
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
        // A review with no usable verdict earns exactly one new assignment;
        // the second one blocks (settled as `blocked`, never reaching here).
        Some(failure::REVIEW_UNSETTLED) => count(&[failure::REVIEW_UNSETTLED]) <= 1,
        // "One more round" from the on-call manager reopens the review once.
        Some(failure::REVIEW_REOPENED) => count(&[failure::REVIEW_REOPENED]) <= 1,
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
            EdgeWhen::VerdictAccepted | EdgeWhen::VerdictChangesRequested => {
                let verdict = if when == EdgeWhen::VerdictAccepted {
                    "accepted"
                } else {
                    "changes_requested"
                };
                index
                    .result(&edge.source, at)
                    .and_then(|attempt| attempt.output.as_ref())
                    .and_then(|output| output.pointer("/fields/verdict"))
                    .and_then(Value::as_str)
                    == Some(verdict)
            }
            EdgeWhen::Failure => false,
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
            GraphValue::List(items) if key == "findings" => {
                lines.push(crate::topology::review::render_findings(items));
            }
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
                    || matches!(
                        attempt.status,
                        AttemptStatus::Running | AttemptStatus::Waiting
                    )
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
        let in_flight_sessions = attempts
            .iter()
            .filter(|attempt| attempt.status.in_flight() && attempt.node_kind == "session")
            .count();
        if !self
            .reserve_ready(
                execution,
                &shape,
                &index,
                &progress,
                ready,
                in_flight_sessions,
            )
            .await?
        {
            // Every ready instance waits for a parallel slot (#633).
            return Ok(Some(waiting));
        }
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

    /// Returns whether any row was written; `false` only when every ready
    /// instance is deferred by the agent parallel-node cap.
    async fn reserve_ready(
        &self,
        execution: &ExecutionRow,
        shape: &GraphShape,
        index: &AttemptIndex<'_>,
        progress: &[RegionProgress],
        ready: Vec<(String, u32, bool)>,
        in_flight_sessions: usize,
    ) -> Result<bool> {
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
        if execution.agent_requested() {
            // Plan §5.3: at most `AGENT_MAX_PARALLEL_NODES` session nodes of
            // an agent-requested execution are in flight; the rest stay
            // ready and are reserved as slots free up.
            let mut free =
                crate::topology::agent::AGENT_MAX_PARALLEL_NODES.saturating_sub(in_flight_sessions);
            reservations.retain(|reservation| {
                if reservation.node_kind != "session" {
                    return true;
                }
                let keep = free > 0;
                free = free.saturating_sub(1);
                keep
            });
        }
        let wrote = !reservations.is_empty() || !settled.is_empty() || !refused.is_empty();
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
                    self.transition(
                        execution,
                        &[ExecutionStatus::Running],
                        ExecutionStatus::Failed,
                        Some(message),
                        None,
                        None,
                    )
                    .await?;
                    return Ok(true);
                }
                Err(error) => return Err(error),
            }
        }
        self.reserve(execution, &reservations).await?;
        Ok(wrote)
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
            let mut input = crate::topology::starters::source_input(execution.input.as_ref());
            input.remove("base_commit");
            // The on-call seat is the executor's, not the node's.
            input.remove(rsi_common::topology_agent::ON_CALL_INPUT_KEY);
            // #1641 S5a: the accepted Issue snapshot reads as the Issue.
            if let Some(issue) = input
                .get(crate::topology::starters::ISSUE_INPUT_KEY)
                .and_then(crate::topology::starters::render_issue)
            {
                input.insert(
                    crate::topology::starters::ISSUE_INPUT_KEY,
                    GraphValue::String(issue),
                );
            }
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
        // An accepted review ends its fix loop: nothing is left to change.
        let review_accepted = shape.region_nodes(region).iter().any(|node| {
            shape.steps().is_review(node)
                && index
                    .result(node, iteration)
                    .and_then(|attempt| attempt.output.as_ref())
                    .and_then(|output| output.pointer("/fields/verdict"))
                    .and_then(Value::as_str)
                    == Some("accepted")
        });
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
        let decision = if review_accepted {
            "halt:review_accepted".to_owned()
        } else {
            match shape.decide_region(region, iteration, lead_halted, predicate_met) {
                RegionDecision::Continue => "continue".to_owned(),
                RegionDecision::Halt(reason) => format!("halt:{reason}"),
            }
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
        if attempt.node_kind == "command" {
            // #1641 S4b: a settled command never keeps its governor slot.
            self.effects.release_build_slot(attempt.id);
        }
        self.publish([update]);
        Ok(())
    }

    /// Launch, adopt, or settle one in-flight attempt. Returns `true` when a
    /// durable row changed.
    async fn drive_attempt(&self, execution: &ExecutionRow, attempt: &AttemptRow) -> Result<bool> {
        if attempt.node_kind == "command" {
            return self.drive_command(execution, attempt).await;
        }
        if attempt.node_kind == "review" {
            return self.drive_review(execution, attempt).await;
        }
        if attempt.node_kind == "land" {
            return self.drive_land(execution, attempt).await;
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
            // A session that ended with a question waits for the on-call ruling
            // (#1641 S3c).
            AttemptStatus::Waiting => self.drive_session_decision(execution, attempt).await,
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
            // Retries and recovery of a launch already in progress continue
            // admitted work. Check spent invocation keys before consulting the
            // hold, so a lost launch can settle even while the host is busy.
            // #1641 S4b: the hold covers every execution's later nodes; only
            // the first launches of an operator-requested execution are
            // immediate (nothing has settled yet).
            if attempt.attempt_no == 1
                && attempt.status == AttemptStatus::Reserved
                && self.effects.launch_held(
                    execution.agent_requested()
                        || rows::load_attempts(&store, execution.id)?
                            .iter()
                            .any(|other| !other.status.in_flight()),
                    execution.created_at,
                    attempt,
                )
            {
                return Ok(false);
            }
            // #633 (plan §5.3): an agent-requested launch re-checks live
            // manager policy and charges `max_created_sessions`; a refusal
            // creates no session and blocks the execution.
            if execution.agent_requested()
                && let Some(code) = crate::topology::agent::launch_gate(&store, execution, attempt)?
            {
                drop(store);
                self.settle(
                    execution,
                    attempt,
                    AttemptStatus::Blocked,
                    Settlement {
                        failure_class: Some(failure::POLICY_REFUSED),
                        error: Some(code.to_owned()),
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
                } else if let Some(kind) = admission_hold_kind(&error) {
                    // #1641 S4b: capacity pressure (-32029) is a delay, never
                    // a failure: the sandbox gate refused before any session
                    // or invocation existed, so the attempt returns to
                    // `Reserved` and the next tick launches it again.
                    return self.hold_attempt(execution, attempt, kind).await;
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

    /// #1641 S4b: return a launching attempt to `Reserved` because resource
    /// pressure refused it, and record one `admission_hold{kind}` event per
    /// transition (a tick that finds it still held writes nothing). Returns
    /// whether a durable row changed.
    pub(crate) async fn hold_attempt(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        kind: &str,
    ) -> Result<bool> {
        let update = {
            let store = self.store.lock().await;
            rows::release_launching(&store, attempt.id)?;
            rows::note_admission_hold(&store, execution.id, attempt, kind)?
        };
        let changed = update.is_some();
        self.publish(update);
        Ok(changed)
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
            .clone()
            .or_else(|| attempt.sandbox_root.clone());
        // A session that waits on a human answer needs the on-call manager.
        let waiting = observed.status == SessionStatus::WaitingApproval;
        self.reconcile_on_call(execution, attempt, waiting).await?;
        let paused = self
            .account_node_wait(
                execution,
                attempt,
                waiting,
                observed.waiting_since,
                observed.waited_ms,
            )
            .await?;
        // The restart journal owns a session it cut off (deploy drain or
        // graceful restart) and continues it at startup: live, never settled.
        let journal_owned = observed.restart_intent_pending
            && matches!(
                observed.status,
                SessionStatus::Interrupted | SessionStatus::Failed
            );
        // A crash or an unjournaled restart cut: continue the same session
        // once for this boot instead of preserving or retrying it.
        if previous_boot
            && !journal_owned
            && observed.resumable
            && observed.cut_by_restart()
            && execution.status != ExecutionStatus::Cancelling
        {
            match self
                .resume_after_restart(execution, attempt, observed.restart_cut_fence)
                .await?
            {
                ResumeOutcome::Continued => return Ok(true),
                ResumeOutcome::Deferred => return Ok(false),
                ResumeOutcome::Refused => {}
            }
        }
        let (status, settlement) = match observed.status {
            // A cancelling execution does not wait on the journal: the
            // session is terminal, so there is nothing to interrupt and the
            // attempt settles `cancelled` (the journal row is left alone).
            status
                if !status.is_terminal()
                    || (journal_owned && execution.status != ExecutionStatus::Cancelling) =>
            {
                return self.observe_live(execution, attempt, paused).await;
            }
            SessionStatus::Completed => {
                let observed = self.observe_completed(execution, attempt, sandbox).await?;
                if observed.0 == AttemptStatus::Waiting {
                    // Parked on a question for the on-call manager; nothing
                    // to settle.
                    return Ok(true);
                }
                observed
            }
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

    /// #1641 S4a: continue the node's own session, cut off by a restart this
    /// attempt's previous boot never saw end, through the normal continue path
    /// with the daemon's restart prompt. At most once per attempt per boot.
    /// `Continued`: the attempt stays `running` (`node_resumed_after_restart`).
    /// `Refused`: already tried this boot, or the continuation was refused
    /// (`node_resume_refused`); the caller falls back to the preserve/retry
    /// of plan §3.4. `Deferred`: a competing continuation owns the session
    /// (#1728); the attempt is left as it is.
    async fn resume_after_restart(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        observed_restart_cut: Option<crate::store::manager_actions::fence::RestartCutFenceV1>,
    ) -> Result<ResumeOutcome> {
        if rows::node_resume_tried(&*self.store.lock().await, attempt.id, self.boot_id)? {
            return Ok(ResumeOutcome::Refused);
        }
        let outcome = self
            .effects
            .resume_cut_session(
                attempt.session_id,
                rsi_common::daemon_message::wrap("daemon-restart", RESTART_RESUME_PROMPT),
                observed_restart_cut,
            )
            .await;
        // Another continuation already owns the session (an operator got
        // there first), or has run since this observation (the turn cursor
        // moved): the observation is stale, so nothing is recorded, nothing
        // is preserved and the next observation follows the session.
        if outcome.as_ref().err().is_some_and(|error| {
            use crate::store::manager_actions::fence::{
                CONTINUATION_TARGET_BUSY, CONTINUATION_TURN_CHANGED, continuation_fence_code,
            };
            matches!(
                continuation_fence_code(error),
                Some(CONTINUATION_TARGET_BUSY | CONTINUATION_TURN_CHANGED)
            )
        }) {
            tracing::info!(
                execution_id = %execution.id,
                node = %attempt.node_id,
                session_id = %attempt.session_id,
                "restart resume of a topology node session deferred: another continuation owns it"
            );
            return Ok(ResumeOutcome::Deferred);
        }
        let reason = outcome.as_ref().err().map(ToString::to_string);
        let update = {
            let store = self.store.lock().await;
            rows::record_node_resume(
                &store,
                execution.id,
                &attempt.node_id,
                attempt.id,
                self.boot_id,
                match &reason {
                    None => Ok(()),
                    Some(reason) => Err(reason.as_str()),
                },
            )?
        };
        self.publish([update]);
        if let Some(reason) = reason {
            tracing::warn!(
                execution_id = %execution.id,
                node = %attempt.node_id,
                session_id = %attempt.session_id,
                %reason,
                "restart resume of a topology node session refused; preserving instead"
            );
        }
        Ok(if outcome.is_ok() {
            ResumeOutcome::Continued
        } else {
            ResumeOutcome::Refused
        })
    }

    /// #1641 S3a: a node that needs the on-call manager (its session waits on
    /// an answer) while no seat is live records one `on_call_unavailable`
    /// event, one operator attention message and a snapshot `waiting`; the
    /// matching `on_call_restored` clears it when a seat is live again or the
    /// node stops needing one. Resolved from rows every tick, once per
    /// transition. Nothing here changes the attempt.
    pub(crate) async fn reconcile_on_call(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        needs: bool,
    ) -> Result<()> {
        let needs = needs && execution.status != ExecutionStatus::Cancelling;
        let seat = oncall::seat_of(execution.input.as_ref());
        let (update, unavailable) = {
            let store = self.store.lock().await;
            let waiting = rows::on_call_waits(&store, execution.id)?
                .iter()
                .any(|wait| wait.node_id == attempt.node_id);
            if !needs && !waiting {
                return Ok(());
            }
            let resolved = oncall::resolve(&store, execution.project_id, &seat)?;
            match (needs, resolved) {
                (true, oncall::OnCall::Unavailable { reason }) => (
                    rows::record_on_call_unavailable(
                        &store,
                        execution.id,
                        &attempt.node_id,
                        attempt.id,
                        reason,
                        &seat,
                    )?,
                    Some(reason),
                ),
                (true, oncall::OnCall::Live { .. }) => (
                    rows::record_on_call_restored(
                        &store,
                        execution.id,
                        &attempt.node_id,
                        "on_call_live",
                    )?,
                    None,
                ),
                (false, _) => (
                    rows::record_on_call_restored(
                        &store,
                        execution.id,
                        &attempt.node_id,
                        "no_longer_needed",
                    )?,
                    None,
                ),
            }
        };
        let Some(update) = update else {
            return Ok(());
        };
        self.publish([update]);
        if let Some(reason) = unavailable {
            self.effects.notify_operator(
                "warn",
                format!(
                    "Topology execution {} ({}): node '{}' needs the on-call manager and none is \
                     live ({reason}). It waits until a seat is live; answer its question \
                     yourself or restore the manager.",
                    execution.id, execution.name, attempt.node_id
                ),
            );
        }
        Ok(())
    }

    /// #1641 S3b: open or close the attempt's wait on an answer and return the
    /// time already spent waiting. The node wall clock pauses for it: a node
    /// that waits for the on-call manager is not running, so the wait must not
    /// spend the node's wall time.
    pub(crate) async fn account_node_wait(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        waiting: bool,
        waiting_since: Option<chrono::DateTime<Utc>>,
        session_waited_ms: Option<u64>,
    ) -> Result<chrono::TimeDelta> {
        // The idle-session stall timer must not end a deliberate wait.
        oncall::note_answer_wait(attempt.session_id, waiting);
        let (updates, waited) = {
            let store = self.store.lock().await;
            // Waits the session finished between two observations left no
            // open wait; its own total still has them (#1704).
            let mut updates = vec![rows::record_node_waits_between_observations(
                &store,
                execution.id,
                &attempt.node_id,
                attempt.id,
                session_waited_ms,
            )?];
            updates.push(if waiting {
                // The wait began when the session started waiting, which may
                // be before this first observation of it (#1704).
                rows::record_node_wait_started(
                    &store,
                    execution.id,
                    &attempt.node_id,
                    attempt.id,
                    waiting_since,
                    attempt.started_at.unwrap_or(execution.created_at),
                )?
            } else {
                rows::record_node_wait_ended(
                    &store,
                    execution.id,
                    &attempt.node_id,
                    attempt.id,
                    session_waited_ms,
                )?
            });
            (updates, rows::node_waited(&store, attempt.id)?)
        };
        self.publish(updates.into_iter().flatten());
        Ok(waited.total(Utc::now()))
    }

    /// A live session: interrupt it while cancelling, or bound its wall time
    /// (less the time it spent waiting on an answer).
    async fn observe_live(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        paused: chrono::TimeDelta,
    ) -> Result<bool> {
        if execution.status == ExecutionStatus::Cancelling {
            self.effects.interrupt(attempt.session_id).await;
            return Ok(false);
        }
        if !wall_time_expired_after(execution, attempt, paused) {
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
            // A node that asks a question mid-work keeps its sandbox as it is:
            // the same session continues in it after the ruling (#1641 S3c).
            if self.parks_on_question(execution, attempt).await? {
                return Ok((AttemptStatus::Waiting, Settlement::default()));
            }
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
            if self
                .park_on_blocker_question(execution, attempt, &handoff, &content)
                .await?
            {
                return Ok((AttemptStatus::Waiting, Settlement::default()));
            }
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

    /// Whether a finished session with a dirty sandbox ended on a BLOCKED
    /// handoff that names a question, and was parked on a decision for it.
    async fn parks_on_question(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        let output = self.effects.output(attempt.session_id).await?;
        let Some(GraphValue::String(content)) = output.get("content") else {
            return Ok(false);
        };
        let content = content.clone();
        let Ok(handoff) = parse_pipeline_handoff_v2(&content, "") else {
            return Ok(false);
        };
        if handoff.strict_status == PipelineStatusV2::Complete {
            return Ok(false);
        }
        self.park_on_blocker_question(execution, attempt, &handoff, &content)
            .await
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

/// Whether the attempt outlived its node wall time or the execution deadline.
pub(crate) fn wall_time_expired(execution: &ExecutionRow, attempt: &AttemptRow) -> bool {
    wall_time_expired_after(execution, attempt, chrono::TimeDelta::zero())
}

/// `wall_time_expired` with the node's paused (waiting-on-an-answer) time added
/// to its own wall deadline; the execution deadline stays absolute.
pub(crate) fn wall_time_expired_after(
    execution: &ExecutionRow,
    attempt: &AttemptRow,
    paused: chrono::TimeDelta,
) -> bool {
    let started = attempt.started_at.unwrap_or(execution.created_at);
    let wall_deadline = started + SESSION_WALL_TIME + paused;
    let deadline = execution
        .deadline_at
        .map_or(wall_deadline, |deadline| deadline.min(wall_deadline));
    Utc::now() >= deadline
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
