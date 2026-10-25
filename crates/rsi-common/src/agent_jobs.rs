//! Wire types for daemon-owned durable jobs (#1002 slice 1).
//!
//! `AgentSubmitJob` hands a long operation (test, build, landing, cloud gate,
//! cloud sweep, benchmark run)
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

/// Most binaries one `check`/`build` job names with `bins`.
pub const JOB_MAX_BUILD_BINS: usize = 8;
/// Bounds for an aggregate release job, independent of single-package builds.
pub const JOB_MAX_RELEASE_MEMBERS: usize = 16;
/// Maximum total named binaries in an aggregate release build.
pub const JOB_MAX_RELEASE_BINS: usize = 64;

/// Non-release dev features a `check`/`build` job or an exact package `test` job
/// may enable, per package. A
/// feature outside this list is refused naming it, so a job can never turn on an
/// arbitrary feature (`e2e-seed` compiles the isolated-state seeder; `build.rs`
/// refuses it in a release profile).
pub const JOB_BUILD_FEATURE_ALLOWLIST: &[(&str, &[&str])] = &[("rsid", &["e2e-seed"])];

/// The features `package` lets a `check`/`build`/package `test` job enable.
#[must_use]
pub fn allowed_build_features(package: &str) -> &'static [&'static str] {
    JOB_BUILD_FEATURE_ALLOWLIST
        .iter()
        .find(|(name, _)| *name == package)
        .map_or(&[], |(_, features)| *features)
}

/// Why `features` on a `job` job (`check`, `build` or `test`) is refused, naming
/// the allowed features of `package`; `None` when every entry is allowed. One
/// text for every job kind (kaizen #160, #171).
fn dev_features_refusal(
    features: &[String],
    package: Option<&str>,
    release: bool,
    job: &str,
) -> Option<String> {
    let allowed = package.map_or(&[][..], allowed_build_features);
    let name = package.unwrap_or("a workspace build");
    let allowed_text = if allowed.is_empty() {
        "none".to_string()
    } else {
        allowed.join(", ")
    };
    if release {
        return Some(format!(
            "params.features: refused with release, dev features such as `e2e-seed` are \
             never built in a release profile; drop params.release or params.features \
             (allowed features for `{name}`: {allowed_text})"
        ));
    }
    if let Some(i) = (0..features.len()).find(|&i| features[..i].contains(&features[i])) {
        return Some(format!(
            "params.features: names `{}` twice; list each feature once (allowed features \
             for `{name}`: {allowed_text})",
            features[i]
        ));
    }
    let bad = features.iter().find(|f| !allowed.contains(&f.as_str()))?;
    Some(format!(
        "params.features: entry `{bad}` is not allowed for `{name}` ({job} job); drop it or \
         use one of the allowed features for `{name}`: {allowed_text}"
    ))
}
pub const JOB_MAX_TOKEN_BYTES: usize = 200;
/// Longest shard `filterset`: up to [`JOB_MAX_TEST_FILTERS`] `|`-joined atoms.
pub const JOB_MAX_FILTERSET_BYTES: usize = 1024;
/// #1584: a recipe run may carry at most this many explicit focused
/// `PACKAGE=FILTER` test filters (each bounded like a landing `--test-filter`).
pub const JOB_RECIPE_MAX_FILTERS: usize = 16;
/// #3181: where a `bench` job's committed run config lives.
pub const JOB_BENCH_CONFIG_DIR: &str = "eval/bench/tb2/runs/";
/// #3191: a Meta-Harness candidate's run config is `<dir><id>/run.toml`.
pub const JOB_BENCH_CANDIDATES_DIR: &str = "eval/bench/tb2/candidates/";
/// #3191: the file name of a candidate's run config.
pub const JOB_BENCH_CANDIDATE_CONFIG: &str = "run.toml";
/// #3181: the largest `budget_usd` a `bench` job may name. The OpenRouter spend cap
/// is the real bound; this only rejects an absurd request up front.
pub const JOB_BENCH_MAX_BUDGET_USD: f64 = 1000.0;

/// #3181: `bench.enabled` is off (the default): no benchmark run is admitted.
pub const BENCH_DISABLED: &str = "bench_disabled";
/// #3181: the run config is missing, unreadable, malformed or names an unknown
/// split or task (`bench_config_invalid: <reason>`).
pub const BENCH_CONFIG_INVALID: &str = "bench_config_invalid";
/// #3191: the run overlay is malformed or breaks a limit
/// (`bench_overlay_invalid: <overlay code>: <field>`).
pub const BENCH_OVERLAY_INVALID: &str = "bench_overlay_invalid";
/// #3191: the leak audit found held-out or search-split content in the overlay or its
/// files, or could not run (`bench_overlay_leak_audit_failed: <hits>`). The run is refused.
pub const BENCH_OVERLAY_LEAK_AUDIT_FAILED: &str = "bench_overlay_leak_audit_failed";
/// #3181: the run config (or the files the run reads) is not committed and clean
/// at the job's HEAD, or the worktree is not a git checkout.
pub const BENCH_CONFIG_UNCOMMITTED: &str = "bench_config_uncommitted";
/// #3181: the config's docker host is not in the operator's `bench.docker_hosts`.
pub const BENCH_DOCKER_HOST_NOT_ALLOWED: &str = "bench_docker_host_not_allowed";
/// #3181: the config's docker host is not `ssh://` plus `bench.laptop_host`.
pub const BENCH_LAPTOP_HOST_MISMATCH: &str = "bench_laptop_host_mismatch";
/// #3181: the config launches OpenRouter-billed sessions while the OpenRouter
/// spend cap is `off`: no paid run before the operator arms the cap.
pub const BENCH_SPEND_CAP_OFF: &str = "bench_spend_cap_off";
/// #3181: the Rust runner's pin check reports drift from the pinned dataset.
pub const BENCH_PIN_CHECK_FAILED: &str = "bench_pin_check_failed";
/// #3181: a laptop preflight check failed (`bench_preflight_failed: <reason>`).
pub const BENCH_PREFLIGHT_FAILED: &str = "bench_preflight_failed";
/// #3181: the run config changed between submit and start.
pub const BENCH_CONFIG_CHANGED: &str = "bench_config_changed";
/// #3181: the daemon could not record the run or bind its adapter socket.
pub const BENCH_RUN_OPEN_FAILED: &str = "bench_run_open_failed";
/// #3181: `scripts/bench-transport` failed (not a pin or preflight failure).
pub const BENCH_TRANSPORT_FAILED: &str = "bench_transport_failed";
/// #3181: the submitting session has no project to attribute the run to.
pub const BENCH_PROJECT_UNKNOWN: &str = "bench_project_unknown";
/// #3181: the `held` reason of a queued `bench` job that waits for its start gates.
pub const JOB_HELD_FOR_BENCH: &str = "held_for_bench_admission";

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
/// Hub #33: the no-sandbox session's registered project root is a git
/// repository, so agent sessions of that project get sandboxes and its root is
/// a shared worktree no job may run in; run the job from a sandboxed session.
pub const JOB_PROJECT_ROOT_SANDBOXED: &str = "job_project_root_sandboxed";
pub const JOB_NOT_FOUND: &str = "job_not_found";
/// `landing`, `cloud_gate`, `cloud_sweep` and `bench` publish to `rolling` / spend the
/// operator's cloud or model grant: only the current appointed manager or current Epic lead may submit.
pub const JOB_KIND_NOT_AUTHORIZED: &str = "job_kind_not_authorized";
/// #1235: `sandbox_session_id` names a session that is still live; a job
/// runs only in a terminal session's sandbox (one writer per sandbox).
pub const JOB_SANDBOX_SESSION_LIVE: &str = "job_sandbox_session_live";
pub const JOB_LAUNCH_FAILED: &str = "job_launch_failed";
/// Hub #3024: the `scoped_test` tool copy that would run (the rolling tip's, or the
/// candidate's own) loads a helper under `scripts/` that its tree does not contain.
/// Refused up front with the file named, never run with another copy of the tool.
pub const JOB_SCOPED_TEST_TOOL_HELPER_MISSING: &str = "scoped_test_tool_helper_missing";
/// A forced rolling tool cannot be resolved/materialized with its expected blobs.
pub const JOB_SCOPED_TEST_ROLLING_TOOL_UNAVAILABLE: &str = "scoped_test_rolling_tool_unavailable";
/// A scoped-test base or head cannot be resolved to a commit in the job worktree.
pub const JOB_SCOPED_TEST_REF_UNRESOLVED: &str = "job_scoped_test_ref_unresolved";
/// The OS or requested workflow has no supported durable backend.
pub const JOB_PLATFORM_UNSUPPORTED: &str = "job_platform_unsupported";
/// The host cannot enforce the daemon-owned lifecycle fence around a test
/// payload; candidate/baseline/recipe code must not run without it (#637).
pub const JOB_TEST_SOURCE_UNCONTAINED: &str = "job_test_source_uncontained";
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
/// #582: the `held` and `hold.reason` of a running `test` or `build` job whose
/// cargo-slot governor wait is held by the host disk floor. The unit has
/// started but compiles nothing until free space reaches the floor; `hold`
/// carries the numbers, so the owner reads why it waits without scanning a log.
pub const JOB_HELD_FOR_DISK_FLOOR: &str = "disk_floor";
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
/// refusal too, as does a job held for capacity on a host with no cargo-slot
/// governor (macOS) past its own build-admission allowance, not the drain cap
/// (#177). The detail names the hold and its observed value.
pub const JOB_ADMISSION_TIMED_OUT: &str = "job_admission_timed_out";
/// #618: the `failure_class` of a job that never got admission (a governor slot,
/// the disk or memory floor, a lock) and so ran nothing. Like
/// `infrastructure_interrupted` it is not the worker's failure and does not
/// count toward its two-run verification limit: resubmit once the receipt's
/// `admission.blocked_by` (or the result detail) no longer holds.
pub const FAILURE_CLASS_ADMISSION_TIMEOUT: &str = "infrastructure_admission_timeout";
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
    /// #3181: one pinned benchmark run (Terminal-Bench on Harbor over the laptop).
    Bench,
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
            Self::Bench => "bench",
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
            "bench" => Self::Bench,
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

/// hub #3852: how much of a job `AgentGetJob` returns. `full` (the default)
/// is the whole stored job; `compact` is the bounded view of
/// [`crate::agent_job_compact`], small enough to read in one tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentJobFieldsV1 {
    #[default]
    Full,
    Compact,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGetJobRequestV1 {
    pub job_id: Uuid,
    #[serde(default)]
    pub fields: AgentJobFieldsV1,
}

/// `AgentCancelJob` (#1106): stop one of your running jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCancelJobRequestV1 {
    pub job_id: Uuid,
}

/// hub #3889: how much of each job `AgentListJobs` returns. `compact` (the
/// default) is one bounded row per job ([`AgentJobV1::compact_row`]); `full`
/// restores the whole stored job, receipt included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentJobListFieldsV1 {
    #[default]
    Compact,
    Full,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentListJobsRequestV1 {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub fields: AgentJobListFieldsV1,
}

/// #1638: run `scripts/scoped-test --base <base> [--head <head>]` in the
/// caller's sandbox. Bare refs and a tool policy only: no test filters or script flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedTestParams {
    /// Branch, `origin/<branch>` or full commit id the diff is taken against.
    pub base: String,
    /// Candidate to verify; must be the sandbox HEAD when omitted or named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Run the trusted rolling tool even when the candidate changes the tool.
    /// Candidate Python tests are still selected from the candidate tree.
    #[serde(default)]
    pub rolling_tool: bool,
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
    /// Run one declared just/make gate from the worktree's `.rsi/jobs.toml`
    /// (`desktop-install` builds the standalone `desktop/src-tauri` workspace and
    /// installs it into a job-owned throwaway HOME, #3239).
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
    /// Package runs: select one binary target's unit tests with `--bin <name>`
    /// (for example `rsid`'s `rsi-rolling-land`). Same protections as
    /// `test_target`: requires nonempty exact filters; exclusive with `lib_only`
    /// and `test_target`. Lets an `at_commit` run reach a bin unit test (#4019).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
    /// #171: package runs only: dev features of `package` named in
    /// [`JOB_BUILD_FEATURE_ALLOWLIST`] (for example `rsid` `e2e-seed`), passed
    /// as one `--features` list so a feature-gated test target such as
    /// `--test e2e_seed` can run as a job. Refused text is shared with
    /// `check`/`build` (see [`TestJobParams::test_features_refusal`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// #1099 candidate receipt (manager/Epic lead only): the branch or full
    /// commit to verify against `origin/rolling` in a temporary detached
    /// worktree. The typed receipt is `result.receipt`.
    #[serde(default)]
    pub candidate_receipt: Option<String>,
    /// #3410: run this package test at a past commit (full 40-hex id of any
    /// commit in the project repository) instead of at the sandbox HEAD. The job
    /// runner checks the commit out into a scratch worktree, runs the exact
    /// filters there and removes the scratch, so the sandbox and its branch are
    /// untouched. Package runs only: `exact` true, nonempty `filters` and one of
    /// `lib_only`, `test_target` or `bin`. Open to any session with a sandbox (a QA-red
    /// fixer confirming a culprit, a reviewer running a source's new tests);
    /// never acceptance evidence, which is only the `scoped_test` receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_commit: Option<String>,
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

/// One release package and its explicit binaries. Each member gets its own
/// Cargo invocation, so dependency features cannot unify across members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseBuildMember {
    pub package: String,
    pub bins: Vec<String>,
}

/// `cargo check|build` through the cargo build-slot wrapper, or (`test_no_run`,
/// #2692) the shard-feature test-harness sweep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildJobParams {
    pub command: BuildCommand,
    /// Sequential, feature-isolated release builds in one job/target/source
    /// fence. Requires build, release and sha; excludes the single-build flags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub release_members: Vec<ReleaseBuildMember>,
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
    /// `test_no_run`: one build with these `test-shard-*` features enabled
    /// together (mutually exclusive with `shards`). `check`/`build`: dev
    /// features of `package` named in [`JOB_BUILD_FEATURE_ALLOWLIST`] (for
    /// example `rsid` `e2e-seed`), refused with `release`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// `check`/`build` with `package` only: build just these binaries
    /// (`--bin NAME` each, at most [`JOB_MAX_BUILD_BINS`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bins: Vec<String>,
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
    /// Which run the cloud host does (#3844). Absent means the full-library QA
    /// sweep, exactly as before; `build` is the Python-free build-and-test lane
    /// (`scripts/cloud-build.sh`). Same authority, admission and spend net.
    #[serde(default, skip_serializing_if = "SweepLane::is_default")]
    pub lane: SweepLane,
}

/// The run a `cloud_sweep` job does on its ephemeral spot host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepLane {
    /// `make rolling-library-qa` (the original, default sweep).
    #[default]
    Qa,
    /// `cargo build --workspace --locked` plus a small test set (#3844).
    Build,
}

impl SweepLane {
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Qa)
    }
}

/// #3181: `scripts/bench-transport` for one committed benchmark run config
/// (kind `bench`). Only the appointed manager or an Epic lead may submit it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchJobParams {
    /// Repository path of a committed `eval/bench/tb2/runs/*.toml` or
    /// `eval/bench/tb2/candidates/<id>/run.toml`.
    pub config: String,
    /// The most the run may spend (USD). The run's M4 budget; the daemon admits it
    /// only when it fits what is left of the OpenRouter spend cap.
    pub budget_usd: f64,
}

// `budget_usd` is finite (validated at submit), so `==` is reflexive.
impl Eq for BenchJobParams {}

/// Validated, typed parameters for one job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobParams {
    Test(TestJobParams),
    Build(BuildJobParams),
    Landing(LandingJobParams),
    CloudGate(LandingJobParams),
    CloudSweep(CloudSweepJobParams),
    Bench(BenchJobParams),
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
            Self::Bench(_) => JobKind::Bench,
        }
    }
}

/// A refused job request: the stable `code` callers match on and, for a
/// `job_invalid_params` refusal, the one-line reason naming the offending field
/// and the fix. Every `job_invalid_params` is built here so none is bare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRefusal {
    pub code: &'static str,
    pub reason: Option<String>,
}

impl JobRefusal {
    /// `job_invalid_params` naming `field` (a `params.*` path or a request
    /// field) and what to do about it.
    #[must_use]
    pub fn invalid_params(field: &str, fix: impl AsRef<str>) -> Self {
        Self::invalid_params_text(format!("{field}: {}", fix.as_ref()))
    }

    /// `job_invalid_params` with a reason that already starts with its field.
    #[must_use]
    pub fn invalid_params_text(reason: String) -> Self {
        Self {
            code: JOB_INVALID_PARAMS,
            reason: Some(reason),
        }
    }

    /// The refusal text: `code`, then `: reason` when there is one.
    #[must_use]
    pub fn detail(&self) -> String {
        match &self.reason {
            Some(reason) => format!("{}: {reason}", self.code),
            None => self.code.to_string(),
        }
    }

    /// `source_session_id` (alias `sandbox_session_id`) on a job that cannot
    /// run in another session's sandbox.
    #[must_use]
    pub fn source_session_scope() -> Self {
        Self::invalid_params(
            "source_session_id",
            "(alias sandbox_session_id) is only accepted for kind landing and cloud_gate, \
             without worktree; for a test in another worktree use worktree (managers only)",
        )
    }

    /// A `qa_lane` job runs in the caller's own sandbox.
    #[must_use]
    pub fn qa_lane_scope() -> Self {
        Self::invalid_params(
            "params.qa_lane",
            "runs in your own sandbox; drop worktree and source_session_id",
        )
    }

    /// `worktree` must be an absolute path.
    #[must_use]
    pub fn worktree_not_absolute() -> Self {
        Self::invalid_params(
            "worktree",
            "must be an absolute path to a worktree of your repository",
        )
    }

    /// The request's test timeout exceeds the recipe declaration's cap.
    #[must_use]
    pub fn timeout_over_recipe(requested: u32, cap: u32) -> Self {
        Self::invalid_params(
            "params.timeout_minutes",
            format!(
                "{requested} exceeds the recipe's declared timeout_minutes cap of {cap}; \
                 use at most {cap} or omit it"
            ),
        )
    }
}

impl From<&'static str> for JobRefusal {
    fn from(code: &'static str) -> Self {
        Self { code, reason: None }
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

    /// An `at_commit` run (#3410): one exact package test at a past commit.
    #[must_use]
    pub const fn is_at_commit(&self) -> bool {
        self.at_commit.is_some()
    }

    /// A `candidate_receipt` run: only the appointed manager or an Epic lead
    /// may submit it (it fetches and builds an arbitrary branch).
    #[must_use]
    pub const fn is_candidate_receipt(&self) -> bool {
        self.candidate_receipt.is_some()
    }

    /// Why `features` is refused on a `test` job, naming the allowed features
    /// (the `check`/`build` text); `None` when it is empty or every entry is
    /// allowed for the package. A non-package run (shard, recipe, scoped test,
    /// candidate receipt) cannot carry features.
    #[must_use]
    pub fn test_features_refusal(&self) -> Option<String> {
        if self.features.is_empty() {
            return None;
        }
        if self.package.is_none() {
            return Some(
                "params.features: needs params.package on a test job (shard, recipe, \
                 scoped_test and candidate_receipt runs take no features); add the package \
                 or drop features (allowed features: `rsid`: e2e-seed)"
                    .to_string(),
            );
        }
        dev_features_refusal(&self.features, self.package.as_deref(), false, "test")
    }

    fn validate(&self) -> Result<(), JobRefusal> {
        if let Some(reason) = self.test_features_refusal() {
            return Err(JobRefusal::invalid_params_text(reason));
        }
        if let Some(target) = &self.test_target {
            if !bare_token(target, &[]) {
                return Err(JobRefusal::invalid_params(
                    "params.test_target",
                    "must be an integration test name (ASCII letters, digits, `_`, `-`; no \
                     leading `-`, path or shell syntax)",
                ));
            }
            if self.package.is_none() {
                return Err(JobRefusal::invalid_params(
                    "params.test_target",
                    "needs params.package; add the package that owns the test target",
                ));
            }
            if self.shard.is_some()
                || self.recipe.is_some()
                || self.scoped_test.is_some()
                || self.candidate_receipt.is_some()
            {
                return Err(JobRefusal::invalid_params(
                    "params.test_target",
                    "only combines with params.package; drop params.shard, recipe, \
                     scoped_test and candidate_receipt",
                ));
            }
            if self.lib_only || self.bin.is_some() {
                return Err(JobRefusal::invalid_params(
                    "params.test_target",
                    "is mutually exclusive with params.lib_only and params.bin; drop one of them",
                ));
            }
            if !self.exact || self.filters.is_empty() {
                return Err(JobRefusal::invalid_params(
                    "params.test_target",
                    "requires params.exact true and nonempty params.filters (whole test names)",
                ));
            }
        }
        if let Some(bin) = &self.bin {
            self.validate_bin(bin)?;
        }
        if let Some(percent) = self.cpu_quota_percent {
            if !cpu_quota_percent_in_bounds(percent) {
                return Err(JobRefusal::invalid_params(
                    "params.cpu_quota_percent",
                    format!(
                        "{percent} is outside {JOB_CPU_QUOTA_MIN_PERCENT}-{JOB_CPU_QUOTA_MAX_PERCENT}; \
                         use a percent of one CPU in that range or omit it"
                    ),
                ));
            }
            if self.recipe.is_some() || self.candidate_receipt.is_some() {
                return Err(JobRefusal::invalid_params(
                    "params.cpu_quota_percent",
                    "is not accepted with params.recipe or candidate_receipt; omit it",
                ));
            }
        }
        if let Some(minutes) = self.timeout_minutes
            && !(JOB_TEST_TIMEOUT_MIN_MINS..=JOB_TEST_TIMEOUT_MAX_MINS).contains(&minutes)
        {
            return Err(JobRefusal::invalid_params(
                "params.timeout_minutes",
                format!(
                    "{minutes} is outside {JOB_TEST_TIMEOUT_MIN_MINS}-{JOB_TEST_TIMEOUT_MAX_MINS}; \
                     use minutes in that range or omit it"
                ),
            ));
        }
        if let Some(qa) = &self.qa_lane {
            self.validate_qa_lane(qa)?;
        }
        if let Some(commit) = &self.at_commit {
            self.validate_at_commit(commit)?;
        }
        if let Some(scoped) = &self.scoped_test {
            return self.validate_scoped_test(scoped);
        }
        if let Some(recipe) = &self.recipe {
            return self.validate_recipe(recipe);
        }
        if let Some(candidate) = &self.candidate_receipt {
            return self.validate_candidate_receipt(candidate);
        }
        self.validate_shard_or_package()
    }

    /// #4019: `bin` selects one binary target's unit tests with the same
    /// protections as `test_target`.
    fn validate_bin(&self, bin: &str) -> Result<(), JobRefusal> {
        if !bare_token(bin, &[]) {
            return Err(JobRefusal::invalid_params(
                "params.bin",
                "must be a binary target name (ASCII letters, digits, `_`, `-`; no leading \
                 `-`, path or shell syntax)",
            ));
        }
        if self.package.is_none() {
            return Err(JobRefusal::invalid_params(
                "params.bin",
                "needs params.package; add the package that owns the binary",
            ));
        }
        if self.shard.is_some()
            || self.recipe.is_some()
            || self.scoped_test.is_some()
            || self.candidate_receipt.is_some()
        {
            return Err(JobRefusal::invalid_params(
                "params.bin",
                "only combines with params.package; drop params.shard, recipe, \
                 scoped_test and candidate_receipt",
            ));
        }
        if self.lib_only || self.test_target.is_some() {
            return Err(JobRefusal::invalid_params(
                "params.bin",
                "is mutually exclusive with params.lib_only and params.test_target; drop \
                 all but one",
            ));
        }
        if !self.exact || self.filters.is_empty() {
            return Err(JobRefusal::invalid_params(
                "params.bin",
                "requires params.exact true and nonempty params.filters (whole test names)",
            ));
        }
        Ok(())
    }

    /// #3410: `at_commit` is one exact package test at one commit and nothing
    /// else, so it names exactly what runs: whole test names (`exact`), in the
    /// lib or one integration target.
    fn validate_at_commit(&self, commit: &str) -> Result<(), JobRefusal> {
        if !full_lower_hex_oid(commit) {
            return Err(JobRefusal::invalid_params(
                "params.at_commit",
                "must be a full 40-character lowercase hex commit id of this repository",
            ));
        }
        let other = if self.scoped_test.is_some() {
            Some("params.scoped_test")
        } else if self.recipe.is_some() {
            Some("params.recipe")
        } else if self.shard.is_some() {
            Some("params.shard")
        } else if self.candidate_receipt.is_some() {
            Some("params.candidate_receipt")
        } else if self.qa_lane.is_some() {
            Some("params.qa_lane")
        } else if self.filterset.is_some() {
            Some("params.filterset")
        } else {
            None
        };
        if let Some(field) = other {
            return Err(JobRefusal::invalid_params(
                field,
                "cannot be combined with params.at_commit, which runs one exact package test \
                 at a commit; drop it",
            ));
        }
        if self.package.is_none() {
            return Err(JobRefusal::invalid_params(
                "params.at_commit",
                "needs params.package; name the package that owns the test",
            ));
        }
        if !self.exact || self.filters.is_empty() {
            return Err(JobRefusal::invalid_params(
                "params.at_commit",
                "requires params.exact true and nonempty params.filters (whole test names)",
            ));
        }
        let targets = usize::from(self.lib_only)
            + usize::from(self.test_target.is_some())
            + usize::from(self.bin.is_some());
        if targets != 1 {
            return Err(JobRefusal::invalid_params(
                "params.at_commit",
                "needs exactly one of params.lib_only, params.test_target or params.bin, so \
                 only the one target is built at that commit",
            ));
        }
        if self.package.as_deref() == Some("rsi") && self.test_target.as_deref() == Some("e2e_tui")
        {
            return Err(JobRefusal::invalid_params(
                "params.test_target",
                "e2e_tui needs the scripts/e2e.sh setup and is not offered with params.at_commit",
            ));
        }
        Ok(())
    }

    fn validate_qa_lane(&self, qa: &QaLaneParams) -> Result<(), JobRefusal> {
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
        if !known_shard {
            return Err(JobRefusal::invalid_params(
                "params.qa_lane",
                "needs params.shard to be a known shard (for example `store-01`, `other-03`)",
            ));
        }
        if !full_lower_hex_oid(&qa.sha) {
            return Err(JobRefusal::invalid_params(
                "params.qa_lane.sha",
                "must be a full 40-character lowercase hex commit id",
            ));
        }
        if self.candidate_receipt.is_some() || self.recipe.is_some() {
            return Err(JobRefusal::invalid_params(
                "params.qa_lane",
                "does not combine with params.recipe or candidate_receipt; use a known shard",
            ));
        }
        if self
            .timeout_minutes
            .is_some_and(|m| m > JOB_QA_LANE_TIMEOUT_MAX_MINS)
        {
            return Err(JobRefusal::invalid_params(
                "params.timeout_minutes",
                format!("a qa_lane job is capped at {JOB_QA_LANE_TIMEOUT_MAX_MINS} minutes"),
            ));
        }
        Ok(())
    }

    fn validate_scoped_test(&self, scoped: &ScopedTestParams) -> Result<(), JobRefusal> {
        if !candidate_ref(&scoped.base) {
            return Err(JobRefusal::invalid_params(
                "params.scoped_test.base",
                "must be a bare branch, `origin/<branch>` or a 40-hex commit (no options or \
                 revision expressions); use `origin/rolling`",
            ));
        }
        if scoped
            .head
            .as_deref()
            .is_some_and(|head| !candidate_ref(head))
        {
            return Err(JobRefusal::invalid_params(
                "params.scoped_test.head",
                "must be a bare branch, `origin/<branch>` or a 40-hex commit; omit it for HEAD",
            ));
        }
        let other = if self.recipe.is_some() {
            Some("params.recipe")
        } else if self.shard.is_some() {
            Some("params.shard")
        } else if self.package.is_some() {
            Some("params.package")
        } else if self.candidate_receipt.is_some() {
            Some("params.candidate_receipt")
        } else if self.filterset.is_some() {
            Some("params.filterset")
        } else if !self.filters.is_empty() {
            Some("params.filters")
        } else if self.lib_only {
            Some("params.lib_only")
        } else if self.exact {
            Some("params.exact")
        } else if self.qa_lane.is_some() {
            Some("params.qa_lane")
        } else {
            None
        };
        match other {
            Some(field) => Err(JobRefusal::invalid_params(
                field,
                "cannot be combined with params.scoped_test, which is exclusive with every \
                 other test form; drop it",
            )),
            None => Ok(()),
        }
    }

    fn validate_recipe(&self, recipe: &str) -> Result<(), JobRefusal> {
        if !bare_token(recipe, &['.']) {
            return Err(JobRefusal::invalid_params(
                "params.recipe",
                "must be a declared recipe name (letters, digits, `_`, `-`, `.`; no leading `-`)",
            ));
        }
        let other = if self.shard.is_some() {
            Some("params.shard")
        } else if self.package.is_some() {
            Some("params.package")
        } else if self.candidate_receipt.is_some() {
            Some("params.candidate_receipt")
        } else if self.filterset.is_some() {
            Some("params.filterset")
        } else if self.lib_only {
            Some("params.lib_only")
        } else if self.exact {
            Some("params.exact")
        } else {
            None
        };
        if let Some(field) = other {
            return Err(JobRefusal::invalid_params(
                field,
                "cannot be combined with params.recipe; a recipe run takes only a recipe name \
                 and optional PACKAGE=FILTER params.filters",
            ));
        }
        if self.filters.len() > JOB_RECIPE_MAX_FILTERS {
            return Err(JobRefusal::invalid_params(
                "params.filters",
                format!(
                    "a recipe run takes at most {JOB_RECIPE_MAX_FILTERS} PACKAGE=FILTER entries"
                ),
            ));
        }
        if let Some(bad) = self.filters.iter().find(|f| !lander_filter(f)) {
            return Err(JobRefusal::invalid_params(
                "params.filters",
                format!(
                    "entry `{bad}` must be PACKAGE=FILTER (both parts nonempty, no leading `-`)"
                ),
            ));
        }
        Ok(())
    }

    fn validate_candidate_receipt(&self, candidate: &str) -> Result<(), JobRefusal> {
        let other = if self.shard.is_some() {
            Some("params.shard")
        } else if self.package.is_some() {
            Some("params.package")
        } else if self.filterset.is_some() {
            Some("params.filterset")
        } else if !self.filters.is_empty() {
            Some("params.filters")
        } else if self.lib_only {
            Some("params.lib_only")
        } else if self.exact {
            Some("params.exact")
        } else {
            None
        };
        if let Some(field) = other {
            return Err(JobRefusal::invalid_params(
                field,
                "cannot be combined with params.candidate_receipt, which takes only a branch \
                 or commit; drop it",
            ));
        }
        if !candidate_ref(candidate) {
            return Err(JobRefusal::invalid_params(
                "params.candidate_receipt",
                "must be a bare branch, `origin/<branch>` or a 40-hex commit (no options or \
                 revision expressions)",
            ));
        }
        Ok(())
    }

    fn validate_shard_or_package(&self) -> Result<(), JobRefusal> {
        match (&self.shard, &self.package) {
            (Some(shard), None) => {
                if !bare_token(shard, &[]) {
                    return Err(JobRefusal::invalid_params(
                        "params.shard",
                        "must be a shard name (letters, digits, `_`, `-`; no leading `-`)",
                    ));
                }
                if !self.filters.is_empty() || self.lib_only || self.exact {
                    return Err(JobRefusal::invalid_params(
                        "params.shard",
                        "runs a whole shard; params.filters, lib_only and exact are for \
                         params.package runs (use params.filterset to narrow a shard)",
                    ));
                }
                if self
                    .filterset
                    .as_deref()
                    .is_some_and(|f| !test_filterset(f))
                {
                    return Err(JobRefusal::invalid_params(
                        "params.filterset",
                        format!(
                            "must be one or more nextest atoms `test(NAME)`, `test(=NAME)` or \
                             `test(/REGEX/)` joined by ` | ` (at most {JOB_MAX_TEST_FILTERS} \
                             atoms, {JOB_MAX_FILTERSET_BYTES} bytes)"
                        ),
                    ));
                }
                Ok(())
            }
            (None, Some(package)) => {
                if !bare_token(package, &['.']) {
                    return Err(JobRefusal::invalid_params(
                        "params.package",
                        "must be a workspace member name (letters, digits, `_`, `-`, `.`)",
                    ));
                }
                if self.filterset.is_some() {
                    return Err(JobRefusal::invalid_params(
                        "params.filterset",
                        "is for params.shard runs; a params.package run narrows with \
                         params.filters",
                    ));
                }
                if self.filters.len() > JOB_MAX_TEST_FILTERS {
                    return Err(JobRefusal::invalid_params(
                        "params.filters",
                        format!("at most {JOB_MAX_TEST_FILTERS} filters are accepted"),
                    ));
                }
                if let Some(bad) = self.filters.iter().find(|f| !bare_token(f, &[':', '.'])) {
                    return Err(JobRefusal::invalid_params(
                        "params.filters",
                        format!(
                            "entry `{bad}` must be a bare test-name token (letters, digits, \
                             `_`, `-`, `:`, `.`; no leading `-`, at most {JOB_MAX_TOKEN_BYTES} \
                             bytes)"
                        ),
                    ));
                }
                Ok(())
            }
            (Some(_), Some(_)) => Err(JobRefusal::invalid_params(
                "params.shard",
                "cannot be combined with params.package; name exactly one of shard, package, \
                 recipe, scoped_test or candidate_receipt",
            )),
            (None, None) => Err(JobRefusal::invalid_params(
                "params",
                "a test job needs exactly one of shard, package, recipe, scoped_test or \
                 candidate_receipt (for acceptance use {\"scoped_test\":{\"base\":\"origin/rolling\"}})",
            )),
        }
    }
}

impl BuildJobParams {
    fn validate(&self) -> Result<(), JobRefusal> {
        if let Some(percent) = self.cpu_quota_percent
            && !cpu_quota_percent_in_bounds(percent)
        {
            return Err(JobRefusal::invalid_params(
                "params.cpu_quota_percent",
                format!(
                    "{percent} is outside {JOB_CPU_QUOTA_MIN_PERCENT}-{JOB_CPU_QUOTA_MAX_PERCENT}; \
                     use a percent of one CPU in that range or omit it"
                ),
            ));
        }
        if self
            .sha
            .as_deref()
            .is_some_and(|sha| !full_lower_hex_oid(sha))
        {
            return Err(JobRefusal::invalid_params(
                "params.sha",
                "must be a full 40-character lowercase hex commit id; omit it to build HEAD",
            ));
        }
        if !self.release_members.is_empty() {
            return self.validate_release_members();
        }
        if self.command == BuildCommand::TestNoRun {
            return self.validate_test_no_run();
        }
        if self.no_default_features {
            return Err(JobRefusal::invalid_params(
                "params.no_default_features",
                "is for command test_no_run only; drop it on a check or build job",
            ));
        }
        if !self.shards.is_empty() {
            return Err(JobRefusal::invalid_params(
                "params.shards",
                "is for command test_no_run only; drop it on a check or build job",
            ));
        }
        if let Some(reason) = self
            .build_features_refusal()
            .or_else(|| self.build_bins_refusal())
        {
            return Err(JobRefusal::invalid_params_text(reason));
        }
        match (&self.package, self.workspace) {
            (Some(package), false) if bare_token(package, &['.']) => Ok(()),
            (Some(_), false) => Err(JobRefusal::invalid_params(
                "params.package",
                "must be a workspace member name (letters, digits, `_`, `-`, `.`)",
            )),
            (None, true) => Ok(()),
            _ => Err(JobRefusal::invalid_params(
                "params.package",
                "a check or build job needs exactly one of params.package (a workspace member) \
                 or params.workspace true",
            )),
        }
    }
}

impl BuildJobParams {
    fn validate_release_members(&self) -> Result<(), JobRefusal> {
        if self.command != BuildCommand::Build
            || !self.release
            || self.sha.is_none()
            || self.package.is_some()
            || self.workspace
            || self.all_targets
            || self.no_default_features
            || !self.features.is_empty()
            || !self.bins.is_empty()
            || !self.shards.is_empty()
        {
            return Err(JobRefusal::invalid_params(
                "params.release_members",
                "needs command build, release true and sha; omit package, workspace, all_targets, no_default_features, features, bins and shards",
            ));
        }
        let mut packages = std::collections::BTreeSet::new();
        let mut bins = std::collections::BTreeSet::new();
        if self.release_members.len() > JOB_MAX_RELEASE_MEMBERS
            || self.release_members.iter().any(|member| {
                !bare_token(&member.package, &['.'])
                    || !packages.insert(member.package.as_str())
                    || member.bins.is_empty()
                    || member
                        .bins
                        .iter()
                        .any(|bin| !bare_token(bin, &['.']) || !bins.insert(bin.as_str()))
            })
            || bins.len() > JOB_MAX_RELEASE_BINS
        {
            return Err(JobRefusal::invalid_params(
                "params.release_members",
                format!(
                    "needs distinct package names and nonempty, globally distinct bins (at most {JOB_MAX_RELEASE_MEMBERS} packages and {JOB_MAX_RELEASE_BINS} bins); use bare names"
                ),
            ));
        }
        Ok(())
    }

    /// Why `features` is refused on a `check`/`build` job, naming the allowed
    /// features; `None` when it is empty or every entry is allowed for the package.
    #[must_use]
    pub fn build_features_refusal(&self) -> Option<String> {
        if self.features.is_empty() || self.command == BuildCommand::TestNoRun {
            return None;
        }
        let package = self.package.as_deref().filter(|_| !self.workspace);
        dev_features_refusal(&self.features, package, self.release, self.command.as_str())
    }

    /// Why `bins` is refused on a `check`/`build` job; `None` when it is empty
    /// or valid (needs `package`, bare names, at most [`JOB_MAX_BUILD_BINS`],
    /// no duplicates).
    #[must_use]
    pub fn build_bins_refusal(&self) -> Option<String> {
        if self.bins.is_empty() {
            return None;
        }
        if self.command == BuildCommand::TestNoRun || self.package.is_none() || self.workspace {
            return Some("params.bins needs params.package on a check or build job".to_string());
        }
        let mut seen = std::collections::BTreeSet::new();
        let ok = self.bins.len() <= JOB_MAX_BUILD_BINS
            && self
                .bins
                .iter()
                .all(|b| bare_token(b, &['.']) && seen.insert(b.as_str()));
        (!ok).then(|| {
            format!(
                "params.bins must be at most {JOB_MAX_BUILD_BINS} distinct binary names \
                 (letters, digits, `_`, `-`, `.`)"
            )
        })
    }

    /// #2692: `test_no_run` names one package and either one feature set
    /// (`features`) or a shard sweep (`shards`), never both; features are
    /// `test-shard-*` names so the job cannot enable an arbitrary feature.
    fn validate_test_no_run(&self) -> Result<(), JobRefusal> {
        if self.workspace || !self.bins.is_empty() || self.all_targets || self.release {
            return Err(JobRefusal::invalid_params(
                "params.command",
                "test_no_run takes one params.package and features or shards only; drop \
                 params.workspace, bins, all_targets and release",
            ));
        }
        if !self
            .package
            .as_deref()
            .is_some_and(|package| bare_token(package, &['.']))
        {
            return Err(JobRefusal::invalid_params(
                "params.package",
                "test_no_run needs a workspace member name (letters, digits, `_`, `-`, `.`)",
            ));
        }
        if self.features.len() > JOB_MAX_SHARD_SWEEP {
            return Err(JobRefusal::invalid_params(
                "params.features",
                format!("at most {JOB_MAX_SHARD_SWEEP} features are accepted"),
            ));
        }
        if let Some(bad) = self.features.iter().find(|f| !shard_feature(f)) {
            return Err(JobRefusal::invalid_params(
                "params.features",
                format!("entry `{bad}` must be a `test-shard-NAME` feature"),
            ));
        }
        if self.shards.len() > JOB_MAX_SHARD_SWEEP {
            return Err(JobRefusal::invalid_params(
                "params.shards",
                format!("at most {JOB_MAX_SHARD_SWEEP} shards are accepted"),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        if let Some(bad) = self
            .shards
            .iter()
            .find(|shard| !bare_token(shard, &[]) || !seen.insert(shard.as_str()))
        {
            return Err(JobRefusal::invalid_params(
                "params.shards",
                format!("entry `{bad}` must be a distinct shard name (letters, digits, `_`, `-`)"),
            ));
        }
        if !self.features.is_empty() && !self.shards.is_empty() {
            return Err(JobRefusal::invalid_params(
                "params.shards",
                "is mutually exclusive with params.features; name one of them",
            ));
        }
        Ok(())
    }
}

/// A `test-shard-NAME` feature (no `package/feature` form).
fn shard_feature(value: &str) -> bool {
    value
        .strip_prefix("test-shard-")
        .is_some_and(|name| bare_token(name, &[]))
}

impl LandingJobParams {
    fn validate(&self) -> Result<(), JobRefusal> {
        if !full_lower_hex_oid(&self.accepted) {
            return Err(JobRefusal::invalid_params(
                "params.accepted",
                "must be the full 40-character lowercase hex SHA of the accepted commit",
            ));
        }
        if self.test_filters.len() > JOB_MAX_TEST_FILTERS {
            return Err(JobRefusal::invalid_params(
                "params.test_filters",
                format!("at most {JOB_MAX_TEST_FILTERS} filters are accepted"),
            ));
        }
        if let Some(bad) = self.test_filters.iter().find(|f| !lander_filter(f)) {
            return Err(JobRefusal::invalid_params(
                "params.test_filters",
                format!(
                    "entry `{bad}` must be PACKAGE=FILTER (both parts nonempty, no leading `-`)"
                ),
            ));
        }
        Ok(())
    }
}

impl CloudSweepJobParams {
    fn validate(&self) -> Result<(), JobRefusal> {
        full_lower_hex_oid(&self.sha).then_some(()).ok_or_else(|| {
            JobRefusal::invalid_params(
                "params.sha",
                "must be the full 40-character lowercase hex SHA of the current rolling tip",
            )
        })
    }
}

impl BenchJobParams {
    fn validate(&self) -> Result<(), JobRefusal> {
        if !bench_config_path_ok(&self.config) {
            return Err(JobRefusal::invalid_params(
                "params.config",
                format!(
                    "must name a committed run config directly under {JOB_BENCH_CONFIG_DIR} \
                     (for example {JOB_BENCH_CONFIG_DIR}smoke-oracle.toml) or a candidate's \
                     {JOB_BENCH_CANDIDATES_DIR}<id>/{JOB_BENCH_CANDIDATE_CONFIG}"
                ),
            ));
        }
        if !(self.budget_usd.is_finite()
            && self.budget_usd > 0.0
            && self.budget_usd <= JOB_BENCH_MAX_BUDGET_USD)
        {
            return Err(JobRefusal::invalid_params(
                "params.budget_usd",
                format!("must be a number above 0 and at most {JOB_BENCH_MAX_BUDGET_USD}"),
            ));
        }
        Ok(())
    }
}

/// `eval/bench/tb2/runs/<name>.toml` (a plain file name under the runs directory) or
/// `eval/bench/tb2/candidates/<id>/run.toml` (#3191), never a path that leaves them.
#[must_use]
pub fn bench_config_path_ok(path: &str) -> bool {
    if path.len() > JOB_MAX_TOKEN_BYTES {
        return false;
    }
    if let Some(stem) = path
        .strip_prefix(JOB_BENCH_CONFIG_DIR)
        .and_then(|name| name.strip_suffix(".toml"))
    {
        return bare_token(stem, &['.']) && !stem.contains("..");
    }
    path.strip_prefix(JOB_BENCH_CANDIDATES_DIR)
        .and_then(|rest| rest.strip_suffix(JOB_BENCH_CANDIDATE_CONFIG))
        .and_then(|id| id.strip_suffix('/'))
        .is_some_and(|id| bare_token(id, &['.']) && !id.starts_with('.') && !id.contains(".."))
}

impl AgentSubmitJobRequestV1 {
    /// Validate the name, replay key and per-kind parameters.
    ///
    /// # Errors
    /// The stable `job_*` code for the first invalid field.
    pub fn typed_params(&self) -> Result<JobParams, &'static str> {
        self.typed_params_detailed().map_err(|refusal| refusal.code)
    }

    /// [`Self::typed_params`] with the reason for a `job_invalid_params`
    /// refusal: the offending field and a one-line fix.
    ///
    /// # Errors
    /// The refusal for the first invalid field.
    pub fn typed_params_detailed(&self) -> Result<JobParams, JobRefusal> {
        if let Some(name) = &self.name
            && (name.is_empty()
                || name.len() > JOB_MAX_NAME_BYTES
                || name.chars().any(char::is_control))
        {
            return Err(JOB_NAME_INVALID.into());
        }
        if let Some(key) = &self.idempotency_key
            && (key.is_empty() || key.len() > JOB_MAX_IDEMPOTENCY_BYTES || key.contains('\0'))
        {
            return Err(JOB_KEY_INVALID.into());
        }
        let params = if self.params.is_null() {
            serde_json::json!({})
        } else {
            self.params.clone()
        };
        if !params.is_object() {
            return Err(JobRefusal::invalid_params(
                "params",
                "must be a JSON object of typed fields (for example {\"scoped_test\":{\"base\":\"origin/rolling\"}})",
            ));
        }
        if self.sandbox_session_id.is_some()
            && (!matches!(self.kind, JobKind::Landing | JobKind::CloudGate)
                || self.worktree.is_some())
        {
            return Err(JobRefusal::source_session_scope());
        }
        if self
            .worktree
            .as_deref()
            .is_some_and(|w| !w.starts_with('/'))
        {
            return Err(JobRefusal::worktree_not_absolute());
        }
        let decode = |kind: &str| {
            let kind = kind.to_string();
            move |error: serde_json::Error| {
                JobRefusal::invalid_params(
                    "params",
                    format!(
                        "does not match the {kind} job fields ({error}); see \
                         AgentGetAuthorityCatalog verb AgentSubmitJob"
                    ),
                )
            }
        };
        Ok(match self.kind {
            JobKind::Test => {
                let p: TestJobParams = serde_json::from_value(params).map_err(decode("test"))?;
                p.validate()?;
                if p.qa_lane.is_some()
                    && (self.worktree.is_some() || self.sandbox_session_id.is_some())
                {
                    return Err(JobRefusal::qa_lane_scope());
                }
                JobParams::Test(p)
            }
            JobKind::Build => {
                let p: BuildJobParams = serde_json::from_value(params).map_err(decode("build"))?;
                p.validate()?;
                JobParams::Build(p)
            }
            JobKind::Landing => {
                let p: LandingJobParams =
                    serde_json::from_value(params).map_err(decode("landing"))?;
                p.validate()?;
                JobParams::Landing(p)
            }
            JobKind::CloudGate => {
                let p: LandingJobParams =
                    serde_json::from_value(params).map_err(decode("cloud_gate"))?;
                p.validate()?;
                JobParams::CloudGate(p)
            }
            JobKind::CloudSweep => {
                let p: CloudSweepJobParams =
                    serde_json::from_value(params).map_err(decode("cloud_sweep"))?;
                p.validate()?;
                JobParams::CloudSweep(p)
            }
            JobKind::Bench => {
                let p: BenchJobParams = serde_json::from_value(params).map_err(decode("bench"))?;
                p.validate()?;
                JobParams::Bench(p)
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
        match self.typed_params_detailed() {
            Err(refusal) if refusal.code == code => refusal.detail(),
            _ => code.to_string(),
        }
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
    /// The `build` lane's receipt (#3844); absent for the QA sweep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<CloudBuildReceiptV1>,
}

/// One step of the build lane and its exit code (`skipped` when an earlier step
/// failed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudBuildStepV1 {
    pub step: String,
    pub exit: String,
}

/// What the `build` lane records: the exit codes it classified the verdict from
/// and what the spend ledger says about the host. Money is kept as the ledger's
/// own decimal text, never re-computed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudBuildReceiptV1 {
    /// The step (`build` or `test`) whose exit code made the run RED, or the
    /// setup step (`checkout`, `toolchain`) that left it INCOMPLETE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_step: Option<String>,
    #[serde(default)]
    pub steps: Vec<CloudBuildStepV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_type: Option<String>,
    /// Host seconds the ledger's closing line recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<u64>,
    /// `reserved $X` of the run's reservation line, in USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_usd: Option<String>,
    /// `est compute $X` of the closing line, in USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_usd: Option<String>,
    /// `ok` or `FAILED`: the closing line's `terraform destroy` word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destroy: Option<String>,
    /// The closing line's `leftover gate instances:` value (`none` when clean).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leftover: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

/// Daemon-observed scoped-test tool identity, independent of script output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopedToolSource {
    Rolling,
    Candidate,
    Sandbox,
}

/// One file in the selected tool set; blob hashes the bytes actually launched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedToolFileV1 {
    pub path: String,
    pub blob: Option<String>,
}

/// Recorded by the daemon before launch in agent_jobs.result_json, preserved
/// through settlement/restart. Never populated from a receipt or environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedToolProvenanceV1 {
    pub source: ScopedToolSource,
    pub path: String,
    pub rev: Option<String>,
    pub files: Vec<ScopedToolFileV1>,
}

/// Typed result delivered once to the owner. Absent fields did not apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileVerificationBindingV1 {
    pub version: u32,
    pub head: String,
    pub cwd: String,
    pub clean: bool,
    pub profile_digest: String,
    /// Absolute daemon-selected executables and SHA-256 of their bytes.
    pub tools: Vec<ScopedToolFileV1>,
    /// Exact argv selected by the daemon; project output cannot supply it.
    pub argv: Vec<String>,
    pub commands: u32,
    pub warning_policy: String,
}

/// A recipe is an opaque verification command, not an individual test count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileVerificationV1 {
    pub binding: ProfileVerificationBindingV1,
    pub completed_commands: u32,
    /// `generic_command_exit` applies to generic projects; never a Rust waiver.
    pub warning_verdict: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentJobResultV1 {
    /// Daemon-only profile evidence. Never parsed from a project log/receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_verification: Option<ProfileVerificationV1>,
    pub exit_code: Option<i32>,
    /// #618: the typed infrastructure class of a failure that is not the job's own
    /// (`infrastructure_admission_timeout`); the receipt, when there is one, carries
    /// the same value. Absent on every other result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
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
    /// `log_dir`, per-package `status`/`completed`, `failures`, optional
    /// `failure_class: "infrastructure_interrupted"` for signal-only compile/test
    /// failures with `ok: false`); the
    /// Previously submitted `scoped-test*` recipes settle the same way and `failing_tests` then
    /// lists the receipt's exact failures (#1795). `test` jobs with
    /// `candidate_receipt` (#1099): the typed candidate
    /// receipt `scripts/check-touched-shards` produced (base, head, merge
    /// clean, shards compiled, audit verdicts, migrations, suggested filters).
    #[serde(default)]
    pub receipt: Option<serde_json::Value>,
    /// Daemon-owned tool-under-test evidence; receipt.tool is self-reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoped_tool: Option<ScopedToolProvenanceV1>,
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
    /// #282: `build` jobs: the cargo target directory the unit really built
    /// in (the daemon's `rsi-build-target` log line), so a consumer such as
    /// `scripts/install-release.sh --build-receipt` finds `release/` there and
    /// not in its own caller's `CARGO_TARGET_DIR`. Absent when the log names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_dir: Option<String>,
    /// Successful aggregate release builds: every package/bin compiled within
    /// this result's source fence and target. Staging requires inventory coverage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub release_members: Vec<ReleaseBuildMember>,
    /// #3181: `bench` jobs: the run summary (record path, trial counts, spend,
    /// the stop reason when the run did not complete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench: Option<BenchJobResultV1>,
}

/// One shard of a `bench` run: its `scripts/bench-transport` outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchShardResultV1 {
    pub shard: String,
    /// `ok`, `failed` or `skipped` (the run stopped before it).
    pub status: String,
    /// Harbor trials the shard produced.
    pub trials: u32,
}

/// #3181: the typed result of a `bench` job. Money is in micro-USD so the
/// result stays `Eq`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchJobResultV1 {
    pub run_id: String,
    pub config: String,
    pub config_digest: String,
    /// `~/.rsi/bench/<run>/record.json`, when the record was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_path: Option<String>,
    /// The record's `observation_fingerprint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_fingerprint: Option<String>,
    #[serde(default)]
    pub shards: Vec<BenchShardResultV1>,
    /// Expected trials (tasks x k).
    pub expected_trials: u32,
    /// Trials with a result (Harbor ran them).
    pub trials: u32,
    pub valid_trials: u32,
    pub passed_trials: u32,
    pub invalid_trials: u32,
    pub infra_interrupted_trials: u32,
    /// Expected trials that never ran (the run stopped first).
    pub not_run_trials: u32,
    /// Spent by the run's sessions (daemon increments).
    pub spent_micro_usd: u64,
    pub budget_micro_usd: u64,
    /// Why the run did not complete: `budget_exhausted`, `cancelled`,
    /// `pin_check_failed`, `transport_failed`. Absent for a complete run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
}

/// #582: why a live job is waiting on host capacity, read from the governor
/// when the job is reported (never stored). Today the one reason is
/// [`JOB_HELD_FOR_DISK_FLOOR`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobHoldV1 {
    pub reason: String,
    /// Free bytes on the build filesystem at the governor's last sample.
    pub free_bytes: u64,
    /// The effective disk floor in bytes.
    pub floor_bytes: u64,
    /// Terminal-worker build caches the reclaim sweep would take now, counted
    /// up to a bound ([`JOB_HOLD_RECLAIM_COUNT_MAX`]); 0 means reclaim cannot
    /// help and the owner or manager must free space.
    pub reclaimable_candidates: u32,
}

/// The reclaimable-candidate count on a [`JobHoldV1`] stops at this bound.
pub const JOB_HOLD_RECLAIM_COUNT_MAX: u32 = 32;

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
    /// drain or a staged deploy holds job starts (#2190, #3373), or
    /// `sandbox_job_queued` behind an earlier local test/build job of the same
    /// owner. Absent once it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<String>,
    /// #582: the typed capacity hold of a running job, projected live from the
    /// governor (`reason: disk_floor`, free bytes, floor, reclaimable
    /// candidates). Absent when nothing holds the job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<JobHoldV1>,
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

impl AgentListJobsResultV1 {
    /// The wire value `AgentListJobs` returns for `fields`: the whole jobs
    /// (`full`) or the bounded rows of [`AgentJobV1::compact_row`] (hub #3889).
    pub fn render(&self, fields: AgentJobListFieldsV1) -> serde_json::Value {
        match fields {
            AgentJobListFieldsV1::Full => serde_json::json!({ "jobs": self.jobs }),
            AgentJobListFieldsV1::Compact => serde_json::json!({
                "view": "compact",
                "jobs": self.jobs.iter().map(AgentJobV1::compact_row).collect::<Vec<_>>(),
            }),
        }
    }
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

    /// #1638/#579: bare refs and a boolean tool policy, no test filters.
    #[test]
    fn scoped_test_jobs_take_only_bare_refs() {
        let ok = |params| request(JobKind::Test, params).typed_params().is_ok();
        assert!(ok(json!({"scoped_test":{"base":"origin/rolling"}})));
        let forced = request(
            JobKind::Test,
            json!({"scoped_test":{"base":"origin/rolling","rolling_tool":true}}),
        )
        .typed_params()
        .unwrap();
        assert!(
            matches!(forced, JobParams::Test(ref p) if p.scoped_test.as_ref().unwrap().rolling_tool)
        );
        assert!(!ok(
            json!({"scoped_test":{"base":"origin/rolling","rolling_tool":"true"}})
        ));
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

    /// #3844: the build lane is a `lane` of `cloud_sweep`; an absent lane is the
    /// original QA sweep and is stored absent, so existing rows and requests
    /// are byte-for-byte what they were.
    #[test]
    fn cloud_sweep_lane_is_optional_qa_or_build_and_nothing_else() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let typed = |params| request(JobKind::CloudSweep, params).typed_params();
        let default = typed(json!({"sha":oid})).unwrap();
        assert!(matches!(&default, JobParams::CloudSweep(p) if p.lane == SweepLane::Qa));
        assert_eq!(
            serde_json::to_value(&default).unwrap(),
            json!({"kind":"cloud_sweep","sha":oid})
        );
        assert_eq!(typed(json!({"sha":oid,"lane":"qa"})).unwrap(), default);
        let build = typed(json!({"sha":oid,"lane":"build"})).unwrap();
        assert!(matches!(&build, JobParams::CloudSweep(p) if p.lane == SweepLane::Build));
        assert_eq!(
            serde_json::to_value(&build).unwrap(),
            json!({"kind":"cloud_sweep","sha":oid,"lane":"build"})
        );
        for bad in [
            json!({"sha":oid,"lane":"deploy"}),
            json!({"sha":oid,"lane":"BUILD"}),
            json!({"sha":oid,"lane":null}),
            json!({"sha":oid,"lane":["build"]}),
            json!({"sha":oid,"lane":"build","extra":1}),
            json!({"lane":"build"}),
        ] {
            assert_eq!(typed(bad.clone()).unwrap_err(), JOB_INVALID_PARAMS, "{bad}");
        }
        // Authority is per kind: the build lane needs no new permission.
        assert_eq!(build.kind(), JobKind::CloudSweep);
    }

    #[test]
    fn bench_params_are_a_committed_runs_config_and_a_finite_positive_budget() {
        let ok = request(
            JobKind::Bench,
            json!({"config":"eval/bench/tb2/runs/smoke-oracle.toml","budget_usd":6.0}),
        )
        .typed_params();
        let Ok(JobParams::Bench(bench)) = ok else {
            panic!("a runs config with a budget is accepted: {ok:?}");
        };
        assert_eq!(bench.config, "eval/bench/tb2/runs/smoke-oracle.toml");
        assert_eq!(bench.budget_usd, 6.0);
        assert_eq!(JobParams::Bench(bench).kind(), JobKind::Bench);
        assert_eq!(JobKind::Bench.as_str(), "bench");
        assert_eq!(JobKind::parse("bench"), Some(JobKind::Bench));
        // #3191: a candidate's run config is accepted too, and nothing else under candidates/.
        let candidate = "eval/bench/tb2/candidates/c03-1a2b3c4d5e6f/run.toml";
        assert!(bench_config_path_ok(candidate));
        assert!(matches!(
            request(
                JobKind::Bench,
                json!({"config": candidate, "budget_usd": 2.0}),
            )
            .typed_params(),
            Ok(JobParams::Bench(_))
        ));
        for bad in [
            "eval/bench/tb2/candidates/run.toml",
            "eval/bench/tb2/candidates/c03/overlay.toml",
            "eval/bench/tb2/candidates/c03/sub/run.toml",
            "eval/bench/tb2/candidates/../runs/run.toml",
            "eval/bench/tb2/candidates/.c03/run.toml",
            "eval/bench/tb2/candidates/-c03/run.toml",
            "eval/bench/tb2/candidates//run.toml",
            "/eval/bench/tb2/candidates/c03/run.toml",
        ] {
            assert!(!bench_config_path_ok(bad), "{bad}");
        }
        for bad in [
            // Not under the runs directory, or not a plain toml file name.
            json!({"config":"eval/bench/tb2/dataset.toml","budget_usd":1.0}),
            json!({"config":"eval/bench/tb2/runs/../dataset.toml","budget_usd":1.0}),
            json!({"config":"eval/bench/tb2/runs/sub/x.toml","budget_usd":1.0}),
            json!({"config":"eval/bench/tb2/runs/x.json","budget_usd":1.0}),
            json!({"config":"eval/bench/tb2/runs/.toml","budget_usd":1.0}),
            json!({"config":"eval/bench/tb2/runs/-x.toml","budget_usd":1.0}),
            json!({"config":"/abs/eval/bench/tb2/runs/x.toml","budget_usd":1.0}),
            // The budget is a positive finite number within the sanity bound.
            json!({"config":"eval/bench/tb2/runs/x.toml","budget_usd":0}),
            json!({"config":"eval/bench/tb2/runs/x.toml","budget_usd":-1.0}),
            json!({"config":"eval/bench/tb2/runs/x.toml","budget_usd":1000.01}),
            json!({"config":"eval/bench/tb2/runs/x.toml","budget_usd":"1"}),
            json!({"config":"eval/bench/tb2/runs/x.toml"}),
            json!({"budget_usd":1.0}),
            // No other field: the agent supplies no command, host or model.
            json!({"config":"eval/bench/tb2/runs/x.toml","budget_usd":1.0,"host":"ssh://x"}),
            json!({}),
        ] {
            assert_eq!(
                request(JobKind::Bench, bad.clone()).typed_params(),
                Err(JOB_INVALID_PARAMS),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_bench_result_round_trips_and_stays_absent_on_other_jobs() {
        let result = AgentJobResultV1 {
            bench: Some(BenchJobResultV1 {
                run_id: "r".into(),
                config: "eval/bench/tb2/runs/x.toml".into(),
                config_digest: "sha256:c".into(),
                record_path: Some("/b/r/record.json".into()),
                observation_fingerprint: Some("sha256:f".into()),
                shards: vec![BenchShardResultV1 {
                    shard: "search-01".into(),
                    status: "ok".into(),
                    trials: 3,
                }],
                expected_trials: 4,
                trials: 3,
                valid_trials: 3,
                passed_trials: 2,
                invalid_trials: 1,
                infra_interrupted_trials: 0,
                not_run_trials: 1,
                spent_micro_usd: 420_000,
                budget_micro_usd: 6_000_000,
                stopped: Some("budget_exhausted".into()),
            }),
            ..AgentJobResultV1::default()
        };
        let wire = serde_json::to_value(&result).unwrap();
        assert_eq!(wire["bench"]["stopped"], "budget_exhausted");
        assert_eq!(wire["bench"]["spent_micro_usd"], 420_000);
        assert_eq!(
            serde_json::from_value::<AgentJobResultV1>(wire).unwrap(),
            result
        );
        let plain = serde_json::to_value(AgentJobResultV1::default()).unwrap();
        assert!(plain.get("bench").is_none(), "{plain}");
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
    fn release_members_require_a_pinned_isolated_explicit_build() {
        let valid = json!({"command":"build","release":true,"sha":"a".repeat(40),
            "release_members":[{"package":"server","bins":["server","cli"]},
                {"package":"guards","bins":["guard"]}]});
        assert!(
            request(JobKind::Build, valid.clone())
                .typed_params()
                .is_ok()
        );
        let many = json!({"command":"build","release":true,"sha":"a".repeat(40),
            "release_members":[{"package":"tools","bins":(0..14).map(|n|format!("tool-{n}")).collect::<Vec<_>>()}]});
        assert!(request(JobKind::Build, many).typed_params().is_ok());
        for extra in [
            json!({"command":"check"}),
            json!({"command":"test_no_run"}),
            json!({"release":false}),
            json!({"sha":null}),
            json!({"sha":"bad"}),
            json!({"workspace":true}),
            json!({"package":"server"}),
            json!({"all_targets":true}),
            json!({"no_default_features":true}),
            json!({"features":["preserve_order"]}),
            json!({"bins":["server"]}),
            json!({"shards":["other-01"]}),
            json!({"release_members":[{"package":"server","bins":[]}]}),
            json!({"release_members":[{"package":"--workspace","bins":["server"]}]}),
            json!({"release_members":[{"package":"server","bins":["$(false)"]}]}),
            json!({"release_members":[{"package":"server","bins":["server"]},{"package":"server","bins":["cli"]}]}),
            json!({"release_members":[{"package":"server","bins":["server"]},{"package":"guards","bins":["server"]}]}),
            json!({"release_members":[{"package":"server","bins":["server"],"features":["anything"]}]}),
            json!({"release_members":(0..=JOB_MAX_RELEASE_MEMBERS).map(|n|json!({"package":format!("p-{n}"),"bins":[format!("b-{n}")]})).collect::<Vec<_>>()}),
            json!({"release_members":[{"package":"tools","bins":(0..=JOB_MAX_RELEASE_BINS).map(|n|format!("tool-{n}")).collect::<Vec<_>>()}]}),
        ] {
            let mut bad = valid.clone();
            bad.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(
                request(JobKind::Build, bad.clone()).typed_params().is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn build_job_admits_allowlisted_dev_features_and_bins() {
        let ok = request(
            JobKind::Build,
            json!({"command":"build","package":"rsid","features":["e2e-seed"],
                   "bins":["rsid","rsi-e2e-seed"]}),
        )
        .typed_params();
        let Ok(JobParams::Build(ok)) = ok else {
            panic!("an allowlisted feature and bins are accepted: {ok:?}");
        };
        assert_eq!(ok.features, ["e2e-seed"]);
        assert_eq!(ok.bins, ["rsid", "rsi-e2e-seed"]);
        assert!(matches!(
            request(
                JobKind::Build,
                json!({"command":"check","package":"rsid","features":["e2e-seed"]})
            )
            .typed_params(),
            Ok(JobParams::Build(_))
        ));
    }

    #[test]
    fn package_test_job_admits_allowlisted_features_and_refuses_others_like_a_build() {
        let feature = allowed_build_features("rsid")[0];
        let ok = request(
            JobKind::Test,
            json!({"package":"rsid","test_target":"seed_suite","exact":true,
                   "filters":["one"],"features":[feature]}),
        )
        .typed_params();
        let Ok(JobParams::Test(ok)) = ok else {
            panic!("an allowlisted feature on a package test is accepted: {ok:?}");
        };
        assert_eq!(ok.features, [feature]);
        // An ordinary package test keeps its stored form (no `features` key).
        let plain = request(JobKind::Test, json!({"package":"rsid","lib_only":true}));
        let Ok(JobParams::Test(plain)) = plain.typed_params() else {
            panic!("a plain package test is accepted");
        };
        assert!(plain.features.is_empty());
        assert!(
            serde_json::to_value(&plain)
                .unwrap()
                .get("features")
                .is_none()
        );
        for (params, needle) in [
            (
                json!({"package":"rsid","features":["test-ollama"]}),
                "entry `test-ollama` is not allowed for `rsid` (test job)",
            ),
            (
                json!({"package":"rsid-store","features":[feature]}),
                "allowed features for `rsid-store`: none",
            ),
            (
                json!({"package":"rsid","features":[feature, feature]}),
                "names `e2e-seed` twice",
            ),
            (
                json!({"shard":"store-01","features":[feature]}),
                "params.features: needs params.package",
            ),
            (
                json!({"recipe":"check-cpu","features":[feature]}),
                "params.features: needs params.package",
            ),
            (
                json!({"scoped_test":{"base":"origin/rolling"},"features":[feature]}),
                "params.features: needs params.package",
            ),
        ] {
            let req = request(JobKind::Test, params.clone());
            let code = req.typed_params().expect_err(&params.to_string());
            assert_eq!(code, JOB_INVALID_PARAMS, "{params}");
            let text = req.refusal_detail(code);
            assert!(text.contains(needle), "{params}: {text}");
        }
        // The refusal names the allowed set, identical to the build text.
        let text = request(JobKind::Test, json!({"package":"rsid","features":["nope"]}))
            .refusal_detail(JOB_INVALID_PARAMS);
        assert!(
            text.contains("allowed features for `rsid`: e2e-seed"),
            "{text}"
        );
    }

    #[test]
    fn build_job_refuses_other_features_naming_the_allowed_ones() {
        for (params, needle) in [
            (
                json!({"command":"build","package":"rsid","features":["test-ollama"]}),
                "entry `test-ollama` is not allowed for `rsid`",
            ),
            (
                json!({"command":"build","package":"rsid-store","features":["e2e-seed"]}),
                "allowed features for `rsid-store`: none",
            ),
            (
                json!({"command":"build","workspace":true,"features":["e2e-seed"]}),
                "not allowed for `a workspace build`",
            ),
            (
                json!({"command":"build","package":"rsid","release":true,"features":["e2e-seed"]}),
                "refused with release",
            ),
            (
                json!({"command":"build","package":"rsid","features":["e2e-seed","e2e-seed"]}),
                "names `e2e-seed` twice",
            ),
            (
                json!({"command":"build","workspace":true,"bins":["rsid"]}),
                "params.bins needs params.package",
            ),
            (
                json!({"command":"build","package":"rsid","bins":["--release"]}),
                "params.bins must be at most 8",
            ),
        ] {
            let req = request(JobKind::Build, params.clone());
            let code = req.typed_params().expect_err(&params.to_string());
            assert_eq!(code, JOB_INVALID_PARAMS, "{params}");
            let text = req.refusal_detail(code);
            assert!(text.contains(needle), "{params}: {text}");
        }
        let text = request(
            JobKind::Build,
            json!({"command":"build","package":"rsid","features":["nope"]}),
        )
        .refusal_detail(JOB_INVALID_PARAMS);
        assert!(
            text.contains("allowed features for `rsid`: e2e-seed"),
            "{text}"
        );
        // `bins` is not a test_no_run field.
        assert_eq!(
            request(
                JobKind::Build,
                json!({"command":"test_no_run","package":"rsid","bins":["rsid"]})
            )
            .typed_params()
            .unwrap_err(),
            JOB_INVALID_PARAMS
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
    /// #3410: `at_commit` is one exact package test at one full commit id.
    #[test]
    fn at_commit_runs_one_exact_package_test_at_a_full_commit_id() {
        let ok = |value: serde_json::Value| request(JobKind::Test, value).typed_params().is_ok();
        let sha = "a".repeat(40);
        let lib = json!({"at_commit": sha, "package":"rsid", "filters":["a::b"],
            "exact":true, "lib_only":true});
        assert!(ok(lib.clone()));
        assert!(ok(
            json!({"at_commit": sha, "package":"rsid-store", "filters":["t"],
            "exact":true, "test_target":"e2e_seed"})
        ));
        for bad in [
            "",
            "HEAD",
            "origin/rolling",
            "--upload-pack=x",
            "A".repeat(40).as_str(),
            "a".repeat(39).as_str(),
            "a".repeat(41).as_str(),
        ] {
            let mut value = lib.clone();
            value["at_commit"] = json!(bad);
            assert!(!ok(value), "{bad}");
        }
        // It names exactly what runs: a package, whole names, one target.
        let without = |key: &str| {
            let mut value = lib.clone();
            value.as_object_mut().unwrap().remove(key);
            value
        };
        assert!(!ok(without("package")));
        assert!(!ok(without("exact")));
        assert!(!ok(without("filters")));
        assert!(!ok(without("lib_only")));
        let mut both = lib.clone();
        both["test_target"] = json!("e2e_seed");
        assert!(!ok(both));
        let mut e2e = json!({"at_commit": sha, "package":"rsi", "filters":["t"],
            "exact":true, "test_target":"e2e_tui"});
        assert!(!ok(e2e.clone()));
        e2e["test_target"] = json!("other");
        assert!(ok(e2e));
        // It combines with no other test form.
        for (key, extra) in [
            ("shard", json!("other-01")),
            ("recipe", json!("x")),
            ("candidate_receipt", json!("rolling")),
            ("scoped_test", json!({"base":"origin/rolling"})),
            ("filterset", json!("test(x)")),
            ("qa_lane", json!({"sha": "a".repeat(40)})),
        ] {
            let mut value = lib.clone();
            value[key] = extra;
            assert!(!ok(value), "{key}");
        }
        // The shared per-job knobs still apply.
        let mut capped = lib.clone();
        capped["cpu_quota_percent"] = json!(200);
        assert!(ok(capped));
        // Without the field, params round-trip byte-identical (no new key).
        let plain = request(JobKind::Test, json!({"package":"rsid"}))
            .typed_params()
            .unwrap();
        assert!(!serde_json::to_string(&plain).unwrap().contains("at_commit"));
    }

    /// #4019: `bin` selects one binary target for an `at_commit` run and keeps the
    /// `test_target` protections: a bare name, exact whole-test filters, a package
    /// and no other target form.
    #[test]
    fn bin_selects_one_binary_target_with_the_test_target_protections() {
        let ok = |value: serde_json::Value| request(JobKind::Test, value).typed_params().is_ok();
        let sha = "a".repeat(40);
        let at = json!({"at_commit": sha, "package":"rsid", "filters":["tests::t"],
            "exact":true, "bin":"rsi-rolling-land"});
        assert!(ok(at.clone()));
        // The plain package form takes it too.
        let mut plain = at.clone();
        plain.as_object_mut().unwrap().remove("at_commit");
        assert!(ok(plain.clone()));
        // It is the one target: not with lib_only or test_target, and at_commit
        // still needs exactly one of the three.
        for (key, extra) in [
            ("lib_only", json!(true)),
            ("test_target", json!("seed_suite")),
        ] {
            let mut value = at.clone();
            value[key] = extra;
            assert!(!ok(value), "{key}");
        }
        // Same refusals as test_target: bad names, no exact, no filters, no package.
        for bad in ["", "-x", "../x", "a;id", "a b", "a::b"] {
            let mut value = at.clone();
            value["bin"] = json!(bad);
            assert!(!ok(value), "{bad}");
        }
        for missing in ["package", "exact", "filters"] {
            let mut value = at.clone();
            value.as_object_mut().unwrap().remove(missing);
            assert!(!ok(value), "{missing}");
        }
        let mut empty = at.clone();
        empty["filters"] = json!([]);
        assert!(!ok(empty));
        for (key, extra) in [
            ("shard", json!("other-01")),
            ("recipe", json!("x")),
            ("candidate_receipt", json!("rolling")),
            ("scoped_test", json!({"base":"origin/rolling"})),
            ("filterset", json!("test(x)")),
            ("qa_lane", json!({"sha": "a".repeat(40)})),
        ] {
            let mut value = plain.clone();
            value[key] = extra;
            assert!(!ok(value), "{key}");
        }
        // Without the field, params round-trip byte-identical (no new key).
        let none = request(JobKind::Test, json!({"package":"rsid"}))
            .typed_params()
            .unwrap();
        assert!(!serde_json::to_string(&none).unwrap().contains("\"bin\""));
    }

    /// Kaizen #168: a `job_invalid_params` refusal always names the offending
    /// field and a fix; the code stays `job_invalid_params` for callers.
    #[test]
    fn every_invalid_params_refusal_names_a_field() {
        let sha = "a".repeat(40);
        let session = Uuid::new_v4().to_string();
        // (kind, request fields, expected substring of the reason)
        let cases: Vec<(&str, serde_json::Value, &str)> = vec![
            (
                "test",
                json!({"params": []}),
                "params: must be a JSON object",
            ),
            (
                "test",
                json!({"params": {"bogus": 1}}),
                "params: does not match the test job fields",
            ),
            (
                "test",
                json!({"params": {}}),
                "params: a test job needs exactly one of",
            ),
            (
                "test",
                json!({"params": {"scoped_test": {"base": "origin/rolling"}}, "source_session_id": session}),
                "source_session_id: (alias sandbox_session_id) is only accepted for kind landing and cloud_gate",
            ),
            (
                "test",
                json!({"params": {"scoped_test": {"base": "origin/rolling"}}, "sandbox_session_id": session}),
                "source_session_id:",
            ),
            (
                "landing",
                json!({"params": {"accepted": sha}, "source_session_id": session, "worktree": "/w"}),
                "source_session_id:",
            ),
            (
                "test",
                json!({"params": {"scoped_test": {"base": "origin/rolling"}}, "worktree": "rel/path"}),
                "worktree: must be an absolute path",
            ),
            (
                "test",
                json!({"params": {"shard": "store-01", "qa_lane": {"sha": sha}}, "worktree": "/w"}),
                "params.qa_lane:",
            ),
            (
                "test",
                json!({"params": {"package": "rsid", "test_target": "-x", "exact": true, "filters": ["a"]}}),
                "params.test_target:",
            ),
            (
                "test",
                json!({"params": {"shard": "store-01", "test_target": "t", "exact": true, "filters": ["a"]}}),
                "params.test_target:",
            ),
            (
                "test",
                json!({"params": {"test_target": "t", "exact": true, "filters": ["a"]}}),
                "params.test_target: needs params.package",
            ),
            (
                "test",
                json!({"params": {"package": "rsid", "test_target": "t", "lib_only": true, "exact": true, "filters": ["a"]}}),
                "params.test_target:",
            ),
            (
                "test",
                json!({"params": {"package": "rsid", "test_target": "t"}}),
                "params.test_target:",
            ),
            (
                "test",
                json!({"params": {"package": "rsid", "cpu_quota_percent": 50}}),
                "params.cpu_quota_percent: 50 is outside",
            ),
            (
                "test",
                json!({"params": {"recipe": "r", "cpu_quota_percent": 200}}),
                "params.cpu_quota_percent:",
            ),
            (
                "test",
                json!({"params": {"package": "rsid", "timeout_minutes": 1}}),
                "params.timeout_minutes: 1 is outside",
            ),
            (
                "test",
                json!({"params": {"package": "rsid", "timeout_minutes": 500}}),
                "params.timeout_minutes:",
            ),
            (
                "test",
                json!({"params": {"shard": "nope", "qa_lane": {"sha": sha}}}),
                "params.qa_lane:",
            ),
            (
                "test",
                json!({"params": {"shard": "store-01", "qa_lane": {"sha": "xyz"}}}),
                "params.qa_lane.sha:",
            ),
            (
                "test",
                json!({"params": {"shard": "store-01", "qa_lane": {"sha": sha}, "recipe": "r"}}),
                "params.qa_lane:",
            ),
            (
                "test",
                json!({"params": {"shard": "store-01", "qa_lane": {"sha": sha}, "timeout_minutes": 120}}),
                "params.timeout_minutes:",
            ),
            (
                "test",
                json!({"params": {"scoped_test": {"base": "--x"}}}),
                "params.scoped_test.base:",
            ),
            (
                "test",
                json!({"params": {"scoped_test": {"base": "a", "head": "b..c"}}}),
                "params.scoped_test.head:",
            ),
            (
                "test",
                json!({"params": {"scoped_test": {"base": "a"}, "package": "rsid"}}),
                "params.package:",
            ),
            (
                "test",
                json!({"params": {"scoped_test": {"base": "a"}, "exact": true}}),
                "params.exact:",
            ),
            (
                "test",
                json!({"params": {"recipe": "-r"}}),
                "params.recipe:",
            ),
            (
                "test",
                json!({"params": {"recipe": "r", "shard": "store-01"}}),
                "params.shard:",
            ),
            (
                "test",
                json!({"params": {"recipe": "r", "filters": ["noequals"]}}),
                "params.filters:",
            ),
            (
                "test",
                json!({"params": {"recipe": "r", "filters": vec!["a=b"; JOB_RECIPE_MAX_FILTERS + 1]}}),
                "params.filters:",
            ),
            (
                "test",
                json!({"params": {"candidate_receipt": "a;b"}}),
                "params.candidate_receipt:",
            ),
            (
                "test",
                json!({"params": {"candidate_receipt": "x", "package": "rsid"}}),
                "params.package:",
            ),
            ("test", json!({"params": {"shard": "-x"}}), "params.shard:"),
            (
                "test",
                json!({"params": {"shard": "store-01", "lib_only": true}}),
                "params.shard:",
            ),
            (
                "test",
                json!({"params": {"shard": "store-01", "filterset": "nextest(x)"}}),
                "params.filterset must be",
            ),
            (
                "test",
                json!({"params": {"shard": "store-01", "package": "rsid"}}),
                "params.shard:",
            ),
            (
                "test",
                json!({"params": {"package": "rsid;x"}}),
                "params.package:",
            ),
            (
                "test",
                json!({"params": {"package": "rsid", "filterset": "test(a)"}}),
                "params.filterset:",
            ),
            (
                "test",
                json!({"params": {"package": "rsid", "filters": ["-x"]}}),
                "params.filters:",
            ),
            (
                "test",
                json!({"params": {"package": "rsid", "filters": vec!["a"; JOB_MAX_TEST_FILTERS + 1]}}),
                "params.filters:",
            ),
            (
                "build",
                json!({"params": {"command": "check", "package": "rsid", "cpu_quota_percent": 5000}}),
                "params.cpu_quota_percent:",
            ),
            (
                "build",
                json!({"params": {"command": "check", "package": "rsid", "sha": "short"}}),
                "params.sha:",
            ),
            (
                "build",
                json!({"params": {"command": "check", "package": "rsid", "no_default_features": true}}),
                "params.no_default_features:",
            ),
            (
                "build",
                json!({"params": {"command": "check", "package": "rsid", "shards": ["store-01"]}}),
                "params.shards:",
            ),
            (
                "build",
                json!({"params": {"command": "check", "package": "rsid", "features": ["nope"]}}),
                "params.features: entry `nope` is not allowed for `rsid` (check job); drop it or \
                 use one of the allowed features for `rsid`: e2e-seed",
            ),
            (
                "build",
                json!({"params": {"command": "build", "package": "rsid", "features": ["e2e-seed", "e2e-seed"]}}),
                "params.features: names `e2e-seed` twice; list each feature once",
            ),
            (
                "build",
                json!({"params": {"command": "build", "package": "rsid", "release": true, "features": ["e2e-seed"]}}),
                "params.features: refused with release",
            ),
            (
                "build",
                json!({"params": {"command": "build", "workspace": true, "features": ["e2e-seed"]}}),
                "params.features: entry `e2e-seed` is not allowed for `a workspace build`",
            ),
            (
                "test",
                json!({"params": {"package": "rsid", "features": ["nope"]}}),
                "params.features: entry `nope` is not allowed for `rsid` (test job); drop it or \
                 use one of the allowed features for `rsid`: e2e-seed",
            ),
            (
                "test",
                json!({"params": {"package": "rsid-store", "features": ["e2e-seed"]}}),
                "allowed features for `rsid-store`: none",
            ),
            (
                "test",
                json!({"params": {"package": "rsid", "features": ["e2e-seed", "e2e-seed"]}}),
                "params.features: names `e2e-seed` twice; list each feature once",
            ),
            (
                "test",
                json!({"params": {"shard": "store-01", "features": ["e2e-seed"]}}),
                "params.features: needs params.package on a test job",
            ),
            (
                "test",
                json!({"params": {"scoped_test": {"base": "origin/rolling"}, "features": ["e2e-seed"]}}),
                "params.features: needs params.package on a test job",
            ),
            (
                "build",
                json!({"params": {"command": "check", "package": "rsid", "bins": ["a"], "workspace": true}}),
                "params.bins",
            ),
            (
                "build",
                json!({"params": {"command": "check", "package": "bad name"}}),
                "params.package:",
            ),
            (
                "build",
                json!({"params": {"command": "check"}}),
                "params.package:",
            ),
            (
                "build",
                json!({"params": {"command": "test_no_run", "package": "rsid", "release": true}}),
                "params.command:",
            ),
            (
                "build",
                json!({"params": {"command": "test_no_run"}}),
                "params.package:",
            ),
            (
                "build",
                json!({"params": {"command": "test_no_run", "package": "rsid", "features": ["other"]}}),
                "params.features:",
            ),
            (
                "build",
                json!({"params": {"command": "test_no_run", "package": "rsid", "features": vec!["test-shard-a"; JOB_MAX_SHARD_SWEEP + 1]}}),
                "params.features:",
            ),
            (
                "build",
                json!({"params": {"command": "test_no_run", "package": "rsid", "shards": ["a", "a"]}}),
                "params.shards:",
            ),
            (
                "build",
                json!({"params": {"command": "test_no_run", "package": "rsid", "shards": ["a"], "features": ["test-shard-b"]}}),
                "params.shards:",
            ),
            (
                "landing",
                json!({"params": {"accepted": "abc"}}),
                "params.accepted:",
            ),
            (
                "landing",
                json!({"params": {"accepted": sha, "test_filters": ["noequals"]}}),
                "params.test_filters:",
            ),
            (
                "landing",
                json!({"params": {"accepted": sha, "test_filters": vec!["a=b"; JOB_MAX_TEST_FILTERS + 1]}}),
                "params.test_filters:",
            ),
            (
                "cloud_gate",
                json!({"params": {"accepted": "abc"}}),
                "params.accepted:",
            ),
            (
                "cloud_sweep",
                json!({"params": {"sha": "abc"}}),
                "params.sha:",
            ),
        ];
        for (kind, fields, expect) in cases {
            let mut value = fields.clone();
            value["kind"] = json!(kind);
            let request: AgentSubmitJobRequestV1 = serde_json::from_value(value.clone())
                .unwrap_or_else(|error| panic!("{value}: {error}"));
            let code = request
                .typed_params()
                .expect_err(&format!("{value} must be refused"));
            assert_eq!(code, JOB_INVALID_PARAMS, "{value}");
            let detail = request.refusal_detail(code);
            let reason = detail
                .strip_prefix("job_invalid_params: ")
                .unwrap_or_else(|| panic!("{value}: bare refusal `{detail}`"));
            let field = reason.split([':', ' ']).next().unwrap_or_default();
            assert!(
                field == "params"
                    || field.starts_with("params.")
                    || matches!(field, "source_session_id" | "worktree"),
                "{value}: `{detail}` names no field"
            );
            assert!(
                reason.contains(expect),
                "{value}: `{detail}` lacks `{expect}`"
            );
            assert!(
                reason.len() > field.len() + 8,
                "{value}: `{detail}` gives no fix"
            );
        }
    }

    /// Kaizen #168: no production `job_invalid_params` is constructed bare.
    #[test]
    fn no_production_site_builds_a_bare_job_invalid_params() {
        let own = include_str!("agent_jobs.rs");
        let production = own.split("#[cfg(test)]\nmod tests").next().unwrap();
        for bare in [
            "Err(JOB_INVALID_PARAMS)",
            "ok_or(JOB_INVALID_PARAMS)",
            "|_| JOB_INVALID_PARAMS",
        ] {
            assert!(
                !production.contains(bare),
                "bare refusal `{bare}` in agent_jobs.rs"
            );
        }
        let root = std::path::Path::new(crate::manifest_dir!()).join("../rsid/src");
        for file in ["session/agent_jobs_verb.rs", "agent_jobs/recipe.rs"] {
            let Ok(text) = std::fs::read_to_string(root.join(file)) else {
                continue;
            };
            let production = text.split("#[cfg(test)]").next().unwrap();
            assert!(
                !production.contains("InvalidParam(JOB_INVALID_PARAMS"),
                "bare job_invalid_params in {file}; use job_refusal(JobRefusal::..)"
            );
        }
    }
}
