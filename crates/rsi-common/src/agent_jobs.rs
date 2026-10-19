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
/// #2692: most shards (or features) one `test_no_run` job builds.
pub const JOB_MAX_SHARD_SWEEP: usize = 32;
pub const JOB_MAX_TOKEN_BYTES: usize = 200;
/// Longest shard `filterset`: up to [`JOB_MAX_TEST_FILTERS`] `|`-joined atoms.
pub const JOB_MAX_FILTERSET_BYTES: usize = 1024;
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
/// Scoped-test recipes are retired; submit `params.scoped_test` instead.
pub const JOB_RECIPE_USE_SCOPED_TEST: &str = "job_recipe_use_scoped_test";
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
/// Hub #3024: the `scoped_test` tool copy that would run (the rolling tip's, or the
/// candidate's own) loads a helper under `scripts/` that its tree does not contain.
/// Refused up front with the file named, never run with another copy of the tool.
pub const JOB_SCOPED_TEST_TOOL_HELPER_MISSING: &str = "scoped_test_tool_helper_missing";
/// The OS or requested workflow has no supported durable backend.
pub const JOB_PLATFORM_UNSUPPORTED: &str = "job_platform_unsupported";
/// #2823: the daemon's PATH resolves a `rustc` other than the version the
/// repository's `rust-toolchain.toml` pins (or none at all); refused before any build.
pub const JOB_TOOLCHAIN_MISMATCH: &str = "toolchain_mismatch";
pub const JOB_KEY_CONFLICT: &str = "job_idempotency_key_conflict";
/// `AgentCancelJob` (#1106): the refusal recorded on a job its owner stopped.
/// The job settles `failed` with this refusal, the same typed route the
/// scratch-quota stop uses, so no new state or migration is needed.
pub const JOB_CANCELLED: &str = "job_cancelled";
/// #2048: a staged `interrupt_workers` deploy stopped a running `test` job
/// (`scoped_test` included) once its grace passed. The job settles `failed` with
/// this refusal and the deploy's id in `deploy_id`; the owner's job-terminal
/// wake fires as usual so it can resubmit once on the new daemon.
pub const JOB_INTERRUPTED_BY_DEPLOY: &str = "interrupted_by_deploy";
/// #2190: the `held` reason of a `queued` `test`/`build` job that waits for a
/// hub deploy to settle. While a staged `interrupt_workers` deploy would stop
/// the job after its grace, a new job is accepted durably but not started: no
/// unit, no timeout clock. It starts, in submission order, once the deploy
/// settles, is abandoned or reaches its deadline.
pub const JOB_HELD_FOR_DEPLOY: &str = "held_for_deploy";
/// #2524: the `held` reason of a `queued` build or test job that waits for a
/// host at or above its thermal ceiling (`job_thermal_ceiling_c`) to cool. No
/// unit and no timeout clock; it starts, in submission order, once the host is
/// below the ceiling.
pub const JOB_HELD_FOR_THERMAL: &str = "held_for_thermal";
/// #2937: the `held` reason of a `queued` build or test job on a host with no
/// cargo-slot governor (macOS) while the platform build slots are all in use or
/// available memory is below the governor's floor. No unit; it starts, in
/// submission order, once capacity frees.
pub const JOB_HELD_FOR_CAPACITY: &str = "held_for_capacity";
/// #1337: a `test` job ran past its wall-clock timeout. The daemon stopped its
/// unit and settled it `failed` with this refusal (the cancel route: no new
/// state or migration).
pub const JOB_TIMED_OUT: &str = "job_timed_out";
/// Hub #2954: a `build` job must compile exactly one commit. The worktree HEAD
/// moved (or a clean tree became dirty) between the daemon recording it at
/// submit and the slot grant, a poll while it ran, or the finish; the daemon
/// stopped the unit and settled it `failed` with this refusal, naming both SHAs.
pub const JOB_SOURCE_MOVED: &str = "job_source_moved";
/// Hub #2954: `params.sha` on a `build` job named a commit other than the
/// worktree HEAD at submit; refused up front, nothing recorded or launched.
pub const JOB_SOURCE_SHA_MISMATCH: &str = "job_source_sha_mismatch";
/// Hub #3025: the daemon could not write the submit-time source record of a
/// `build` job (the `<id>.source` sidecar), so the job could not be held to one
/// commit. Refused before the job row exists or any unit launches; a build is
/// never accepted and run unenforced.
pub const JOB_SOURCE_RECORD_FAILED: &str = "job_source_record_failed";
/// #1832: a `test` job's unit never wrote its `job-start` line before the
/// separate start admission limit, so the command never ran (systemd delay or a
/// wedged launch). Settled `failed` like [`JOB_TIMED_OUT`] but named a start
/// delay with the unit's state, never a test timeout.
pub const JOB_START_TIMED_OUT: &str = "job_start_timed_out";
/// #1611: a job held before its unit launched (queued behind a deploy drain)
/// waited past the admission cap (the drain hold cap plus a margin) and was
/// settled `failed` with this refusal. Distinct from [`JOB_TIMED_OUT`]: the
/// execution budget (`timeout_minutes`) starts at `job-start` and excludes
/// build admission. Build admission exceeding its separate cap uses this
/// refusal too.
pub const JOB_ADMISSION_TIMED_OUT: &str = "job_admission_timed_out";
/// #192: an ordinary package `test` job with `filters` exited 0 but its whole
/// log proves libtest ran no test (every test binary printed a summary and the
/// summaries total zero passed and zero failed). Settled `failed` with the
/// observed `exit_code` 0 kept: a mistyped `exact` name must not read as green.
pub const JOB_FILTER_MATCHED_NO_TESTS: &str = "job_filter_matched_no_tests";
/// #1337: `timeout_minutes` above the operator default needs the appointed
/// manager or an Epic lead, or the bounded live Issue-worker QA shard form.
pub const JOB_TIMEOUT_NOT_AUTHORIZED: &str = "job_timeout_not_authorized";
/// #1337: the operator setting `job_test_timeout_mins` (default and bounds).
/// A `test` job without `timeout_minutes` is stopped after the default; a
/// manager may raise one job up to the maximum (the unit's own cap).
pub const JOB_TEST_TIMEOUT_DEFAULT_MINS: u32 = 20;
pub const JOB_TEST_TIMEOUT_MIN_MINS: u32 = 5;
pub const JOB_TEST_TIMEOUT_MAX_MINS: u32 = 180;
/// #161: bounds of the optional per-job `cpu_quota_percent` (percent of one CPU)
/// a package test, shard, `scoped_test` or build job may request. The request
/// only lowers the budget the daemon derives (host ceiling, hub build share,
/// operator settings); it never raises it. The ceiling matches a declared
/// recipe's `cpu_quota_percent` bound.
pub const JOB_CPU_QUOTA_MIN_PERCENT: u32 = 100;
pub const JOB_CPU_QUOTA_MAX_PERCENT: u32 = 1600;
/// #1979: the default timeout of a `scoped_test` job that names none. The
/// tool budgets `--runtime-max-sec` per package (1800 s by default) and the job
/// derives both budgets from its timeout, so the 20-minute test default killed a
/// plan of two 25-minute packages. The effective default is the larger of this
/// and `job_test_timeout_mins`, capped at [`JOB_TEST_TIMEOUT_MAX_MINS`]; other
/// test jobs keep `job_test_timeout_mins`.
pub const JOB_SCOPED_TEST_TIMEOUT_DEFAULT_MINS: u32 = 60;
/// #2089: the recipe that measures every `rsid` and `rsid-store` library test
/// in one nextest JUnit run (`make test-timings`). The first attempt reached
/// 3256 of 7646 tests in the 20 minutes after a 3m20s build and was killed
/// before any report was written, and a worker cannot raise the timeout itself.
/// The daemon, not the worker-editable manifest, owns this default: a job that
/// names no timeout gets the larger of [`JOB_TEST_TIMINGS_TIMEOUT_DEFAULT_MINS`]
/// and `job_test_timeout_mins` (capped at [`JOB_TEST_TIMEOUT_MAX_MINS`]); the
/// manifest declaration can only lower it, and only a larger explicit timeout
/// needs authority.
pub const JOB_TEST_TIMINGS_RECIPE: &str = "test-timings";
pub const JOB_TEST_TIMINGS_TIMEOUT_DEFAULT_MINS: u32 = 90;
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

/// Job lifecycle. `Queued` is accepted but held before unit launch, by a
/// deploy drain or an earlier local peer (the `held` field names why).
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
    ///
    /// #2313: canonical wire name `source_session_id` (the name the landing
    /// verbs use); `sandbox_session_id` is a deprecated alias accepted for
    /// one release.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "source_session_id",
        alias = "sandbox_session_id"
    )]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Epic lead), at most 90 minutes and two outstanding QA lanes per owner (queued or running).
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
    /// project gate can use in place of its derived selection (#1584).
    #[serde(default)]
    pub filters: Vec<String>,
    /// Package runs: match whole test names with libtest `--exact`.
    /// Defaults to substring matching; no filters still runs every test.
    #[serde(default)]
    pub exact: bool,
    /// Package runs: pass `--lib`.
    #[serde(default)]
    pub lib_only: bool,
    /// Package runs: select one integration test with `--test <name>`.
    /// Requires nonempty exact filters; exclusive with `lib_only`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_target: Option<String>,
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
    /// The budget starts at the wrapper's `job-start`, excluding build slot and
    /// sandbox/artifact lock waits. Launch/build admission is bounded separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u32>,
    /// #161: lower this job's CPU budget (percent of one CPU, 100..=1600) before
    /// its unit starts. The daemon takes the smaller of this and its derived
    /// ceiling for the unit's `CPUQuota`, the scoped-test `--cpu-quota` and the
    /// build/test thread counts, so it can only reduce, never raise, the
    /// operator-controlled budget. Omitted keeps the derived budget. Refused for
    /// a declared `recipe` (its manifest owns the cap) and `candidate_receipt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_quota_percent: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildCommand {
    Check,
    Build,
    /// #2692: compile a package's lib test harness (`cargo test --lib
    /// --no-run`) for explicit features or a sweep of shard features, through
    /// `scripts/shard-feature-sweep`.
    TestNoRun,
}

impl BuildCommand {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Build => "build",
            Self::TestNoRun => "test_no_run",
        }
    }
}

/// `cargo check|build` through the cargo build-slot wrapper, or (`test_no_run`,
/// #2692) the shard-feature test-harness sweep.
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
    /// `test_no_run` only: pass `--no-default-features`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_default_features: bool,
    /// `test_no_run` only: one build with these `test-shard-*` features enabled
    /// together. Mutually exclusive with `shards`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// `test_no_run` only: shard names (`store-01` is feature
    /// `test-shard-store-01`) built one after another, each with
    /// `--no-default-features`, cleaning the package between shards so the
    /// target holds one shard's harness at a time. The receipt carries one
    /// entry per shard (exit code, warnings). Mutually exclusive with
    /// `features`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shards: Vec<String>,
    /// #161: lower this build's CPU budget (percent of one CPU, 100..=1600)
    /// before its unit starts; see [`TestJobParams::cpu_quota_percent`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_quota_percent: Option<u32>,
    /// Hub #2954: pin the commit this build must compile (40 lowercase hex).
    /// Submit is refused `job_source_sha_mismatch` when the job worktree HEAD
    /// differs. Omitted: the HEAD at submit is still recorded and enforced, the
    /// pin only lets the caller state which commit it means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

/// #161: whether a requested per-job CPU budget is inside the accepted bounds.
#[must_use]
pub const fn cpu_quota_percent_in_bounds(percent: u32) -> bool {
    percent >= JOB_CPU_QUOTA_MIN_PERCENT && percent <= JOB_CPU_QUOTA_MAX_PERCENT
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

/// The shard script's `--filterset`: one or more nextest atoms `test(NAME)`,
/// `test(=NAME)` or `test(/REGEX/)` joined by `|` (#1605), the shapes the
/// landing gate and `check-touched-shards` emit. Never a general nextest
/// expression: only these atoms and `|`.
fn test_filterset(value: &str) -> bool {
    value.len() <= JOB_MAX_FILTERSET_BYTES && filterset_terms_ok(value, JOB_MAX_TEST_FILTERS)
}

/// The inner text of one atom: `NAME`, `=NAME` or `/REGEX/`.
fn filterset_atom_ok(inner: &str) -> bool {
    if let Some(regex) = inner.strip_prefix('/').and_then(|r| r.strip_suffix('/')) {
        return !regex.is_empty() && inner.bytes().all(|b| (0x20..=0x7e).contains(&b));
    }
    let name = inner.strip_prefix('=').unwrap_or(inner);
    !name.is_empty()
        && name.len() <= JOB_MAX_TOKEN_BYTES
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'))
}

/// `test(ATOM)` then, optionally, `|` and more terms. A regex atom may itself
/// hold `)`, so the first `)` whose atom is valid and which is followed by the
/// end or by `| test(` closes it; there is no backtracking.
fn filterset_terms_ok(rest: &str, terms_left: usize) -> bool {
    if terms_left == 0 {
        return false;
    }
    let Some(body) = rest.strip_prefix("test(") else {
        return false;
    };
    for (end, _) in body.match_indices(')') {
        if !filterset_atom_ok(&body[..end]) {
            continue;
        }
        let tail = &body[end + 1..];
        if tail.is_empty() {
            return true;
        }
        let Some(next) = tail.trim_start_matches(' ').strip_prefix('|') else {
            continue;
        };
        let next = next.trim_start_matches(' ');
        if next.starts_with("test(") {
            return filterset_terms_ok(next, terms_left - 1);
        }
    }
    false
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

    /// The full-suite timing measurement recipe (#2089).
    #[must_use]
    pub fn is_test_timings(&self) -> bool {
        self.recipe.as_deref() == Some(JOB_TEST_TIMINGS_RECIPE)
    }

    /// A `candidate_receipt` run: only the appointed manager or an Epic lead
    /// may submit it (it fetches and builds an arbitrary branch).
    #[must_use]
    pub const fn is_candidate_receipt(&self) -> bool {
        self.candidate_receipt.is_some()
    }

    fn validate(&self) -> Result<(), &'static str> {
        if let Some(target) = &self.test_target
            && (!bare_token(target, &[])
                || self.package.is_none()
                || self.shard.is_some()
                || self.recipe.is_some()
                || self.scoped_test.is_some()
                || self.candidate_receipt.is_some()
                || self.lib_only
                || !self.exact
                || self.filters.is_empty())
        {
            return Err(JOB_INVALID_PARAMS);
        }
        let cpu_ok = match self.cpu_quota_percent {
            None => true,
            Some(percent) => {
                cpu_quota_percent_in_bounds(percent)
                    && self.recipe.is_none()
                    && self.candidate_receipt.is_none()
            }
        };
        if !cpu_ok {
            return Err(JOB_INVALID_PARAMS);
        }
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
        if self
            .cpu_quota_percent
            .is_some_and(|percent| !cpu_quota_percent_in_bounds(percent))
        {
            return Err(JOB_INVALID_PARAMS);
        }
        if self
            .sha
            .as_deref()
            .is_some_and(|sha| !full_lower_hex_oid(sha))
        {
            return Err(JOB_INVALID_PARAMS);
        }
        if self.command == BuildCommand::TestNoRun {
            return self.validate_test_no_run();
        }
        if self.no_default_features || !self.features.is_empty() || !self.shards.is_empty() {
            return Err(JOB_INVALID_PARAMS);
        }
        let scope = match (&self.package, self.workspace) {
            (Some(package), false) => bare_token(package, &['.']),
            (None, true) => true,
            _ => false,
        };
        scope.then_some(()).ok_or(JOB_INVALID_PARAMS)
    }
}

impl BuildJobParams {
    /// #2692: `test_no_run` names one package and either one feature set
    /// (`features`) or a shard sweep (`shards`), never both; features are
    /// `test-shard-*` names so the job cannot enable an arbitrary feature.
    fn validate_test_no_run(&self) -> Result<(), &'static str> {
        let package_ok = !self.workspace
            && !self.all_targets
            && !self.release
            && self
                .package
                .as_deref()
                .is_some_and(|package| bare_token(package, &['.']));
        let features_ok = self.features.len() <= JOB_MAX_SHARD_SWEEP
            && self.features.iter().all(|f| shard_feature(f));
        let mut seen = std::collections::BTreeSet::new();
        let shards_ok = self.shards.len() <= JOB_MAX_SHARD_SWEEP
            && self
                .shards
                .iter()
                .all(|shard| bare_token(shard, &[]) && seen.insert(shard.as_str()));
        let exclusive = self.features.is_empty() || self.shards.is_empty();
        (package_ok && features_ok && shards_ok && exclusive)
            .then_some(())
            .ok_or(JOB_INVALID_PARAMS)
    }
}

/// A `test-shard-NAME` feature (no `package/feature` form).
fn shard_feature(value: &str) -> bool {
    value
        .strip_prefix("test-shard-")
        .is_some_and(|name| bare_token(name, &[]))
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

impl AgentSubmitJobRequestV1 {
    /// The refusal text for a `typed_params` `code`: the stable code, then (for
    /// an over-size filter) the field path, the offending filter and the limit.
    #[must_use]
    pub fn refusal_detail(&self, code: &str) -> String {
        if code != JOB_INVALID_PARAMS {
            return code.to_string();
        }
        let lander = self.kind != JobKind::Test || self.params.get("recipe").is_some();
        let limit = if lander {
            crate::rolling_queue::ROLLING_QUEUE_MAX_FILTER_BYTES
        } else {
            JOB_MAX_TOKEN_BYTES
        };
        if let Some(filterset) = self.params.get("filterset").and_then(|v| v.as_str())
            && !test_filterset(filterset)
        {
            return format!(
                "{code}: params.filterset must be one or more nextest atoms `test(NAME)`, \
                 `test(=NAME)` or `test(/REGEX/)` joined by ` | ` (at most {JOB_MAX_TEST_FILTERS} \
                 atoms, {JOB_MAX_FILTERSET_BYTES} bytes); other nextest expressions are not accepted"
            );
        }
        for field in ["filters", "test_filters"] {
            let Some(items) = self.params.get(field).and_then(|v| v.as_array()) else {
                continue;
            };
            let filters: Vec<String> = items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if let Some(detail) = crate::rolling_queue::oversize_filter_detail_at(
                &format!("params.{field}"),
                &filters,
                limit,
            ) {
                return format!("{code}: {detail}");
            }
        }
        code.to_string()
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

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero_u32(value: &u32) -> bool {
    *value == 0
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
    /// Failures the run had that `failing_tests` does not name: names past its
    /// bound, and failures the nextest summary counts that no status line in
    /// the log tail names (#185). Zero means the list is complete.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub failing_tests_omitted: u32,
    /// Bounded tail of the job log.
    pub detail: Option<String>,
    /// `cloud_sweep` jobs: the typed sweep verdict.
    #[serde(default)]
    pub sweep: Option<CloudSweepResultV1>,
    /// `test` jobs with `scoped_test` (#1638): the `SCOPED_TEST_RECEIPT` the
    /// script printed last (`ok`, `exit_code`, `base`, `head`, `filters`,
    /// `log_dir`, per-package `status`/`completed`, `failures`); the
    /// Previously submitted `scoped-test*` recipes settle the same way and `failing_tests` then
    /// lists the receipt's exact failures (#1795). `test` jobs with
    /// `candidate_receipt` (#1099): the typed candidate
    /// receipt `scripts/check-touched-shards` produced (base, head, merge
    /// clean, shards compiled, audit verdicts, migrations, suggested filters).
    #[serde(default)]
    pub receipt: Option<serde_json::Value>,
    /// `test`/`build` jobs (#1756): seconds the run spent waiting for a governor
    /// build slot or a sandbox/artifact lock, read from the log's `rsi-phase`
    /// lines. That time does not count against the job's `timeout_minutes`; absent when the
    /// log shows no such wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_wait_secs: Option<u64>,
    /// #2048: the deploy that stopped this job (`interrupted_by_deploy`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_id: Option<Uuid>,
    /// Hub #2954: `build` jobs: the job worktree's full HEAD commit the daemon
    /// observed when the job ended (equal to the commit recorded at submit
    /// unless the job failed `job_source_moved`, which names both). A deploy
    /// compares it with the SHA it is asked to install. Absent when the
    /// worktree is not a readable git checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_head: Option<String>,
    /// Hub #2954: `build` jobs: whether that worktree had uncommitted changes
    /// (tracked or untracked, ignored files aside) when the job ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_dirty: Option<bool>,
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
    /// Why a `queued` job has not started: `held_for_deploy` while a deploy
    /// drain or a staged `interrupt_workers` deploy holds job starts (#2190), or
    /// `sandbox_job_queued` behind an earlier local test/build job of the same
    /// owner. Absent once it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<String>,
    /// Per-job completion wake policy chosen at submit.
    #[serde(default)]
    pub wake: JobWake,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSubmitJobReceiptV1 {
    /// #269: the job id again at the top level, so a reply cut by a pager or
    /// `tail` still carries it (replies serialize with sorted keys, so this lands
    /// after the large `job` object while `job.id` sits near the head). A lost id
    /// is recoverable with `AgentListJobs`.
    #[serde(default)]
    pub job_id: Uuid,
    pub job: AgentJobV1,
    /// True when an identical idempotent retry returned the original job.
    pub replayed: bool,
    /// #2190: for a job `held_for_deploy`, when the hold ends at the latest (the
    /// deploy's deadline; RFC3339). The job starts earlier once the deploy settles
    /// or is abandoned. Absent for a job that was not held by a deploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCancelJobResultV1 {
    /// The job after the call: `failed` with refusal `job_cancelled` when this
    /// call stopped it, or its existing terminal state otherwise.
    pub job: AgentJobV1,
    /// True when this call stopped a running job; false when it had already
    /// settled (a repeat cancel changes nothing).
    pub cancelled: bool,
    /// #2213: the current manager that issued this cancel for a job owned by
    /// another session in its live scope (the audit receipt; the job's owner is
    /// `job.owner_session_id`). Absent when the owner cancelled its own job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_by_manager: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentListJobsResultV1 {
    pub jobs: Vec<AgentJobV1>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// #269: the submit receipt names the job id at the top level and in `job`,
    /// and a receipt without the field (an older reply) still reads.
    #[test]
    fn submit_receipt_carries_job_id_at_top_level() {
        let id = Uuid::new_v4();
        let job = json!({
            "id": id, "kind": "test", "name": null, "state": "queued",
            "owner_session_id": Uuid::new_v4(), "unit_name": "u", "cwd": "/w",
            "log_path": "/l", "params": {"kind": "test", "shard": "store-01"}, "exit_code": null,
            "result": null, "created_at": "2026-01-01T00:00:00Z", "finished_at": null
        });
        let reply = json!({"job_id": id, "job": job, "replayed": false});
        let receipt: AgentSubmitJobReceiptV1 = serde_json::from_value(reply).unwrap();
        assert_eq!(receipt.job_id, id);
        assert_eq!(receipt.job.id, id);
        let wire = serde_json::to_value(&receipt).unwrap();
        assert_eq!(wire["job_id"], json!(id));
        assert_eq!(wire["job"]["id"], json!(id));
        let old = json!({"job": job, "replayed": false});
        let old: AgentSubmitJobReceiptV1 = serde_json::from_value(old).unwrap();
        assert_eq!(old.job.id, id);
    }

    #[test]
    fn job_param_filter_refusals_name_path_filter_and_limit() {
        let long = format!("rsid={}", "z".repeat(300));
        for (kind, params, path) in [
            (
                JobKind::Landing,
                json!({"accepted": "a".repeat(40), "test_filters": [long]}),
                "params.test_filters[0]",
            ),
            (
                JobKind::Test,
                json!({"recipe": "scoped", "filters": ["rsid=ok", long]}),
                "params.filters[1]",
            ),
        ] {
            let req = request(kind, params);
            let code = req.typed_params().unwrap_err();
            let text = req.refusal_detail(code);
            assert!(
                text.starts_with(&format!("{JOB_INVALID_PARAMS}: {path} is 305 bytes")),
                "{text}"
            );
            assert!(text.contains("256-byte limit"), "{text}");
        }
        let req = request(
            JobKind::Test,
            json!({"package": "rsid", "filters": ["a".repeat(250)]}),
        );
        let text = req.refusal_detail(req.typed_params().unwrap_err());
        assert!(text.contains("200-byte limit"), "{text}");
    }

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
            json!({"filterset":"test(a) | all()"}),
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
            json!({"scoped_test":{"base":"origin/rolling"},"recipe":"check-cpu"}),
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
            "recipe":"check-cpu",
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
            request(JobKind::Test, json!({"recipe":"check-cpu","filters":many}))
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
                    json!({"recipe":"check-cpu","filters":filters})
                )
                .typed_params()
                .unwrap_err(),
                JOB_INVALID_PARAMS,
                "{filters}"
            );
        }
    }

    #[test]
    fn package_test_target_accepts_plain_names_and_round_trips() {
        for target in ["manifest_dir_guard", "integration-test", "test1"] {
            let parsed = request(
                JobKind::Test,
                json!({"package":"rsi-common","test_target":target,"exact":true,"filters":["tests::one","tests::two"]}),
            )
            .typed_params()
            .unwrap();
            assert!(
                matches!(&parsed, JobParams::Test(p) if p.test_target.as_deref() == Some(target))
            );
            assert_eq!(
                serde_json::from_value::<JobParams>(serde_json::to_value(&parsed).unwrap())
                    .unwrap(),
                parsed
            );
        }
        let old = request(JobKind::Test, json!({"package":"rsi-common"}))
            .typed_params()
            .unwrap();
        assert!(matches!(old, JobParams::Test(p) if p.test_target.is_none()));
    }

    #[test]
    fn package_test_target_refuses_invalid_names_and_types() {
        for target in [
            json!(""),
            json!("--help"),
            json!("../test"),
            json!("tests/test"),
            json!("tests\\test"),
            json!("a.b"),
            json!("a::b"),
            json!("a b"),
            json!("a\nb"),
            json!("a;id"),
            json!("$(id)"),
            json!("`id`"),
            json!("a|b"),
            json!("a&b"),
            json!("a*"),
            json!("é"),
            json!("a".repeat(JOB_MAX_TOKEN_BYTES + 1)),
            json!(true),
            json!(1),
            json!([]),
            json!({}),
        ] {
            assert_eq!(
                request(JobKind::Test, json!({"package":"rsi-common","test_target":target,"exact":true,"filters":["one"]})).typed_params(),
                Err(JOB_INVALID_PARAMS),
                "{target}"
            );
        }
    }

    #[test]
    fn package_test_target_requires_exact_filters_and_exclusive_package_form() {
        let valid = json!({"package":"rsi-common","test_target":"manifest_dir_guard","exact":true,"filters":["one"]});
        for extra in [
            json!({"lib_only":true}),
            json!({"exact":false}),
            json!({"filters":[]}),
            json!({"package":null}),
            json!({"shard":"other-01"}),
            json!({"recipe":"check-cpu"}),
            json!({"scoped_test":{"base":"origin/rolling"}}),
            json!({"candidate_receipt":"rolling"}),
            json!({"filterset":"test(one)"}),
            json!({"qa_lane":{"sha":"a".repeat(40)}}),
        ] {
            let mut value = valid.clone();
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert_eq!(
                request(JobKind::Test, value.clone()).typed_params(),
                Err(JOB_INVALID_PARAMS),
                "{value}"
            );
        }
        for missing in ["package", "exact", "filters"] {
            let mut value = valid.clone();
            value.as_object_mut().unwrap().remove(missing);
            assert_eq!(
                request(JobKind::Test, value).typed_params(),
                Err(JOB_INVALID_PARAMS),
                "{missing}"
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
            json!({"recipe":"check-cpu"}),
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
                cpu_quota_percent: None,
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

    /// #161: the optional per-job CPU budget is bounded, optional and kept out
    /// of declared recipes and candidate receipts; omitted stays omitted.
    #[test]
    fn cpu_quota_percent_is_bounded_optional_and_refused_for_recipes() {
        let scoped = json!({"scoped_test":{"base":"origin/rolling"}});
        let package = json!({"package":"rsid"});
        let shard = json!({"shard":"other-01"});
        for (kind, base) in [
            (JobKind::Test, scoped),
            (JobKind::Test, package),
            (JobKind::Test, shard),
            (JobKind::Build, json!({"command":"check","workspace":true})),
        ] {
            for ok in [JOB_CPU_QUOTA_MIN_PERCENT, 200, JOB_CPU_QUOTA_MAX_PERCENT] {
                let mut params = base.clone();
                params["cpu_quota_percent"] = json!(ok);
                let typed = request(kind, params).typed_params().unwrap();
                let stored = serde_json::to_value(&typed).unwrap();
                assert_eq!(stored["cpu_quota_percent"], json!(ok));
                assert_eq!(serde_json::from_value::<JobParams>(stored).unwrap(), typed);
            }
            for bad in [
                0,
                JOB_CPU_QUOTA_MIN_PERCENT - 1,
                JOB_CPU_QUOTA_MAX_PERCENT + 1,
            ] {
                let mut params = base.clone();
                params["cpu_quota_percent"] = json!(bad);
                assert_eq!(
                    request(kind, params).typed_params().unwrap_err(),
                    JOB_INVALID_PARAMS,
                    "{bad}"
                );
            }
            // Omitted is stored omitted: the daemon-derived budget applies.
            let typed = request(kind, base).typed_params().unwrap();
            let stored = serde_json::to_value(&typed).unwrap();
            assert!(stored.get("cpu_quota_percent").is_none());
        }
        // A declared recipe owns its cap; a candidate receipt builds elsewhere.
        for params in [
            json!({"recipe":"e2e","cpu_quota_percent":200}),
            json!({"candidate_receipt":"rsi/x","cpu_quota_percent":200}),
        ] {
            assert_eq!(
                request(JobKind::Test, params).typed_params().unwrap_err(),
                JOB_INVALID_PARAMS
            );
        }
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
    fn test_no_run_names_shard_features_and_a_shard_sweep() {
        let one = request(
            JobKind::Build,
            json!({"command":"test_no_run","package":"rsid","no_default_features":true,
                   "features":["test-shard-store-01"]}),
        )
        .typed_params();
        let Ok(JobParams::Build(one)) = one else {
            panic!("a one-feature harness build is accepted: {one:?}");
        };
        assert_eq!(one.command, BuildCommand::TestNoRun);
        assert_eq!(one.command.as_str(), "test_no_run");
        assert!(one.no_default_features);
        assert_eq!(one.features, ["test-shard-store-01"]);
        let sweep = request(
            JobKind::Build,
            json!({"command":"test_no_run","package":"rsid-store","shards":["store-01","other-02"]}),
        )
        .typed_params();
        let Ok(JobParams::Build(sweep)) = sweep else {
            panic!("a shard sweep is accepted: {sweep:?}");
        };
        assert_eq!(sweep.shards, ["store-01", "other-02"]);
        // The new fields stay out of an ordinary check's stored form.
        let check = request(JobKind::Build, json!({"command":"check","workspace":true}));
        let Ok(JobParams::Build(check)) = check.typed_params() else {
            panic!("an ordinary check is accepted");
        };
        assert_eq!(
            serde_json::to_value(&check).unwrap(),
            json!({"command":"check","package":null,"workspace":true,"all_targets":false,"release":false})
        );
    }

    #[test]
    fn test_no_run_refuses_what_could_enable_an_arbitrary_feature_or_scope() {
        let too_many: Vec<String> = (0..=JOB_MAX_SHARD_SWEEP)
            .map(|n| format!("s-{n}"))
            .collect();
        let bad = [
            json!({"command":"test_no_run","workspace":true}),
            json!({"command":"test_no_run"}),
            json!({"command":"test_no_run","package":"rsid","release":true}),
            json!({"command":"test_no_run","package":"rsid","all_targets":true}),
            json!({"command":"test_no_run","package":"rsid","features":["test-ollama"]}),
            json!({"command":"test_no_run","package":"rsid","features":["rsid-core/test-seam"]}),
            json!({"command":"test_no_run","package":"rsid","features":["test-shard-"]}),
            json!({"command":"test_no_run","package":"rsid","features":["test-shard-a"],"shards":["a"]}),
            json!({"command":"test_no_run","package":"rsid","shards":["--features"]}),
            json!({"command":"test_no_run","package":"rsid","shards":["a b"]}),
            json!({"command":"test_no_run","package":"rsid","shards":["a","a"]}),
            json!({"command":"test_no_run","package":"rsid","shards":too_many}),
            json!({"command":"check","package":"rsid","features":["test-shard-a"]}),
            json!({"command":"check","package":"rsid","no_default_features":true}),
            json!({"command":"build","package":"rsid","shards":["a"]}),
        ];
        for params in bad {
            assert_eq!(
                request(JobKind::Build, params.clone()).typed_params().err(),
                Some(JOB_INVALID_PARAMS),
                "{params}"
            );
        }
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

    /// #1605: a composite nextest filterset (the landing gate's own shape) is a
    /// valid shard job param on every host; anything beyond the atoms is refused
    /// with a detail that names the supported form.
    #[test]
    fn a_shard_job_accepts_composite_filtersets_and_refusal_names_the_supported_form() {
        let max = "test(a) | ".repeat(JOB_MAX_TEST_FILTERS - 1) + "test(a)";
        assert!(max.len() <= JOB_MAX_FILTERSET_BYTES);
        let good = [
            "test(a)",
            "test(a) | test(b)",
            "test(a)|test(b)",
            "test(=store::tests::a) | test(=store::tests::b) | test(/^sandbox::/)",
            "test(/^session::(?:a|b)::[a-z]+$/) | test(x)",
            "test(/f(x)/) | test(/g/)",
            max.as_str(),
        ];
        for filterset in good {
            let req = request(
                JobKind::Test,
                json!({"shard":"other-01","filterset":filterset}),
            );
            let Ok(JobParams::Test(p)) = req.typed_params() else {
                panic!("refused {filterset}");
            };
            assert_eq!(p.filterset.as_deref(), Some(filterset));
        }
        let too_many = "test(a) | ".repeat(JOB_MAX_TEST_FILTERS) + "test(a)";
        let too_long = format!("test({})", "a".repeat(JOB_MAX_FILTERSET_BYTES));
        let bad = [
            "",
            "test()",
            "test(a) |",
            "| test(a)",
            "test(a) || test(b)",
            "test(a) | all()",
            "test(a) & test(b)",
            "not test(a)",
            "test(a) | test(b",
            "test(/a\tb/)\n",
            "test(a) | test(\u{e9})",
            "test(a); rm -rf /",
            too_many.as_str(),
            too_long.as_str(),
        ];
        for filterset in bad {
            let req = request(
                JobKind::Test,
                json!({"shard":"other-01","filterset":filterset}),
            );
            assert_eq!(req.typed_params(), Err(JOB_INVALID_PARAMS), "{filterset:?}");
            let detail = req.refusal_detail(JOB_INVALID_PARAMS);
            assert!(
                detail.contains("params.filterset") && detail.contains("test(=NAME)"),
                "{filterset:?}: {detail}"
            );
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
                json!({"shard":"x","filterset":"test(a) | all()"}),
            ),
            request(
                JobKind::Test,
                json!({"shard":"other-01","filterset":"test(a) |"}),
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
