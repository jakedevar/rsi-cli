//! Wire types for daemon-owned durable jobs (#1002 slice 1).
//!
//! `AgentSubmitJob` hands a long operation (test, build, landing, cloud gate,
//! cloud sweep)
//! to the daemon, which runs it in its own systemd (Linux) or launchd (macOS) service outside
//! every session scope and wakes the owner once with the typed result.
//! `AgentGetJob` reads one job back. Parameters are typed: an agent never
//! supplies a command line, only the fields below.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const JOB_MAX_NAME_BYTES: usize = 80;
pub const JOB_MAX_IDEMPOTENCY_BYTES: usize = 128;
pub const JOB_MAX_TEST_FILTERS: usize = 32;
pub const JOB_MAX_TOKEN_BYTES: usize = 200;
/// #1584: a recipe run may carry at most this many explicit focused
/// `PACKAGE=FILTER` test filters (each bounded like a landing `--test-filter`).
pub const JOB_RECIPE_MAX_FILTERS: usize = 16;

/// Stable refusal codes; each is the whole `InvalidParam`/`PolicyDenied` text.
pub const JOB_INVALID_REQUEST: &str = "job_invalid_request";
pub const JOB_INVALID_PARAMS: &str = "job_invalid_params";
/// The worktree has no manifest, or it does not declare the requested recipe.
pub const JOB_RECIPE_NOT_ALLOWED: &str = "job_recipe_not_allowed";
/// The project recipe manifest is malformed, oversized or escapes the worktree.
pub const JOB_RECIPE_INVALID: &str = "job_recipe_invalid";
pub const JOB_NAME_INVALID: &str = "job_name_invalid";
pub const JOB_KEY_INVALID: &str = "job_idempotency_key_invalid";
pub const JOB_KIND_UNSUPPORTED: &str = "job_kind_unsupported";
pub const JOB_DIR_NOT_ALLOWED: &str = "job_directory_not_allowed";
pub const JOB_NOT_FOUND: &str = "job_not_found";
/// `landing`, `cloud_gate` and `cloud_sweep` publish to `rolling` / spend the
/// operator's cloud grant: only the current appointed manager or current Epic lead may submit.
pub const JOB_KIND_NOT_AUTHORIZED: &str = "job_kind_not_authorized";
/// #1235: `sandbox_session_id` names a session that is still live; a job
/// runs only in a terminal session's sandbox (one writer per sandbox).
pub const JOB_SANDBOX_SESSION_LIVE: &str = "job_sandbox_session_live";
pub const JOB_LAUNCH_FAILED: &str = "job_launch_failed";
/// The OS or requested workflow has no supported durable backend.
pub const JOB_PLATFORM_UNSUPPORTED: &str = "job_platform_unsupported";
pub const JOB_KEY_CONFLICT: &str = "job_idempotency_key_conflict";
/// `AgentCancelJob` (#1106): the refusal recorded on a job its owner stopped.
/// The job settles `failed` with this refusal, the same typed route the
/// scratch-quota stop uses, so no new state or migration is needed.
pub const JOB_CANCELLED: &str = "job_cancelled";
/// #1337: a `test` job ran past its wall-clock timeout. The daemon stopped its
/// unit and settled it `failed` with this refusal (the cancel route: no new
/// state or migration).
pub const JOB_TIMED_OUT: &str = "job_timed_out";
/// #1611: a job held before its unit launched (queued behind a deploy drain)
/// waited past the admission cap (the drain hold cap plus a margin) and was
/// settled `failed` with this refusal. Distinct from [`JOB_TIMED_OUT`]: the
/// execution budget (`timeout_minutes`) only starts when the unit launches, so
/// a job that never ran never "timed out".
pub const JOB_ADMISSION_TIMED_OUT: &str = "job_admission_timed_out";
/// #1337: `timeout_minutes` above the operator default needs the appointed
/// manager or an Epic lead, or the bounded live Issue-worker QA shard form.
pub const JOB_TIMEOUT_NOT_AUTHORIZED: &str = "job_timeout_not_authorized";
/// #1337: the operator setting `job_test_timeout_mins` (default and bounds).
/// A `test` job without `timeout_minutes` is stopped after the default; a
/// manager may raise one job up to the maximum (the unit's own cap).
pub const JOB_TEST_TIMEOUT_DEFAULT_MINS: u32 = 20;
pub const JOB_TEST_TIMEOUT_MIN_MINS: u32 = 5;
pub const JOB_TEST_TIMEOUT_MAX_MINS: u32 = 180;
/// #1520: closed QA shard jobs have a fixed ceiling, including manager submits.
pub const JOB_QA_LANE_TIMEOUT_MAX_MINS: u32 = 90;
pub const JOB_QA_LANE_LIMIT: &str = "job_qa_lane_limit";
pub const JOB_QA_LANE_SHA_MISMATCH: &str = "job_qa_lane_sha_mismatch";
pub const JOB_QA_LANE_MAX_RUNNING: u32 = 2;

/// Submission HEAD label for a QA shard; no dirtiness or exact-tested-bytes proof.
/// Eligibility comes from daemon state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QaLaneParams {
    pub sha: String,
}

/// Typed job kind. Strings match the SQLite CHECK exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Test,
    Build,
    Landing,
    CloudGate,
    CloudSweep,
}

impl JobKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Build => "build",
            Self::Landing => "landing",
            Self::CloudGate => "cloud_gate",
            Self::CloudSweep => "cloud_sweep",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "test" => Self::Test,
            "build" => Self::Build,
            "landing" => Self::Landing,
            "cloud_gate" => Self::CloudGate,
            "cloud_sweep" => Self::CloudSweep,
            _ => return None,
        })
    }
}

/// Job lifecycle. `Queued` is a job accepted during a deploy drain and held
/// until the drain ends (the `held` field names why); it has no unit yet.
/// `Lost` means the unit ended without recording an exit status (stopped,
/// timed out or killed by the host).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Lost,
}

impl JobState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Lost => "lost",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "lost" => Self::Lost,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// Who is woken when a job settles (#1006). `owner` (the default) delivers one
/// resume wake to the owner per job; `none` settles the job silently: its
/// result stays readable through `AgentGetJob`/`AgentListJobs`, and a caller
/// that wants one wake for a batch arms `AgentScheduleWake` mode `when` with a
/// `jobs_terminal` predicate instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum JobWake {
    #[default]
    Owner,
    None,
}

impl JobWake {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::None => "none",
        }
    }
}

/// `AgentSubmitJob` request. `params` is decoded per `kind` by
/// [`AgentSubmitJobRequestV1::typed_params`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSubmitJobRequestV1 {
    pub kind: JobKind,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default)]
    pub name: Option<String>,
    /// Optional replay key: an identical retry returns the original job.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Appointed managers only: run in this other worktree of the caller's own
    /// repository (an integration worktree) instead of the caller's sandbox.
    #[serde(default)]
    pub worktree: Option<String>,
    /// `owner` (default) or `none`: see [`JobWake`].
    #[serde(default)]
    pub wake: Option<JobWake>,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// #1235: `landing` and `cloud_gate` only: run in this terminal, in-reach
    /// session's sandbox (one writer per sandbox) instead of the caller's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_session_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGetJobRequestV1 {
    pub job_id: Uuid,
}

/// `AgentCancelJob` (#1106): stop one of your running jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCancelJobRequestV1 {
    pub job_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentListJobsRequestV1 {
    #[serde(default)]
    pub limit: Option<u32>,
}

/// #1638: run `scripts/scoped-test --base <base> [--head <head>]` in the
/// caller's sandbox. Two bare refs and nothing else: no filters, no flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedTestParams {
    /// Branch, `origin/<branch>` or full commit id the diff is taken against.
    pub base: String,
    /// Candidate to verify; must be the sandbox HEAD when omitted or named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

/// `cargo test` (or one nextest shard) through the cargo build-slot wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestJobParams {
    /// #1520: known shard only, explicitly delegated live Issue-launch binding (or manager/
    /// Epic lead), at most 90 minutes and two running QA lanes per owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qa_lane: Option<QaLaneParams>,
    /// #1638: `scripts/scoped-test` for this sandbox; the typed scoped-test
    /// receipt is `result.receipt`. Exclusive with every other test form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoped_test: Option<ScopedTestParams>,
    /// Run one declared just/make gate from the worktree's `.rsi/jobs.toml`.
    /// Exclusive with package, shard and candidate-receipt runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
    /// Run `scripts/run-rsid-test-shards.sh shard <name>`.
    #[serde(default)]
    pub shard: Option<String>,
    /// Shard-only `test(NAME)` filterset.
    #[serde(default)]
    pub filterset: Option<String>,
    /// Run `cargo test -p <package>`.
    #[serde(default)]
    pub package: Option<String>,
    /// Test-name filters after `--` (package runs only). Empty runs every test
    /// of the package (#1106). A recipe run may instead carry up to
    /// [`JOB_RECIPE_MAX_FILTERS`] `PACKAGE=FILTER` lander filters that a
    /// scoped-test recipe uses in place of its derived selection (#1584).
    #[serde(default)]
    pub filters: Vec<String>,
    /// Package runs: match whole test names with libtest `--exact`.
    /// Defaults to substring matching; no filters still runs every test.
    #[serde(default)]
    pub exact: bool,
    /// Package runs: pass `--lib`.
    #[serde(default)]
    pub lib_only: bool,
    /// #1099 candidate receipt (manager/Epic lead only): the branch or full
    /// commit to verify against `origin/rolling` in a temporary detached
    /// worktree. The typed receipt is `result.receipt`.
    #[serde(default)]
    pub candidate_receipt: Option<String>,
    /// #1337: wall-clock timeout in minutes
    /// ([`JOB_TEST_TIMEOUT_MIN_MINS`]..=[`JOB_TEST_TIMEOUT_MAX_MINS`]). Omitted
    /// means the operator default (`job_test_timeout_mins`), stamped here at
    /// submit; above that default needs the manager, an Epic lead or the bounded
    /// QA shard form with an explicitly delegated live Issue-worker binding. A
    /// `candidate_receipt` run has no default timeout (the unit cap applies).
    /// The budget starts when the unit launches, not at submit: a job held
    /// before launch is bounded separately (`job_admission_timed_out`, #1611).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildCommand {
    Check,
    Build,
}

impl BuildCommand {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Build => "build",
        }
    }
}

/// `cargo check|build` through the cargo build-slot wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildJobParams {
    pub command: BuildCommand,
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub workspace: bool,
    #[serde(default)]
    pub all_targets: bool,
    #[serde(default)]
    pub release: bool,
}

/// `rsi-rolling-land` (kind `landing`) or `scripts/cloud-gate.sh` (kind
/// `cloud_gate`) for one accepted source commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandingJobParams {
    pub accepted: String,
    #[serde(default)]
    pub test_filters: Vec<String>,
}

/// `scripts/cloud-sweep.sh cloud <sha>` (kind `cloud_sweep`): the full QA sweep
/// of one rolling-tip commit on an ephemeral cloud host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudSweepJobParams {
    pub sha: String,
}

/// Validated, typed parameters for one job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobParams {
    Test(TestJobParams),
    Build(BuildJobParams),
    Landing(LandingJobParams),
    CloudGate(LandingJobParams),
    CloudSweep(CloudSweepJobParams),
}

impl JobParams {
    #[must_use]
    pub const fn kind(&self) -> JobKind {
        match self {
            Self::Test(_) => JobKind::Test,
            Self::Build(_) => JobKind::Build,
            Self::Landing(_) => JobKind::Landing,
            Self::CloudGate(_) => JobKind::CloudGate,
            Self::CloudSweep(_) => JobKind::CloudSweep,
        }
    }
}

fn bare_token(value: &str, extra: &[char]) -> bool {
    !value.is_empty()
        && value.len() <= JOB_MAX_TOKEN_BYTES
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || extra.contains(&c))
}

fn full_lower_hex_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `PACKAGE=FILTER`, the lander's `--test-filter` shape.
fn lander_filter(value: &str) -> bool {
    value.split_once('=').is_some_and(|(package, filter)| {
        value.len() <= crate::rolling_queue::ROLLING_QUEUE_MAX_FILTER_BYTES
            && !package.is_empty()
            && !filter.is_empty()
            && !package.starts_with('-')
            && !filter.starts_with('-')
            && !value.chars().any(char::is_control)
    })
}

/// The shard script's own `test(NAME)` pattern.
fn test_filterset(value: &str) -> bool {
    value
        .strip_prefix("test(")
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(|name| {
            !name.is_empty()
                && name.len() <= JOB_MAX_TOKEN_BYTES
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'))
        })
}

/// A branch name, `origin/<branch>` or full commit id for `candidate_receipt`:
/// never an option, never a revision expression.
fn candidate_ref(value: &str) -> bool {
    bare_token(value, &['/', '.'])
        && !value.contains("..")
        && !value.contains("//")
        && !value.ends_with(['/', '.'])
}

impl TestJobParams {
    /// A `scoped_test` run (#1638).
    #[must_use]
    pub const fn is_scoped_test(&self) -> bool {
        self.scoped_test.is_some()
    }

    /// A `candidate_receipt` run: only the appointed manager or an Epic lead
    /// may submit it (it fetches and builds an arbitrary branch).
    #[must_use]
    pub const fn is_candidate_receipt(&self) -> bool {
        self.candidate_receipt.is_some()
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self
            .timeout_minutes
            .is_some_and(|m| !(JOB_TEST_TIMEOUT_MIN_MINS..=JOB_TEST_TIMEOUT_MAX_MINS).contains(&m))
        {
            return Err(JOB_INVALID_PARAMS);
        }
        if let Some(qa) = &self.qa_lane {
            let known_shard = self.shard.as_deref().is_some_and(|shard| {
                matches!(
                    shard,
                    "store-01"
                        | "store-02"
                        | "store-03"
                        | "store-04"
                        | "session-01"
                        | "session-02"
                        | "session-03"
                        | "session-04"
                        | "session-05"
                        | "memory-01"
                        | "memory-02"
                        | "other-01"
                        | "other-02"
                        | "other-03"
                        | "other-04"
                        | "other-05"
                )
            });
            if !known_shard
                || !full_lower_hex_oid(&qa.sha)
                || self.candidate_receipt.is_some()
                || self.recipe.is_some()
                || self
                    .timeout_minutes
                    .is_some_and(|m| m > JOB_QA_LANE_TIMEOUT_MAX_MINS)
            {
                return Err(JOB_INVALID_PARAMS);
            }
        }
        if let Some(scoped) = &self.scoped_test {
            let ok = candidate_ref(&scoped.base)
                && scoped.head.as_deref().is_none_or(candidate_ref)
                && self.recipe.is_none()
                && self.shard.is_none()
                && self.package.is_none()
                && self.candidate_receipt.is_none()
                && self.filterset.is_none()
                && self.filters.is_empty()
                && !self.lib_only
                && !self.exact
                && self.qa_lane.is_none();
            return ok.then_some(()).ok_or(JOB_INVALID_PARAMS);
        }
        if let Some(recipe) = &self.recipe {
            let ok = bare_token(recipe, &['.'])
                && self.shard.is_none()
                && self.package.is_none()
                && self.candidate_receipt.is_none()
                && self.filterset.is_none()
                && self.filters.len() <= JOB_RECIPE_MAX_FILTERS
                && self.filters.iter().all(|f| lander_filter(f))
                && !self.lib_only
                && !self.exact;
            return ok.then_some(()).ok_or(JOB_INVALID_PARAMS);
        }
        if let Some(candidate) = &self.candidate_receipt {
            let ok = self.shard.is_none()
                && self.package.is_none()
                && self.filterset.is_none()
                && self.filters.is_empty()
                && !self.lib_only
                && !self.exact
                && candidate_ref(candidate);
            return ok.then_some(()).ok_or(JOB_INVALID_PARAMS);
        }
        let ok = match (&self.shard, &self.package) {
            (Some(shard), None) => {
                bare_token(shard, &[])
                    && self.filters.is_empty()
                    && !self.lib_only
                    && !self.exact
                    && self.filterset.as_deref().is_none_or(test_filterset)
            }
            (None, Some(package)) => {
                bare_token(package, &['.'])
                    && self.filterset.is_none()
                    && self.filters.len() <= JOB_MAX_TEST_FILTERS
                    && self.filters.iter().all(|f| bare_token(f, &[':', '.']))
            }
            _ => false,
        };
        ok.then_some(()).ok_or(JOB_INVALID_PARAMS)
    }
}

impl BuildJobParams {
    fn validate(&self) -> Result<(), &'static str> {
        let scope = match (&self.package, self.workspace) {
            (Some(package), false) => bare_token(package, &['.']),
            (None, true) => true,
            _ => false,
        };
        scope.then_some(()).ok_or(JOB_INVALID_PARAMS)
    }
}

impl LandingJobParams {
    fn validate(&self) -> Result<(), &'static str> {
        let ok = full_lower_hex_oid(&self.accepted)
            && self.test_filters.len() <= JOB_MAX_TEST_FILTERS
            && self.test_filters.iter().all(|f| lander_filter(f));
        ok.then_some(()).ok_or(JOB_INVALID_PARAMS)
    }
}

impl CloudSweepJobParams {
    fn validate(&self) -> Result<(), &'static str> {
        full_lower_hex_oid(&self.sha)
            .then_some(())
            .ok_or(JOB_INVALID_PARAMS)
    }
}

impl AgentSubmitJobRequestV1 {
    /// Validate the name, replay key and per-kind parameters.
    ///
    /// # Errors
    /// The stable `job_*` code for the first invalid field.
    pub fn typed_params(&self) -> Result<JobParams, &'static str> {
        if let Some(name) = &self.name
            && (name.is_empty()
                || name.len() > JOB_MAX_NAME_BYTES
                || name.chars().any(char::is_control))
        {
            return Err(JOB_NAME_INVALID);
        }
        if let Some(key) = &self.idempotency_key
            && (key.is_empty() || key.len() > JOB_MAX_IDEMPOTENCY_BYTES || key.contains('\0'))
        {
            return Err(JOB_KEY_INVALID);
        }
        let params = if self.params.is_null() {
            serde_json::json!({})
        } else {
            self.params.clone()
        };
        if !params.is_object() {
            return Err(JOB_INVALID_PARAMS);
        }
        let decode_err = |_| JOB_INVALID_PARAMS;
        Ok(match self.kind {
            JobKind::Test => {
                let p: TestJobParams = serde_json::from_value(params).map_err(decode_err)?;
                p.validate()?;
                JobParams::Test(p)
            }
            JobKind::Build => {
                let p: BuildJobParams = serde_json::from_value(params).map_err(decode_err)?;
                p.validate()?;
                JobParams::Build(p)
            }
            JobKind::Landing => {
                let p: LandingJobParams = serde_json::from_value(params).map_err(decode_err)?;
                p.validate()?;
                JobParams::Landing(p)
            }
            JobKind::CloudGate => {
                let p: LandingJobParams = serde_json::from_value(params).map_err(decode_err)?;
                p.validate()?;
                JobParams::CloudGate(p)
            }
            JobKind::CloudSweep => {
                let p: CloudSweepJobParams = serde_json::from_value(params).map_err(decode_err)?;
                p.validate()?;
                JobParams::CloudSweep(p)
            }
        })
    }
}

/// Most failing test names a sweep result lists per class.
pub const SWEEP_MAX_FAILURES: usize = 32;

/// The sweep's overall outcome. A missing or malformed verdict is
/// `Incomplete`, never `Green`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SweepVerdict {
    Green,
    Red,
    Incomplete,
}

impl SweepVerdict {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Green => "GREEN",
            Self::Red => "RED",
            Self::Incomplete => "INCOMPLETE",
        }
    }
}

/// Typed `cloud_sweep` outcome, parsed from the sweep's final
/// `VERDICT <state> <sha>` line and its `QA.md` report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudSweepResultV1 {
    pub verdict: SweepVerdict,
    pub sha: String,
    /// Where the collected report and failure logs are (`~/.rsi/cloud/results/<sha>`).
    pub results_dir: String,
    /// Failing tests the classifier found NEW (at most [`SWEEP_MAX_FAILURES`]).
    #[serde(default)]
    pub new_failures: Vec<String>,
    /// Failing tests matching a known-failure Issue (`#N name`, bounded).
    #[serde(default)]
    pub known_failures: Vec<String>,
}

/// Typed result delivered once to the owner. Absent fields did not apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentJobResultV1 {
    pub exit_code: Option<i32>,
    /// Published tip (`landing` and `cloud_gate` jobs that landed).
    pub landed_sha: Option<String>,
    /// Stable refusal code for a `landing`/`cloud_gate` job that did not land.
    pub refusal: Option<String>,
    #[serde(default)]
    pub failing_tests: Vec<String>,
    /// Bounded tail of the job log.
    pub detail: Option<String>,
    /// `cloud_sweep` jobs: the typed sweep verdict.
    #[serde(default)]
    pub sweep: Option<CloudSweepResultV1>,
    /// `test` jobs with `scoped_test` (#1638): the `SCOPED_TEST_RECEIPT` the
    /// script printed last (`ok`, `exit_code`, `base`, `head`, `filters`,
    /// `log_dir`, per-package `status`/`completed`). `test` jobs with
    /// `candidate_receipt` (#1099): the typed candidate
    /// receipt `scripts/check-touched-shards` produced (base, head, merge
    /// clean, shards compiled, audit verdicts, migrations, suggested filters).
    #[serde(default)]
    pub receipt: Option<serde_json::Value>,
}

/// One job as reported to its owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentJobV1 {
    pub id: Uuid,
    pub kind: JobKind,
    pub name: Option<String>,
    pub state: JobState,
    pub owner_session_id: Uuid,
    pub unit_name: String,
    pub cwd: String,
    pub log_path: String,
    pub params: JobParams,
    pub exit_code: Option<i32>,
    pub result: Option<AgentJobResultV1>,
    pub created_at: String,
    /// When the unit was launched; absent while the job is `queued`. Wall-clock
    /// limits count from here (from `created_at` for a job never held).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// Why a `queued` job has not started: `deploy_draining` while a deploy
    /// drain holds worker starts. Absent once it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<String>,
    /// Per-job completion wake policy chosen at submit.
    #[serde(default)]
    pub wake: JobWake,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSubmitJobReceiptV1 {
    pub job: AgentJobV1,
    /// True when an identical idempotent retry returned the original job.
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCancelJobResultV1 {
    /// The job after the call: `failed` with refusal `job_cancelled` when this
    /// call stopped it, or its existing terminal state otherwise.
    pub job: AgentJobV1,
    /// True when this call stopped a running job; false when it had already
    /// settled (a repeat cancel changes nothing).
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentListJobsResultV1 {
    pub jobs: Vec<AgentJobV1>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(kind: JobKind, params: serde_json::Value) -> AgentSubmitJobRequestV1 {
        AgentSubmitJobRequestV1 {
            project_id: None,
            sandbox_session_id: None,
            kind,
            params,
            name: None,
            idempotency_key: None,
            worktree: None,
            wake: None,
        }
    }

    #[test]
    fn qa_lane_wrong_kind_never_grants_a_timeout_path() {
        for (kind, mut params) in [
            (JobKind::Build, json!({"command":"check","workspace":true})),
            (JobKind::Landing, json!({"accepted":"a".repeat(40)})),
            (JobKind::CloudGate, json!({"accepted":"a".repeat(40)})),
            (JobKind::CloudSweep, json!({"sha":"a".repeat(40)})),
        ] {
            params["qa_lane"] = json!({"sha":"a".repeat(40)});
            assert_eq!(
                request(kind, params).typed_params(),
                Err(JOB_INVALID_PARAMS)
            );
        }
    }

    #[test]
    fn qa_lane_over_ceiling_is_refused_even_with_a_valid_pin() {
        for minutes in [91, 180] {
            assert_eq!(
                request(
                    JobKind::Test,
                    json!({"shard":"store-01",
                "qa_lane":{"sha":"a".repeat(40)}, "timeout_minutes":minutes})
                )
                .typed_params(),
                Err(JOB_INVALID_PARAMS)
            );
        }
    }

    #[test]
    fn qa_lane_wire_accepts_only_pinned_bounded_shards() {
        let value = json!({"shard":"store-01", "qa_lane":{"sha":"a".repeat(40)},
                           "timeout_minutes":90, "filterset":"test(store::tests)"});
        let parsed = request(JobKind::Test, value.clone())
            .typed_params()
            .expect("QA shard");
        let encoded = serde_json::to_value(&parsed).unwrap();
        assert_eq!(
            serde_json::from_value::<JobParams>(encoded).unwrap(),
            parsed
        );
        for patch in [
            json!({"timeout_minutes":91}),
            json!({"shard":"not-a-shard"}),
            json!({"qa_lane":{"sha":"main"}}),
            json!({"qa_lane":{"sha":"A".repeat(40)}}),
            json!({"qa_lane":{"sha":"a".repeat(40),"command":"anything"}}),
            json!({"filterset":"test(a) | test(b)"}),
            json!({"package":"rsid"}),
            json!({"candidate_receipt":"main"}),
            json!({"recipe":"e2e"}),
        ] {
            let mut invalid = value.clone();
            invalid
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            assert_eq!(
                request(JobKind::Test, invalid).typed_params(),
                Err(JOB_INVALID_PARAMS)
            );
        }
        assert_eq!(
            request(
                JobKind::Test,
                json!({"recipe":"e2e","qa_lane":{"sha":"a".repeat(40)},"timeout_minutes":90})
            )
            .typed_params(),
            Err(JOB_INVALID_PARAMS)
        );
        assert_eq!(
            request(
                JobKind::Test,
                json!({"package":"rsid", "qa_lane":{"sha":"a".repeat(40)}})
            )
            .typed_params(),
            Err(JOB_INVALID_PARAMS)
        );
        assert_eq!(
            request(
                JobKind::Build,
                json!({"command":"check", "workspace":true, "qa_lane":{"sha":"a".repeat(40)}})
            )
            .typed_params(),
            Err(JOB_INVALID_PARAMS)
        );
    }

    /// #1638: the scoped-test form takes two bare refs and nothing else.
    #[test]
    fn scoped_test_jobs_take_only_bare_refs() {
        let ok = |params| request(JobKind::Test, params).typed_params().is_ok();
        assert!(ok(json!({"scoped_test":{"base":"origin/rolling"}})));
        assert!(ok(
            json!({"scoped_test":{"base":"origin/rolling","head":"HEAD"},"timeout_minutes":30})
        ));
        let valid = request(
            JobKind::Test,
            json!({"scoped_test":{"base":"rolling","head":"a".repeat(40)}}),
        )
        .typed_params()
        .unwrap();
        assert!(matches!(&valid, JobParams::Test(p) if p.is_scoped_test()));
        assert_eq!(
            serde_json::from_value::<JobParams>(serde_json::to_value(&valid).unwrap()).unwrap(),
            valid
        );
        for bad in [
            json!({"scoped_test":{}}),
            json!({"scoped_test":{"base":""}}),
            json!({"scoped_test":{"base":"--dry-run"}}),
            json!({"scoped_test":{"base":"a..b"}}),
            json!({"scoped_test":{"base":"x y"}}),
            json!({"scoped_test":{"base":"x;id"}}),
            json!({"scoped_test":{"base":"origin/rolling","head":"-x"}}),
            json!({"scoped_test":{"base":"origin/rolling","head":"HEAD~1"}}),
            json!({"scoped_test":{"base":"origin/rolling","args":["--filter","x"]}}),
            json!({"scoped_test":{"base":"origin/rolling"},"args":["--tmpfs"]}),
            json!({"scoped_test":{"base":"origin/rolling"},"filters":["rsid=x"]}),
            json!({"scoped_test":{"base":"origin/rolling"},"recipe":"scoped-test"}),
            json!({"scoped_test":{"base":"origin/rolling"},"package":"rsid"}),
            json!({"scoped_test":{"base":"origin/rolling"},"shard":"other-01"}),
            json!({"scoped_test":{"base":"origin/rolling"},"candidate_receipt":"rolling"}),
            json!({"scoped_test":{"base":"origin/rolling"},"lib_only":true}),
            json!({"scoped_test":{"base":"origin/rolling"},"exact":true}),
            json!({"scoped_test":{"base":"origin/rolling"},"qa_lane":{"sha":"a".repeat(40)}}),
            json!({"scoped_test":{"base":"origin/rolling"},"timeout_minutes":4}),
        ] {
            assert!(!ok(bad.clone()), "{bad}");
        }
    }

    #[test]
    fn recipe_jobs_select_one_declared_gate_without_arguments() {
        let valid = request(
            JobKind::Test,
            json!({"recipe":"check-cpu","timeout_minutes":10}),
        )
        .typed_params()
        .unwrap();
        assert!(matches!(&valid, JobParams::Test(p) if p.recipe.as_deref() == Some("check-cpu")));
        assert_eq!(
            serde_json::from_value::<JobParams>(serde_json::to_value(&valid).unwrap()).unwrap(),
            valid
        );
        for extra in [
            json!({"package":"rsid"}),
            json!({"shard":"other-01"}),
            json!({"candidate_receipt":"rolling"}),
            json!({"filterset":"test(x)"}),
            json!({"filters":["x"]}),
            json!({"lib_only":true}),
            json!({"argv":["sh"]}),
            json!({"command":"id"}),
            json!({"env":{"X":"Y"}}),
        ] {
            let mut value = json!({"recipe":"check-cpu"});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert_eq!(
                request(JobKind::Test, value).typed_params().unwrap_err(),
                JOB_INVALID_PARAMS
            );
        }
        for recipe in ["", "--help", "../gate", "a b", "x;id", "X=Y"] {
            assert_eq!(
                request(JobKind::Test, json!({"recipe":recipe}))
                    .typed_params()
                    .unwrap_err(),
                JOB_INVALID_PARAMS
            );
        }
    }

    /// #1584: a recipe run may carry bounded focused `PACKAGE=FILTER` filters.
    #[test]
    fn recipe_jobs_accept_bounded_focused_lander_filters() {
        let focused = json!({
            "recipe":"scoped-test",
            "filters":["rsid-store=shard:store-01:test(store_open_)", "rsi-common=agent_jobs"]
        });
        let typed = request(JobKind::Test, focused).typed_params().unwrap();
        assert!(matches!(&typed, JobParams::Test(p) if p.filters.len() == 2 && p.recipe.is_some()));
        assert_eq!(
            serde_json::from_value::<JobParams>(serde_json::to_value(&typed).unwrap()).unwrap(),
            typed
        );
        let many = vec!["rsid=agent_jobs"; JOB_RECIPE_MAX_FILTERS];
        assert!(
            request(
                JobKind::Test,
                json!({"recipe":"scoped-test","filters":many})
            )
            .typed_params()
            .is_ok()
        );
        let too_many = vec!["rsid=agent_jobs"; JOB_RECIPE_MAX_FILTERS + 1];
        let too_long = format!("rsid={}", "a".repeat(300));
        let bad = [
            json!(too_many),
            json!(["rsid"]),
            json!(["=x"]),
            json!(["rsid="]),
            json!(["--x=y"]),
            json!(["rsid=--list"]),
            json!(["rsid=a\nb"]),
            json!([too_long]),
        ];
        for filters in bad {
            assert_eq!(
                request(
                    JobKind::Test,
                    json!({"recipe":"scoped-test","filters":filters})
                )
                .typed_params()
                .unwrap_err(),
                JOB_INVALID_PARAMS,
                "{filters}"
            );
        }
    }

    #[test]
    fn package_test_exact_matching_is_typed_optional_and_round_trips() {
        for (extra, exact) in [
            (json!({}), false),
            (json!({"exact":false}), false),
            (json!({"exact":true}), true),
        ] {
            for filters in [
                json!([]),
                json!(["module::tests::one", "module::tests::two"]),
            ] {
                let mut value = json!({"package":"rsi", "lib_only":true, "filters":filters});
                value
                    .as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                let parsed = request(JobKind::Test, value).typed_params().unwrap();
                assert!(matches!(&parsed, JobParams::Test(p) if p.exact == exact));
                assert_eq!(
                    serde_json::from_value::<JobParams>(serde_json::to_value(&parsed).unwrap())
                        .unwrap(),
                    parsed
                );
            }
        }
        let old: JobParams = serde_json::from_value(
            json!({"kind":"test","package":"rsi","filters":["module::tests::one"]}),
        )
        .unwrap();
        assert!(matches!(old, JobParams::Test(p) if !p.exact));
    }

    #[test]
    fn package_test_exact_matching_rejects_wrong_types_and_other_test_forms() {
        for exact in [json!("true"), json!(1), json!(null), json!([]), json!({})] {
            assert_eq!(
                request(JobKind::Test, json!({"package":"rsi","exact":exact})).typed_params(),
                Err(JOB_INVALID_PARAMS)
            );
        }
        for mut value in [
            json!({"shard":"other-01","filterset":"test(one)"}),
            json!({"shard":"other-01","qa_lane":{"sha":"a".repeat(40)}}),
            json!({"recipe":"scoped-test"}),
            json!({"candidate_receipt":"rolling"}),
        ] {
            value["exact"] = json!(true);
            assert_eq!(
                request(JobKind::Test, value.clone()).typed_params(),
                Err(JOB_INVALID_PARAMS)
            );
            value["exact"] = json!(false);
            assert!(request(JobKind::Test, value).typed_params().is_ok());
        }
        for filters in [json!(["--exact"]), json!(["module::tests::one", "--exact"])] {
            assert_eq!(
                request(
                    JobKind::Test,
                    json!({"package":"rsi","filters":filters,"exact":true})
                )
                .typed_params(),
                Err(JOB_INVALID_PARAMS)
            );
        }
    }

    /// #1337: `timeout_minutes` is a bounded test-job field and survives a
    /// store round trip; old rows without it still decode.
    #[test]
    fn test_job_timeout_minutes_is_bounded_and_optional() {
        for minutes in [JOB_TEST_TIMEOUT_MIN_MINS, 20, JOB_TEST_TIMEOUT_MAX_MINS] {
            let params = request(
                JobKind::Test,
                json!({"package":"rsid","timeout_minutes":minutes}),
            )
            .typed_params()
            .unwrap();
            let JobParams::Test(test) = &params else {
                panic!("test params");
            };
            assert_eq!(test.timeout_minutes, Some(minutes));
            let stored = serde_json::to_value(&params).unwrap();
            assert_eq!(serde_json::from_value::<JobParams>(stored).unwrap(), params);
        }
        for bad in [
            0,
            JOB_TEST_TIMEOUT_MIN_MINS - 1,
            JOB_TEST_TIMEOUT_MAX_MINS + 1,
        ] {
            assert_eq!(
                request(
                    JobKind::Test,
                    json!({"package":"rsid","timeout_minutes":bad})
                )
                .typed_params()
                .unwrap_err(),
                JOB_INVALID_PARAMS,
                "{bad}"
            );
        }
        let old: JobParams =
            serde_json::from_value(json!({"kind":"test","package":"rsid","filters":[]})).unwrap();
        assert!(matches!(
            old,
            JobParams::Test(TestJobParams {
                timeout_minutes: None,
                ..
            })
        ));
        // Build jobs take no timeout field.
        assert!(
            request(
                JobKind::Build,
                json!({"command":"check","workspace":true,"timeout_minutes":20})
            )
            .typed_params()
            .is_err()
        );
    }

    #[test]
    fn typed_params_accept_the_five_kinds() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        assert!(matches!(
            request(
                JobKind::Test,
                json!({"package":"rsid","filters":["rolling_queue::tests"],"lib_only":true})
            )
            .typed_params(),
            Ok(JobParams::Test(_))
        ));
        // #1106: a package run needs no filters; it runs every test.
        for all in [
            json!({"package":"rsid"}),
            json!({"package":"rsid","filters":[]}),
            json!({"package":"rsid","lib_only":true}),
        ] {
            assert!(
                matches!(
                    request(JobKind::Test, all.clone()).typed_params(),
                    Ok(JobParams::Test(_))
                ),
                "{all}"
            );
        }
        assert!(matches!(
            request(
                JobKind::Test,
                json!({"shard":"other-01","filterset":"test(foo_bar)"})
            )
            .typed_params(),
            Ok(JobParams::Test(_))
        ));
        assert!(matches!(
            request(
                JobKind::Build,
                json!({"command":"check","workspace":true,"all_targets":true})
            )
            .typed_params(),
            Ok(JobParams::Build(_))
        ));
        assert!(matches!(
            request(
                JobKind::Landing,
                json!({"accepted":oid,"test_filters":["rsid=rolling_queue"]})
            )
            .typed_params(),
            Ok(JobParams::Landing(_))
        ));
        assert!(matches!(
            request(JobKind::CloudGate, json!({"accepted":oid})).typed_params(),
            Ok(JobParams::CloudGate(_))
        ));
        assert!(matches!(
            request(JobKind::CloudSweep, json!({"sha":oid})).typed_params(),
            Ok(JobParams::CloudSweep(_))
        ));
    }

    #[test]
    fn landing_filters_round_trip_every_touched_shard_atom_shape() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let filters = [
            "rsi-common=agent_jobs",
            "rsi-common=test(=agent_jobs)",
            "rsi-common=test(/^agent_jobs::[A-Za-z0-9_]+$/)",
            "rsid=shard:store-01:agent_jobs",
            "rsid=shard:store-01:test(=agent_jobs)",
            "rsid=shard:store-01:test(/^agent_jobs::[A-Za-z0-9_]+$/)",
        ];

        for filter in filters {
            let params = request(
                JobKind::Landing,
                json!({"accepted":oid,"test_filters":[filter]}),
            )
            .typed_params()
            .unwrap_or_else(|error| panic!("{filter}: {error}"));
            let encoded = serde_json::to_value(&params).unwrap();
            let decoded: JobParams = serde_json::from_value(encoded.clone()).unwrap();
            assert_eq!(decoded, params, "{filter}");
            assert_eq!(encoded["test_filters"][0], filter, "{filter}");
        }
    }

    #[test]
    fn typed_params_refuse_anything_that_could_smuggle_a_command() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let bad = [
            request(
                JobKind::Test,
                json!({"package":"rsid; rm -rf /","filters":["a"]}),
            ),
            request(
                JobKind::Test,
                json!({"package":"rsid","filters":["--nocapture"]}),
            ),
            request(JobKind::Test, json!({"package":"rsid","filters":["a b"]})),
            request(
                JobKind::Test,
                json!({"shard":"x","package":"rsid","filters":["a"]}),
            ),
            request(
                JobKind::Test,
                json!({"shard":"x","filterset":"test(a) | test(b)"}),
            ),
            request(JobKind::Test, json!({"command":"rm -rf /"})),
            request(JobKind::Build, json!({"command":"check"})),
            request(
                JobKind::Build,
                json!({"command":"check","package":"a","workspace":true}),
            ),
            request(
                JobKind::Build,
                json!({"command":"install","workspace":true}),
            ),
            request(JobKind::Landing, json!({"accepted":"HEAD"})),
            request(
                JobKind::Landing,
                json!({"accepted":oid,"test_filters":["--repo=x"]}),
            ),
            request(
                JobKind::Landing,
                json!({"accepted":oid,"test_filters":["rsi-common=test(name\n)"]}),
            ),
            request(JobKind::CloudGate, json!({"accepted":oid,"argv":["sh"]})),
            request(JobKind::Landing, json!("sh -c id")),
            request(JobKind::CloudSweep, json!({"sha":"HEAD"})),
            request(JobKind::CloudSweep, json!({"sha":oid.to_uppercase()})),
            request(JobKind::CloudSweep, json!({"sha":oid,"argv":["sh"]})),
            request(JobKind::CloudSweep, json!({"accepted":oid})),
            request(JobKind::CloudSweep, json!({})),
        ];
        for request in bad {
            assert_eq!(request.typed_params().unwrap_err(), JOB_INVALID_PARAMS);
        }
    }

    #[test]
    fn name_and_key_are_bounded() {
        let mut ok = request(JobKind::Build, json!({"command":"check","workspace":true}));
        ok.name = Some("x".repeat(JOB_MAX_NAME_BYTES + 1));
        assert_eq!(ok.typed_params().unwrap_err(), JOB_NAME_INVALID);
        ok.name = Some("check".into());
        ok.idempotency_key = Some(String::new());
        assert_eq!(ok.typed_params().unwrap_err(), JOB_KEY_INVALID);
    }

    #[test]
    fn a_candidate_receipt_takes_only_a_bare_ref() {
        let ok = |value: serde_json::Value| request(JobKind::Test, value).typed_params().is_ok();
        for good in [
            "rsi/8e8b947d-4dcd",
            "origin/rolling",
            "a".repeat(40).as_str(),
        ] {
            assert!(ok(json!({"candidate_receipt": good})), "{good}");
        }
        for bad in [
            "",
            "--upload-pack=x",
            "a..b",
            "a//b",
            "ref/",
            "sh -c id",
            "a;b",
            "HEAD~1",
        ] {
            assert!(!ok(json!({"candidate_receipt": bad})), "{bad}");
        }
        // It takes no other test selector.
        assert!(!ok(json!({"candidate_receipt":"x","package":"rsid"})));
        assert!(!ok(json!({"candidate_receipt":"x","shard":"other-01"})));
        assert!(!ok(json!({"candidate_receipt":"x","lib_only":true})));
    }
}
