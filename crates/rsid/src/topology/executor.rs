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
use rsi_common::agent_contract::{
    PipelineStatusV2, parse_pipeline_handoff_v2, validate_first_line,
};
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
use crate::topology::baton;
use crate::topology::catalog::{CommandPoll, CommandRunner};
use crate::topology::custody::{
    self, PinSelector, TopologyCustody, TopologyForkSource, node_pin_ref, preserved_ref,
};
use crate::topology::graph::{
    GraphShape, InstanceState, MAX_ATTEMPTS_PER_NODE, RegionDecision, RegionProgress,
};
use crate::topology::issue_close::{IssueClose, IssueCloseRequest, close_input_issue};
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
use crate::topology::verify;

/// Wall-time bound for one session node attempt (plan §2.5, fixes F-4).
pub(crate) const SESSION_WALL_TIME: chrono::TimeDelta = chrono::TimeDelta::hours(4);
/// #2697: default share of the node wall time, in percent, after which the
/// execution's owner gets one progress notice. `0` turns the notice off.
pub(crate) const WALL_TIME_NOTICE_PERCENT: u32 = 50;
/// Periodic reconciliation tick (plan §2.3).
pub(crate) const EXECUTOR_TICK: Duration = Duration::from_secs(30);
/// Bounded inner rounds per `advance` call; a step that keeps changing state
/// yields back to the driver after this many rounds.
const ADVANCE_ROUNDS: usize = 32;
/// Settled executions cleaned per settlement-cleanup pass.
pub(crate) const CLEANUP_BATCH: usize = 64;
/// #2476: how long after an owned job settles a completed node without a
/// handoff still counts as between turns while the job's wake resumes it.
const RESUME_IN_FLIGHT_GRACE_SECS: i64 = 180;

/// The prompt a restart-cut node session continues with: the same words the
/// restart journal's own continuation uses.
const RESTART_RESUME_PROMPT: &str = "The daemon restarted after interrupting your active turn. Continue the task from its durable state.";

/// A restart snapshot could not be read; no guarded continuation is safe.
pub(crate) const RESTART_OBSERVATION_UNAVAILABLE: &str = "topology_restart_observation_unavailable";

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
    /// one store snapshot. If unavailable, recovery records a refused resume
    /// and falls back to preserving/retrying the interrupted attempt.
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

impl SessionObservation {
    /// #2549: the daemon's restart lost this session and will not continue it:
    /// a restart cut nothing resumed (`cut_by_restart`), or the restart journal
    /// refused because the provider cannot resume or the dispatch failed. An
    /// operator hold (`operator_paused`, `pending_question`, `review_owned`) or
    /// another owner's claim is not a loss and stays out.
    pub(crate) fn lost_to_restart(&self) -> bool {
        self.cut_by_restart()
            || (self.status == SessionStatus::Interrupted
                && matches!(
                    self.stop_reason.as_deref().map(str::trim),
                    Some(
                        "daemon_restart_resume:not_resumable"
                            | "daemon_restart_resume:dispatch_failed"
                    )
                ))
    }
}

impl SessionObservation {
    /// #2569: the restart journal refused to resume this session because an
    /// operator hold or another owner has it (`operator_paused`,
    /// `pending_question`, `review_owned`). The hold, not the executor, decides
    /// when the session continues; a replacement must never bypass it.
    pub(crate) fn journal_hold(&self) -> bool {
        self.status == SessionStatus::Interrupted
            && matches!(
                self.stop_reason.as_deref().map(str::trim),
                Some(
                    "daemon_restart_resume:operator_paused"
                        | "daemon_restart_resume:pending_question"
                        | "daemon_restart_resume:review_owned"
                )
            )
    }
}

/// What became of an unverified node's chance to be continued (#2263).
enum UnverifiedOutcome {
    /// The session was continued; the attempt stays `running`.
    Continued,
    /// Another continuation owns the session; the next observation follows it.
    Deferred,
    /// Not continued: the node fails `verification_failed`.
    Fail,
    /// A claimed continuation cannot be shown to have landed; it is not resent
    /// and the node fails `verification_failed` naming the uncertainty.
    Uncertain(String),
}

/// Failure text prefix of a node whose verification continuation was claimed
/// but not shown to have reached the session.
pub(crate) const VERIFICATION_CONTINUATION_UNCERTAIN: &str =
    "topology_verification_continuation_uncertain";

/// One answer delivery to a parked node's session (#1715).
#[derive(Clone, Debug)]
pub(crate) struct AnswerContinuation {
    pub(crate) session_id: Uuid,
    pub(crate) prompt: String,
    pub(crate) binding: crate::topology::store::AnswerBinding,
}

/// The `admission_hold` kind of a launch refused because the provider's plan
/// window is exhausted (#3066).
pub(crate) const WAITING_ON_PROVIDER: &str = "waiting_on_provider";

/// The `admission_hold` kind of a launch refused by a restart or deploy drain
/// (#3426).
pub(crate) const RESTART_DRAIN: &str = "restart_drain";

/// #1641 S4b: the `admission_hold` kind of a launch refused by the sandbox
/// capacity gate (`launch.rs`, JSON-RPC code -32029), or `None` for any other
/// error. A direct-root limit is `source_root_limit`; a free-space shortfall is
/// `disk_floor`.
pub(crate) fn admission_hold_kind(error: &DaemonError) -> Option<&'static str> {
    // #3066: a provider plan window that ran out (the five-hour window) is an
    // outage of the account, not a defect of the node. The launch returns to
    // `Reserved` and is retried each tick until the window resets, so an
    // outage never settles a node `launch_refused`.
    if crate::provider_exhaustion::is_usage_window_refusal(error) {
        return Some(WAITING_ON_PROVIDER);
    }
    // #3426: a restart or deploy drain refusal is a hold, not a failure: the
    // launch ran nothing, so the attempt stays `Reserved` (spending no attempt)
    // and launches after the drain, on this or the next daemon incarnation.
    if crate::deploy_drain::is_held_for_restart(error) {
        return Some(RESTART_DRAIN);
    }
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
    /// Freeze the same effective identity the launch will use, including
    /// project defaults, so an outage notice names the refused provider.
    fn prepare_launch_identity(
        &self,
        _config: &mut crate::claude::LaunchConfig,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
    fn session(&self, session_id: Uuid) -> impl Future<Output = Option<SessionObservation>> + Send;
    fn output(&self, session_id: Uuid) -> impl Future<Output = Result<NodeData>> + Send;
    fn interrupt(&self, session_id: Uuid) -> impl Future<Output = ()> + Send;
    /// #3038: stop the node session's own open `test`/`build` jobs (a cancelled
    /// execution must not wait for a job its parked node was waiting on).
    fn cancel_open_jobs(&self, session_id: Uuid) -> impl Future<Output = Result<()>> + Send;
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
    /// Operator setting `structured_handoff_default` (#2759): every session
    /// node behaves as if marked `structured_handoff`. Off by default.
    fn structured_handoff_default(&self) -> bool {
        false
    }
    /// The provider the node's session runs on, for the handoff counters
    /// (#2759). `None` when the daemon does not know.
    fn session_provider(
        &self,
        _session_id: Uuid,
    ) -> impl Future<Output = Option<rsi_common::types::SessionProvider>> + Send {
        async { None }
    }
    /// Suspension point between a resolution's key pre-check and its
    /// recording transaction. Production: no-op; tests force a key race.
    fn before_resolution_record(&self, _execution_id: Uuid) -> impl Future<Output = ()> + Send {
        async {}
    }
    /// Host admission (#1417, #2562): `Some(reason)` (`host_load`, `thermal` or
    /// `disk_pressure`) while the daemon holds this attempt's launch because the
    /// host is too loaded, too hot or too full for a new worker. The attempt
    /// stays `Reserved`, one `admission_hold{kind: reason}` event is written per
    /// transition and the driver asks again on its next tick. `unattended` is
    /// false only for the first launches of an operator-requested execution (an
    /// operator start skips the load queue, but never host pressure); every
    /// later node of any execution is held like any other (#1641 S4b). `since`
    /// (the execution's age) orders held launches oldest-first. Production
    /// forwards to the host-admission gate; the default never holds.
    fn launch_held(
        &self,
        _unattended: bool,
        _since: DateTime<Utc>,
        _attempt: &AttemptRow,
    ) -> Option<&'static str> {
        None
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
    /// Review nodes (#2570): the merge-queue gate's receipt for a published
    /// land entry, shown to the reviewer of the landed source as its test
    /// evidence. The default has none.
    fn land_gate_receipt(
        &self,
        _entry_id: Uuid,
    ) -> impl Future<Output = Option<crate::topology::land::GateReceipt>> + Send {
        async { None }
    }
    /// Review nodes (#2423): the topology gave up on `assignment_id`
    /// (`review_unsettled`). Stop its reviewer if that session is still alive
    /// (a reviewer can keep running after an immutable `blocked` receipt) and
    /// return the session it interrupted. The default has no reviewer to stop.
    fn stop_reviewer(&self, _assignment_id: Uuid) -> impl Future<Output = Option<Uuid>> + Send {
        async { None }
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
    /// Land nodes (#2599): the published tip when `source` is an ancestor of a
    /// freshly fetched `origin/rolling` of `repo`; `None` when it is not or git
    /// cannot say. A refused entry whose source is already on rolling landed.
    fn landed_source_tip(
        &self,
        _repo: PathBuf,
        _source: String,
    ) -> impl Future<Output = Option<String>> + Send {
        async { None }
    }
    /// Land nodes (#3036): the live merge-queue entries of `project_id` ahead
    /// of refused entry `entry_id` (queued, admitted or gating, enqueued
    /// before it) whose own change conflicts with its source. A repair that
    /// merged rolling now would be refused again once such an entry publishes,
    /// so the land waits for each to settle first. Empty when none or when git
    /// cannot say: the repair then runs at once.
    fn land_conflict_blockers(
        &self,
        _repo: PathBuf,
        _project_id: Option<Uuid>,
        _entry_id: Uuid,
    ) -> impl Future<Output = Vec<Uuid>> + Send {
        async { Vec::new() }
    }
    /// #2732: whether `commit` is supplied work not yet on a freshly fetched
    /// `origin/rolling` (git positively says it is no ancestor). An execution
    /// started with such a `base_commit` verifies and lands it, so its
    /// implement node may hand off that commit unchanged. The default says no,
    /// which keeps the new-commit rule.
    fn commit_off_rolling(
        &self,
        _repo: PathBuf,
        _commit: String,
    ) -> impl Future<Output = bool> + Send {
        async { false }
    }
    /// #1641 S3a: one operator attention message (the daemon's system
    /// message bus). Called once per transition into a visible wait, never
    /// per tick. The default drops it.
    fn notify_operator(&self, _level: &str, _message: String) {}

    /// #2697: the percentage of the node wall time after which the owner gets
    /// its single progress notice (`0` is off).
    fn wall_time_notice_percent(&self) -> u32 {
        WALL_TIME_NOTICE_PERCENT
    }

    /// #2697: one notice for the execution's owner (`None`: an operator
    /// execution, so the operator's message bus). `from_session` is the node's
    /// own session, `key` is stable per attempt. Best effort; the default
    /// drops it.
    fn notify_owner(
        &self,
        _owner: Option<Uuid>,
        _from_session: Uuid,
        _key: String,
        _message: String,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

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

    /// #2263: continue a finished `require_verified` node's own session with
    /// its missing evidence. Production takes the fenced continue path for the
    /// exact session (published tip, spawn guard), so a continuation that got
    /// there first refuses this one with a typed fence code. The default is the
    /// plain continuation.
    fn continue_unverified(
        &self,
        session_id: Uuid,
        prompt: String,
    ) -> impl Future<Output = Result<()>> + Send {
        self.continue_session(session_id, prompt)
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
    pub(crate) store: Arc<rsid_core::store_lock::Mutex<Store>>,
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

/// #2651: what an accepted `already_satisfied` handoff recorded.
struct AlreadySatisfied {
    evidence_commit: String,
    /// What closing the input Issue did (it stays open when refused).
    issue_close: Option<IssueClose>,
}

/// Whether `source`'s result is an accepted `already_satisfied` handoff
/// (#2651): it has nothing to land, review or follow up, so its `success` and
/// `completed` edges are untaken and the downstream paths are dead.
fn is_already_satisfied(index: &AttemptIndex<'_>, source: &str, at: u32) -> bool {
    index
        .result(source, at)
        .and_then(|attempt| attempt.output.as_ref())
        .and_then(|output| output.pointer("/fields/already_satisfied"))
        .and_then(Value::as_bool)
        == Some(true)
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
            count(&failure::LOSSES) < MAX_ATTEMPTS_PER_NODE as usize
        }
        // #3340: a session that passed the context-cap baton continues from
        // its commit, on a budget of its own that the losses above neither
        // use nor are used up by.
        Some(failure::CONTEXT_CAP_CONTINUED) => {
            count(&[failure::CONTEXT_CAP_CONTINUED]) <= baton::MAX_CONTEXT_CAP_CONTINUATIONS
        }
        // A reviewer that could not run a check it needed reported
        // infrastructure, not a verdict (#2570): a fresh review, bounded apart
        // from the node's own budget (`review::REVIEW_INFRASTRUCTURE_RETRIES`).
        Some(failure::REVIEW_INFRASTRUCTURE) => {
            count(&[failure::REVIEW_INFRASTRUCTURE])
                <= crate::topology::review::REVIEW_INFRASTRUCTURE_RETRIES
        }
        // A review with no usable verdict earns exactly one new assignment;
        // the second one blocks (settled as `blocked`, never reaching here).
        Some(failure::REVIEW_UNSETTLED | failure::REVIEW_LAUNCH_REFUSED) => {
            count(&[failure::REVIEW_UNSETTLED, failure::REVIEW_LAUNCH_REFUSED]) <= 1
        }
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

/// The query of the attempt that retries `latest`. A session that passed the
/// context-cap baton (#3340) continues with the baton appended to the node's
/// own query; every other retry repeats the query as it was.
fn retry_query(index: &AttemptIndex<'_>, latest: &AttemptRow) -> String {
    if latest.failure_class.as_deref() != Some(failure::CONTEXT_CAP_CONTINUED) {
        return latest.query().to_owned();
    }
    let n = index
        .instance(&latest.node_id, latest.iteration)
        .iter()
        .filter(|attempt| attempt.failure_class.as_deref() == Some(failure::CONTEXT_CAP_CONTINUED))
        .count();
    let text = latest
        .output
        .as_ref()
        .and_then(|output| output.pointer(&format!("/fields/{BATON_OUTPUT_KEY}")))
        .and_then(Value::as_str)
        .unwrap_or_default();
    baton::continuation_query(
        latest.query(),
        text,
        n,
        baton::MAX_CONTEXT_CAP_CONTINUATIONS,
    )
}

/// The output key a `context_cap_continued` attempt records its baton under.
const BATON_OUTPUT_KEY: &str = "context_cap_baton";

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
        // #2688: a land refused `land_requires_review` is routed by its
        // `review_required` edge (and by no other failure edge).
        AttemptStatus::Failed
            if latest.failure_class.as_deref() == Some(failure::LAND_REQUIRES_REVIEW)
                && shape.steps().routes_review_required(node) =>
        {
            InstanceState::Routed
        }
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
            EdgeWhen::Success | EdgeWhen::Completed => {
                !is_already_satisfied(index, &edge.source, at)
            }
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
            EdgeWhen::Failure | EdgeWhen::ReviewRequired => false,
        },
        // Legacy `FailurePolicy::Skip`: downstream still runs.
        InstanceState::Skipped => when == EdgeWhen::Success,
        InstanceState::Routed => {
            // #2688: a land refused `land_requires_review` takes only its
            // `review_required` edges (and `completed`), never the repair
            // route of an ordinary failure; every other routed failure takes
            // no `review_required` edge.
            let needs_review = index.latest(&edge.source, at).is_some_and(|latest| {
                latest.failure_class.as_deref() == Some(failure::LAND_REQUIRES_REVIEW)
            });
            match when {
                EdgeWhen::ReviewRequired => needs_review,
                EdgeWhen::Failure => !needs_review,
                EdgeWhen::Completed => true,
                _ => false,
            }
        }
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
        store: Arc<rsid_core::store_lock::Mutex<Store>>,
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
            // A node's pending ruling holds new effects in this execution,
            // while already running nodes and the answer wait still reconcile.
            if attempt.status == AttemptStatus::Reserved {
                let store = self.store.lock().await;
                if rows::load_attempts(&store, execution.id)?
                    .iter()
                    .any(|other| crate::topology::decision::marker_key(other).is_some())
                {
                    continue;
                }
            }
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
        if attempts
            .iter()
            .any(|attempt| crate::topology::decision::marker_key(attempt).is_some())
        {
            return Ok(Some(Step::Wait));
        }
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
                    // #2549: a restart-preserved attempt retries from its
                    // preserved commit; the work is never dropped.
                    base_commit: attempt
                        .preserved_commit
                        .clone()
                        .filter(|_| attempt.status == AttemptStatus::Interrupted)
                        .unwrap_or_else(|| attempt.base_commit.clone()),
                    query: retry_query(&index, attempt),
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
            let mut error = format!(
                "node '{}' failed: {}",
                failed.node_id,
                failed
                    .error
                    .as_deref()
                    .or(failed.failure_class.as_deref())
                    .unwrap_or("failed")
            );
            // #2584: a failure after the source landed names the landed SHA and
            // the review verdicts, so it is never read as a bare `failed`.
            let context = {
                let store = self.store.lock().await;
                crate::topology::outcome::failure_context(&store, execution.id)
            };
            if let Some(context) = context {
                error.push_str(&format!("; {context}"));
            }
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
            let update = rows::transition_execution(
                &store,
                execution.id,
                from,
                to,
                error,
                output,
                blocked_reason,
            )?;
            // #2427: a run that ends without landing frees its Issue.
            crate::topology::issue_hold::release(&store, execution, to);
            update
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
        let execution_input = crate::topology::starters::source_input(execution.input.as_ref());
        let issue = execution_input
            .get(crate::topology::starters::ISSUE_INPUT_KEY)
            .and_then(crate::topology::starters::render_issue);
        let mut input = if shape.is_source(node_id) {
            let mut input = execution_input;
            input.remove("base_commit");
            // The on-call seat is the executor's, not the node's.
            input.remove(rsi_common::topology_agent::ON_CALL_INPUT_KEY);
            // The review policy is the daemon's landing check, not prompt text.
            input.remove(crate::topology::review_policy::REVIEW_POLICY_INPUT_KEY);
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
        // Every session receives the accepted Issue snapshot, independent of
        // upstream edges and pass_content. Replacing the root's raw value
        // keeps exactly one Issue section; non-Issue inputs retain their meaning.
        if (shape.is_source(node_id) || node_kind == "session")
            && let Some(issue) = issue
        {
            input.insert(
                crate::topology::starters::ISSUE_INPUT_KEY,
                GraphValue::String(issue),
            );
        }
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
            let held = if attempt.attempt_no == 1 && attempt.status == AttemptStatus::Reserved {
                self.effects.launch_held(
                    execution.agent_requested()
                        || rows::load_attempts(&store, execution.id)?
                            .iter()
                            .any(|other| !other.status.in_flight()),
                    execution.created_at,
                    attempt,
                )
            } else {
                None
            };
            if let Some(kind) = held {
                drop(store);
                // #2562: the hold is visible on the attempt, one event per
                // transition, and the node stays `Reserved` for the next tick.
                return self.hold_attempt(execution, attempt, kind).await;
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
                        error: Some(code.error_text()),
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
        let mut request = match Self::launch_request(execution, attempt) {
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
        let prepared = self
            .effects
            .prepare_launch_identity(&mut request.config)
            .await;
        let provider = request.config.provider.unwrap_or_default();
        let project = request.config.project_id;
        let launched = match prepared {
            Ok(()) => self.effects.launch(request).await,
            Err(error) => Err(error),
        };
        match launched {
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
                    if kind == WAITING_ON_PROVIDER
                        && let Some(project) = project
                    {
                        let store = self.store.lock().await;
                        let notice = (|| -> Result<()> {
                            if let Some(reset) =
                                store.provider_usage_window_hold_until(provider, Utc::now())?
                            {
                                store.record_provider_window_held_work(
                                    project,
                                    provider,
                                    reset,
                                    "topology_launch",
                                    attempt.id,
                                    &format!("{} / {}", execution.name, attempt.node_id),
                                    attempt.session_id,
                                    reset,
                                )?;
                            }
                            Ok(())
                        })();
                        if let Err(error) = notice {
                            tracing::error!(%error, attempt = %attempt.id, "could not record provider window launch notice");
                        }
                    }
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
        let mut config = builder.build_attempt(
            node,
            attempt.query().to_owned(),
            attempt.iteration,
            attempt.attempt_no,
            attempt.dedup_key.clone(),
        )?;
        config.title = Some(super::launch::node_session_title(
            &attempt.node_id,
            execution.input.as_ref(),
        ));
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
        let admission_since = if observed.status.is_terminal() {
            self.build_admission_since(attempt.session_id).await?
        } else {
            None
        };
        // Launched (or last seen alive) by an earlier daemon incarnation.
        let previous_boot = attempt.boot_id != Some(self.boot_id);
        let (downtime, clock_update) = {
            let store = self.store.lock().await;
            let (downtime, update) = rows::checkpoint_node_clock(
                &store,
                execution.id,
                attempt,
                self.boot_id,
                observed
                    .waiting_since
                    .filter(|_| observed.status == SessionStatus::WaitingApproval)
                    .into_iter()
                    .chain(admission_since)
                    .min(),
            )?;
            if let Some(root) = &observed.sandbox_root
                && attempt.sandbox_root.is_none()
            {
                rows::record_sandbox_root(&store, attempt.id, root)?;
            }
            if previous_boot && !observed.status.is_terminal() {
                rows::stamp_boot(&store, attempt.id, self.boot_id)?;
            }
            (downtime, update)
        };
        self.publish(clock_update);
        let sandbox = observed
            .sandbox_root
            .clone()
            .or_else(|| attempt.sandbox_root.clone());
        // A session that waits on a human answer needs the on-call manager.
        let waiting = observed.status == SessionStatus::WaitingApproval;
        self.reconcile_on_call(execution, attempt, waiting).await?;
        let (admission_wait, update) = {
            let store = self.store.lock().await;
            rows::account_build_admission_wait(&store, execution, attempt, admission_since)?
        };
        self.publish(update);
        let paused = self
            .account_node_wait(
                execution,
                attempt,
                waiting,
                observed.waiting_since,
                observed.waited_ms,
            )
            .await?
            + downtime
            + admission_wait;
        // The restart journal owns a session it cut off (deploy drain or
        // graceful restart) and continues it at startup: live, never settled.
        let lost_to_restart = observed.lost_to_restart();
        let journal_hold = observed.journal_hold();
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
        // #2549: a restart-lost node with diverged work earns one automatic
        // retry from its preservation point, charged as infrastructure, before
        // the work blocks the execution for a manager.
        let restart_retry = previous_boot
            && lost_to_restart
            && !self.restart_retry_spent(execution, attempt).await?;
        // #3038: a cancelling execution does not wait for a node session that
        // ended its turn to wait on its own wake or job (a `Completed` session
        // between turns, or one whose process died while parked). Interrupting
        // reaches no turn, so its job and wakes are withdrawn here and the
        // attempt settles `cancelled` within this pass.
        let cancel_parked = execution.status == ExecutionStatus::Cancelling
            && match observed.status {
                SessionStatus::Completed => {
                    self.between_turns_without_handoff(attempt.session_id)
                        .await?
                }
                SessionStatus::Interrupted | SessionStatus::Failed => {
                    self.parked_awaiting_resume(attempt.session_id).await?
                }
                _ => false,
            };
        if cancel_parked && !self.withdraw_parked_session(attempt.session_id).await {
            // A job's unit would not stop: the attempt stays running and the
            // next pass retries.
            return Ok(false);
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
            // #2625: a session the daemon will resume (an armed own wake or a
            // live owned job) is running, not terminal, whatever its sandbox
            // holds: a process death between turns leaves it `Interrupted`
            // with a dirty tree, and the wake continues it. Dirty state is
            // judged only once nothing will resume the session.
            SessionStatus::Interrupted | SessionStatus::Failed
                if execution.status != ExecutionStatus::Cancelling
                    && self.parked_awaiting_resume(attempt.session_id).await? =>
            {
                return self.observe_live(execution, attempt, paused).await;
            }
            SessionStatus::Completed if cancel_parked => (
                AttemptStatus::Cancelled,
                Settlement {
                    failure_class: Some(failure::CANCELLED),
                    ..Settlement::default()
                },
            ),
            SessionStatus::Completed
                if self
                    .between_turns_without_handoff(attempt.session_id)
                    .await? =>
            {
                // #1783: the node ended a turn to wait on its own job or wake
                // (the worker contract runs scoped tests as a job). It has not
                // handed off yet; keep it running under its wall-time limit.
                return self.observe_live(execution, attempt, paused).await;
            }
            SessionStatus::Completed => {
                let observed = self.observe_completed(execution, attempt, sandbox).await?;
                if matches!(observed.0, AttemptStatus::Waiting | AttemptStatus::Running) {
                    // Parked on a question for the on-call manager, or
                    // continued with its missing verification evidence (#2263);
                    // nothing to settle.
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
            // #2569: the restart journal's explicit hold over a session whose
            // sandbox still holds nothing of its own: the attempt stays held
            // (running, bounded by its wall time and the execution deadline)
            // with no new provider work, and follows the session when the hold
            // lifts. Diverged work falls through and is preserved as before.
            SessionStatus::Interrupted
                if journal_hold && Self::sandbox_untouched(attempt, &sandbox) =>
            {
                return self.observe_live(execution, attempt, paused).await;
            }
            // Startup restore marks every provider that was live when the
            // daemon died `Failed`; for an attempt this incarnation never saw
            // alive that is the restart interrupting it (plan §2.4 → §3.4).
            SessionStatus::Failed => Self::interrupted_outcome(
                execution,
                attempt,
                sandbox,
                SessionStatus::Interrupted,
                restart_retry,
            ),
            // Interrupted, Archived or Deleted before completing.
            ended => Self::interrupted_outcome(execution, attempt, sandbox, ended, restart_retry),
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
        let outcome = match observed_restart_cut {
            Some(fence) => {
                self.effects
                    .resume_cut_session(
                        attempt.session_id,
                        rsi_common::daemon_message::wrap("daemon-restart", RESTART_RESUME_PROMPT),
                        Some(fence),
                    )
                    .await
            }
            // A read failure is not evidence of a competing turn. Bound it
            // with the existing once-per-boot resume record and fallback.
            None => Err(DaemonError::InvalidParam(
                RESTART_OBSERVATION_UNAVAILABLE.into(),
            )),
        };
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
    /// less answer waits, build admission and daemon downtime.
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
            self.notice_wall_time_progress(execution, attempt, paused)
                .await?;
            return Ok(false);
        }
        if self.parked_awaiting_resume(attempt.session_id).await? {
            if !self.withdraw_parked_session(attempt.session_id).await {
                return Ok(false);
            }
        } else {
            self.effects.interrupt(attempt.session_id).await;
        }
        // A timed-out worker may have already committed its result (or still
        // hold uncommitted work). Preserve that work just as for an interrupted
        // session, so the owner can resolve the attempt rather than hand-drive it.
        if let Some(sandbox) = &attempt.sandbox_root {
            let (status, mut settlement) = Self::preserve(execution, attempt, sandbox);
            if status != AttemptStatus::Interrupted {
                settlement.error = Some(format!(
                    "node session exceeded its wall-time limit; {}",
                    settlement.error.as_deref().unwrap_or("work preserved")
                ));
                self.settle(execution, attempt, status, settlement).await?;
                return Ok(true);
            }
        }
        let settlement = failed(
            failure::TIMEOUT,
            "node session exceeded its wall-time limit",
        );
        self.settle(execution, attempt, AttemptStatus::Failed, settlement)
            .await?;
        Ok(true)
    }

    /// #2697: once an attempt has used the configured share of its node wall
    /// time (waiting on an answer excluded), tell the execution's owner what
    /// it has to show for it, while a re-scope can still save the remaining
    /// time. One durable `node_wall_time_notice` event per attempt keeps it to
    /// a single notice across ticks and daemon restarts.
    async fn notice_wall_time_progress(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        paused: chrono::TimeDelta,
    ) -> Result<()> {
        let percent = self.effects.wall_time_notice_percent();
        if percent == 0 || execution.status == ExecutionStatus::Cancelling {
            return Ok(());
        }
        let started = attempt.started_at.unwrap_or(execution.created_at);
        let now = Utc::now();
        let elapsed = now - started - paused;
        if elapsed < SESSION_WALL_TIME * i32::try_from(percent.min(100)).unwrap_or(100) / 100 {
            return Ok(());
        }
        let progress = attempt
            .sandbox_root
            .as_deref()
            .filter(|sandbox| sandbox.exists())
            .and_then(|sandbox| custody::sandbox_progress(sandbox, &attempt.base_commit).ok());
        let owner = execution
            .requested_by_session_id
            .or(execution.parent_session_id);
        let noted = {
            let store = self.store.lock().await;
            rows::note_land_issue(
                &store,
                execution.id,
                attempt,
                owner,
                "node_wall_time_notice",
                serde_json::json!({
                    "elapsed_secs": elapsed.num_seconds(),
                    "commits": progress.map(|progress| progress.commits),
                    "last_commit_at": progress.and_then(|progress| progress.last_commit_at),
                    "iteration": attempt.iteration,
                    "attempt_no": attempt.attempt_no,
                }),
            )?
        };
        let Some(update) = noted else {
            return Ok(());
        };
        self.publish([update]);
        let message = wall_time_notice(execution, attempt, elapsed, progress, now);
        self.effects
            .notify_owner(
                owner,
                attempt.session_id,
                format!("topology-wall-time:{}", attempt.id),
                message,
            )
            .await;
        Ok(())
    }

    /// #2625: the daemon will resume this session: it owns an armed result wake,
    /// a live job with the default `owner` wake, or a job that settled since its
    /// last assistant turn within `RESUME_IN_FLIGHT_GRACE_SECS` (the wake fired
    /// and the resume turn has not started). The one predicate for "parked",
    /// shared by a `Completed` session between turns (#1783) and an
    /// `Interrupted`/`Failed` one whose process died while parked.
    async fn parked_awaiting_resume(&self, session_id: Uuid) -> Result<bool> {
        let store = self.store.lock().await;
        Ok(store.owner_armed_result_wake(session_id)?.is_some()
            || store.session_has_live_owner_wake_job(session_id)?
            || store.session_job_settled_since_last_turn(
                session_id,
                Utc::now(),
                chrono::Duration::seconds(RESUME_IN_FLIGHT_GRACE_SECS),
            )?)
    }

    /// A jobs-terminal wake pauses only for a job this node owns and only
    /// during admission. A running build/test continues to spend node time.
    async fn build_admission_since(
        &self,
        session_id: Uuid,
    ) -> Result<Option<chrono::DateTime<Utc>>> {
        let store = self.store.lock().await;
        let mut candidates = Vec::new();
        for (wake, _) in store.list_owned_scheduled_jobs(session_id, false, 100)? {
            let Some(state) = store.wake_when_state(wake.id)? else {
                continue;
            };
            for id in state.predicate.jobs_terminal.iter().flatten() {
                let Some(row) = store.get_agent_job(*id)? else {
                    continue;
                };
                if row.job.owner_session_id != session_id {
                    continue;
                }
                candidates.push((row.job, state.armed_at));
            }
        }
        drop(store);
        Ok(candidates
            .into_iter()
            .filter_map(|(job, armed_at)| {
                build_admission_started(&job).map(|wait| wait.max(armed_at))
            })
            .min())
    }

    /// Take a cancelled or timed-out node's parked session out of the daemon's
    /// resume set: stop its open jobs, then disable every wake it owns. A
    /// session whose result wake already fired and whose resume turn is about
    /// to start cannot be recalled, so it is also interrupted. `false` when a
    /// job could not be stopped (retried on the next pass).
    async fn withdraw_parked_session(&self, session_id: Uuid) -> bool {
        if let Err(error) = self.effects.cancel_open_jobs(session_id).await {
            tracing::warn!(%session_id, %error, "stopping topology node: parked node job not stopped");
            return false;
        }
        let disabled = {
            let store = self.store.lock().await;
            store.disable_session_wakes(session_id)
        };
        match disabled {
            Ok(ids) if !ids.is_empty() => {
                tracing::info!(%session_id, wakes = ids.len(), "stopping topology node: parked node wakes withdrawn");
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%session_id, %error, "stopping topology node: parked node wakes not withdrawn");
                return false;
            }
        }
        self.effects.interrupt(session_id).await;
        true
    }

    /// #1783: a completed node session whose last message carries no pipeline
    /// handoff while the daemon will resume it is between turns, not finished.
    /// "Will resume it" is the daemon's parked predicate: an armed result wake
    /// (`resume`, `when`, `on_terminal`; `owner_armed_result_wake`, as
    /// `AgentWorkerRoster` #2311) or a live job with the default `owner` wake
    /// (#2262: its resume wake is armed only when the job settles). A handoff is
    /// a marker line the strict parser accepts (`validate_first_line`, the
    /// check `parse_pipeline_handoff_v2` opens with), not a mention of the
    /// words in prose (#2363) and not an indented or backtick-wrapped example
    /// marker (#2370); a real handoff always settles, whatever wakes remain.
    /// #2476: the rule is the same for every node kind (implement, repair,
    /// review ...). The window in which the job has settled and its wake has
    /// fired but the resume turn has not started has neither a wake row nor a
    /// live job; a job settled since the last assistant message, within
    /// `RESUME_IN_FLIGHT_GRACE_SECS`, still counts as parked.
    async fn between_turns_without_handoff(&self, session_id: Uuid) -> Result<bool> {
        if !self.parked_awaiting_resume(session_id).await? {
            return Ok(false);
        }
        let output = self.effects.output(session_id).await?;
        let content = match output.get("content") {
            Some(GraphValue::String(content)) => content.as_str(),
            _ => "",
        };
        // A baton (#3340) is the session's final word, not a turn boundary.
        Ok(validate_first_line(content).is_err() && baton::parse(content).is_none())
    }

    /// #3340: the session ended its turn with the context-cap baton instead
    /// of a stage handoff. The baton is the node's outcome: the session's
    /// commit (and any uncommitted paths, as a snapshot) is preserved, the
    /// attempt settles `interrupted` as `context_cap_continued`, and
    /// `schedule` relaunches the node from that commit with the baton as
    /// context. A node instance that already continued
    /// [`baton::MAX_CONTEXT_CAP_CONTINUATIONS`] times blocks for a manager
    /// instead, its work preserved.
    async fn settle_context_cap_baton(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        sandbox: &std::path::Path,
        baton: &baton::ContextCapBaton,
    ) -> Result<(AttemptStatus, Settlement)> {
        let prior = {
            let store = self.store.lock().await;
            rows::load_attempts(&store, execution.id)?
                .iter()
                .filter(|row| {
                    row.node_id == attempt.node_id
                        && row.iteration == attempt.iteration
                        && row.id != attempt.id
                        && row.failure_class.as_deref() == Some(failure::CONTEXT_CAP_CONTINUED)
                })
                .count()
        };
        let (preserved_status, settlement) = Self::preserve(execution, attempt, sandbox);
        if preserved_status == AttemptStatus::Failed {
            return Ok((preserved_status, settlement));
        }
        let spent = prior >= baton::MAX_CONTEXT_CAP_CONTINUATIONS;
        if spent && preserved_status == AttemptStatus::Blocked {
            return Ok((
                AttemptStatus::Blocked,
                Settlement {
                    error: Some(format!(
                        "node session passed the context-cap baton after {prior} continuation(s); \
                         work preserved, resolve with ResolveTopologyAttempt"
                    )),
                    ..settlement
                },
            ));
        }
        if spent {
            return Ok((
                AttemptStatus::Failed,
                failed(
                    failure::CONTEXT_CAP_CONTINUED,
                    "node session passed the context-cap baton again with no work to continue \
                     from and its continuations are spent",
                ),
            ));
        }
        let mut output = NodeData::new();
        output.insert(BATON_OUTPUT_KEY, GraphValue::String(baton.text.clone()));
        if let Some(sha) = &baton.sha {
            output.insert("context_cap_baton_sha", GraphValue::String(sha.clone()));
        }
        Ok((
            AttemptStatus::Interrupted,
            Settlement {
                failure_class: Some(failure::CONTEXT_CAP_CONTINUED),
                error: Some(format!(
                    "node session filled its context and passed the baton; the node continues \
                     from {} (continuation {} of {})",
                    settlement
                        .preserved_commit
                        .as_deref()
                        .unwrap_or(&attempt.base_commit),
                    prior + 1,
                    baton::MAX_CONTEXT_CAP_CONTINUATIONS
                )),
                output: Some(serde_json::to_value(&output)?),
                ..settlement
            },
        ))
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
        // #3340: a context-cap baton is a node outcome of its own, judged
        // before the sandbox state (a baton with uncommitted paths is
        // snapshotted and continued, not blocked).
        let last_message = self.effects.output(attempt.session_id).await?;
        if let Some(GraphValue::String(content)) = last_message.get("content")
            && let Some(passed) = baton::parse(content)
        {
            return self
                .settle_context_cap_baton(execution, attempt, &sandbox, &passed)
                .await;
        }
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
            // #2563: a finished commit-expecting node that handed off complete
            // and left a few paths uncommitted is asked once, as the same
            // session, to commit or discard them and repeat its handoff.
            if self
                .finished_complete_with_commit(execution, attempt)
                .await?
                && let UnverifiedOutcome::Continued | UnverifiedOutcome::Deferred = self
                    .continue_dirty_node(
                        execution,
                        attempt,
                        custody::inspect_sandbox(&sandbox, &attempt.base_commit)
                            .map_or(0, |report| report.dirty_paths.len()),
                    )
                    .await?
            {
                return Ok((AttemptStatus::Running, Settlement::default()));
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
        // #2759: a schema-validated last message (Codex's JSON text) is read as
        // the canonical handoff block everywhere below; Claude's arrives
        // already rendered. Free text is unchanged.
        let content =
            rsi_common::structured_handoff::normalize_structured_handoff(&content).into_owned();
        // Whether the turn that wrote this message carried the handoff schema.
        let structured_turn = crate::structured_handoff::finish_turn(attempt.session_id);
        let provider = self.effects.session_provider(attempt.session_id).await;
        crate::structured_handoff::record_observed(provider, structured_turn);
        let handoff = match parse_pipeline_handoff_v2(&content, "") {
            Ok(handoff) => handoff,
            Err(error) => {
                crate::structured_handoff::record_invalid(provider, structured_turn);
                // #2584: the session is asked once, as itself, to repeat the
                // handoff with the parser's error and the required first line;
                // only a second malformed handoff fails the node.
                if let UnverifiedOutcome::Continued | UnverifiedOutcome::Deferred = self
                    .continue_handoff_format(execution, attempt, &error.to_string())
                    .await?
                {
                    return Ok((AttemptStatus::Running, Settlement::default()));
                }
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
        let satisfied_claim = handoff.strict_status == PipelineStatusV2::AlreadySatisfied;
        if !satisfied_claim && handoff.strict_status != PipelineStatusV2::Complete {
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
        // #2568: a retry forked from a preserved commit already holds the
        // earlier attempt's work; its result is measured from the base that work
        // started at, so a complete preserved result may stand unchanged.
        let work_base = self
            .preserved_origin_base(execution, attempt)
            .await?
            .unwrap_or_else(|| attempt.base_commit.clone());
        // #2651: a worker that proved the Issue's acceptance already holds on
        // the base hands off `already_satisfied` instead of a commit.
        //
        // #3045: the Issue closes without landing, so the evidence must already
        // be on a freshly fetched `origin/rolling`. Evidence reachable only from
        // a supplied `base_commit` that is not on rolling is unlanded work: it
        // is handled as the #2732 `complete` handoff at that base (land runs)
        // when that route applies, otherwise refused.
        let evidence_unlanded = if satisfied_claim && let Some(evidence) = &handoff.evidence_commit
        {
            self.effects
                .commit_off_rolling(execution.repo_root.clone(), evidence.clone())
                .await
        } else {
            false
        };
        if evidence_unlanded
            && !(steps.expects_commit(&attempt.node_id)
                && observed.head == work_base
                && work_base == execution.base_commit)
        {
            return Ok((
                AttemptStatus::Failed,
                Settlement {
                    failure_class: Some(failure::HANDOFF_INVALID),
                    error: Some(
                        "already_satisfied evidence_commit is not on origin/rolling; \
                         unlanded work is not already satisfied"
                            .into(),
                    ),
                    ..Settlement::default()
                },
            ));
        }
        let satisfied = if satisfied_claim && !evidence_unlanded {
            match self
                .accept_already_satisfied(
                    execution,
                    attempt,
                    &steps,
                    &handoff,
                    &content,
                    &observed.head,
                    &work_base,
                    &sandbox,
                )
                .await?
            {
                Ok(accepted) => Some(accepted),
                Err(error) => {
                    return Ok((
                        AttemptStatus::Failed,
                        Settlement {
                            failure_class: Some(failure::HANDOFF_INVALID),
                            error: Some(error),
                            ..Settlement::default()
                        },
                    ));
                }
            }
        } else {
            None
        };
        // #2732: an execution started on a supplied `base_commit` that is not
        // on `origin/rolling` exists to verify and land that commit, so its
        // first implement node may hand it off unchanged (its own scoped_test
        // receipt at that head still gates a `require_verified` node).
        let supplied_unlanded = satisfied.is_none()
            && steps.expects_commit(&attempt.node_id)
            && observed.head == work_base
            && work_base == execution.base_commit
            && self
                .effects
                .commit_off_rolling(execution.repo_root.clone(), work_base.clone())
                .await;
        if satisfied.is_none()
            && !supplied_unlanded
            && steps.expects_commit(&attempt.node_id)
            && observed.head == work_base
        {
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
        if satisfied.is_none()
            && let Err(error) = custody::pin_commit(&sandbox, &pin, &observed.head)
        {
            return Ok((
                AttemptStatus::Failed,
                Settlement {
                    failure_class: Some(failure::CUSTODY_REFUSED),
                    error: Some(error.to_string()),
                    ..Settlement::default()
                },
            ));
        }
        // #2249: a verified node settles only on its own scoped_test receipt
        // for exactly this commit. The pin above stays; the failed attempt
        // carries what the daemon read, so a `failure` edge can route it.
        let verification = if satisfied.is_none() && steps.require_verified(&attempt.node_id) {
            match self
                .verify_result(execution, attempt, &observed.head)
                .await?
            {
                Ok(evidence) => Some(evidence),
                Err(unverified) => {
                    // #2263: first the same session gets its missing evidence.
                    let uncertain = match self
                        .continue_unverified_node(execution, attempt, &unverified)
                        .await?
                    {
                        UnverifiedOutcome::Continued | UnverifiedOutcome::Deferred => {
                            return Ok((AttemptStatus::Running, Settlement::default()));
                        }
                        UnverifiedOutcome::Fail => None,
                        UnverifiedOutcome::Uncertain(detail) => Some(detail),
                    };
                    return Self::verification_failure(&unverified, &observed.head, uncertain);
                }
            }
        } else {
            None
        };
        let mut typed_handoff = std::collections::BTreeMap::new();
        typed_handoff.insert(
            "stage".into(),
            GraphValue::String(handoff.handoff.stage.as_str().to_ascii_lowercase()),
        );
        typed_handoff.insert(
            "status".into(),
            GraphValue::String(
                if satisfied.is_some() {
                    "already_satisfied"
                } else {
                    "complete"
                }
                .into(),
            ),
        );
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
        output.insert("base_commit", GraphValue::String(work_base.clone()));
        if let Some(accepted) = &satisfied {
            // No result commit and nothing to land: the downstream land,
            // review and follow-up paths are dead (`edge_taken`).
            output.insert("already_satisfied", GraphValue::Bool(true));
            output.insert(
                "evidence_commit",
                GraphValue::String(accepted.evidence_commit.clone()),
            );
            if let Some(close) = &accepted.issue_close {
                output.insert(
                    "issue_close",
                    serde_json::from_value::<GraphValue>(close.to_json())?,
                );
            }
            output.insert("changed", GraphValue::Bool(false));
        } else {
            output.insert("result_commit", GraphValue::String(observed.head.clone()));
            output.insert("changed", GraphValue::Bool(observed.head != work_base));
        }
        if let Some(evidence) = verification {
            output.insert(
                "verification",
                serde_json::from_value::<GraphValue>(evidence.to_json(None))?,
            );
        }
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
        let (result_commit, pin_ref) = if satisfied.is_some() {
            (None, None)
        } else {
            (Some(observed.head), Some(pin))
        };
        Ok((
            AttemptStatus::Succeeded,
            Settlement {
                result_commit,
                pin_ref,
                output: Some(output),
                ..Settlement::default()
            },
        ))
    }

    /// #2651: accept an `already_satisfied` handoff, or name why it is refused
    /// (`handoff_invalid`). Accepted only for an Issue-pipeline implement node
    /// (a commit-expecting session in a workflow that closes its input Issue),
    /// with no new commit, when the evidence commit is an ancestor of the
    /// attempt's base; the sandbox is already known clean. Accepting closes the
    /// input Issue (idempotently, keyed by the attempt) with a note naming the
    /// evidence commit and the worker's verification lines; a close the owner
    /// may not write is data on the output and leaves the Issue open, as for a
    /// land.
    ///
    /// # Errors
    /// A transient store or git error; the caller asks again on its next tick.
    #[allow(clippy::too_many_arguments)]
    async fn accept_already_satisfied(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        steps: &WorkflowSteps,
        handoff: &rsi_common::agent_contract::StrictPipelineHandoffV2,
        content: &str,
        head: &str,
        work_base: &str,
        sandbox: &std::path::Path,
    ) -> Result<std::result::Result<AlreadySatisfied, String>> {
        if !steps.expects_commit(&attempt.node_id) || !steps.has_close_issue() {
            return Ok(Err(
                "already_satisfied is only valid on an Issue pipeline's implement node".into(),
            ));
        }
        let Some(evidence) = handoff.evidence_commit.as_deref() else {
            return Ok(Err(
                "already_satisfied handoff requires evidence_commit".into()
            ));
        };
        if head != work_base {
            return Ok(Err(
                "already_satisfied handoff must not carry a new commit".into()
            ));
        }
        if !custody::commit_is_ancestor(sandbox, evidence, &attempt.base_commit)? {
            return Ok(Err(format!(
                "evidence_commit {evidence} is not an ancestor of the attempt base {}",
                attempt.base_commit
            )));
        }
        // #3772: the claim must carry its proof (criterion-to-evidence summary
        // and the tests run); a bare word is `already_satisfied_unproven`.
        // #3808: the summary must map each of the Issue's acceptance criteria.
        // #3881: the Issue row is read, the proof checked against it and the
        // close fenced on that row's version under one store lock, so an edit
        // of the criteria cannot slip in between and close on a stale proof.
        let store = self.store.lock().await;
        let (criteria, proved_row_version) =
            match crate::topology::starters::issue_snapshot(execution.input.as_ref()) {
                Some((project_id, display_number)) => {
                    match store.get_issue_by_display_number(project_id, display_number)? {
                        Some(issue) => (
                            crate::topology::issue_close::issue_acceptance_criteria_count(
                                &issue.body,
                            ),
                            Some(issue.row_version),
                        ),
                        None => (1, None),
                    }
                }
                None => (1, None),
            };
        if let Some(gap) = crate::topology::issue_close::satisfied_proof_gap(content, criteria) {
            return Ok(Err(format!(
                "already_satisfied_unproven: the handoff lacks {gap}"
            )));
        }
        let note = crate::topology::issue_close::already_satisfied_note(evidence, content);
        let close = match IssueCloseRequest::for_already_satisfied(
            execution,
            attempt,
            note,
            proved_row_version,
        ) {
            Ok(request) => close_input_issue(&store, &request)?,
            Err(refused) => refused,
        };
        Ok(Ok(AlreadySatisfied {
            evidence_commit: evidence.to_owned(),
            issue_close: Some(close),
        }))
    }

    /// The failed outcome of a node whose commit stayed unverified (#2249). An
    /// `uncertain` continuation is named in the error and shown to the operator
    /// by the caller; the failure still carries what the daemon read.
    fn verification_failure(
        unverified: &verify::Unverified,
        head: &str,
        uncertain: Option<String>,
    ) -> Result<(AttemptStatus, Settlement)> {
        let output = node_data(&serde_json::json!({
            "verification": unverified.to_json(),
            "result_commit": head,
        }));
        let mut error = format!("{}: {}", unverified.reason.as_str(), unverified.detail);
        if let Some(detail) = uncertain {
            error = format!("{VERIFICATION_CONTINUATION_UNCERTAIN}: {detail}; {error}");
        }
        Ok((
            AttemptStatus::Failed,
            Settlement {
                failure_class: Some(failure::VERIFICATION_FAILED),
                error: Some(error),
                output: Some(serde_json::to_value(output)?),
                ..Settlement::default()
            },
        ))
    }

    /// #2263: an unverified `require_verified` node is continued, as the same
    /// session, with its missing evidence, at most
    /// [`rows::MAX_VERIFICATION_CONTINUATIONS`] times per attempt; the next
    /// unverified completion fails `verification_failed`. The attempt stays
    /// `running` and its wall time is not touched. A cancelling execution or a
    /// passed deadline stops the continuation.
    ///
    /// Delivery is at most once: the `node_verification_continued` event is the
    /// durable claim, recorded before the provider effect. A claim whose user
    /// turn cannot be found in the conversation is uncertain: shown, never
    /// resent. Only a typed fence refusal, raised before any effect, takes the
    /// claim back.
    async fn continue_unverified_node(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        unverified: &verify::Unverified,
    ) -> Result<UnverifiedOutcome> {
        self.continue_node(
            execution,
            attempt,
            &rows::VERIFICATION_CONTINUATION,
            unverified.reason.as_str(),
            |n, max, delivery_id| unverified.continuation_message(n, max, delivery_id),
        )
        .await
    }

    /// #2563: a finished implement/repair node that left uncommitted paths is
    /// continued once, as the same session, to commit or discard them and repeat
    /// its handoff line. Same at-most-once claim as the verification
    /// continuation, counted apart from it. `Fail` means "not continued": the
    /// caller preserves the dirty tree and blocks as before.
    async fn continue_dirty_node(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        dirty_paths: usize,
    ) -> Result<UnverifiedOutcome> {
        self.continue_node(
            execution,
            attempt,
            &rows::UNCOMMITTED_CONTINUATION,
            "uncommitted_work",
            |n, max, delivery_id| {
                verify::uncommitted_continuation_message(dirty_paths, n, max, delivery_id)
            },
        )
        .await
    }

    /// #2584: a finished node whose last message the handoff parser refused is
    /// continued once, as the same session, with the parse error and the required
    /// first line. Same at-most-once claim as the other continuations, counted
    /// apart. `Fail`/`Uncertain` mean "not continued": the node fails
    /// `handoff_invalid`.
    async fn continue_handoff_format(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        parse_error: &str,
    ) -> Result<UnverifiedOutcome> {
        // #2759: this is the turn the daemon opens to obtain the handoff, so a
        // structured node has the harness validate its shape. The next provider
        // launch of the session consumes the arm; a continuation that is not
        // delivered takes it back, leaving the turn as free text.
        let structured = self.structured_handoff_node(execution, attempt);
        if structured {
            crate::structured_handoff::arm(attempt.session_id);
        }
        let outcome = self
            .continue_node(
                execution,
                attempt,
                &rows::HANDOFF_FORMAT_CONTINUATION,
                failure::HANDOFF_INVALID,
                |n, max, delivery_id| {
                    verify::handoff_format_continuation_message(parse_error, n, max, delivery_id)
                },
            )
            .await;
        if structured && !matches!(outcome, Ok(UnverifiedOutcome::Continued)) {
            crate::structured_handoff::disarm(attempt.session_id);
        }
        outcome
    }

    /// #2759: the node asks for a schema-validated handoff, by its own
    /// `structured_handoff` field or the daemon setting
    /// `structured_handoff_default`.
    fn structured_handoff_node(&self, execution: &ExecutionRow, attempt: &AttemptRow) -> bool {
        self.effects.structured_handoff_default()
            || WorkflowSteps::from_workflow(&execution.definition)
                .is_ok_and(|steps| steps.structured_handoff(&attempt.node_id))
    }

    async fn continue_node(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        kind: &rows::ContinuationKind,
        reason: &str,
        message: impl FnOnce(u32, u32, Uuid) -> String,
    ) -> Result<UnverifiedOutcome> {
        if execution.status == ExecutionStatus::Cancelling
            || crate::topology::decision::execution_deadline_passed(execution)
        {
            return Ok(UnverifiedOutcome::Fail);
        }
        // An earlier claim must have landed before another may be made.
        let previous = {
            let store = self.store.lock().await;
            match rows::continuations_of(&store, attempt.id, kind)?.pop() {
                Some(claim) => Some((
                    verify::continuation_turn_observed(
                        &store,
                        attempt.session_id,
                        claim.delivery_id,
                    )?,
                    claim,
                )),
                None => None,
            }
        };
        if let Some((false, claim)) = previous {
            return Ok(self
                .uncertain_verification_continuation(
                    execution,
                    attempt,
                    kind,
                    &claim,
                    "no user-turn event is tied to the claimed continuation",
                )
                .await);
        }
        let claim = {
            let store = self.store.lock().await;
            rows::claim_continuation(&store, execution.id, attempt, kind, reason)?
        };
        let (continuation, update) = match claim {
            rows::VerificationClaim::Claimed {
                continuation,
                update,
            } => (continuation, update),
            rows::VerificationClaim::Stopped | rows::VerificationClaim::Exhausted => {
                return Ok(UnverifiedOutcome::Fail);
            }
        };
        self.publish([update]);
        let prompt = message(continuation.n, kind.max, continuation.delivery_id);
        let outcome = self
            .effects
            .continue_unverified(attempt.session_id, prompt)
            .await;
        let Err(error) = outcome else {
            return Ok(UnverifiedOutcome::Continued);
        };
        use crate::store::manager_actions::fence::{
            CONTINUATION_TARGET_BUSY, CONTINUATION_TIP_CHANGED, CONTINUATION_TURN_CHANGED,
            continuation_fence_code,
        };
        match continuation_fence_code(&error) {
            // Another continuation owns or has moved the session: the fence
            // refused before any effect, so the claim is taken back and the
            // next observation follows the session.
            Some(
                code @ (CONTINUATION_TARGET_BUSY
                | CONTINUATION_TURN_CHANGED
                | CONTINUATION_TIP_CHANGED),
            ) => {
                let update = {
                    let store = self.store.lock().await;
                    rows::withdraw_continuation(
                        &store,
                        execution.id,
                        attempt,
                        kind,
                        &continuation,
                        code,
                    )?
                };
                self.publish([update]);
                tracing::info!(
                    execution_id = %execution.id,
                    node = %attempt.node_id,
                    code,
                    "verification continuation deferred: another continuation owns the session"
                );
                Ok(UnverifiedOutcome::Deferred)
            }
            // Any other typed refusal precedes the effect too, but retrying
            // would repeat it: the node fails with the evidence it has.
            Some(_) => Ok(UnverifiedOutcome::Fail),
            None => {
                // The error does not say whether the effect started. Only this
                // continuation's own turn proves it landed.
                let landed = {
                    let store = self.store.lock().await;
                    verify::continuation_turn_observed(
                        &store,
                        attempt.session_id,
                        continuation.delivery_id,
                    )?
                };
                if landed {
                    return Ok(UnverifiedOutcome::Continued);
                }
                Ok(self
                    .uncertain_verification_continuation(
                        execution,
                        attempt,
                        kind,
                        &continuation,
                        &error.to_string(),
                    )
                    .await)
            }
        }
    }

    /// The continuation's provider effect may have started and nothing shows
    /// it landed: show it and never send it again (#945).
    async fn uncertain_verification_continuation(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        kind: &rows::ContinuationKind,
        claim: &rows::VerificationContinuation,
        detail: &str,
    ) -> UnverifiedOutcome {
        self.effects.notify_operator(
            "warn",
            format!(
                "Topology execution {} ({}): node '{}' was sent {} continuation {} but \
                 the daemon cannot tell whether the session received it ({detail}). It is not \
                 sent again; continue the session yourself if it did not.",
                execution.id, execution.name, attempt.node_id, kind.label, claim.n
            ),
        );
        UnverifiedOutcome::Uncertain(detail.to_owned())
    }

    /// #2249: read the node session's own `scoped_test` jobs and the project's
    /// open failure signatures, and judge whether `head` is verified. Reads
    /// recorded state only; a store error is transient (asked again next tick).
    async fn verify_result(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        head: &str,
    ) -> Result<std::result::Result<verify::Evidence, verify::Unverified>> {
        let (jobs, signatures) = {
            let store = self.store.lock().await;
            let jobs = store.list_agent_jobs(attempt.session_id, verify::JOBS_READ)?;
            let signatures = match execution.project_id {
                Some(project) => store.open_failure_signature_issues(project)?,
                None => Vec::new(),
            };
            (jobs, signatures)
        };
        let jobs: Vec<_> = jobs.into_iter().map(|row| row.job).collect();
        Ok(verify::judge(
            &jobs,
            attempt.session_id,
            head,
            &verify::known_failures(&signatures),
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

    /// Whether a finished session ended on a complete handoff for a node that
    /// expects a commit (an implement or repair node), the only node a dirty
    /// tree is continued for (#2563).
    async fn finished_complete_with_commit(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        let steps = WorkflowSteps::from_workflow(&execution.definition)
            .map_err(DaemonError::InvalidParam)?;
        if !steps.expects_commit(&attempt.node_id) {
            return Ok(false);
        }
        let output = self.effects.output(attempt.session_id).await?;
        let Some(GraphValue::String(content)) = output.get("content") else {
            return Ok(false);
        };
        Ok(parse_pipeline_handoff_v2(content, "")
            .is_ok_and(|handoff| handoff.strict_status == PipelineStatusV2::Complete))
    }

    /// #2568: when the attempt was forked from a commit an earlier attempt of
    /// the same node instance preserved (an automatic restart retry or a
    /// resolved `retry`), the base that earlier work started at. `None` for an
    /// ordinary attempt, whose base is its own.
    async fn preserved_origin_base(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<Option<String>> {
        let store = self.store.lock().await;
        let attempts = rows::load_attempts(&store, execution.id)?;
        let mut origin: Option<String> = None;
        let mut base = attempt.base_commit.clone();
        for _ in 0..attempts.len() {
            let Some(earlier) = attempts.iter().find(|row| {
                row.node_id == attempt.node_id
                    && row.iteration == attempt.iteration
                    && row.id != attempt.id
                    && row.attempt_no < attempt.attempt_no
                    && row.preserved_commit.as_deref() == Some(base.as_str())
                    && row.base_commit != base
            }) else {
                break;
            };
            base.clone_from(&earlier.base_commit);
            origin = Some(base.clone());
        }
        Ok(origin)
    }

    /// Whether this node instance already used its one automatic retry from a
    /// restart-preserved commit (#2549); the next loss blocks as before.
    async fn restart_retry_spent(
        &self,
        execution: &ExecutionRow,
        attempt: &AttemptRow,
    ) -> Result<bool> {
        let store = self.store.lock().await;
        Ok(rows::load_attempts(&store, execution.id)?
            .iter()
            .any(|row| {
                row.node_id == attempt.node_id
                    && row.iteration == attempt.iteration
                    && row.id != attempt.id
                    && row.status == AttemptStatus::Interrupted
                    && row.preserved_commit.is_some()
                    // A context-cap continuation (#3340) is not a restart retry.
                    && row.failure_class.as_deref() != Some(failure::CONTEXT_CAP_CONTINUED)
            }))
    }

    /// Whether the attempt's sandbox is absent or still clean at its base: no
    /// work to preserve. A sandbox that cannot be read is not untouched.
    fn sandbox_untouched(attempt: &AttemptRow, sandbox: &Option<PathBuf>) -> bool {
        match sandbox {
            Some(sandbox) if sandbox.exists() => custody::observe_sandbox(sandbox)
                .is_ok_and(|observed| !observed.dirty && observed.head == attempt.base_commit),
            _ => true,
        }
    }

    /// An interrupted or lost session: diverged work is preserved and blocks;
    /// an unchanged sandbox is retryable. `restart_retry` (#2549): the session
    /// was lost to a daemon restart and the instance has its automatic retry
    /// left, so diverged work is preserved and settled `interrupted`
    /// (infrastructure), and the next attempt forks from the preserved commit.
    fn interrupted_outcome(
        execution: &ExecutionRow,
        attempt: &AttemptRow,
        sandbox: Option<PathBuf>,
        status: SessionStatus,
        restart_retry: bool,
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
                Ok(_) => {
                    let (settled, settlement) = Self::preserve(execution, attempt, &sandbox);
                    if restart_retry
                        && settled == AttemptStatus::Blocked
                        && settlement.failure_class == Some(failure::PRESERVED_WORK)
                    {
                        (
                            AttemptStatus::Interrupted,
                            Settlement {
                                failure_class: Some(failure::INTERRUPTED),
                                error: Some(
                                    "interrupted by a daemon restart; diverged work preserved and \
                                     the node retries from it"
                                        .into(),
                                ),
                                ..settlement
                            },
                        )
                    } else {
                        (settled, settlement)
                    }
                }
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

/// The text of the #2697 progress notice.
fn wall_time_notice(
    execution: &ExecutionRow,
    attempt: &AttemptRow,
    elapsed: chrono::TimeDelta,
    progress: Option<custody::SandboxProgress>,
    now: chrono::DateTime<Utc>,
) -> String {
    let minutes = |delta: chrono::TimeDelta| {
        let total = delta.num_minutes().max(0);
        format!("{}h{:02}m", total / 60, total % 60)
    };
    let work = match progress {
        Some(progress) => match progress.last_commit_at {
            Some(at) => format!(
                "{} commit(s) on its branch since the base, the last {} ago (at {})",
                progress.commits,
                minutes(now - at),
                at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
            None => "no commits on its branch since the base".to_owned(),
        },
        None => "its branch could not be read".to_owned(),
    };
    format!(
        "Topology execution {} ({}): node '{}' (attempt {}) has run {} of its {} wall-time \
         limit with {work}. At the limit it is killed and only its work in progress is kept; \
         if it is not converging, re-scope it now.",
        execution.id,
        execution.name,
        attempt.node_id,
        attempt.attempt_no,
        minutes(elapsed),
        minutes(SESSION_WALL_TIME),
    )
}

/// Whether the attempt outlived its node wall time or the execution deadline.
pub(crate) fn wall_time_expired(execution: &ExecutionRow, attempt: &AttemptRow) -> bool {
    wall_time_expired_after(execution, attempt, chrono::TimeDelta::zero())
}

/// The daemon holds some jobs before unit launch; cargo-slot and artifact locks
/// also wait inside an already-running unit. Both are admission, not test time.
fn build_admission_started(job: &rsi_common::agent_jobs::AgentJobV1) -> Option<DateTime<Utc>> {
    use rsi_common::agent_jobs::{JobKind, JobState};
    use std::io::BufRead;

    if !matches!(job.kind, JobKind::Build | JobKind::Test) {
        return None;
    }
    if job.state == JobState::Queued {
        return DateTime::parse_from_rfc3339(&job.created_at)
            .ok()
            .map(|at| at.with_timezone(&Utc));
    }
    if job.state != JobState::Running {
        return None;
    }
    let file = std::fs::File::open(&job.log_path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut pending = None;
    while reader.read_until(b'\n', &mut line).ok()? > 0 {
        if let Some(rest) = String::from_utf8_lossy(&line).strip_prefix("rsi-phase ")
            && let Some((at, phase)) = rest.split_once(' ')
        {
            if phase.starts_with("slot-wait:")
                || phase.starts_with("artifact-lock-wait:")
                || phase.starts_with("sandbox-lock-wait:")
            {
                if let Some(at) = at
                    .parse()
                    .ok()
                    .and_then(|at| DateTime::from_timestamp(at, 0))
                {
                    pending.get_or_insert(at);
                }
            } else if phase.starts_with("slot-granted:")
                || phase.starts_with("artifact-lock-held:")
                || phase.starts_with("sandbox-lock-held:")
            {
                pending = None;
            }
        }
        line.clear();
    }
    pending
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
