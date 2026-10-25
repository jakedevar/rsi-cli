//! Shared boundary of the Rust Terminal-Bench 2 runner (#3678 R1, design #3628, program #3135
//! option B): the serialized `RunnerPlanV1` the daemon will hand to `rsi-eval`, and the
//! `TrialOutcomeV1` the runner hands back. Both are plain data. They carry no credentials, no
//! daemon-private types and no task prose, so `rsi-eval` never depends on `rsid`.
//!
//! Design rows implemented here: "Harbor parity contract" (phase clocks, the typed reward and
//! exception causes) and "Outcomes, records, provenance and budgets" (raw reward map, selected
//! scalar, phase deadlines and times, script exit statuses, typed exception, explicit validity,
//! retry and slot identity).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

/// Schema version of [`RunnerPlanV1`] and [`TrialOutcomeV1`].
pub const BENCH_RUNNER_SCHEMA_VERSION: u32 = 1;

/// Runtime discriminator written to `run.harbor_version` once records are assembled (design
/// "Outcomes, records, provenance and budgets"). The Harbor pin is a parity reference only.
pub const RUNNER_VERSION_DISCRIMINATOR: &str = "rsi-tb2/1;parity=harbor-0.24.0";

/// Runner policy constants that are not Harbor defaults (design, below the parity table):
/// pull/copy/preflight/cleanup operation deadlines, in seconds.
pub const PULL_TIMEOUT_SEC: u64 = 1800;
pub const COPY_TIMEOUT_SEC: u64 = 300;
pub const PREFLIGHT_TIMEOUT_SEC: u64 = 180;
pub const CLEANUP_TIMEOUT_SEC: u64 = 120;
/// Task build/start deadline for the pin (all 89 tasks declare `build_timeout_sec = 600`).
pub const ENVIRONMENT_START_TIMEOUT_SEC: u64 = 600;
/// Agent setup deadline (Harbor `agents/base.py:59`).
pub const AGENT_SETUP_TIMEOUT_SEC: u64 = 360;
/// Bounded window to cancel and settle a timed-out agent before verification (design: 10 s).
pub const CANCEL_SETTLE_TIMEOUT_SEC: u64 = 10;

/// How a trial's agent phase runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerMode {
    /// The official solution script runs in the task container; no model session exists.
    Oracle,
    /// A native Harness session drives the container (later slices).
    Harness,
}

/// `(run, task, slot, retry)`: the identity of one trial attempt. `slot` is one of the `k`
/// statistical slots; `retry` distinguishes replacement attempts inside a slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrialIdentity {
    pub run_id: String,
    pub task: String,
    pub slot: u32,
    pub retry: u32,
}

/// Phase deadlines. Every clock is independent: time spent in one phase never reduces another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeoutPolicy {
    pub environment_start_sec: u64,
    pub agent_setup_sec: u64,
    pub collect_sec: u64,
    pub cleanup_sec: u64,
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            environment_start_sec: ENVIRONMENT_START_TIMEOUT_SEC,
            agent_setup_sec: AGENT_SETUP_TIMEOUT_SEC,
            collect_sec: COPY_TIMEOUT_SEC,
            cleanup_sec: CLEANUP_TIMEOUT_SEC,
        }
    }
}

/// One planned trial: a pinned task image, its resource limits and its own two deadlines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTrial {
    pub identity: TrialIdentity,
    pub image: String,
    pub image_digest: String,
    pub cpus: u64,
    pub memory_mb: u64,
    /// The task's own agent deadline (integral seconds; a fractional pin is refused).
    pub agent_timeout_sec: u64,
    /// The task's own verifier deadline (integral seconds).
    pub verifier_timeout_sec: u64,
    /// Environment variables the task declares for the solution (empty for the pin).
    #[serde(default)]
    pub solution_env: BTreeMap<String, String>,
    /// Environment variables the task declares for the verifier (empty for the pin).
    #[serde(default)]
    pub verifier_env: BTreeMap<String, String>,
    /// Phase user; `None` uses the image `USER`.
    #[serde(default)]
    pub agent_user: Option<String>,
    #[serde(default)]
    pub verifier_user: Option<String>,
}

/// Where and how the runner reaches the remote Docker host (R2, design "Architecture and
/// transport" items 2-6). It names hosts, paths and limits only; it holds no credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePlanV1 {
    /// Docker host the runner talks to, exactly `ssh://<host>` (`bench.docker_hosts` entry).
    pub docker_host: String,
    /// The SSH host for remote directory, stat and probe commands (`bench.laptop_host`). Must be
    /// the host named by `docker_host`.
    pub laptop_host: String,
    /// The operator allowlist (`bench.docker_hosts`); `docker_host` must be one of them.
    pub allowed_docker_hosts: Vec<String>,
    /// The job's requested concurrency.
    pub concurrency: u32,
    /// The operator cap (`bench.max_concurrency`); the effective cap is the smaller of the two.
    pub max_concurrency: u32,
    /// The pinned TB2 checkout on the hub (#3620).
    pub checkout_dir: String,
    /// The pinned `dataset.toml` (#3620).
    pub dataset_path: String,
    /// Hub directory the runner writes trial evidence and outcomes under.
    pub output_root: String,
    /// The run-local bench socket (`<bench root>/<run>/adapter.sock`) a Harness plan drives
    /// (R6, design "Architecture and transport"). Present exactly when the plan is
    /// [`RunnerMode::Harness`]: the oracle has no model session and so holds no such capability.
    #[serde(default)]
    pub socket_path: Option<String>,
    /// The daemon's run stop marker (`<bench root>/<run>/stop.json`). While it exists the runner
    /// starts no further shard, trial or retry; sessions already running are left to finish.
    #[serde(default)]
    pub stop_file: Option<String>,
}

impl RemotePlanV1 {
    /// The run-local socket and stop marker rules (R6): a Harness plan names the socket it drives
    /// (an absolute path, no `..`) and the oracle holds none; a stop marker path is absolute.
    ///
    /// # Errors
    /// The violated rule, as text.
    pub fn validate_run_paths(&self, mode: RunnerMode) -> Result<(), String> {
        let absolute = |what: &str, path: &str| -> Result<(), String> {
            let p = std::path::Path::new(path);
            if !p.is_absolute()
                || p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(format!("{what} must be an absolute path without `..`"));
            }
            Ok(())
        };
        match (mode, &self.socket_path) {
            (RunnerMode::Harness, None) => {
                return Err("a harness plan names no run socket".into());
            }
            (RunnerMode::Harness, Some(socket)) => absolute("socket_path", socket)?,
            (RunnerMode::Oracle, Some(_)) => {
                return Err("an oracle plan must not carry a run socket".into());
            }
            (RunnerMode::Oracle, None) => {}
        }
        if let Some(stop) = &self.stop_file {
            absolute("stop_file", stop)?;
        }
        Ok(())
    }

    /// The concurrency the runner may use: `min(concurrency, max_concurrency)`, at least 1.
    #[must_use]
    pub fn effective_concurrency(&self) -> u32 {
        self.concurrency.min(self.max_concurrency).max(1)
    }
}

/// The plan the runner executes. It holds no credentials (design "Architecture and transport").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerPlanV1 {
    pub schema_version: u32,
    pub run_id: String,
    pub mode: RunnerMode,
    #[serde(default)]
    pub timeouts: TimeoutPolicy,
    pub trials: Vec<PlannedTrial>,
    /// Remote Docker transport; required by `rsi-eval bench tb2 preflight/run`.
    #[serde(default)]
    pub remote: Option<RemotePlanV1>,
}

impl RunnerPlanV1 {
    /// A plan of the current schema version with the default timeout policy.
    #[must_use]
    pub fn new(run_id: impl Into<String>, mode: RunnerMode, trials: Vec<PlannedTrial>) -> Self {
        Self {
            schema_version: BENCH_RUNNER_SCHEMA_VERSION,
            run_id: run_id.into(),
            mode,
            timeouts: TimeoutPolicy::default(),
            trials,
            remote: None,
        }
    }

    /// Check the plan is runnable: supported version, non-empty identity fields, positive
    /// deadlines, every trial of this run, and no duplicate `(task, slot, retry)`.
    ///
    /// # Errors
    /// The first violated rule, as text.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != BENCH_RUNNER_SCHEMA_VERSION {
            return Err(format!(
                "unsupported schema_version {}",
                self.schema_version
            ));
        }
        if self.run_id.is_empty() {
            return Err("run_id is empty".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for trial in &self.trials {
            let id = &trial.identity;
            if id.run_id != self.run_id {
                return Err(format!("trial {} belongs to run {}", id.task, id.run_id));
            }
            if id.task.is_empty() {
                return Err("trial task is empty".into());
            }
            if trial.agent_timeout_sec == 0 || trial.verifier_timeout_sec == 0 {
                return Err(format!("trial {} has a zero deadline", id.task));
            }
            if !seen.insert((id.task.clone(), id.slot, id.retry)) {
                return Err(format!(
                    "duplicate trial {} slot {} retry {}",
                    id.task, id.slot, id.retry
                ));
            }
        }
        if let Some(remote) = &self.remote {
            remote.validate_run_paths(self.mode)?;
        }
        Ok(())
    }
}

/// The lifecycle of one trial (design "Native Harness trial and recovery").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialState {
    Prepared,
    EnvironmentReady,
    AgentRunning,
    AgentSettled,
    Verifying,
    Collected,
    Cleaned,
    Terminal,
}

/// Why a trial has no ordinary graded reward, or what happened on the way to one. Typed so
/// retry and record code never parses prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialExceptionKind {
    /// The environment did not become ready, or took longer than its start deadline.
    EnvironmentStartFailed,
    EnvironmentStartTimeout,
    /// The solution directory could not be uploaded or prepared.
    SolutionUploadFailed,
    /// The solution (agent) process could not be started or observed.
    AgentExecFailed,
    /// The agent (oracle) ran past the task's agent deadline. The verifier still runs.
    AgentTimeout,
    /// Remaining agent processes could not be stopped, so the container cannot be verified.
    AgentSettleFailed,
    /// The tests directory could not be uploaded or prepared.
    VerifierUploadFailed,
    /// The verifier process could not be started or observed.
    VerifierExecFailed,
    /// The verification phase ran past the task's verifier deadline.
    VerifierTimeout,
    RewardMissing,
    RewardEmpty,
    RewardParse,
    RewardNonFinite,
    /// A confirmed transport or daemon interruption; the only retryable cause.
    InfraInterrupted,
    /// The daemon refused the trial's route (invalid route verdict); never retried (R3a: the
    /// record layer reads this as `invalid_route`).
    RouteRejected,
    /// The global spend cap or the run budget refused the trial; never retried.
    SpendCapRejected,
    /// The plan, layout or protocol is unsupported or malformed.
    InvalidPlan,
    /// The operator cancelled the run (R5): live sessions were cancelled before teardown. The
    /// row is invalid and never replaced.
    Cancelled,
}

impl TrialExceptionKind {
    /// The snake_case name serde writes; the record's `exception_type` and `failure_cause` text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EnvironmentStartFailed => "environment_start_failed",
            Self::EnvironmentStartTimeout => "environment_start_timeout",
            Self::SolutionUploadFailed => "solution_upload_failed",
            Self::AgentExecFailed => "agent_exec_failed",
            Self::AgentTimeout => "agent_timeout",
            Self::AgentSettleFailed => "agent_settle_failed",
            Self::VerifierUploadFailed => "verifier_upload_failed",
            Self::VerifierExecFailed => "verifier_exec_failed",
            Self::VerifierTimeout => "verifier_timeout",
            Self::RewardMissing => "reward_missing",
            Self::RewardEmpty => "reward_empty",
            Self::RewardParse => "reward_parse",
            Self::RewardNonFinite => "reward_non_finite",
            Self::InfraInterrupted => "infra_interrupted",
            Self::RouteRejected => "route_rejected",
            Self::SpendCapRejected => "spend_cap_rejected",
            Self::InvalidPlan => "invalid_plan",
            Self::Cancelled => "cancelled",
        }
    }

    /// Only a confirmed infrastructure interruption earns the one replacement attempt.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::InfraInterrupted)
    }
}

/// A typed exception with its context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrialException {
    pub kind: TrialExceptionKind,
    pub message: String,
}

/// Wall time of one phase on its own clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseTiming {
    pub state: TrialState,
    /// The phase's own deadline, `None` for an unbounded bookkeeping step.
    pub deadline_ms: Option<u64>,
    pub elapsed_ms: u64,
}

/// Outcome of copying the evidence directories to the hub (design: collection failure is
/// explicit and never fabricates a trace hash).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CollectionStatus {
    pub attempted: bool,
    pub collected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Outcome of removing the trial's owned resources; reported apart from the grade.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CleanupStatus {
    pub attempted: bool,
    pub cleaned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Everything one trial produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrialOutcomeV1 {
    pub schema_version: u32,
    pub identity: TrialIdentity,
    pub mode: RunnerMode,
    /// Raw reward map as the verifier wrote it (bools normalized to 0 and 1).
    #[serde(default)]
    pub reward_map: BTreeMap<String, f64>,
    /// The scalar reward: `reward`, else the first value in key order. `None` when invalid.
    pub reward: Option<f64>,
    /// `true` only when a finite official reward was read within the verifier deadline.
    pub reward_valid: bool,
    /// Exit status of the solution script (oracle mode), recorded even when nonzero.
    #[serde(default)]
    pub oracle_exit_code: Option<i32>,
    /// Exit status of the verifier script; informational, never the grade.
    #[serde(default)]
    pub verifier_exit_code: Option<i32>,
    /// Every exception, in the order it happened. When `reward_valid` is false the last one is
    /// the cause ([`TrialOutcomeV1::invalid_cause`]); an agent timeout before a valid reward is
    /// kept as the stop reason.
    #[serde(default)]
    pub exceptions: Vec<TrialException>,
    /// States reached, in order.
    pub states: Vec<TrialState>,
    pub phases: Vec<PhaseTiming>,
    pub collection: CollectionStatus,
    pub cleanup: CleanupStatus,
    /// Tokens and spend are zero for an oracle; the daemon owns them for a Harness trial.
    #[serde(default)]
    pub model_sessions: u32,
}

impl TrialOutcomeV1 {
    /// The typed cause of an invalid reward: the last exception, `None` for a valid reward.
    #[must_use]
    pub fn invalid_cause(&self) -> Option<&TrialException> {
        if self.reward_valid {
            None
        } else {
            self.exceptions.last()
        }
    }

    /// `passed` as the record layer defines it: a valid reward of at least 1.0.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.reward_valid && self.reward.is_some_and(|r| r >= 1.0)
    }
}

// ---- trial join key --------------------------------------------------------------------------

fn sanitize_component(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The stable name of one trial attempt: its directory key on the laptop and the daemon trial id
/// (`bench_tasks.trial`) that joins the runner outcome to the daemon row at record assembly
/// (R3a). One definition, so the runner, the Harness client (R4) and the record agree.
#[must_use]
pub fn trial_key(identity: &TrialIdentity) -> String {
    format!(
        "{}-s{}-r{}",
        sanitize_component(&identity.task),
        identity.slot,
        identity.retry
    )
}

// ---- evaluator identity ------------------------------------------------------------------------

/// SHA-256 of the exact Harbor 0.24.0 sdist the runner's parity contract was derived from
/// (design "Evidence and reproducible source references").
pub const RUNNER_REFERENCE_ARCHIVE_SHA256: &str =
    "6c5185e6a76abc677ed3f95e688dc0dbf8597e4fa50847cb9a37a4448c91b1e5";
/// The effective retry policy (design parity row on retries): one replacement, only for a
/// confirmed infrastructure interruption, after a one second delay.
pub const RUNNER_RETRY_POLICY: &str = "infra_interrupted;max_replacements=1;delay_sec=1";

const SOURCES_DOMAIN: &str = "rsi.tb2.runner.sources.v1";
const EVALUATOR_DOMAIN: &str = "rsi.tb2.runner.evaluator.v1";
/// A manifest value for a source file that was not present at build time.
pub const SOURCE_MISSING: &str = "missing";

/// The deadline and classification constants a changed evaluator must not hide (design: "Include
/// deadline/classification constants"). Order and spelling are part of the digest.
#[must_use]
pub fn runner_constants_line() -> String {
    format!(
        "pull={PULL_TIMEOUT_SEC},copy={COPY_TIMEOUT_SEC},preflight={PREFLIGHT_TIMEOUT_SEC},\
         cleanup={CLEANUP_TIMEOUT_SEC},environment_start={ENVIRONMENT_START_TIMEOUT_SEC},\
         agent_setup={AGENT_SETUP_TIMEOUT_SEC},cancel_settle={CANCEL_SETTLE_TIMEOUT_SEC}"
    )
}

fn sha256_prefixed(bytes: &[u8]) -> String {
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

/// The runner's source identities: repository-relative path to the lowercase sha256 hex of the
/// file (or [`SOURCE_MISSING`]). It is stamped into `rsi-eval` at build time and verified by the
/// daemon at admission. It holds no run ids, timestamps, worktree paths or build SHA.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunnerSourceManifest {
    entries: BTreeMap<String, String>,
}

impl RunnerSourceManifest {
    /// # Errors
    /// An empty path or value, a separator inside either, a value that is neither a lowercase
    /// sha256 hex nor [`SOURCE_MISSING`], or a repeated path.
    pub fn from_entries<I: IntoIterator<Item = (String, String)>>(
        entries: I,
    ) -> Result<Self, String> {
        let mut map = BTreeMap::new();
        for (path, value) in entries {
            let bad_path =
                path.is_empty() || path.starts_with('/') || path.contains(['\n', '\t', ';', '=']);
            if bad_path {
                return Err(format!("invalid source path {path:?}"));
            }
            let hex = value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !hex && value != SOURCE_MISSING {
                return Err(format!("invalid source digest for {path}"));
            }
            if map.insert(path.clone(), value).is_some() {
                return Err(format!("duplicate source {path}"));
            }
        }
        Ok(Self { entries: map })
    }

    /// Parse the `path=hex;path=hex` text `rsi-eval` is stamped with.
    ///
    /// # Errors
    /// An empty manifest or any [`Self::from_entries`] violation.
    pub fn parse(text: &str) -> Result<Self, String> {
        let entries = text
            .split(';')
            .filter(|part| !part.is_empty())
            .map(|part| {
                part.split_once('=')
                    .map(|(path, value)| (path.to_string(), value.to_string()))
                    .ok_or_else(|| format!("manifest entry {part:?} has no '='"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if entries.is_empty() {
            return Err("empty source manifest".into());
        }
        Self::from_entries(entries)
    }

    /// The `path=hex;path=hex` text, sorted by path.
    #[must_use]
    pub fn to_text(&self) -> String {
        self.entries
            .iter()
            .map(|(path, value)| format!("{path}={value}"))
            .collect::<Vec<_>>()
            .join(";")
    }

    #[must_use]
    pub fn entries(&self) -> &BTreeMap<String, String> {
        &self.entries
    }

    /// Paths whose source was absent at build time. A stamp with any is not admissible.
    #[must_use]
    pub fn missing(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(_, value)| value.as_str() == SOURCE_MISSING)
            .map(|(path, _)| path.as_str())
            .collect()
    }

    /// `sha256:<hex>` of the sorted `path TAB value` lines under a domain tag.
    #[must_use]
    pub fn source_digest(&self) -> String {
        let mut text = format!("{SOURCES_DOMAIN}\n");
        for (path, value) in &self.entries {
            text.push_str(&format!("{path}\t{value}\n"));
        }
        sha256_prefixed(text.as_bytes())
    }

    /// The evaluator identity digest written to the record's legacy `adapter_tree_hash`: the
    /// sources plus every policy constant that defines how a trial is graded. A changed runner
    /// source, deadline or retry policy changes it, and with it the observation fingerprint.
    #[must_use]
    pub fn evaluator_digest(&self) -> String {
        let text = format!(
            "{EVALUATOR_DOMAIN}\nrunner_version={RUNNER_VERSION_DISCRIMINATOR}\n\
             reference_archive_sha256={RUNNER_REFERENCE_ARCHIVE_SHA256}\n\
             retry_policy={RUNNER_RETRY_POLICY}\nconstants={}\nsources={}\n",
            runner_constants_line(),
            self.source_digest()
        );
        sha256_prefixed(text.as_bytes())
    }

    /// The record's `sampling.runner` object (design: version, reference archive digest, retry
    /// policy and runner source digest).
    #[must_use]
    pub fn sampling_runner(&self) -> serde_json::Value {
        serde_json::json!({
            "reference_archive_sha256": RUNNER_REFERENCE_ARCHIVE_SHA256,
            "retry_policy": RUNNER_RETRY_POLICY,
            "source_digest": self.source_digest(),
            "version": RUNNER_VERSION_DISCRIMINATOR,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trial(task: &str, slot: u32) -> PlannedTrial {
        PlannedTrial {
            identity: TrialIdentity {
                run_id: "r1".into(),
                task: task.into(),
                slot,
                retry: 0,
            },
            image: "x/y:1".into(),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            cpus: 1,
            memory_mb: 2048,
            agent_timeout_sec: 900,
            verifier_timeout_sec: 600,
            solution_env: BTreeMap::new(),
            verifier_env: BTreeMap::new(),
            agent_user: None,
            verifier_user: None,
        }
    }

    fn remote(socket: Option<&str>, stop: Option<&str>) -> RemotePlanV1 {
        RemotePlanV1 {
            docker_host: "ssh://arch-laptop".into(),
            laptop_host: "arch-laptop".into(),
            allowed_docker_hosts: vec!["ssh://arch-laptop".into()],
            concurrency: 1,
            max_concurrency: 1,
            checkout_dir: "/c".into(),
            dataset_path: "/d".into(),
            output_root: "/o".into(),
            socket_path: socket.map(str::to_owned),
            stop_file: stop.map(str::to_owned),
        }
    }

    #[test]
    fn a_harness_plan_names_its_run_socket_and_the_oracle_holds_none() {
        let mut harness = RunnerPlanV1::new("r1", RunnerMode::Harness, vec![trial("a", 0)]);
        harness.remote = Some(remote(Some("/o/r1/adapter.sock"), Some("/o/r1/stop.json")));
        harness.validate().unwrap_or_else(|e| panic!("{e}"));

        harness.remote = Some(remote(None, Some("/o/r1/stop.json")));
        assert!(
            harness
                .validate()
                .is_err_and(|e| e.contains("no run socket"))
        );
        harness.remote = Some(remote(Some("o/r1/adapter.sock"), None));
        assert!(harness.validate().is_err_and(|e| e.contains("socket_path")));
        harness.remote = Some(remote(Some("/o/../etc/adapter.sock"), None));
        assert!(harness.validate().is_err());

        let mut oracle = RunnerPlanV1::new("r1", RunnerMode::Oracle, vec![trial("a", 0)]);
        oracle.remote = Some(remote(None, Some("/o/r1/stop.json")));
        oracle.validate().unwrap_or_else(|e| panic!("{e}"));
        oracle.remote = Some(remote(Some("/o/r1/adapter.sock"), None));
        assert!(oracle.validate().is_err_and(|e| e.contains("oracle")));
        oracle.remote = Some(remote(None, Some("stop.json")));
        assert!(oracle.validate().is_err_and(|e| e.contains("stop_file")));
    }

    #[test]
    fn a_plan_written_before_the_run_paths_still_reads() {
        let mut plan = RunnerPlanV1::new("r1", RunnerMode::Oracle, vec![trial("a", 0)]);
        plan.remote = Some(remote(None, None));
        let mut value = serde_json::to_value(&plan).unwrap_or_default();
        let object = value["remote"]
            .as_object_mut()
            .unwrap_or_else(|| panic!("remote"));
        object.remove("socket_path");
        object.remove("stop_file");
        let back: RunnerPlanV1 = serde_json::from_value(value).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back, plan);
    }

    #[test]
    fn a_plan_round_trips_and_validates() {
        let plan = RunnerPlanV1::new("r1", RunnerMode::Oracle, vec![trial("a", 0), trial("a", 1)]);
        plan.validate().unwrap_or_else(|e| panic!("{e}"));
        let json = serde_json::to_string(&plan).unwrap_or_default();
        let back: RunnerPlanV1 = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back, plan);
        assert_eq!(back.timeouts.environment_start_sec, 600);
        assert_eq!(back.timeouts.agent_setup_sec, 360);
    }

    #[test]
    fn a_plan_refuses_foreign_duplicate_and_zero_deadline_trials() {
        let mut foreign = trial("a", 0);
        foreign.identity.run_id = "other".into();
        assert!(
            RunnerPlanV1::new("r1", RunnerMode::Oracle, vec![foreign])
                .validate()
                .is_err()
        );
        assert!(
            RunnerPlanV1::new("r1", RunnerMode::Oracle, vec![trial("a", 0), trial("a", 0)])
                .validate()
                .is_err()
        );
        let mut zero = trial("a", 0);
        zero.verifier_timeout_sec = 0;
        assert!(
            RunnerPlanV1::new("r1", RunnerMode::Oracle, vec![zero])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn only_an_infrastructure_interruption_is_retryable() {
        assert!(TrialExceptionKind::InfraInterrupted.retryable());
        for kind in [
            TrialExceptionKind::AgentTimeout,
            TrialExceptionKind::VerifierTimeout,
            TrialExceptionKind::RewardMissing,
            TrialExceptionKind::RewardEmpty,
            TrialExceptionKind::RewardParse,
            TrialExceptionKind::RewardNonFinite,
            TrialExceptionKind::InvalidPlan,
            TrialExceptionKind::Cancelled,
        ] {
            assert!(!kind.retryable(), "{kind:?}");
        }
    }

    // ---- R3a: evaluator identity ----

    const ONE: &str = "7692c3ad3540bb803c020b3aee66cd8887123234ea0c6e7143c0add73ff431ed";
    const TWO: &str = "3fc4ccfe745870e2c0d99f71f30ff0656c8dedd41cc1d7d3d376b0dbe685e2f3";

    fn manifest() -> RunnerSourceManifest {
        RunnerSourceManifest::parse(&format!("b/two.rs={TWO};a/one.rs={ONE}"))
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// Golden values computed independently with `sha256sum` over the documented byte layout
    /// (`printf 'rsi.tb2.runner.sources.v1\na/one.rs\t<sha256(one)>\n...' | sha256sum`), so a
    /// silent change of the layout, a constant or the retry policy fails here and, in the build,
    /// changes every stamped evaluator identity.
    #[test]
    fn the_evaluator_digests_match_the_documented_byte_layout() {
        let m = manifest();
        assert_eq!(
            m.source_digest(),
            "sha256:68c9af97c8e1de5cd5e015a4c1f348d0eb0343dc5a1ce5b78e9935a7277c2ced"
        );
        assert_eq!(
            m.evaluator_digest(),
            "sha256:f63a444a229ae35c4c1fcd7e1eee4f7f62f34bae3cf58c295f9911d39da2a3b8"
        );
        // Entries are sorted, so input order cannot change the identity.
        assert_eq!(m.to_text(), format!("a/one.rs={ONE};b/two.rs={TWO}"));
    }

    #[test]
    fn one_changed_source_or_a_missing_source_changes_the_evaluator_digest() {
        let base = manifest();
        let changed = RunnerSourceManifest::parse(&format!("a/one.rs={TWO};b/two.rs={TWO}"))
            .unwrap_or_else(|e| panic!("{e}"));
        let missing = RunnerSourceManifest::parse(&format!("a/one.rs=missing;b/two.rs={TWO}"))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(base.evaluator_digest(), changed.evaluator_digest());
        assert_ne!(base.evaluator_digest(), missing.evaluator_digest());
        assert_eq!(missing.missing(), ["a/one.rs"]);
        assert!(base.missing().is_empty());
    }

    #[test]
    fn a_malformed_manifest_is_refused() {
        for bad in [
            "",
            "a.rs",
            "a.rs=XYZ",
            &format!("/abs.rs={ONE}"),
            &format!("a.rs={ONE};a.rs={TWO}"),
            &format!("a.rs={}", ONE.to_uppercase()),
        ] {
            assert!(RunnerSourceManifest::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_sampling_runner_names_version_reference_retry_policy_and_sources() {
        let sampling = manifest().sampling_runner();
        assert_eq!(sampling["version"], RUNNER_VERSION_DISCRIMINATOR);
        assert_eq!(
            sampling["reference_archive_sha256"],
            RUNNER_REFERENCE_ARCHIVE_SHA256
        );
        assert_eq!(sampling["retry_policy"], RUNNER_RETRY_POLICY);
        assert_eq!(sampling["source_digest"], manifest().source_digest());
    }

    #[test]
    fn exception_names_agree_with_serde_and_the_trial_key_is_sanitized() {
        for kind in [
            TrialExceptionKind::EnvironmentStartFailed,
            TrialExceptionKind::EnvironmentStartTimeout,
            TrialExceptionKind::SolutionUploadFailed,
            TrialExceptionKind::AgentExecFailed,
            TrialExceptionKind::AgentTimeout,
            TrialExceptionKind::AgentSettleFailed,
            TrialExceptionKind::VerifierUploadFailed,
            TrialExceptionKind::VerifierExecFailed,
            TrialExceptionKind::VerifierTimeout,
            TrialExceptionKind::RewardMissing,
            TrialExceptionKind::RewardEmpty,
            TrialExceptionKind::RewardParse,
            TrialExceptionKind::RewardNonFinite,
            TrialExceptionKind::InfraInterrupted,
            TrialExceptionKind::RouteRejected,
            TrialExceptionKind::SpendCapRejected,
            TrialExceptionKind::InvalidPlan,
        ] {
            assert_eq!(
                serde_json::to_value(kind).unwrap_or_default(),
                kind.as_str()
            );
            assert!(!kind.retryable() || kind == TrialExceptionKind::InfraInterrupted);
        }
        let id = TrialIdentity {
            run_id: "r".into(),
            task: "a b/c".into(),
            slot: 2,
            retry: 1,
        };
        assert_eq!(trial_key(&id), "a_b_c-s2-r1");
    }
}
