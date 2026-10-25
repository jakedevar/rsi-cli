//! #3178 (program #3135 M3 S4): the bench run adapter protocol.
//!
//! A Harbor trial on the laptop starts, watches and stops exactly one daemon
//! Harness session for its task container through a per-run unix socket. The
//! socket serves only the five methods named here, for one run. This module
//! holds the wire types and the typed refusal codes; the behaviour lives in
//! `rsid::bench`.
//!
//! None of these methods is an agent verb: they are reachable only through a
//! run's own adapter socket (`~/.rsi/bench/<run>/adapter.sock`), never through
//! the daemon socket, `AGENT_VERBS`, `READ_VERBS` or a native tool.

use serde::{Deserialize, Serialize};

/// Start one trial: launch its session. Idempotent per (run, trial).
pub const METHOD_TASK_START: &str = "BenchTaskStart";
/// Read a trial's state, stop reason, usage and route verdict.
pub const METHOD_TASK_STATUS: &str = "BenchTaskStatus";
/// Stop a trial's session (Harbor timeout or cancellation).
pub const METHOD_TASK_CANCEL: &str = "BenchTaskCancel";
/// Settle a trial: final usage, build digest and trace handle.
pub const METHOD_TASK_FINISH: &str = "BenchTaskFinish";
/// Page a trial's full, untruncated trace.
pub const METHOD_TRACE_READ: &str = "BenchTraceRead";

/// The only methods a run's adapter socket serves.
pub const BENCH_METHODS: [&str; 5] = [
    METHOD_TASK_START,
    METHOD_TASK_STATUS,
    METHOD_TASK_CANCEL,
    METHOD_TASK_FINISH,
    METHOD_TRACE_READ,
];

/// Typed refusal: the socket serves only the five Bench methods.
pub const BENCH_METHOD_NOT_ALLOWED: &str = "bench_method_not_allowed";
/// Typed refusal: the run is not active (finished, cancelled or unknown).
pub const BENCH_RUN_NOT_ACTIVE: &str = "bench_run_not_active";
/// Typed refusal: the task is not in the run's pinned task set.
pub const BENCH_TASK_UNKNOWN: &str = "bench_task_unknown";
/// Typed refusal: no trial of that id in this run (another run's trial reads the same).
pub const BENCH_TRIAL_UNKNOWN: &str = "bench_trial_unknown";
/// Typed refusal: the trial id was started with different parameters.
pub const BENCH_TRIAL_CONFLICT: &str = "bench_trial_conflict";
/// Typed refusal: the run's remaining budget cannot cover the per-task cap.
pub const BENCH_RUN_BUDGET_INSUFFICIENT: &str = "bench_run_budget_insufficient";
/// Typed refusal: (OpenRouter, model, effort) is not in the project's `allowed_launches`.
pub const BENCH_LAUNCH_NOT_ALLOWED: &str = "bench_launch_not_allowed";
/// Typed refusal: the run already has its concurrency cap of live trials.
pub const BENCH_CONCURRENCY_EXCEEDED: &str = "bench_concurrency_exceeded";
/// Typed refusal: a malformed request (`bench_invalid_request:<reason>`).
pub const BENCH_INVALID_REQUEST: &str = "bench_invalid_request";
/// Typed refusal: the trial is not finished, so it has no final usage yet.
pub const BENCH_TASK_NOT_FINISHED: &str = "bench_task_not_finished";
/// Typed refusal: the trial's trace cannot be read.
pub const BENCH_TRACE_UNAVAILABLE: &str = "bench_trace_unavailable";
/// Terminal reason of a session the per-task USD cap stopped. The task is graded normally.
pub const BENCH_TASK_BUDGET_EXHAUSTED: &str = "bench_task_budget_exhausted";
/// Typed refusal of a model call: a session whose task is no longer `running`
/// never spends (a resumed session after a daemon restart, a cancelled task).
pub const BENCH_TASK_NOT_RUNNING: &str = "bench_task_not_running";
/// Typed refusal of a model call: an earlier call settled with no cost, so the
/// session's spend is unknown and the per-task cap cannot be judged (fail closed).
pub const BENCH_TASK_COST_UNKNOWN: &str = "bench_task_cost_unknown";
/// Typed refusal of a model call: the trace could not be written, so the trial
/// would not have a full trace.
pub const BENCH_TRACE_WRITE_FAILED: &str = "bench_trace_write_failed";
/// The typed codes a bench model-call refusal ends a session with. The monitor
/// records each as the session's stop reason, and none is ever healed.
pub const BENCH_SESSION_STOP_CODES: [&str; 4] = [
    BENCH_TASK_BUDGET_EXHAUSTED,
    BENCH_TASK_NOT_RUNNING,
    BENCH_TASK_COST_UNKNOWN,
    BENCH_TRACE_WRITE_FAILED,
];
/// Stop reason of a trial the daemon ended at its `agent_timeout_sec`.
pub const BENCH_AGENT_TIMEOUT: &str = "bench_agent_timeout";
/// Stop reason of a trial a daemon restart interrupted.
pub const BENCH_INFRA_INTERRUPTED: &str = "infra_interrupted";
/// Stop reason of a trial the adapter cancelled.
pub const BENCH_CANCELLED: &str = "bench_cancelled";

/// Longest accepted identifier (task, trial, container, project, host).
pub const MAX_ID_LEN: usize = 256;
/// Longest accepted instruction (bytes).
pub const MAX_INSTRUCTION_BYTES: usize = 512 * 1024;
/// Longest accepted `agent_timeout_sec` (24 h).
pub const MAX_AGENT_TIMEOUT_SEC: u64 = 86_400;
/// Largest concurrency a run may declare.
pub const MAX_CONCURRENCY: u32 = 64;
/// Largest page `BenchTraceRead` returns.
pub const MAX_TRACE_PAGE_BYTES: u64 = 1024 * 1024;
/// Default page `BenchTraceRead` returns.
pub const DEFAULT_TRACE_PAGE_BYTES: u64 = 256 * 1024;

/// A run's lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchRunState {
    /// Its adapter socket is bound and it accepts `BenchTaskStart`.
    Active,
    /// The run completed; no new trials.
    Finished,
    /// The run was stopped; no new trials.
    Cancelled,
}

impl BenchRunState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Finished => "finished",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(Self::Active),
            "finished" => Some(Self::Finished),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// A trial's lifecycle. `starting` is the claim before the session exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchTaskState {
    Starting,
    Running,
    /// The session ended on its own (a normal end, the per-task cap or a failure
    /// the task is graded on); the stop reason says which.
    Finished,
    /// The adapter or the agent timeout stopped it.
    Cancelled,
    /// A daemon restart interrupted it. Never resumed; the adapter reads this state.
    InfraInterrupted,
}

impl BenchTaskState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Cancelled => "cancelled",
            Self::InfraInterrupted => "infra_interrupted",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "starting" => Some(Self::Starting),
            "running" => Some(Self::Running),
            "finished" => Some(Self::Finished),
            "cancelled" => Some(Self::Cancelled),
            "infra_interrupted" => Some(Self::InfraInterrupted),
            _ => None,
        }
    }

    /// Whether the trial can still spend or change.
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

/// A name safe to use as a path component and a `docker` argument: ASCII
/// alphanumerics, `.`, `_` and `-`; never leading with `-` or `.`; no `..`.
fn id_ok(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_ID_LEN
        && !text.starts_with(['-', '.'])
        && !text.contains("..")
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `BenchTaskStart` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchTaskStartParams {
    pub task: String,
    pub trial: String,
    pub compose_project: String,
    pub container: String,
    pub workdir: String,
    #[serde(default)]
    pub user: Option<String>,
    pub instruction: String,
    pub agent_timeout_sec: u64,
}

impl BenchTaskStartParams {
    /// Shape check only; the daemon checks the run, the grant and the container.
    ///
    /// # Errors
    /// The `<reason>` of `bench_invalid_request:<reason>`.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !id_ok(&self.task) {
            return Err("task");
        }
        if !id_ok(&self.trial) {
            return Err("trial");
        }
        if !id_ok(&self.compose_project) {
            return Err("compose_project");
        }
        if !id_ok(&self.container) {
            return Err("container");
        }
        if self.workdir.is_empty()
            || self.workdir.len() > 4096
            || !self.workdir.starts_with('/')
            || self.workdir.contains('\0')
        {
            return Err("workdir");
        }
        if self.user.as_deref().is_some_and(|user| !id_ok(user)) {
            return Err("user");
        }
        if self.instruction.trim().is_empty()
            || self.instruction.len() > MAX_INSTRUCTION_BYTES
            || self.instruction.contains('\0')
        {
            return Err("instruction");
        }
        if self.agent_timeout_sec == 0 || self.agent_timeout_sec > MAX_AGENT_TIMEOUT_SEC {
            return Err("agent_timeout_sec");
        }
        Ok(())
    }
}

/// Parameters of Status, Cancel and Finish: the trial of this run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchTrialParams {
    pub trial: String,
}

/// `BenchTraceRead` parameters: a page of the trace from a byte offset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchTraceReadParams {
    pub trial: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub limit_bytes: Option<u64>,
}

/// One trial's usage, from the increment SQL (never the session-cumulative rows).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BenchUsage {
    /// `None` when no call has a known cost (cost capture unproven or no call yet).
    pub cost_usd: Option<f64>,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub wall_ms: u64,
    /// Settled model calls.
    pub calls: u64,
}

/// Where a trial's trace is and how to check it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchTraceHandle {
    pub path: String,
    pub bytes: u64,
    /// Recorded at finish; `None` while the trial is live.
    pub sha256: Option<String>,
}

/// A trial's state, as `BenchTaskStart`, `BenchTaskStatus` and `BenchTaskCancel` answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchTaskStatusV1 {
    pub run_id: String,
    pub task: String,
    pub trial: String,
    pub attempt: u32,
    pub state: BenchTaskState,
    pub stop_reason: Option<String>,
    pub session_id: Option<String>,
    pub usage: BenchUsage,
    /// `valid` or `invalid_route`; `None` until the session ended.
    pub route_verdict: Option<String>,
    pub route_violations: Vec<String>,
    pub harness_build_digest: Option<String>,
    pub trace: Option<BenchTraceHandle>,
}

/// One page of a trace. `data` is whole JSONL lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchTraceChunkV1 {
    pub offset: u64,
    pub next_offset: u64,
    pub eof: bool,
    pub data: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start() -> BenchTaskStartParams {
        BenchTaskStartParams {
            task: "fix-git".into(),
            trial: "fix-git__abc".into(),
            compose_project: "fix-git__abc".into(),
            container: "fix-git__abc-main-1".into(),
            workdir: "/app".into(),
            user: Some("agent".into()),
            instruction: "Fix the repo.".into(),
            agent_timeout_sec: 900,
        }
    }

    #[test]
    fn the_adapter_serves_exactly_five_methods() {
        assert_eq!(BENCH_METHODS.len(), 5);
        for method in BENCH_METHODS {
            assert!(method.starts_with("BenchTask") || method == METHOD_TRACE_READ);
        }
    }

    #[test]
    fn start_params_validate_each_field() {
        assert_eq!(start().validate(), Ok(()));
        let mut bad = start();
        bad.task = "-rf".into();
        assert_eq!(bad.validate(), Err("task"));
        let mut bad = start();
        bad.workdir = "app".into();
        assert_eq!(bad.validate(), Err("workdir"));
        let mut bad = start();
        bad.instruction = "  ".into();
        assert_eq!(bad.validate(), Err("instruction"));
        let mut bad = start();
        bad.agent_timeout_sec = 0;
        assert_eq!(bad.validate(), Err("agent_timeout_sec"));
        let mut bad = start();
        bad.container = "a b".into();
        assert_eq!(bad.validate(), Err("container"));
        for escape in ["../x", "a/b", ".hidden", "a..b"] {
            let mut bad = start();
            bad.trial = escape.into();
            assert_eq!(bad.validate(), Err("trial"), "{escape}");
        }
        let mut bad = start();
        bad.user = Some(String::new());
        assert_eq!(bad.validate(), Err("user"));
    }

    #[test]
    fn unknown_start_fields_are_refused() {
        let wire = serde_json::json!({
            "task": "t", "trial": "r", "compose_project": "p", "container": "c",
            "workdir": "/app", "instruction": "i", "agent_timeout_sec": 1,
            "provider": "Claude"
        });
        assert!(serde_json::from_value::<BenchTaskStartParams>(wire).is_err());
    }

    #[test]
    fn task_states_round_trip_and_only_two_are_live() {
        for state in [
            BenchTaskState::Starting,
            BenchTaskState::Running,
            BenchTaskState::Finished,
            BenchTaskState::Cancelled,
            BenchTaskState::InfraInterrupted,
        ] {
            assert_eq!(BenchTaskState::parse(state.as_str()), Some(state));
        }
        assert!(BenchTaskState::Running.is_live());
        assert!(!BenchTaskState::InfraInterrupted.is_live());
        assert_eq!(BenchRunState::parse("active"), Some(BenchRunState::Active));
        assert_eq!(BenchRunState::parse("nope"), None);
    }
}
