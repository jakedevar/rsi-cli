//! Daemon-owned durable agent jobs (#1002 slice 1).
//!
//! `AgentSubmitJob` turns typed parameters into a fixed argv, records a
//! `running` job row and starts the argv in a transient systemd unit (Linux)
//! or launchd service (macOS). The service is outside every session scope, so a
//! session's end or a daemon restart does not kill it. The unit's wrapper writes
//! the exit status to a status file as its last act; the runner polls the
//! `running` rows, settles each exactly once (CAS in the store, which also
//! inserts the single owner resume wake) and, on startup, runs the same poll to
//! reconcile jobs that finished while rsid was down.

use crate::error::{DaemonError, Result};
use crate::rolling_queue::{all_failing_tests, landed_tip, tail};
use crate::store::Store;
use crate::store::agent_jobs::{AgentJobRow, NewAgentJob};
use chrono::{DateTime, Utc};
use rsi_common::agent_jobs::{
    AgentJobResultV1, AgentJobV1, BuildJobParams, CloudSweepResultV1, JOB_ADMISSION_TIMED_OUT,
    JOB_CANCELLED, JOB_DIR_NOT_ALLOWED, JOB_FILTER_MATCHED_NO_TESTS, JOB_HELD_FOR_CAPACITY,
    JOB_HELD_FOR_THERMAL, JOB_INTERRUPTED_BY_DEPLOY, JOB_LAUNCH_FAILED, JOB_NOT_FOUND,
    JOB_START_TIMED_OUT, JOB_TEST_TIMEOUT_DEFAULT_MINS, JOB_TIMED_OUT, JobKind, JobParams,
    JobState, JobWake, LandingJobParams, SWEEP_MAX_FAILURES, SweepVerdict, TestJobParams,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

#[cfg(any(target_os = "macos", test))]
mod launchd;

mod recipe;
mod scoped_tool;
mod source;
mod toolchain;

const TICK: Duration = Duration::from_secs(5);
/// A `running` row with no active unit and no status file is only declared
/// `lost` after this long, so a submit between its insert and its launch is
/// never mistaken for a vanished unit.
const LAUNCH_GRACE_SECS: i64 = 60;
/// Disk a test/build job's private TMPDIR may hold before the job is failed
/// with [`JOB_SCRATCH_QUOTA_EXCEEDED`] and the scratch removed. A constant, not
/// an operator setting: it only bounds a runaway job.
const JOB_SCRATCH_QUOTA_BYTES: u64 = 32 * 1024 * MIB;
/// Typed refusal recorded when a job's scratch outgrew the quota.
pub(crate) const JOB_SCRATCH_QUOTA_EXCEEDED: &str = "job_scratch_quota_exceeded";
const LOG_TAIL_READ_BYTES: u64 = 256 * 1024;
const MAX_FAILING_TESTS: usize = 32;
const STRIP_NAMESPACE_ENV: &str = "RSI_PROCESS_OWNERSHIP_NAMESPACE";
/// #1337: a timed test job's unit keeps running this long past its timeout
/// before systemd/launchd stops it, so the daemon's poll (which settles it
/// `job_timed_out`) acts first; the unit cap is only the backstop for a
/// daemon that is down.
const JOB_TIMEOUT_UNIT_GRACE_SECS: u64 = 5 * 60;
/// Independent admission budget for governor, sandbox and artifact-lock waits.
/// #1854: ten minutes bounds pre-build stalls separately from execution.
/// Unit caps carry this allowance; execution time excludes the entire wait.
pub(crate) const JOB_SLOT_WAIT_ALLOWANCE_SECS: u64 = 10 * 60;
/// A never-started unit has an admission bound independent of its test budget.
const JOB_START_WAIT_MAX_SECS: u64 = 10 * 60;
/// The recipe the merge queue's async full-library QA run submits (#2092,
/// #2112, #2120), on the hub and on a satellite alike.
pub(crate) const LIBRARY_QA_RECIPE: &str = "rolling-library-qa";
/// Scoped jobs include admission in their wall budget, retaining two minutes to run.
const SCOPED_JOB_RUNTIME_RESERVE_SECS: u64 = 120;
const SANDBOX_JOB_QUEUED: &str = "sandbox_job_queued";
const JOB_SANDBOX_HOLDER_DEAD: &str = "job_sandbox_holder_dead";
const JOB_SANDBOX_HOLDER_STALLED: &str = "job_sandbox_holder_stalled";

/// #1611: how long past the deploy drain's hold cap a job may stay `queued`
/// (unlaunched) before it is failed `job_admission_timed_out`. A job held for
/// a deploy waits as long as the hold lasts; the clock runs from the later of
/// its submission and the hold's last engagement (#2190), so a job the hold
/// released but that still did not launch within cap + margin is stuck, not
/// waiting.
const JOB_ADMISSION_MARGIN_SECS: i64 = 5 * 60;

/// #1611: the longest a job may wait unlaunched after its hold ended, from the
/// drain's hold cap (the default cap when the drain is uncapped) plus the margin.
fn admission_cap_secs(drain: &crate::deploy_drain::DeployDrain) -> i64 {
    let hold = drain.hold_cap().map_or(
        i64::try_from(rsi_common::agent_deploy::DEPLOY_DRAIN_HOLD_DEFAULT_SECS).unwrap_or(600),
        |cap| cap.num_seconds(),
    );
    hold.saturating_add(JOB_ADMISSION_MARGIN_SECS)
}

/// #1611: whether a queued job has waited unlaunched past `cap_secs`, counted
/// from the later of its submission and `hold_engaged` (#2190).
fn admission_expired(
    job: &AgentJobV1,
    now: DateTime<Utc>,
    cap_secs: i64,
    hold_engaged: DateTime<Utc>,
) -> bool {
    DateTime::parse_from_rfc3339(&job.created_at).is_ok_and(|created| {
        (now - created.with_timezone(&Utc).max(hold_engaged)).num_seconds() > cap_secs
    })
}

/// The typed `job_admission_timed_out` settlement of a job that never launched.
fn admission_timed_out_result(cap_secs: i64) -> AgentJobResultV1 {
    AgentJobResultV1 {
        refusal: Some(JOB_ADMISSION_TIMED_OUT.into()),
        detail: Some(format!(
            "the job was held for more than {} minutes before its unit launched (deploy drain hold cap plus margin) and was failed without running; its execution timeout never started. Resubmit it",
            cap_secs / 60
        )),
        ..AgentJobResultV1::default()
    }
}

fn is_library_qa(params: &JobParams) -> bool {
    matches!(params, JobParams::Test(t) if t.recipe.as_deref() == Some(LIBRARY_QA_RECIPE))
}

/// #2211: how long a `build` job (it carries no timeout of its own) may wait
/// for a governor slot. A release build queued behind worker test jobs on the
/// one-slot hub must wait for its turn, not lose a ten-minute race; the bound
/// only stops a build from waiting for ever behind a wedged holder.
pub(crate) const BUILD_ADMISSION_WAIT_SECS: u64 = 60 * 60;

/// #2149, #2211: how long a job may wait for build admission (governor slot,
/// artifact or sandbox lock) before it settles `job_admission_timed_out`. A job
/// whose caller does not need an answer inside ten minutes waits for capacity
/// up to its own timeout instead of the ordinary ten-minute cap: the async
/// full-library QA run (#2149) and every ordinary test job (#2211), whose
/// execution budget never counts the wait. A build job waits
/// [`BUILD_ADMISSION_WAIT_SECS`]. A scoped test keeps the wait inside its wall
/// budget (it bounds its own slot wait) and keeps the ordinary cap here.
#[must_use]
pub(crate) fn admission_allowance_secs(params: &JobParams) -> u64 {
    match params {
        JobParams::Test(test) if !test.is_scoped_test() => job_timeout_secs(params)
            .unwrap_or(0)
            .max(JOB_SLOT_WAIT_ALLOWANCE_SECS),
        JobParams::Build(_) => BUILD_ADMISSION_WAIT_SECS,
        _ => JOB_SLOT_WAIT_ALLOWANCE_SECS,
    }
}

/// #2211: stretch an ordinary cargo job's slot wait to its admission allowance:
/// cargo-slot learns the longer limit through its environment and the unit's
/// runtime cap carries the extra wait, so a job queued behind others is not
/// stopped by either clock while it waits. Execution time excludes the wait.
fn widen_admission(mut command: JobCommand, params: &JobParams) -> JobCommand {
    let widened = match params {
        JobParams::Test(test) => !test.is_scoped_test() && !is_library_qa(params),
        JobParams::Build(_) => true,
        _ => false,
    };
    let allowance = admission_allowance_secs(params);
    if widened && allowance > JOB_SLOT_WAIT_ALLOWANCE_SECS {
        command.runtime_max_secs += allowance - JOB_SLOT_WAIT_ALLOWANCE_SECS;
        command.admission_max_secs = Some(allowance);
    }
    command
}

/// #1337: the wall-clock timeout a test job carries (stamped at submit).
#[must_use]
pub(crate) fn job_timeout_secs(params: &JobParams) -> Option<u64> {
    match params {
        JobParams::Test(p) => p.timeout_minutes.map(|minutes| u64::from(minutes) * 60),
        _ => None,
    }
}

/// The fixed command a job runs, before the systemd wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCommand {
    pub argv: Vec<String>,
    /// `RuntimeMaxSec` for the unit.
    pub runtime_max_secs: u64,
    /// `TimeoutStopSec`: how long a stop waits before SIGKILL (the cloud
    /// gate's destroy trap runs inside this window).
    pub stop_timeout_secs: u64,
    /// The log is cut at this many bytes (`head -c` in the wrapper).
    pub log_max_bytes: u64,
    /// `MemoryMax` for the unit, in GiB.
    pub memory_max_gib: u64,
    /// `CPUQuota` for the unit, in percent of one CPU.
    pub cpu_quota_percent: u64,
    /// #2112: background work (the async full-library QA run) yields the CPU
    /// to gates, workers and other jobs: `CPUWeight` [`BACKGROUND_CPU_WEIGHT`]
    /// (the default is 100) and nice [`BACKGROUND_NICE`], inside the same
    /// `CPUQuota` ceiling.
    pub background: bool,
    /// #2211: how long `scripts/cargo-slot` may wait for a governor slot
    /// (`RSI_CARGO_ADMISSION_MAX_SEC` in the unit's environment) before it exits
    /// 124 `admission-timeout`. `None` keeps cargo-slot's own ten minutes.
    pub admission_max_secs: Option<u64>,
}

/// #2112: the `CPUWeight` of a background job unit (the worker slice's weight;
/// systemd's default, which other job units and the daemon's gates run at, is 100).
pub(crate) const BACKGROUND_CPU_WEIGHT: u32 = 20;
/// #2112: the nice level of a background job's processes, for hosts or cgroup
/// levels where weights do not separate it from the gates.
pub(crate) const BACKGROUND_NICE: i32 = 10;

const MIB: u64 = 1024 * 1024;

/// Where the fixed tools live.
#[derive(Debug, Clone)]
pub struct JobTools {
    pub cargo_slot: PathBuf,
    pub lander: PathBuf,
}

impl JobTools {
    /// `RSI_JOB_CARGO_SLOT` / `RSI_ROLLING_LAND_BIN`, else the standard
    /// per-user locations.
    #[must_use]
    pub fn discover() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let cargo_slot = std::env::var_os("RSI_JOB_CARGO_SLOT")
            .map(PathBuf::from)
            // The slot wrapper uses Linux tools. On Mac, env is the direct
            // command prefix; launchd supplies the smaller Cargo limits.
            .or_else(|| {
                if cfg!(target_os = "macos") {
                    Some(PathBuf::from("/usr/bin/env"))
                } else {
                    home.as_ref().map(|h| h.join(".rsi/bin/cargo-slot"))
                }
            })
            .unwrap_or_else(|| PathBuf::from("cargo-slot"));
        Self {
            cargo_slot,
            lander: crate::rolling_queue::LanderLauncher::discover()
                .binary()
                .to_path_buf(),
        }
    }
}

fn s(value: &str) -> String {
    value.to_string()
}

fn slot_cargo(tools: &JobTools, cargo_args: Vec<String>) -> Vec<String> {
    let mut argv = vec![
        tools.cargo_slot.display().to_string(),
        s("env"),
        s("-u"),
        s(STRIP_NAMESPACE_ENV),
        s("cargo"),
    ];
    argv.extend(cargo_args);
    argv
}

/// The line `scripts/candidate-receipt.sh` ends with: the compact JSON receipt.
const RECEIPT_LINE_PREFIX: &str = "RECEIPT_JSON ";
/// The line `scripts/scoped-test` ends with (#1638): its compact JSON receipt.
const SCOPED_RECEIPT_LINE_PREFIX: &str = "SCOPED_TEST_RECEIPT ";
/// The line `scripts/shard-feature-sweep` ends with (#2692): its compact JSON
/// receipt, one entry per shard.
const SHARD_SWEEP_RECEIPT_LINE_PREFIX: &str = "SHARD_SWEEP_RECEIPT ";

/// #1638: `scripts/scoped-test` for the sandbox, with only the two validated
/// refs and fixed budgets derived from the job's timeout (the same 120 s and
/// 60 s margins `make scoped-test` uses at the 20-minute default).
/// #2087: the CPU ceiling (percent of one CPU) of a `scoped_test` job unit:
/// half the host's threads, between two and twelve cores. `scripts/scoped-test`
/// picks the actual test threads from the host load inside this ceiling, so a
/// busy host still gets fewer. The operator's `worker_cpu_quota_percent` (#1992)
/// caps worker session scopes, not job units (test and build jobs already run
/// at 1600%), so it does not apply here.
///
/// #2119: capped by the hub build budget (`hub_build_cpu_percent` of the
/// host's threads, the `rsi-build.slice` quota), so a job never sizes its
/// threads for the whole host while N jobs share the slice.
#[must_use]
pub(crate) fn scoped_test_cpu_ceiling_percent(host_threads: usize) -> u64 {
    scoped_test_cpu_ceiling_for(host_threads, rsid_core::build_slice::current_percent())
}

/// [`scoped_test_cpu_ceiling_percent`] for an explicit hub build share.
#[must_use]
pub(crate) fn scoped_test_cpu_ceiling_for(host_threads: usize, hub_build_percent: u32) -> u64 {
    rsid_core::build_slice::per_job_ceiling_percent(
        host_threads,
        hub_build_percent,
        (host_threads / 2).clamp(2, 12) as u64 * 100,
    )
}

/// The default CPU ceiling of a cargo job unit (percent of one CPU), before the
/// hub build budget caps it.
const CARGO_JOB_CPU_QUOTA_PERCENT: u64 = 1600;

/// #2119: job kinds whose units hold cargo builds or tests and so run in
/// `rsi-build.slice` (the kinds `prepare_build_environment` gives a build
/// environment).
fn runs_in_build_slice(params: &JobParams) -> bool {
    matches!(
        params,
        JobParams::Test(_) | JobParams::Build(_) | JobParams::Landing(_) | JobParams::CloudGate(_)
    )
}

/// #2823: the unit gets the daemon's own PATH (`launch`), so that PATH is what
/// decides which `rustc` a cargo job runs.
pub(crate) fn check_unit_toolchain(
    params: &JobParams,
    cwd: &Path,
) -> std::result::Result<(), String> {
    if !runs_in_build_slice(params) {
        return Ok(());
    }
    toolchain::check(std::env::var("PATH").ok().as_deref(), cwd)
}

/// #2524: the thermal hold in force on this host now. The predicate is shared
/// with manager creates and topology node launches (#2562, `host_pressure`).
fn thermal_hold() -> Option<rsid_core::thermal_guard::ThermalHold> {
    crate::host_pressure::thermal_hold()
}

fn thermal_hold_detail(hold: rsid_core::thermal_guard::ThermalHold) -> String {
    format!(
        "held for thermal: the host CPU is {:.1} C, at or above the {} C job_thermal_ceiling_c; the job starts once it cools",
        hold.temperature_c, hold.ceiling_c
    )
}

/// #2937: the capacity hold in force for a new cargo unit on a host whose job
/// runtime has no cargo-slot governor (macOS); `None` everywhere else. The
/// running count is the cargo-unit jobs (`runs_in_build_slice`) already running.
fn capacity_hold(store: &Store) -> Option<crate::host_memory::JobCapacityHold> {
    #[cfg(not(test))]
    if !crate::host_memory::CAPACITY_GATE_ENABLED {
        return None;
    }
    let running = store
        .list_running_agent_jobs()
        .map(|rows| {
            rows.iter()
                .filter(|row| runs_in_build_slice(&row.job.params))
                .count()
        })
        .unwrap_or(0);
    crate::host_memory::host_capacity_hold(u32::try_from(running).unwrap_or(u32::MAX))
}

fn host_threads() -> usize {
    rsid_core::build_slice::host_threads()
}

fn scoped_test_command(
    tools: &JobTools,
    p: &rsi_common::agent_jobs::ScopedTestParams,
    timeout_minutes: Option<u32>,
    tool: &scoped_tool::ScopedTool,
) -> JobCommand {
    let (script, tool_env) = tool.launch();
    let timeout_secs = u64::from(timeout_minutes.unwrap_or(JOB_TEST_TIMEOUT_DEFAULT_MINS)) * 60;
    let cpu_quota_percent = scoped_test_cpu_ceiling_percent(host_threads());
    // Derive the diff before acquiring a slot: Python/docs-only jobs need none.
    let mut argv = vec![s("env"), s("-u"), s(STRIP_NAMESPACE_ENV)];
    // `NAME=value` entries name the tool copy for the receipt (#1938).
    argv.extend(tool_env);
    argv.extend([script, s("--base"), p.base.clone()]);
    if let Some(head) = &p.head {
        argv.extend([s("--head"), head.clone()]);
    }
    argv.extend([
        s("--runtime-max-sec"),
        timeout_secs
            .saturating_sub(SCOPED_JOB_RUNTIME_RESERVE_SECS)
            .max(1)
            .to_string(),
        s("--total-max-sec"),
        timeout_secs.saturating_sub(60).max(1).to_string(),
        s("--job-max-sec"),
        timeout_secs.to_string(),
        s("--cargo-slot"),
        tools.cargo_slot.display().to_string(),
        s("--cpu-quota"),
        cpu_quota_percent.to_string(),
    ]);
    JobCommand {
        argv,
        runtime_max_secs: timeout_secs + JOB_TIMEOUT_UNIT_GRACE_SECS,
        stop_timeout_secs: 60,
        log_max_bytes: 64 * MIB,
        memory_max_gib: 24,
        cpu_quota_percent,
        background: false,
        admission_max_secs: None,
    }
}

fn test_command(tools: &JobTools, p: &TestJobParams) -> Vec<String> {
    // #1099: the wrapper makes its own temporary worktree and shares the build
    // slot through `check-touched-shards`, so it runs unwrapped.
    if let Some(candidate) = &p.candidate_receipt {
        return vec![s("scripts/candidate-receipt.sh"), candidate.clone()];
    }
    if let Some(shard) = &p.shard {
        let mut argv = vec![
            tools.cargo_slot.display().to_string(),
            s("env"),
            s("-u"),
            s(STRIP_NAMESPACE_ENV),
            s("scripts/run-rsid-test-shards.sh"),
            s("shard"),
            shard.clone(),
        ];
        if let Some(filterset) = &p.filterset {
            argv.extend([s("--filterset"), filterset.clone()]);
        }
        return argv;
    }
    let mut args = vec![s("test"), s("-p"), p.package.clone().unwrap_or_default()];
    if p.lib_only {
        args.push(s("--lib"));
    }
    if let Some(target) = &p.test_target {
        args.extend([s("--test"), target.clone()]);
    }
    // #1106: one failing binary must not hide the others; every binary runs.
    args.push(s("--no-fail-fast"));
    // No filters runs every test; exact matching is a libtest option.
    if !p.filters.is_empty() || p.exact {
        args.push(s("--"));
        args.extend(p.filters.iter().cloned());
        if p.exact {
            args.push(s("--exact"));
        }
    }
    slot_cargo(tools, args)
}

/// #2692: the shard-feature sweep for a `test_no_run` build job. The script
/// builds each configuration in turn inside the same slot and cleans the
/// package between them; the daemon only names the package and the shards.
fn shard_sweep_command(tools: &JobTools, p: &BuildJobParams) -> Vec<String> {
    let mut argv = vec![
        tools.cargo_slot.display().to_string(),
        s("env"),
        s("-u"),
        s(STRIP_NAMESPACE_ENV),
        s("scripts/shard-feature-sweep"),
        s("--package"),
        p.package.clone().unwrap_or_default(),
    ];
    if p.no_default_features {
        argv.push(s("--no-default-features"));
    }
    for feature in &p.features {
        argv.extend([s("--feature"), feature.clone()]);
    }
    for shard in &p.shards {
        argv.extend([s("--shard"), shard.clone()]);
    }
    argv
}

/// How long a `test_no_run` job may run: builds of the package's harness, one
/// per shard, each up to half an hour, never less than a plain build's two
/// hours and never more than a cloud sweep's eight.
fn shard_sweep_runtime_max_secs(p: &BuildJobParams) -> u64 {
    let builds = p.shards.len().max(1) as u64;
    (3600 + builds * 1800).clamp(2 * 3600, 8 * 3600)
}

fn build_command(tools: &JobTools, p: &BuildJobParams) -> Vec<String> {
    if p.command == rsi_common::agent_jobs::BuildCommand::TestNoRun {
        return shard_sweep_command(tools, p);
    }
    let mut args = vec![p.command.as_str().to_string()];
    match &p.package {
        Some(package) => args.extend([s("-p"), package.clone()]),
        None => args.push(s("--workspace")),
    }
    if p.all_targets {
        args.push(s("--all-targets"));
    }
    // #2516: a check reports every crate's errors in one pass, not just the
    // first failing crate's, so a worker without foreground cargo pays one job
    // round trip per batch of errors instead of one per crate.
    if p.command == rsi_common::agent_jobs::BuildCommand::Check {
        args.push(s("--keep-going"));
    }
    if p.release {
        args.push(s("--release"));
    }
    slot_cargo(tools, args)
}

fn lander_args(cwd: &Path, p: &LandingJobParams) -> Vec<String> {
    let mut args = vec![
        s("--repo"),
        cwd.display().to_string(),
        s("--remote"),
        s("origin"),
        s("--accepted"),
        p.accepted.clone(),
    ];
    for filter in &p.test_filters {
        args.extend([s("--test-filter"), filter.clone()]);
    }
    args
}

/// The refusal code a command-building failure settles a queued job with: the
/// typed tool-helper refusal (hub #3024) when that is the cause, else
/// `job_launch_failed`.
fn launch_refusal_code(error: &DaemonError) -> String {
    let message = error.to_string();
    if message.contains(rsi_common::agent_jobs::JOB_SCOPED_TEST_TOOL_HELPER_MISSING) {
        return rsi_common::agent_jobs::JOB_SCOPED_TEST_TOOL_HELPER_MISSING.into();
    }
    JOB_LAUNCH_FAILED.into()
}

/// Build the fixed argv and unit timeouts for validated parameters.
///
/// # Errors
/// `job_launch_failed` when a cloud gate has no trusted script.
pub(crate) fn job_command(
    tools: &JobTools,
    params: &JobParams,
    cwd: &Path,
    gate_script: Option<&Path>,
) -> Result<JobCommand> {
    let mut command = widen_admission(
        unbudgeted_job_command(tools, params, cwd, gate_script)?,
        params,
    );
    if runs_in_build_slice(params) {
        // #2119: no unit asks for more than the slice's whole budget.
        command.cpu_quota_percent =
            rsid_core::build_slice::current_job_ceiling_percent(command.cpu_quota_percent);
    }
    Ok(apply_requested_cpu_budget(command, params))
}

/// #161: the per-job CPU budget the caller asked for, if any.
fn requested_cpu_quota_percent(params: &JobParams) -> Option<u64> {
    match params {
        JobParams::Test(test) => test.cpu_quota_percent,
        JobParams::Build(build) => build.cpu_quota_percent,
        _ => None,
    }
    .map(u64::from)
}

/// Thread counts a requested budget gives a plain cargo job: one per whole CPU
/// the unit may use, never above `cargo-slot`'s own default of four, so a
/// request can only lower parallelism.
fn cpu_budget_threads(cpu_quota_percent: u64) -> u64 {
    (cpu_quota_percent / 100).clamp(1, CARGO_SLOT_DEFAULT_THREADS)
}

/// `cargo-slot`'s default `CARGO_BUILD_JOBS` and test-thread count.
const CARGO_SLOT_DEFAULT_THREADS: u64 = 4;

/// #161: apply the caller's lower CPU budget before the unit starts. The unit's
/// `CPUQuota` becomes the smaller of the derived quota (host ceiling, hub build
/// share, kind default) and the request, so the operator's controls stay the
/// ceiling. A `scoped_test` gets the same value as `--cpu-quota` (the script
/// sizes its test threads and nested units from it); a plain cargo job gets
/// matching build and test thread counts. Omitted keeps `command` untouched.
fn apply_requested_cpu_budget(mut command: JobCommand, params: &JobParams) -> JobCommand {
    let Some(requested) = requested_cpu_quota_percent(params) else {
        return command;
    };
    command.cpu_quota_percent = command.cpu_quota_percent.min(requested);
    let quota = command.cpu_quota_percent;
    if matches!(params, JobParams::Test(test) if test.is_scoped_test()) {
        let flag = command.argv.iter().position(|arg| arg == "--cpu-quota");
        let value = flag.and_then(|flag| command.argv.get_mut(flag + 1));
        if let Some(value) = value {
            *value = value
                .parse::<u64>()
                .map_or(quota, |planned| planned.min(quota))
                .to_string();
        }
        // The script's `cargo-slot` holder sets `CARGO_BUILD_JOBS` itself (the
        // stamped value never reaches it), so a lowered budget must name the
        // build jobs too; `env -u NAME` is the prefix, assignments follow it.
        let at = command
            .argv
            .iter()
            .position(|arg| arg == "-u")
            .map_or(1, |at| at + 2);
        command.argv.insert(
            at.min(command.argv.len()),
            format!("RSI_JOB_BUILD_JOBS={}", cpu_budget_threads(quota)),
        );
        return command;
    }
    let threads = cpu_budget_threads(quota);
    command.argv.splice(
        0..0,
        [
            s("env"),
            format!("RSI_JOB_BUILD_JOBS={threads}"),
            format!("RUST_TEST_THREADS={threads}"),
            format!("NEXTEST_TEST_THREADS={threads}"),
        ],
    );
    command
}

fn unbudgeted_job_command(
    tools: &JobTools,
    params: &JobParams,
    cwd: &Path,
    gate_script: Option<&Path>,
) -> Result<JobCommand> {
    if let JobParams::Test(test) = params
        && test.recipe.is_some()
    {
        let mut command = recipe::command(tools, test, cwd)?;
        if is_library_qa(params) {
            // Full-library QA needs the same host-derived ceiling as scoped
            // jobs (#2087), rather than two cores for ~7600 tests (#2092),
            // at a lower priority than the gates it follows up (#2112).
            command.cpu_quota_percent = scoped_test_cpu_ceiling_percent(host_threads());
            command.background = true;
            // recipe::command prefixes make/just with cargo-slot env -u NAME.
            // Keep nextest threads inside the unit's ceiling on every host.
            command.argv.insert(
                4,
                format!(
                    "RSI_LIBRARY_QA_TEST_THREADS={}",
                    command.cpu_quota_percent / 100,
                ),
            );
            // #2149: nobody waits on this run, so cargo-slot waits for a build
            // slot as long as the daemon lets the job wait (its own timeout),
            // and the unit's runtime cap carries that longer allowance.
            let allowance = admission_allowance_secs(params);
            command.runtime_max_secs += allowance.saturating_sub(JOB_SLOT_WAIT_ALLOWANCE_SECS);
            command.argv.splice(
                0..0,
                [s("env"), format!("RSI_CARGO_ADMISSION_MAX_SEC={allowance}")],
            );
        }
        return Ok(command);
    }
    if let JobParams::Test(test) = params
        && let Some(scoped) = &test.scoped_test
    {
        // #1938: the verification tool is the rolling tip's, not the
        // sandbox's possibly stale copy, unless the candidate edits the tool.
        // Hub #3024: a tool copy that loads a helper its tree lacks is refused,
        // naming the file, rather than run with another copy of the tool.
        let tool = match jobs_dir() {
            Err(_) => scoped_tool::ScopedTool::Worktree,
            Ok(dir) => scoped_tool::resolve(
                cwd,
                &scoped.base,
                scoped.head.as_deref(),
                &dir.join("scoped-test-tools"),
            )
            .map_err(DaemonError::InvalidParam)?,
        };
        return Ok(scoped_test_command(
            tools,
            scoped,
            test.timeout_minutes,
            &tool,
        ));
    }
    let (argv, runtime_max_secs, stop_timeout_secs) = match params {
        JobParams::Test(p) => (
            test_command(tools, p),
            job_timeout_secs(params).map_or(3 * 3600, |secs| {
                secs + JOB_TIMEOUT_UNIT_GRACE_SECS + JOB_SLOT_WAIT_ALLOWANCE_SECS
            }),
            60,
        ),
        JobParams::Build(p) => (
            build_command(tools, p),
            if p.command == rsi_common::agent_jobs::BuildCommand::TestNoRun {
                shard_sweep_runtime_max_secs(p)
            } else {
                2 * 3600
            },
            60,
        ),
        JobParams::Landing(p) => {
            let mut argv = vec![
                tools.cargo_slot.display().to_string(),
                tools.lander.display().to_string(),
            ];
            argv.extend(lander_args(cwd, p));
            (argv, 4 * 3600, 120)
        }
        JobParams::CloudGate(p) => {
            // Never `cwd/scripts/cloud-gate.sh`: the caller's sandbox is
            // writable and this script runs with the operator's cloud grant.
            let script = gate_script
                .filter(|script| script.is_file())
                .ok_or_else(|| DaemonError::InvalidParam(JOB_LAUNCH_FAILED.into()))?;
            let mut argv = vec![script.display().to_string(), s("--")];
            argv.extend(lander_args(cwd, p));
            (argv, 6 * 3600, 20 * 60)
        }
        JobParams::CloudSweep(p) => {
            // The sweep script is a sibling of the embedded gate script, in
            // the daemon-owned trusted tree; never the caller's checkout. The
            // caller's repository is only where the rolling tip is read from.
            let script = gate_script
                .map(|gate| gate.with_file_name("cloud-sweep.sh"))
                .filter(|script| script.is_file())
                .ok_or_else(|| DaemonError::InvalidParam(JOB_LAUNCH_FAILED.into()))?;
            // The fetch happens in a daemon-owned bare mirror beside the trusted
            // tree (`<jobs_dir>/sweep-mirror.git`), never with the caller's git
            // configuration.
            let mirror = script
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .ok_or_else(|| DaemonError::InvalidParam(JOB_LAUNCH_FAILED.into()))?
                .join("sweep-mirror.git");
            let argv = vec![
                script.display().to_string(),
                s("cloud"),
                p.sha.clone(),
                s("--repo"),
                cwd.display().to_string(),
                s("--mirror"),
                mirror.display().to_string(),
            ];
            // The script's own lifetime cap is 420 minutes; the stop timeout
            // lets its EXIT trap destroy the host.
            (argv, 8 * 3600, 20 * 60)
        }
    };
    // Conservative per-kind resource caps: cargo kinds get a large share of the
    // desktop; the cloud gate mostly waits on the remote host.
    let (log_max_bytes, memory_max_gib, cpu_quota_percent) = match params {
        JobParams::Test(_) | JobParams::Build(_) | JobParams::Landing(_) => {
            (64 * MIB, 24, CARGO_JOB_CPU_QUOTA_PERCENT)
        }
        JobParams::CloudGate(_) => (32 * MIB, 2, 100),
        // The sweep also bundles the rolling history for the host.
        JobParams::CloudSweep(_) => (32 * MIB, 4, 200),
    };
    Ok(JobCommand {
        argv,
        runtime_max_secs,
        stop_timeout_secs,
        log_max_bytes,
        memory_max_gib,
        cpu_quota_percent,
        background: false,
        admission_max_secs: None,
    })
}

/// The cloud gate's and cloud sweep's scripts and the gate Terraform, embedded
/// in the daemon binary. The gate and sweep run with the operator's cloud credentials, so it must never run from
/// the (agent-writable) submitting sandbox or a caller-movable ref: the bytes
/// are the ones this daemon was built with.
const TRUSTED_GATE_FILES: &[(&str, &str, u32)] = &[
    (
        "scripts/cloud-gate.sh",
        include_str!("../../../scripts/cloud-gate.sh"),
        0o755,
    ),
    (
        "scripts/cloud-spend.py",
        include_str!("../../../scripts/cloud-spend.py"),
        0o755,
    ),
    (
        "scripts/cloud-sweep.sh",
        include_str!("../../../scripts/cloud-sweep.sh"),
        0o755,
    ),
    (
        "scripts/cloud-sweep-excerpts.sh",
        include_str!("../../../scripts/cloud-sweep-excerpts.sh"),
        0o755,
    ),
    (
        "scripts/cloud-sweep-verdict.py",
        include_str!("../../../scripts/cloud-sweep-verdict.py"),
        0o755,
    ),
    (
        "infra/aws/gate/bootstrap.sh.tftpl",
        include_str!("../../../infra/aws/gate/bootstrap.sh.tftpl"),
        0o644,
    ),
    (
        "infra/aws/gate/main.tf",
        include_str!("../../../infra/aws/gate/main.tf"),
        0o644,
    ),
    (
        "infra/aws/gate/outputs.tf",
        include_str!("../../../infra/aws/gate/outputs.tf"),
        0o644,
    ),
    (
        "infra/aws/gate/variables.tf",
        include_str!("../../../infra/aws/gate/variables.tf"),
        0o644,
    ),
    (
        "infra/aws/gate/versions.tf",
        include_str!("../../../infra/aws/gate/versions.tf"),
        0o644,
    ),
];

/// A real directory at `path`, mode 0700. A symlink or file planted there is
/// removed and replaced; a symlink is never followed.
fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => {
            return std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
        }
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

/// Write the embedded gate tree under `<jobs_dir>/trusted-gate` (stable, so
/// Terraform's plugin cache in it survives across gates; files are rewritten
/// only when they differ) and return the script path.
///
/// The tree is daemon-owned: `jobs_dir` must itself be a real directory, every
/// directory below it is recreated if it is a symlink or file, files are
/// written through a fresh exclusive temp file and renamed, and before
/// returning the script and the Terraform directory must canonicalize beneath
/// `jobs_dir`, so nothing the caller controls through a link is written or run.
pub(crate) fn materialize_trusted_gate(jobs_dir: &Path) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let refuse = |message: &str| {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, message.to_string())
    };
    if !std::fs::symlink_metadata(jobs_dir)?.file_type().is_dir() {
        return Err(refuse("jobs directory is not a real directory"));
    }
    let root = jobs_dir.join("trusted-gate");
    ensure_private_dir(&root)?;
    for (relative, content, mode) in TRUSTED_GATE_FILES {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            let mut dir = root.clone();
            for part in parent
                .strip_prefix(&root)
                .map_err(|_| refuse("path outside gate root"))?
                .components()
            {
                dir.push(part);
                ensure_private_dir(&dir)?;
            }
        }
        let unchanged = std::fs::symlink_metadata(&path)
            .is_ok_and(|meta| meta.file_type().is_file())
            && std::fs::read_to_string(&path).is_ok_and(|have| have == *content);
        if !unchanged {
            let tmp = path.with_extension("rsi-tmp");
            match std::fs::symlink_metadata(&tmp) {
                Ok(_) => std::fs::remove_file(&tmp)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(*mode)
                .open(&tmp)?;
            file.write_all(content.as_bytes())?;
            drop(file);
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(*mode))?;
            std::fs::rename(&tmp, &path)?;
        }
    }
    let script = root.join("scripts/cloud-gate.sh");
    let base = std::fs::canonicalize(jobs_dir)?;
    for checked in [script.clone(), root.join("infra/aws/gate")] {
        if !std::fs::canonicalize(&checked)?.starts_with(&base) {
            return Err(refuse("trusted gate resolves outside the jobs directory"));
        }
    }
    Ok(script)
}

/// Everything the launcher needs to start one unit.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub unit_name: String,
    pub cwd: PathBuf,
    pub log_path: PathBuf,
    pub status_path: PathBuf,
    pub command: JobCommand,
    pub build_environment: Option<BuildEnvironment>,
}

/// Git's repository-discovery ceiling. A unit that has a private `TMPDIR` also
/// gets `GIT_CEILING_DIRECTORIES=<that TMPDIR>` (#2612): the jobs directory lives
/// under the operator's home directory, which is itself a repository, so without
/// it a plain `git` in a test's temporary directory that is not (yet) a repository
/// discovers the home repository, and `scripts/scratch_git.py` takes that enclosing
/// repository for the assigned sandbox and refuses every fixture. Git does not
/// look at the ceiling directory or above it, and only for a working directory
/// below it: the job's source checkout and every repository created inside the
/// private `TMPDIR` are found as before.
const GIT_CEILING_DIRECTORIES_ENV: &str = "GIT_CEILING_DIRECTORIES";

/// Jobs that build locally receive these explicit, non-secret environment values.
#[derive(Debug, Clone)]
pub struct BuildEnvironment {
    pub tmp_dir: PathBuf,
    pub target_dir: PathBuf,
    /// The shard runner's artifact lock inside `target_dir`
    /// ([`ARTIFACT_LOCK_FILENAME`]). The unit holds it shared while its command
    /// runs, so a concurrent shard run's cleanup (`cargo clean -p rsid`, taken
    /// only under an exclusive lock) never deletes this job's binaries. `None`
    /// for shard runs: the runner takes and releases the lock itself, and an
    /// extra holder would keep its final cleanup from ever running.
    pub artifact_lock: Option<PathBuf>,
    /// Linux job batches serialize before governor admission, avoiding Cargo
    /// waits hidden inside the execution budget. Other backends keep Cargo
    /// serialization as their portable fallback.
    pub admission_lock: Option<PathBuf>,
    /// The build settings the owning session's shell received
    /// (`<sandbox>/.rsi-build-env.json`, written when the daemon stamps them), so
    /// the unit compiles exactly as the worker's shell does (#1741): the same
    /// profile, incremental mode and compiler wrapper, hence the same
    /// artifacts in the same target.
    pub recorded_env: Vec<(String, String)>,
}

/// Read the session build settings beside `target_dir`, keeping only the
/// names the daemon stamps; a missing or unreadable record yields none.
fn recorded_build_env(target_dir: &Path) -> Vec<(String, String)> {
    use crate::sandbox::execution_scratch::{BUILD_ENV_RECORD, RECORDED_BUILD_ENV};
    // Sessions launched before #2401 retain their old target-local record
    // until their next provider launch. Never replace an unreadable new stamp
    // with stale legacy settings: only its absence permits the fallback.
    let bytes = match std::fs::read(target_dir.with_file_name(BUILD_ENV_RECORD)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::read(target_dir.join(BUILD_ENV_RECORD))
        }
        result => result,
    };
    let Ok(bytes) = bytes else {
        return Vec::new();
    };
    let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        tracing::warn!(target = %target_dir.display(), "unreadable sandbox build-env record");
        return Vec::new();
    };
    let Some(env) = record.get("env").and_then(|env| env.as_object()) else {
        return Vec::new();
    };
    RECORDED_BUILD_ENV
        .iter()
        .filter_map(|name| {
            let value = env.get(*name)?.as_str()?;
            (!value.contains(['\n', '\0'])).then(|| (name.to_string(), value.to_string()))
        })
        .collect()
}

/// Lock file `scripts/run-rsid-test-shards.sh` keeps in the Cargo target
/// directory; every job that uses the target holds it shared.
const ARTIFACT_LOCK_FILENAME: &str = ".rsid-test-shards.lock";

fn job_tmp_dir(log_path: &Path) -> PathBuf {
    log_path.with_extension("tmp")
}

/// Sandboxed jobs always build in the daemon-designated target. Cargo config
/// cannot send reviewer/worker output outside reclaim (#1864).
fn cargo_target_dir(cwd: &Path) -> PathBuf {
    cwd.join("target")
}

/// The sandbox root that contains `cwd`: the owner's own sandbox, or a sibling
/// worker sandbox under the same base (or the daemon's default sandbox base).
/// `None` for a worktree outside every sandbox root.
fn enclosing_sandbox_root(cwd: &Path, owner_sandbox: &Path, data_dir: &Path) -> Option<PathBuf> {
    if cwd.starts_with(owner_sandbox) {
        return Some(owner_sandbox.to_path_buf());
    }
    let owner_base = owner_sandbox.parent().map(Path::to_path_buf);
    let default_base = data_dir.join("sandboxes").canonicalize().ok();
    owner_base.into_iter().chain(default_base).find_map(|base| {
        let first = cwd.strip_prefix(&base).ok()?.components().next()?;
        Some(base.join(first))
    })
}

/// Pick the Cargo target for a job. A cwd inside a sandbox (the owner's own
/// root or subdirectory, or another worker's sandbox) builds in that sandbox's
/// daemon-designated target, keeping the warm-pool seed and the sandbox's
/// reclaim. Only a worktree outside every sandbox root (an appointed manager's
/// integration worktree) gets a registered per-worktree target (#1900).
fn job_target_dir(cwd: &Path, sandbox: Option<&Path>, data_dir: &Path) -> std::io::Result<PathBuf> {
    let Some(sandbox) = sandbox else {
        return Ok(cargo_target_dir(cwd));
    };
    let canonical_cwd = cwd.canonicalize()?;
    let owner = sandbox.canonicalize()?;
    match enclosing_sandbox_root(&canonical_cwd, &owner, data_dir) {
        Some(root) if root == owner && canonical_cwd == owner => Ok(cargo_target_dir(cwd)),
        Some(root) => Ok(cargo_target_dir(&root)),
        None => crate::agent_scratch_reclaim::manager_target(cwd),
    }
}

fn prepare_build_environment(
    params: &JobParams,
    cwd: &Path,
    log: &Path,
    owner_is_manager: bool,
    sandbox: Option<&Path>,
) -> std::io::Result<Option<BuildEnvironment>> {
    prepare_build_environment_in(
        params,
        cwd,
        log,
        owner_is_manager,
        &rsi_common::identity::data_dir(),
        sandbox,
    )
}

/// #2176: the one registered target every `rolling-library-qa` snapshot
/// under `jobs` builds in (as [`prepare_build_environment_in`] picks it).
pub(crate) fn library_qa_target(jobs: &Path) -> std::io::Result<PathBuf> {
    crate::agent_scratch_reclaim::manager_target(&jobs.join(LIBRARY_QA_RECIPE))
}

fn prepare_build_environment_in(
    params: &JobParams,
    cwd: &Path,
    log: &Path,
    owner_is_manager: bool,
    data_dir: &Path,
    sandbox: Option<&Path>,
) -> std::io::Result<Option<BuildEnvironment>> {
    if !matches!(
        params,
        JobParams::Test(_) | JobParams::Build(_) | JobParams::Landing(_) | JobParams::CloudGate(_)
    ) {
        return Ok(None);
    }
    // A delegated landing builds in the reached worker's sandbox, rather than
    // borrowing the submitting manager's scratch or build record.
    let sandbox = matches!(params, JobParams::Test(_) | JobParams::Build(_))
        .then_some(sandbox)
        .flatten();
    let queue_qa = matches!(params, JobParams::Test(t)
        if t.recipe.as_deref() == Some(LIBRARY_QA_RECIPE))
        && !sandbox.is_some_and(|root| cwd.starts_with(root));
    let target_dir = if queue_qa {
        // Queue snapshots outlive their enqueuer's sandbox. Their target has
        // its own registered custody, including when that sandbox is gone.
        let root = cwd.parent().and_then(Path::parent).unwrap_or(cwd);
        // All automatic snapshots share one registered target. The durable
        // admission/artifact lock serializes them; no per-landing target leak.
        let root = if root
            .file_name()
            .is_some_and(|name| name == LIBRARY_QA_RECIPE)
        {
            root
        } else {
            cwd
        };
        crate::agent_scratch_reclaim::manager_target(root)?
    } else if crate::release_target::uses_release_target(params, owner_is_manager) {
        crate::release_target::release_target_dir_in(data_dir)
    } else {
        job_target_dir(cwd, sandbox, data_dir)?
    };
    let source_target = sandbox
        .map(cargo_target_dir)
        .unwrap_or_else(|| target_dir.clone());
    if queue_qa {
        crate::qa_target_trim::enforce_cap(&target_dir, rsid_core::qa_target_cap::current_bytes());
    }
    if crate::release_target::uses_release_target(params, owner_is_manager) {
        // #1218: one persistent target for manager release builds, bounded.
        // A busy target (a build holds its lock) is simply kept.
        std::fs::create_dir_all(&target_dir)?;
        match crate::release_target::enforce_size_cap(
            &target_dir,
            crate::release_target::RELEASE_TARGET_MAX_BYTES,
        ) {
            Ok(crate::release_target::CapOutcome::Reclaimed { bytes, retained }) => tracing::info!(
                target = %target_dir.display(),
                bytes,
                retained,
                "release target exceeded its size cap and its Cargo output was removed"
            ),
            Ok(crate::release_target::CapOutcome::Held { bytes }) => tracing::warn!(
                target = %target_dir.display(),
                bytes,
                "release target exceeded its size cap but a process holds it; kept"
            ),
            Ok(_) => {}
            Err(error) => tracing::warn!(
                target = %target_dir.display(),
                %error,
                "cannot enforce the release target size cap"
            ),
        }
    }

    let tmp_dir = job_tmp_dir(log);
    let runs_shard_runner = matches!(params, JobParams::Test(t) if t.shard.is_some());
    let artifact_lock = (!runs_shard_runner).then(|| target_dir.join(ARTIFACT_LOCK_FILENAME));
    let admission_lock = (cfg!(target_os = "linux")
        && matches!(params, JobParams::Test(_) | JobParams::Build(_)))
    .then(|| target_dir.join(".rsi-job-admission.lock"));
    if artifact_lock.is_some() || admission_lock.is_some() {
        // flock(1) creates the lock file but not its directory.
        std::fs::create_dir_all(&target_dir)?;
    }
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(&tmp_dir)?;
    let checked = (|| {
        // These filesystem magic numbers and nix's numeric type API are Linux-only.
        #[cfg(target_os = "linux")]
        {
            let fs = nix::sys::statfs::statfs(&tmp_dir).map_err(std::io::Error::from)?;
            // RAMFS_MAGIC is not named by nix's statfs module.
            if fs.filesystem_type() == nix::sys::statfs::TMPFS_MAGIC
                || fs.filesystem_type().0 == 0x858458f6
            {
                return Err(std::io::Error::other(
                    "job TMPDIR is on tmpfs/ramfs; configure a disk-backed jobs directory",
                ));
            }
        }
        Ok(Some(BuildEnvironment {
            tmp_dir: tmp_dir.canonicalize()?,
            recorded_env: recorded_build_env(&source_target),
            target_dir,
            artifact_lock,
            admission_lock,
        }))
    })();
    if checked.is_err() {
        cleanup_job_tmp(log);
    }
    checked
}

fn cleanup_job_tmp(log: &Path) {
    let tmp = job_tmp_dir(log);
    if let Err(error) = std::fs::remove_dir_all(&tmp) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %tmp.display(), %error, "cannot remove agent job TMPDIR");
        }
    }
}

/// Starts units and reports whether one is still alive. The production
/// implementations are systemd on Linux and launchd on macOS; tests substitute
/// a fake. Backend handles are stable service labels, never bare process IDs.
pub trait JobRuntime: Send + Sync {
    /// Durable platform handle. Mac includes its bootstrap domain, so restart
    /// discovery never confuses a GUI service with a user-domain service.
    fn unit_name(&self, id: Uuid) -> String {
        format!("rsi-job-{id}")
    }
    /// Refuse workflows that the platform cannot run before inserting a row.
    /// # Errors
    /// A stable platform refusal.
    fn validate(&self, _params: &JobParams) -> std::result::Result<(), &'static str> {
        Ok(())
    }
    /// #2823: refuse a cargo job whose unit would build with a `rustc` other
    /// than the one `cwd`'s `rust-toolchain.toml` pins, before a row exists. The
    /// default admits (a fake runtime owns no PATH).
    /// # Errors
    /// `toolchain_mismatch: ...` naming the expected and found compiler and PATH.
    fn check_toolchain(&self, _params: &JobParams, _cwd: &Path) -> std::result::Result<(), String> {
        Ok(())
    }
    /// # Errors
    /// A message when the unit could not be started.
    fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String>;
    /// True while the unit is active, activating or deactivating.
    fn unit_active(&self, unit_name: &str) -> bool;
    /// Stop a unit that is still running. `Ok` means the unit is confirmed
    /// stopped (already inactive or absent counts). The default does nothing.
    ///
    /// # Errors
    /// A message when the stop failed and the unit may still be running.
    fn stop_unit(&self, _unit_name: &str) -> std::result::Result<(), String> {
        Ok(())
    }
    /// #1832: a one-line account of the unit's state (load/active/sub state,
    /// result), recorded when a job never starts its command. `None` when the
    /// backend has nothing to say.
    fn unit_diagnostics(&self, _unit_name: &str) -> Option<String> {
        None
    }
}

/// Use the same backend for submission, cancellation and restart polling.
#[must_use]
pub fn platform_job_runtime() -> Arc<dyn JobRuntime> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(SystemdJobRuntime::default())
    }
    #[cfg(target_os = "macos")]
    {
        Arc::new(launchd::LaunchdJobRuntime::default())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Arc::new(UnsupportedJobRuntime)
    }
}

#[cfg(any(test, not(any(target_os = "linux", target_os = "macos"))))]
struct UnsupportedJobRuntime;

#[cfg(any(test, not(any(target_os = "linux", target_os = "macos"))))]
impl JobRuntime for UnsupportedJobRuntime {
    fn validate(&self, _params: &JobParams) -> std::result::Result<(), &'static str> {
        Err(rsi_common::agent_jobs::JOB_PLATFORM_UNSUPPORTED)
    }
    fn launch(&self, _spec: &LaunchSpec) -> std::result::Result<(), String> {
        Err(rsi_common::agent_jobs::JOB_PLATFORM_UNSUPPORTED.into())
    }
    fn unit_active(&self, _unit_name: &str) -> bool {
        false
    }
}

/// The wrapper that runs inside the unit: append output to the log (the daemon
/// pre-writes its `queue-wait`/`unit-launch` phases, #1608), run the fixed argv and record its exit status atomically as the very last act.
///
/// The log is a bounded writer: `head -c` keeps at most `cap` bytes, so a
/// runaway job cannot fill the disk. When the cap is reached the job's pipe
/// closes (it fails on its next write), the log ends with a marker line and the
/// recorded status is 153 (128 + SIGXFSZ) even if the job later exits 0, so an
/// oversized log fails visibly instead of being silently truncated.
///
/// #1832/#1830: GNU `head -c` writes through stdio, which fully buffers a file
/// stdout, so every `job-start`/`slot-wait` line and all test output stayed in
/// head's buffer until 4 KiB or EOF, and a unit stopped at its timeout lost
/// them. `stdbuf -o0` makes head write each chunk as it reads it.
const WRAPPER: &str = r#"log=$1; status=$2; cap=$3; shift 3
if command -v stdbuf >/dev/null 2>&1; then bound() { stdbuf -o0 head -c "$cap"; }; else bound() { head -c "$cap"; }; fi
( PYTHONUNBUFFERED=1; RSI_JOB_STARTED_AT=$(date +%s); export PYTHONUNBUFFERED RSI_JOB_STARTED_AT; printf 'rsi-phase %s job-start\n' "$RSI_JOB_STARTED_AT"; "$@" 2>&1 </dev/null; printf '%s\n' "$?" >"$status.code" ) | bound >>"$log"
code=$(cat "$status.code" 2>/dev/null || echo 1)
rm -f "$status.code"
size=$(wc -c <"$log")
if [ "$size" -ge "$cap" ]; then
  printf '\nrsi-job: log reached the %s-byte cap; the job was failed\n' "$cap" >>"$log"
  code=153
fi
printf '%s\n' "$code" >"$status.tmp" && mv "$status.tmp" "$status""#;

/// #1591: take the shard runner's artifact lock shared, saying so in the job
/// log when it is contended (who holds it, how long this unit waited) instead of
/// blocking silently. `$1` is the lock file, the rest is the job's own argv.
/// Phase lines are `rsi-phase <epoch> <name>[: detail]`, read back by
/// [`last_phase`].
const LOCK_WAIT: &str = r#"lock=$1; shift
t0=$(date +%s)
if ! flock --shared --nonblock "$lock" true 2>/dev/null; then
  holder=
  ino=$(stat -c %i "$lock" 2>/dev/null)
  if [ -n "$ino" ] && [ -r /proc/locks ]; then
    pid=$(awk -v i="$ino" '$6 ~ (":" i "$") && $4 == "WRITE" { print $5; exit }' /proc/locks)
    [ -n "$pid" ] && holder=" held by pid $pid ($(tr '\0' ' ' </proc/$pid/cmdline 2>/dev/null | cut -c1-120))"
  fi
  printf 'rsi-phase %s artifact-lock-wait: %s%s\n' "$t0" "$lock" "$holder"
fi
exec flock --shared "$lock" /bin/sh -c 'printf "rsi-phase %s artifact-lock-held: waited %ss\n" "$(date +%s)" "$(( $(date +%s) - $1 ))"; shift; exec "$@"' sh "$t0" "$@""#;

/// Serialize jobs sharing a Cargo target before they consume governor slots.
/// The parent holds fd 9 while its child runs with that fd closed, so nested
/// wrappers cannot accidentally release this lock. systemd owns both processes.
#[cfg(any(target_os = "linux", test))]
const ADMISSION_WAIT: &str = r#"lock=$1; shift
exec 9>>"$lock" || exit 1
t0=$(date +%s)
if ! flock --exclusive --nonblock 9; then
  printf 'rsi-phase %s sandbox-lock-wait: %s\n' "$t0" "$lock"
  flock --exclusive 9 || exit 1
fi
printf 'rsi-phase %s sandbox-lock-held: waited %ss\n' "$(date +%s)" "$(( $(date +%s) - t0 ))"
"$@" 9>&-
exit "$?""#;

/// `systemd-run --user` transient services.
#[derive(Debug, Clone)]
pub struct SystemdJobRuntime {
    systemd_run: PathBuf,
    systemctl: PathBuf,
}

impl Default for SystemdJobRuntime {
    fn default() -> Self {
        Self {
            systemd_run: PathBuf::from("/usr/bin/systemd-run"),
            systemctl: PathBuf::from("/usr/bin/systemctl"),
        }
    }
}

/// #1227: every agent job unit the daemon stops is logged with why, on the
/// same `rsid::signal` target as the daemon's process signals.
fn log_unit_stop(unit_name: &str, reason: &str) {
    tracing::info!(
        target: "rsid::signal",
        unit = %format!("{unit_name}.service"),
        signal = "systemctl stop",
        reason,
        "daemon stopping an agent job unit"
    );
}

/// The environment names a job unit sets itself, apart from the session's
/// recorded build settings (`RECORDED_BUILD_ENV`). This is the one list of them
/// (#2629): `systemd_run_args`, the launchd plist and the tests that bound a unit's
/// environment all name them here, so a new required variable is one constant,
/// one entry in [`JOB_UNIT_ENV_NAMES`] and one place that sets it
/// ([`BuildEnvironment::scratch_env`] for build-scratch variables). Launchd's
/// platform-only build tuning (`CARGO_BUILD_JOBS` and the debug-info profile) is
/// not shared: it is launchd's own list.
pub(crate) mod job_env {
    pub(crate) const PATH: &str = "PATH";
    pub(crate) const HOME: &str = "HOME";
    pub(crate) const RSI_JOB_ID: &str = "RSI_JOB_ID";
    pub(crate) const TMPDIR: &str = "TMPDIR";
    pub(crate) const CARGO_TARGET_DIR: &str = "CARGO_TARGET_DIR";
    pub(crate) const RSI_CARGO_ADMISSION_MAX_SEC: &str = "RSI_CARGO_ADMISSION_MAX_SEC";
}

/// Every non-recorded name a job unit may carry. A unit never receives a name
/// outside this list and `RECORDED_BUILD_ENV`; the tests that enforce that read
/// it (a shard build without those tests would see it unused).
#[cfg(test)]
#[allow(dead_code)]
pub(crate) const JOB_UNIT_ENV_NAMES: &[&str] = &[
    job_env::PATH,
    job_env::HOME,
    job_env::RSI_JOB_ID,
    job_env::TMPDIR,
    GIT_CEILING_DIRECTORIES_ENV,
    job_env::CARGO_TARGET_DIR,
    job_env::RSI_CARGO_ADMISSION_MAX_SEC,
];

impl BuildEnvironment {
    /// The non-recorded build-scratch variables every backend sets for a unit with
    /// a build environment: the private `TMPDIR`, the Git ceiling at it (#2612)
    /// and the Cargo target directory.
    pub(crate) fn scratch_env(&self) -> Vec<(&'static str, String)> {
        vec![
            (job_env::TMPDIR, self.tmp_dir.display().to_string()),
            (
                GIT_CEILING_DIRECTORIES_ENV,
                self.tmp_dir.display().to_string(),
            ),
            (
                job_env::CARGO_TARGET_DIR,
                self.target_dir.display().to_string(),
            ),
        ]
    }
}

/// The `systemd-run` command line for one unit. Separate from `launch` so the
/// exact arguments are testable without systemd.
pub(crate) fn systemd_run_args(spec: &LaunchSpec, path_env: Option<&str>) -> Vec<String> {
    let mut args = vec![
        s("--user"),
        s("--quiet"),
        s("--no-block"),
        s("--collect"),
        format!("--unit={}", spec.unit_name),
        format!("--working-directory={}", spec.cwd.display()),
        format!(
            "--property=TimeoutStopSec={}",
            spec.command.stop_timeout_secs
        ),
        format!("--property=RuntimeMaxSec={}", spec.command.runtime_max_secs),
        format!("--property=MemoryMax={}G", spec.command.memory_max_gib),
        format!("--property=CPUQuota={}%", spec.command.cpu_quota_percent),
    ];
    if let Some(max) = spec.command.admission_max_secs {
        args.push(format!(
            "--setenv={}={max}",
            job_env::RSI_CARGO_ADMISSION_MAX_SEC
        ));
    }
    if spec.command.background {
        args.push(format!("--property=CPUWeight={BACKGROUND_CPU_WEIGHT}"));
        args.push(format!("--property=Nice={BACKGROUND_NICE}"));
    }
    // #2119: every cargo build and test unit shares one CPU budget. These are
    // the units with a build environment (see `runs_in_build_slice`).
    if spec.build_environment.is_some() {
        args.push(format!("--slice={}", rsid_core::build_slice::SLICE));
    }
    if let Some(path) = path_env {
        args.push(format!("--setenv={}={path}", job_env::PATH));
    }
    if let Some(home) = std::env::var_os(job_env::HOME) {
        args.push(format!(
            "--setenv={}={}",
            job_env::HOME,
            home.to_string_lossy()
        ));
    }
    // #2127: a job's nested units (scripts/scoped-test's `rsi-scoped-test-*`)
    // carry this job's id in their names, so the CPU andon attributes them.
    if let Some(job_id) = spec.unit_name.strip_prefix("rsi-job-") {
        args.push(format!("--setenv={}={job_id}", job_env::RSI_JOB_ID));
    }
    if let Some(env) = &spec.build_environment {
        for (name, value) in env.scratch_env() {
            args.push(format!("--setenv={name}={value}"));
        }
        for (name, value) in &env.recorded_env {
            args.push(format!("--setenv={name}={value}"));
        }
    }
    args.extend([
        s("--"),
        s("/bin/sh"),
        s("-c"),
        s(WRAPPER),
        s("rsi-job"),
        spec.log_path.display().to_string(),
        spec.status_path.display().to_string(),
        spec.command.log_max_bytes.to_string(),
    ]);
    #[cfg(any(target_os = "linux", test))]
    if let Some(lock) = spec
        .build_environment
        .as_ref()
        .and_then(|env| env.admission_lock.as_ref())
    {
        args.extend([
            s("/bin/sh"),
            s("-c"),
            s(ADMISSION_WAIT),
            s("rsi-admission"),
            lock.display().to_string(),
        ]);
    }
    if let Some(lock) = spec
        .build_environment
        .as_ref()
        .and_then(|env| env.artifact_lock.as_ref())
    {
        args.extend([
            s("/bin/sh"),
            s("-c"),
            s(LOCK_WAIT),
            s("rsi-lock"),
            lock.display().to_string(),
        ]);
    }
    args.extend(spec.command.argv.iter().cloned());
    args
}

impl SystemdJobRuntime {
    /// #2119: make `rsi-build.slice` carry the live hub budget before a build
    /// unit starts in it, and say so in the job's log. A slice that cannot be
    /// configured never blocks the job; the line and a warning name why.
    /// #2282: the writes go through the gated user-manager runner, so a
    /// process that is not the hub daemon (a throwaway daemon, a test) reads
    /// the slices and leaves them alone.
    fn ensure_build_slice(&self, spec: &LaunchSpec) {
        let report = rsid_core::build_slice::apply_pair_with(
            &rsid_core::build_slice::user_manager_runner(&self.systemctl),
            rsid_core::build_slice::host_threads(),
            rsid_core::build_slice::current_percent(),
        );
        if report.is_unavailable() {
            tracing::warn!(
                detail = ?report.detail,
                "rsi-build.slice could not be configured; the build unit runs without the shared CPU/memory budget"
            );
        }
        append_job_log(
            &spec.log_path.display().to_string(),
            &format!(
                "rsi-build-slice {} {} CPUQuota={}% ({}% of host threads, {} cores) MemoryHigh={:?} MemoryMax={:?}{}\n",
                report.slice,
                report.state.as_str(),
                report.cpu_quota_percent,
                report.hub_build_cpu_percent,
                report.budget_cores,
                report.memory.map(|memory| memory.high_bytes),
                report.memory.map(|memory| memory.max_bytes),
                report
                    .detail
                    .as_deref()
                    .map(|detail| format!(": {detail}"))
                    .unwrap_or_default(),
            ),
        );
    }
}

impl JobRuntime for SystemdJobRuntime {
    fn check_toolchain(&self, params: &JobParams, cwd: &Path) -> std::result::Result<(), String> {
        check_unit_toolchain(params, cwd)
    }
    fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
        if spec.build_environment.is_some() {
            self.ensure_build_slice(spec);
        }
        let path = std::env::var("PATH").ok();
        let output = Command::new(&self.systemd_run)
            .args(systemd_run_args(spec, path.as_deref()))
            .env_remove("RSI_SESSION_TOKEN")
            .output()
            .map_err(|error| format!("cannot start systemd-run: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "systemd-run exited {}: {}",
                output.status,
                tail(&String::from_utf8_lossy(&output.stderr))
            ))
        }
    }

    fn unit_active(&self, unit_name: &str) -> bool {
        Command::new(&self.systemctl)
            .args(["--user", "is-active", &format!("{unit_name}.service")])
            .output()
            .map(|output| {
                matches!(
                    String::from_utf8_lossy(&output.stdout).trim(),
                    "active" | "activating" | "deactivating" | "reloading"
                )
            })
            .unwrap_or(false)
    }

    fn unit_diagnostics(&self, unit_name: &str) -> Option<String> {
        let output = Command::new(&self.systemctl)
            .args(["--user", "show", &format!("{unit_name}.service")])
            .args([
                "-p",
                "LoadState,ActiveState,SubState,Result,ExecMainStatus,ActiveEnterTimestamp,StateChangeTimestamp",
            ])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        (!line.is_empty()).then_some(line)
    }

    fn stop_unit(&self, unit_name: &str) -> std::result::Result<(), String> {
        let output = Command::new(&self.systemctl)
            .args(["--user", "stop", &format!("{unit_name}.service")])
            .output()
            .map_err(|error| format!("cannot run systemctl stop: {error}"))?;
        // An absent or already-inactive unit makes `stop` exit nonzero: that
        // still counts as stopped. Only a unit that is alive is a failure.
        if self.unit_active(unit_name) {
            return Err(format!(
                "unit {unit_name} is still active after systemctl stop ({}): {}",
                output.status,
                tail(&String::from_utf8_lossy(&output.stderr))
            ));
        }
        Ok(())
    }
}

/// `<data dir>/jobs`, created on demand.
pub(crate) fn jobs_dir() -> std::io::Result<PathBuf> {
    let dir = rsi_common::identity::data_dir().join("jobs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn git_common_dir(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
        .and_then(|p| p.canonicalize().ok())
}

/// A manager-supplied worktree is allowed only when it is a directory whose
/// git common dir is the same repository as the caller's own root.
pub(crate) fn same_repository_worktree(own_root: &Path, worktree: &Path) -> Option<PathBuf> {
    let worktree = worktree.canonicalize().ok().filter(|p| p.is_dir())?;
    let own = git_common_dir(own_root)?;
    (git_common_dir(&worktree)? == own).then_some(worktree)
}

/// Resolve the job's working directory. The caller's own sandbox is always
/// allowed; any other directory needs manager authority and must be a
/// worktree of the caller's repository.
///
/// # Errors
/// `job_directory_not_allowed`.
pub(crate) fn resolve_cwd(
    sandbox_root: Option<&Path>,
    working_dir: &Path,
    is_manager: bool,
    requested: Option<&Path>,
) -> Result<PathBuf> {
    let denied = || DaemonError::PolicyDenied(JOB_DIR_NOT_ALLOWED.into());
    match requested {
        None => sandbox_root
            .and_then(|root| root.canonicalize().ok())
            .filter(|p| p.is_dir())
            .ok_or_else(denied),
        Some(path) => {
            if !is_manager {
                return Err(denied());
            }
            let own = sandbox_root.unwrap_or(working_dir);
            same_repository_worktree(own, path).ok_or_else(denied)
        }
    }
}

/// Inputs the submit verb resolved from the token-bound caller.
#[derive(Debug, Clone)]
pub(crate) struct SubmitContext {
    pub owner: Uuid,
    pub project_id: Option<Uuid>,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub params: JobParams,
    pub idempotency_key: Option<String>,
    pub wake: JobWake,
}

/// Record and launch one job. A launch failure settles the row `failed`
/// without a wake (the caller gets the error synchronously). With `hold`
/// (a deploy drain, #1566) the row is recorded `queued` and no unit is
/// launched. Same-sandbox local peers also queue before launch (#1921);
/// [`start_queued_jobs`] launches them after their predecessor settles.
pub(crate) fn submit(
    store: &Store,
    runtime: &dyn JobRuntime,
    tools: &JobTools,
    jobs_dir: &Path,
    ctx: SubmitContext,
    now: DateTime<Utc>,
) -> Result<(AgentJobRow, bool)> {
    submit_with_hold(store, runtime, tools, jobs_dir, ctx, false, now)
}

/// [`submit`] with the deploy-drain `hold` choice made by the caller.
pub(crate) fn submit_with_hold(
    store: &Store,
    runtime: &dyn JobRuntime,
    tools: &JobTools,
    jobs_dir: &Path,
    mut ctx: SubmitContext,
    hold: bool,
    now: DateTime<Utc>,
) -> Result<(AgentJobRow, bool)> {
    recipe::fit_timeout(&mut ctx.params, &ctx.cwd)?;
    runtime
        .validate(&ctx.params)
        .map_err(|code| DaemonError::InvalidParam(code.into()))?;
    let gate_script = matches!(
        ctx.params,
        JobParams::CloudGate(_) | JobParams::CloudSweep(_)
    )
    .then(|| materialize_trusted_gate(jobs_dir))
    .transpose()
    .map_err(|error| DaemonError::Process(format!("{JOB_LAUNCH_FAILED}: {error}")))?;
    let command = job_command(tools, &ctx.params, &ctx.cwd, gate_script.as_deref())?;
    // Hub #2954: a build compiles one commit. Pin check up front, then the
    // worktree state is recorded for the start, running and finish checks.
    let source = source::at_submit(&ctx.params, &ctx.cwd)?;
    let id = Uuid::new_v4();
    let log_path = jobs_dir.join(format!("{id}.log")).display().to_string();
    let new = NewAgentJob {
        id,
        owner_session_id: ctx.owner,
        project_id: ctx.project_id,
        name: ctx.name,
        params: ctx.params,
        cwd: ctx.cwd.display().to_string(),
        unit_name: runtime.unit_name(id),
        log_path,
        status_path: jobs_dir.join(format!("{id}.status")).display().to_string(),
        idempotency_key: ctx.idempotency_key,
        wake: ctx.wake,
    };
    // Authority and validation have already passed. A deploy drain takes
    // precedence over local peers, including its bounded admission hold.
    let peer_wait = !hold
        && matches!(new.params, JobParams::Test(_) | JobParams::Build(_))
        && local_job_predecessor(store, new.owner_session_id, &new.cwd, None)?.is_some();
    // #2524: a host at or above its thermal ceiling starts no cargo unit.
    let thermal = if !hold && !peer_wait && runs_in_build_slice(&new.params) {
        thermal_hold()
    } else {
        None
    };
    // #2937: a host with no governor client bounds cargo units itself.
    let capacity = if !hold && !peer_wait && thermal.is_none() && runs_in_build_slice(&new.params) {
        capacity_hold(store)
    } else {
        None
    };
    let state = if hold || peer_wait || thermal.is_some() || capacity.is_some() {
        JobState::Queued
    } else {
        JobState::Running
    };
    // Hub #3025: the record is written before the job row exists, so a build
    // whose record cannot be written is refused, never accepted unenforced.
    if let Some(state) = &source {
        source::record(&new.log_path, state)?;
    }
    let inserted = store.insert_agent_job_with_hold_reason(
        &new,
        now,
        state,
        peer_wait
            .then_some(SANDBOX_JOB_QUEUED)
            .or(thermal.is_some().then_some(JOB_HELD_FOR_THERMAL))
            .or(capacity.is_some().then_some(JOB_HELD_FOR_CAPACITY)),
    );
    let (row, replayed) = match inserted {
        Ok(inserted) => inserted,
        Err(error) => {
            source::discard(&new.log_path);
            return Err(error);
        }
    };
    if replayed {
        // The replay's own job keeps its record; this fresh id has no job.
        source::discard(&new.log_path);
        return Ok((row, replayed));
    }
    if let Some(hot) = thermal {
        append_phase(
            &new.log_path,
            now,
            &format!(
                "queue-wait: {}; the unit has not been launched",
                thermal_hold_detail(hot)
            ),
        );
        return Ok((row, false));
    }
    if let Some(full) = capacity {
        append_phase(
            &new.log_path,
            now,
            &format!(
                "queue-wait: {}; the unit has not been launched",
                full.detail()
            ),
        );
        return Ok((row, false));
    }
    if hold || peer_wait {
        append_phase(
            &new.log_path,
            now,
            if peer_wait {
                "queue-wait: earlier local job owns this sandbox; the unit has not been launched"
            } else {
                "queue-wait: held by the deploy drain; the unit has not been launched"
            },
        );
        return Ok((row, false));
    }
    launch_recorded(store, runtime, &new, command, false)?;
    Ok((row, false))
}

/// Called under the store mutex, including immediately before the launch CAS.
/// Only earlier queued peers block: later peers must never form a wait cycle.
fn local_job_predecessor(
    store: &Store,
    owner: Uuid,
    cwd: &str,
    current: Option<Uuid>,
) -> Result<Option<AgentJobRow>> {
    let sandbox = store
        .get_session(owner)?
        .and_then(|session| session.sandbox_root);
    let scope = |path: &str| {
        let path = PathBuf::from(path);
        sandbox
            .as_ref()
            .filter(|root| path.starts_with(root))
            .map_or(path.clone(), PathBuf::from)
    };
    let target_scope = scope(cwd);
    Ok(store
        .list_running_agent_jobs()?
        .into_iter()
        .chain(
            store
                .list_queued_agent_jobs()?
                .into_iter()
                .take_while(|row| Some(row.job.id) != current),
        )
        .find(|row| {
            row.job.owner_session_id == owner
                && Some(row.job.id) != current
                && matches!(row.job.kind, JobKind::Test | JobKind::Build)
                && scope(&row.job.cwd) == target_scope
        }))
}

/// A peer's admission clock is suspended while its running predecessor is
/// alive and making progress. Log modification is durable across restarts and
/// captures ordinary build/test output as well as phase lines. A silent holder
/// gets the same ten-minute allowance as build admission; never stop its unit
/// here, since failing a waiter does not transfer lock ownership.
fn sandbox_holder_failure(
    holder: &AgentJobRow,
    runtime: &dyn JobRuntime,
    now: DateTime<Utc>,
) -> Option<AgentJobResultV1> {
    let job = &holder.job;
    if job.state != JobState::Running || read_status(&holder.status_path).is_some() {
        return None;
    }
    let wall = job_wall_secs(job, now).unwrap_or(0);
    let (refusal, reason) = if !runtime.unit_active(&job.unit_name) {
        if wall < LAUNCH_GRACE_SECS {
            return None;
        }
        (
            JOB_SANDBOX_HOLDER_DEAD,
            "its service is inactive without an exit status".into(),
        )
    } else if timed_out(job, now) {
        (
            JOB_SANDBOX_HOLDER_STALLED,
            "its start, admission or execution budget is exhausted".into(),
        )
    } else {
        let progress = std::fs::metadata(&job.log_path)
            .and_then(|meta| meta.modified())
            .ok()
            .map(DateTime::<Utc>::from);
        let idle = progress.map_or(wall, |at| (now - at).num_seconds().max(0));
        if idle <= JOB_SLOT_WAIT_ALLOWANCE_SECS as i64 {
            return None;
        }
        (
            JOB_SANDBOX_HOLDER_STALLED,
            format!("no log progress for {idle}s (limit {JOB_SLOT_WAIT_ALLOWANCE_SECS}s)"),
        )
    };
    Some(AgentJobResultV1 {
        refusal: Some(refusal.into()),
        detail: Some(format!(
            "queued sandbox job failed without launching behind holder {} (unit {}): {reason}; holder log: {}; the waiter's execution timeout never started",
            job.id, job.unit_name, job.log_path
        )),
        ..AgentJobResultV1::default()
    })
}

/// #1608: append one `rsi-phase <epoch> <text>` line to a job's log from the
/// daemon, before the unit's wrapper exists, so a job that never starts (held
/// by a deploy drain, or stuck launching) still shows where it waited. Best
/// effort: evidence must never fail a submit. The wrapper appends after it.
fn append_phase(log_path: &str, now: DateTime<Utc>, text: &str) {
    let line = format!("rsi-phase {} {text}\n", now.timestamp());
    append_job_log(log_path, &line);
}

fn append_job_log(log_path: &str, line: &str) {
    use std::io::Write;
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .and_then(|mut file| file.write_all(line.as_bytes()));
    if let Err(error) = result {
        tracing::warn!(%error, log_path, "could not write the job phase line");
    }
}

/// Launch the unit of a recorded `running` job. A failure settles the row
/// `failed` (with the owner wake when `wake_on_failure`) and returns the error.
fn launch_recorded(
    store: &Store,
    runtime: &dyn JobRuntime,
    new: &NewAgentJob,
    command: JobCommand,
    wake_on_failure: bool,
) -> Result<()> {
    let id = new.id;
    append_phase(
        &new.log_path,
        Utc::now(),
        &format!("unit-launch: requesting service {}", new.unit_name),
    );
    let launch = (|| {
        let cwd = PathBuf::from(&new.cwd);
        let owner = store
            .get_session(new.owner_session_id)
            .map_err(|error| format!("cannot resolve job owner: {error}"))?;
        let sandbox = owner
            .as_ref()
            .and_then(|session| session.sandbox_root.as_deref())
            .map(Path::new);
        let owner_is_manager = store
            .agent_authority_projection(new.owner_session_id)
            .is_ok_and(|projection| projection.is_manager);
        let build_environment = prepare_build_environment(
            &new.params,
            &cwd,
            Path::new(&new.log_path),
            owner_is_manager,
            sandbox,
        )
        .map_err(|error| format!("cannot prepare job build environment: {error}"))?;

        let spec = LaunchSpec {
            unit_name: new.unit_name.clone(),
            cwd,
            log_path: PathBuf::from(&new.log_path),
            status_path: PathBuf::from(&new.status_path),
            command,
            build_environment,
        };
        if let Some(build) = &spec.build_environment {
            append_job_log(
                &new.log_path,
                &format!("rsi-build-target {}\n", build.target_dir.display()),
            );
        }
        runtime.launch(&spec)
    })();
    if let Err(message) = launch {
        // A controller error after dispatch can leave real work alive. Keep
        // its row and scratch recoverable unless service stop is confirmed.
        if runtime.unit_active(&new.unit_name) {
            log_unit_stop(&new.unit_name, "launch failed after dispatch");
            let stopped = runtime.stop_unit(&new.unit_name);
            if stopped.is_err() || runtime.unit_active(&new.unit_name) {
                return Err(DaemonError::Process(format!(
                    "{JOB_LAUNCH_FAILED}: {message}; job {id} remains running because service stop could not be confirmed"
                )));
            }
        }
        let result = AgentJobResultV1 {
            detail: Some(tail(&message)),
            refusal: Some(JOB_LAUNCH_FAILED.into()),
            ..AgentJobResultV1::default()
        };
        store.settle_agent_job(id, JobState::Failed, &result, wake_on_failure, Utc::now())?;
        cleanup_job_tmp(Path::new(&new.log_path));
        tracing::warn!(%id, %message, "agent job launch failed");
        return Err(DaemonError::Process(format!(
            "{JOB_LAUNCH_FAILED}: {message}"
        )));
    }
    Ok(())
}

/// Launch eligible `queued` jobs after the drain and earlier local peers,
/// oldest first. The start CASes `queued -> running` before the unit is
/// launched, so a concurrent cancel or a second poll never double-launches.
/// A launch failure settles the job `failed` and wakes its owner as usual.
/// Returns the number of jobs launched.
pub(crate) async fn start_queued_jobs(
    store: &Arc<tokio::sync::Mutex<Store>>,
    runtime: &Arc<dyn JobRuntime>,
    tools: &JobTools,
    drain: &crate::deploy_drain::DeployDrain,
) -> Result<usize> {
    let queued = {
        let store = Arc::clone(store);
        tokio::task::spawn_blocking(move || {
            crate::store_lock::blocking_lock(&store, "agent_jobs::list_queued")
                .list_queued_agent_jobs()
        })
        .await
        .map_err(|error| DaemonError::Process(format!("queued job list join: {error}")))??
    };
    let mut started = 0;
    let admission_cap = admission_cap_secs(drain);
    for row in queued {
        let owner = row.job.owner_session_id;
        let has_parent = {
            let store = crate::store_lock::lock(&store, "agent_jobs::start_queued_jobs").await;
            store
                .get_session(owner)?
                .is_none_or(|session| session.parent_id.is_some())
        };
        // #2190: a queued job waits for a staged `interrupt_workers` deploy
        // too, and a held job's admission clock does not run.
        if drain.holds_job_start(Some(owner), has_parent) {
            continue;
        }
        // #2524: a hot host starts no cargo unit; the admission clock restarts
        // when it cools, as it does after a deploy hold.
        if runs_in_build_slice(&row.job.params) && thermal_hold().is_some() {
            drain.note_job_hold_engaged(Utc::now());
            continue;
        }
        if row.job.held.as_deref() != Some(SANDBOX_JOB_QUEUED)
            && admission_expired(
                &row.job,
                Utc::now(),
                admission_cap,
                drain.job_hold_last_engaged(),
            )
        {
            let store = crate::store_lock::lock(&store, "agent_jobs::start_queued_jobs").await;
            let settled = store.settle_agent_job_outcome(
                row.job.id,
                JobState::Failed,
                &admission_timed_out_result(admission_cap),
                row.job.wake == JobWake::Owner,
                Utc::now(),
            )?;
            if settled.is_some() {
                cleanup_job_tmp(Path::new(&row.job.log_path));
            }
            continue;
        }
        // #2937: with no governor client the daemon holds a cargo unit while
        // the platform build slots are full or memory is low. Unlike the
        // thermal hold, the admission clock keeps running, as cargo-slot's
        // own wait does, so a wedged host fails the job visibly.
        if runs_in_build_slice(&row.job.params)
            && capacity_hold(
                &*crate::store_lock::lock(&store, "agent_jobs::start_queued_jobs").await,
            )
            .is_some()
        {
            continue;
        }
        let runtime = Arc::clone(runtime);
        let store = Arc::clone(store);
        let tools = tools.clone();
        let launched = tokio::task::spawn_blocking(move || -> Result<bool> {
            let job = &row.job;
            let new = NewAgentJob {
                id: job.id,
                owner_session_id: job.owner_session_id,
                project_id: None,
                name: job.name.clone(),
                params: job.params.clone(),
                cwd: job.cwd.clone(),
                unit_name: job.unit_name.clone(),
                log_path: job.log_path.clone(),
                status_path: row.status_path.clone(),
                idempotency_key: None,
                wake: job.wake,
            };
            let store = crate::store_lock::blocking_lock(&store, "agent_jobs::start_queued_jobs");
            if matches!(job.kind, JobKind::Test | JobKind::Build)
                && let Some(holder) =
                    local_job_predecessor(&store, job.owner_session_id, &job.cwd, Some(job.id))?
            {
                let now = Utc::now();
                if let Some(result) = sandbox_holder_failure(&holder, &*runtime, now) {
                    let settled = store.settle_agent_job_outcome(
                        job.id,
                        JobState::Failed,
                        &result,
                        job.wake == JobWake::Owner,
                        now,
                    )?;
                    if settled.is_some() {
                        append_phase(
                            &job.log_path,
                            now,
                            result.detail.as_deref().unwrap_or_default(),
                        );
                        cleanup_job_tmp(Path::new(&job.log_path));
                    }
                }
                return Ok(false);
            }
            if let Err(result) = source::check(job, "when the held job was about to start") {
                let settled = store.settle_agent_job_outcome(
                    job.id,
                    JobState::Failed,
                    &result,
                    job.wake == JobWake::Owner,
                    Utc::now(),
                )?;
                if settled.is_some() {
                    cleanup_job_tmp(Path::new(&job.log_path));
                }
                return Ok(false);
            }
            let command = match job_command(&tools, &new.params, Path::new(&new.cwd), None) {
                Ok(command) => command,
                Err(error) => {
                    let result = AgentJobResultV1 {
                        detail: Some(tail(&error.to_string())),
                        refusal: Some(launch_refusal_code(&error)),
                        ..AgentJobResultV1::default()
                    };
                    store.settle_agent_job(
                        job.id,
                        JobState::Failed,
                        &result,
                        job.wake == JobWake::Owner,
                        Utc::now(),
                    )?;
                    return Ok(false);
                }
            };
            if !store.start_queued_agent_job(job.id, Utc::now())? {
                return Ok(false);
            }
            Ok(
                launch_recorded(&store, &*runtime, &new, command, job.wake == JobWake::Owner)
                    .is_ok(),
            )
        })
        .await
        .map_err(|error| DaemonError::Process(format!("queued job start join: {error}")))??;
        started += usize::from(launched);
    }
    Ok(started)
}

/// The exit status the wrapper recorded, if it finished.
fn read_status(path: &str) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_log_tail(path: &str) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(LOG_TAIL_READ_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    let _ = file.take(LOG_TAIL_READ_BYTES).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A run's failing tests, bounded, and how many more it had than it names.
#[derive(Debug, Default, PartialEq, Eq)]
struct FailingTests {
    names: Vec<String>,
    omitted: u32,
}

/// Failing test names from libtest (`test x ... FAILED`) and nextest
/// (`FAIL [ 0.1s] ( 5/9) crate name`, `TIMEOUT [ 60s] ( 6/9) crate name`)
/// output.
///
/// A nextest failure is reported once, as `crate name` without the progress
/// counter, and a timeout once as `TIMEOUT crate name` (the label first, so
/// the last word is still the exact test path). Nextest also echoes the
/// failing test's libtest output, so the same test would reappear as a bare
/// `name`; the bare form is dropped when a `crate name` line already names it
/// (#2172). One outcome per exact `<binary> <test>`, the latest status line
/// winning (a test that failed, then timed out, is one timeout); the same test
/// path in two binaries stays two names.
///
/// `omitted` counts every failure the run had but this list does not name: the
/// names past [`MAX_FAILING_TESTS`], and failures nextest's own summary counts
/// that no status line in the log tail names. An absent name is never a clean
/// test (#185).
fn failing_test_report(text: &str) -> FailingTests {
    let mut bare = all_failing_tests(text);
    // One outcome per exact `<binary> <test>`: the latest status line wins, so a
    // test that failed and then timed out (or the final section repeating the
    // live one) is named once, with its last terminal outcome.
    let mut outcomes: std::collections::BTreeMap<&str, bool> = std::collections::BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        let (timed_out, rest) = if let Some(rest) = line.strip_prefix("FAIL [") {
            (false, rest)
        } else if let Some(rest) = line.strip_prefix("TIMEOUT [") {
            (true, rest)
        } else {
            continue;
        };
        let Some((_, name)) = rest.split_once(']') else {
            continue;
        };
        let name = strip_nextest_progress(name.trim());
        if !name.is_empty() {
            outcomes.insert(name, timed_out);
        }
    }
    bare.retain(|bare| {
        !outcomes
            .keys()
            .any(|full| full.split_whitespace().next_back() == Some(bare.as_str()))
    });
    let named_by_nextest = outcomes.len();
    let mut names: Vec<String> = outcomes
        .into_iter()
        .map(|(name, timed_out)| (timed_out, name))
        .map(|(timed_out, name)| {
            if timed_out {
                format!("TIMEOUT {name}")
            } else {
                name.to_string()
            }
        })
        .chain(bare)
        .collect();
    names.sort();
    names.dedup();
    let unnamed = nextest_summary_failures(text).map_or(0, |counted| {
        counted.saturating_sub(u32::try_from(named_by_nextest).unwrap_or(u32::MAX))
    });
    let overflow = names.len().saturating_sub(MAX_FAILING_TESTS);
    names.truncate(MAX_FAILING_TESTS);
    FailingTests {
        names,
        omitted: unnamed.saturating_add(u32::try_from(overflow).unwrap_or(u32::MAX)),
    }
}

/// Failures plus timeouts the last nextest summary line counts
/// (`Summary [..] 8223 tests run: 8201 passed (95 slow), 7 failed, 15 timed
/// out, 22 skipped`); `None` when the log has no summary line.
fn nextest_summary_failures(text: &str) -> Option<u32> {
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("Summary [") && line.contains(" run:"))?;
    let (_, counts) = line.split_once(" run:")?;
    let words: Vec<&str> = counts.split_whitespace().collect();
    let mut total = 0u32;
    for (index, word) in words.iter().enumerate() {
        let Ok(count) = word.parse::<u32>() else {
            continue;
        };
        let label = words.get(index + 1).map(|w| w.trim_end_matches(','));
        let timed_out = label == Some("timed")
            && words
                .get(index + 2)
                .is_some_and(|next| next.starts_with("out"));
        if label == Some("failed") || timed_out {
            total = total.saturating_add(count);
        }
    }
    Some(total)
}

/// `name` without nextest's leading progress counter, `( 5774/8220) `: the
/// position of the test in this run says nothing about which test failed.
fn strip_nextest_progress(name: &str) -> &str {
    let Some((counter, rest)) = name
        .strip_prefix('(')
        .and_then(|inner| inner.split_once(')'))
    else {
        return name;
    };
    let is_count = |digits: &str| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    match counter.split_once('/') {
        Some((done, total)) if is_count(done.trim()) && is_count(total.trim()) => rest.trim_start(),
        _ => name,
    }
}

/// Where `cloud-sweep.sh` collects one sweep (`~/.rsi/cloud/results/<sha>`).
fn sweep_results_dir(sha: &str) -> PathBuf {
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join(".rsi/cloud/results")
        .join(sha)
}

/// The sweep's final machine line: exactly `VERDICT <STATE> <sha> new=<n>`.
/// Only the last line that starts with `VERDICT ` counts; it must match this
/// grammar word for word (no other tokens, GREEN only with `new=0`, RED only
/// with `new>=1`) and name `sha`. Anything else, or no line, is `None` (the
/// caller treats that as INCOMPLETE). Human notes belong on other lines.
pub(crate) fn parse_sweep_verdict_line(log: &str, sha: &str) -> Option<(SweepVerdict, usize)> {
    let line = log
        .lines()
        .rev()
        .map(|line| line.trim_end_matches(['\r', ' ', '\t']))
        .find(|line| line.starts_with("VERDICT "))?;
    let mut words = line.split(' ');
    let (_, state, line_sha, count) = (words.next()?, words.next()?, words.next()?, words.next()?);
    if words.next().is_some() || line_sha != sha {
        return None;
    }
    let digits = count.strip_prefix("new=")?;
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let new: usize = digits.parse().ok()?;
    match (state, new) {
        ("GREEN", 0) => Some((SweepVerdict::Green, 0)),
        ("RED", 1..) => Some((SweepVerdict::Red, new)),
        ("INCOMPLETE", _) => Some((SweepVerdict::Incomplete, new)),
        _ => None,
    }
}

/// The verdict of the sweep's final machine line (`parse_sweep_verdict_line`).
pub(crate) fn parse_sweep_verdict(log: &str, sha: &str) -> Option<SweepVerdict> {
    parse_sweep_verdict_line(log, sha).map(|(verdict, _)| verdict)
}

/// `parse_sweep_failures` plus the exact, unbounded number of `- NEW ` lines in
/// the verdict section (counted even when the bounded list drops names).
pub(crate) fn parse_sweep_report(report: &str) -> (Vec<String>, Vec<String>, usize) {
    let section = report
        .rfind("## Verdict")
        .map_or("", |start| &report[start..]);
    let (mut new, mut known) = (Vec::new(), Vec::new());
    let mut new_count = 0;
    for line in section.lines() {
        let line = line.trim();
        let (bucket, rest) = if let Some(rest) = line.strip_prefix("- NEW ") {
            new_count += 1;
            (&mut new, rest)
        } else if let Some(rest) = line.strip_prefix("- KNOWN ") {
            (&mut known, rest)
        } else {
            continue;
        };
        let Some((issue, tail)) = rest.split_once('`') else {
            continue;
        };
        let Some((name, after)) = tail.split_once('`') else {
            continue;
        };
        let issue = issue.trim();
        let mut entry = if issue.is_empty() {
            name.to_string()
        } else {
            format!("{issue} {name}")
        };
        // A crashed or timed-out test carries its class (#1120).
        if let Some(class) = sweep_failure_class(after) {
            entry.push_str(&format!(" [{class}]"));
        }
        if bucket.len() < SWEEP_MAX_FAILURES && !entry.is_empty() {
            bucket.push(entry);
        }
    }
    (new, known, new_count)
}

/// The `crash` or `timeout` class tag (`[crash]`) right after a verdict line's
/// backticked test name, when the report writer marked one (#1120).
fn sweep_failure_class(after_name: &str) -> Option<&'static str> {
    let tag = after_name.trim_start();
    ["crash", "timeout"].into_iter().find(|class| {
        tag.strip_prefix('[')
            .and_then(|t| t.strip_prefix(class))
            .is_some_and(|t| t.starts_with(']'))
    })
}

/// Largest `QA.md` a GREEN may rest on. A bigger report is never truncated
/// silently: the verdict is INCOMPLETE.
const SWEEP_REPORT_MAX_BYTES: u64 = 2 * MIB;

/// Lanes `scripts/cloud-sweep.sh remote` runs besides the 16 rsid library
/// shards (`rsid-<shard>`); a complete report has a row for each.
const SWEEP_FIXED_LANES: [&str; 7] = [
    "rsid-integrations",
    "rsid-bins",
    "rsi",
    "rsi-common",
    "other-workspace",
    "rsid-doctests",
    "other-doctests",
];
/// Every lane the sweep runs, by label: `rsid-<shard>` for each library shard
/// (the union of the `test-shard-*` features of the `rsid` and `rsid-store`
/// manifests, the same list `check-rsid-test-shards.py --list-shards` gives the
/// sweep script) plus the fixed lanes. A GREEN report has each exactly once and
/// no other lane.
fn sweep_expected_lanes() -> Vec<String> {
    let shards: std::collections::BTreeSet<String> = [
        include_str!("../Cargo.toml"),
        include_str!("../../rsid-store/Cargo.toml"),
    ]
    .into_iter()
    .flat_map(str::lines)
    .filter_map(|line| {
        let shard = line.trim().strip_prefix("test-shard-")?;
        let (name, _) = shard.split_once(" = ")?;
        (name != "mode").then(|| format!("rsid-{name}"))
    })
    .collect();
    let mut lanes: Vec<String> = shards.into_iter().collect();
    lanes.extend(SWEEP_FIXED_LANES.iter().map(|lane| lane.to_string()));
    lanes.sort_unstable();
    lanes
}

/// The one anchored `Tip SHA: `<40-hex>`` header value on `line`, if it is one.
fn tip_sha_header(line: &str) -> Option<&str> {
    let value = line
        .trim_end()
        .strip_prefix("Tip SHA: `")?
        .strip_suffix('`')?;
    (value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    .then_some(value)
}

/// Structural check of the report body, mirroring what the QA.md writer
/// (`cloud-sweep-report.py` plus `cloud-sweep-verdict.py`) emits for a GREEN:
/// exactly one anchored Tip SHA header equal to `sha`, one `Lanes: N` header,
/// a table of N complete rows (label plus four numeric cells) naming exactly
/// the expected lanes once each, exactly one `## Verdict` section after the
/// table with no NEW entry and no incomplete/harness marker, and a `- KNOWN`
/// entry whenever a lane exited nonzero (the writer calls that RED, or
/// INCOMPLETE for a harness failure, unless every failing name is known).
fn sweep_report_is_complete(text: &str, sha: &str) -> bool {
    let (mut tips, mut lane_headers, mut declared) = (0, 0, 0usize);
    let (mut in_table, mut labels) = (false, Vec::<&str>::new());
    let (mut verdicts, mut nonzero_exit, mut in_verdict) = (0, false, false);
    let mut known = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.contains(":build_or_harness_failure") || trimmed.starts_with("Incomplete:") {
            return false;
        }
        if trimmed.starts_with("## Verdict") {
            verdicts += 1;
            in_verdict = true;
            in_table = false;
            continue;
        }
        if in_verdict {
            if trimmed.starts_with("- NEW ") || trimmed.starts_with("Tip SHA:") {
                return false;
            }
            known |= trimmed.starts_with("- KNOWN ");
            continue;
        }
        if trimmed.starts_with("Tip SHA:") {
            tips += 1;
            if tip_sha_header(line) != Some(sha) {
                return false;
            }
        } else if let Some(count) = trimmed.strip_prefix("Lanes:") {
            lane_headers += 1;
            declared = count.trim().parse().unwrap_or(0);
        } else if trimmed == "| Lane | Pass | Fail | Skip | Exit |" {
            in_table = true;
        } else if in_table && trimmed.starts_with('|') {
            let cells: Vec<&str> = trimmed
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect();
            if cells
                .iter()
                .all(|cell| cell.chars().all(|c| c == '-' || c == ':'))
            {
                continue; // the header separator
            }
            let numeric =
                |cell: &&str| !cell.is_empty() && cell.bytes().all(|b| b.is_ascii_digit());
            if cells.len() != 5 || cells[0].is_empty() || !cells[1..].iter().all(numeric) {
                return false;
            }
            nonzero_exit |= cells[4] != "0" && !cells[4].bytes().all(|b| b == b'0');
            labels.push(cells[0]);
        } else {
            in_table = false;
        }
    }
    let mut seen = labels.clone();
    seen.sort_unstable();
    tips == 1
        && lane_headers == 1
        && verdicts == 1
        && declared == labels.len()
        && seen == sweep_expected_lanes()
        && (known || !nonzero_exit)
}

/// Why a report cannot back a GREEN, or `None` when it is complete and exact:
/// readable, within the size cap, structurally complete for this SHA (one Tip
/// SHA header, every lane row), with a verdict section whose NEW count agrees
/// with the log's and whose final nonempty line is the log's verdict line.
fn sweep_report_problem(
    report: &std::io::Result<Vec<u8>>,
    sha: &str,
    log_verdict: (SweepVerdict, usize),
    new_count: usize,
) -> Option<&'static str> {
    let bytes = match report {
        Ok(bytes) => bytes,
        Err(_) => return Some("sweep_report_unreadable"),
    };
    if bytes.len() as u64 > SWEEP_REPORT_MAX_BYTES {
        return Some("sweep_report_oversized");
    }
    let text = String::from_utf8_lossy(bytes);
    if !sweep_report_is_complete(&text, sha) || !text.contains("## Verdict") {
        return Some("sweep_report_incomplete");
    }
    let last = text.lines().rev().find(|line| !line.trim().is_empty());
    let final_line = last.and_then(|line| parse_sweep_verdict_line(line, sha));
    if new_count != log_verdict.1 || final_line != Some(log_verdict) {
        return Some("sweep_report_mismatch");
    }
    None
}

/// The typed outcome of a finished `cloud_sweep` job. Anything but a clean,
/// exact `VERDICT <STATE> <sha> new=<n>` line is INCOMPLETE. A GREEN also
/// needs a zero exit and a complete, readable report for this SHA whose NEW
/// count agrees with the line; otherwise it is INCOMPLETE too.
#[cfg(test)]
pub(crate) fn classify_cloud_sweep(
    sha: &str,
    exit_code: Option<i32>,
    log: &str,
    result: AgentJobResultV1,
    results_dir: &Path,
) -> (JobState, AgentJobResultV1) {
    classify_cloud_sweep_timed(sha, exit_code, log, result, results_dir, None)
}

/// Longest filtered log tail an INCOMPLETE sweep's detail may carry.
const SWEEP_DETAIL_TAIL_BYTES: usize = 600;

/// Whether a log line is ssh client noise (post-quantum key-exchange and
/// "Warning: Permanently added" warnings) that never belongs in a wake.
fn is_ssh_client_noise(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("post-quantum")
        || lower.contains("store now, decrypt later")
        || lower.contains("permanently added")
        || lower.contains("openssh.com/pq.html")
        || lower.contains("upgraded to a newer version")
        || lower.trim_start().starts_with("** ")
}

/// The last few log lines without ssh client warnings, bounded.
fn filtered_log_tail(log: &str) -> String {
    let lines: Vec<&str> = log
        .lines()
        .filter(|line| !line.trim().is_empty() && !is_ssh_client_noise(line))
        .collect();
    let start = lines.len().saturating_sub(4);
    let joined = lines[start..].join(" | ");
    let mut cut = joined.len().saturating_sub(SWEEP_DETAIL_TAIL_BYTES);
    while !joined.is_char_boundary(cut) {
        cut += 1;
    }
    joined[cut..].to_string()
}

/// The `Lanes: <n>` count of a `QA.md` report.
fn sweep_report_lanes(report: &str) -> Option<usize> {
    report
        .lines()
        .find_map(|line| line.trim().strip_prefix("Lanes:")?.trim().parse().ok())
}

/// The compact summary a sweep wake and typed result carry instead of the
/// log: NEW and KNOWN counts, lane count and wall time. The verdict, sha,
/// bounded failure names, results dir and log path travel as typed fields;
/// the full report stays in the results dir. An INCOMPLETE sweep with no
/// report adds a short ssh-warning-free log tail so the cause is visible.
fn sweep_detail(
    new_count: usize,
    (crashed, timed_out): (usize, usize),
    known_count: usize,
    lanes: Option<usize>,
    wall_secs: Option<i64>,
    incomplete_tail: Option<String>,
) -> String {
    let mut text = format!("new={new_count}; known={known_count}");
    if crashed > 0 {
        text.push_str(&format!("; crash={crashed}"));
    }
    if timed_out > 0 {
        text.push_str(&format!("; timeout={timed_out}"));
    }
    if let Some(lanes) = lanes {
        text.push_str(&format!("; lanes={lanes}"));
    }
    if let Some(secs) = wall_secs {
        text.push_str(&format!("; wall={}m{:02}s", secs / 60, secs % 60));
    }
    if let Some(tail) = incomplete_tail.filter(|tail| !tail.is_empty()) {
        text.push_str(&format!("; log_tail={tail}"));
    }
    text
}

/// [`classify_cloud_sweep`] with the job's wall time in seconds, when known.
pub(crate) fn classify_cloud_sweep_timed(
    sha: &str,
    exit_code: Option<i32>,
    log: &str,
    mut result: AgentJobResultV1,
    results_dir: &Path,
    wall_secs: Option<i64>,
) -> (JobState, AgentJobResultV1) {
    use std::io::Read;
    // One byte past the cap tells "exactly the cap" from "larger".
    let report: std::io::Result<Vec<u8>> =
        std::fs::File::open(results_dir.join("QA.md")).and_then(|file| {
            let mut bytes = Vec::new();
            file.take(SWEEP_REPORT_MAX_BYTES + 1)
                .read_to_end(&mut bytes)?;
            Ok(bytes)
        });
    let report_text = report
        .as_ref()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default();
    let (new_failures, known_failures, new_count) = parse_sweep_report(&report_text);
    let line = parse_sweep_verdict_line(log, sha);
    let mut verdict = line.map_or(SweepVerdict::Incomplete, |(verdict, _)| verdict);
    if let Some(line) = line.filter(|(verdict, _)| *verdict == SweepVerdict::Green) {
        let problem = if exit_code != Some(0) {
            Some("sweep_exit_nonzero")
        } else {
            sweep_report_problem(&report, sha, line, new_count)
        };
        if let Some(problem) = problem {
            verdict = SweepVerdict::Incomplete;
            result.refusal = Some(problem.into());
        }
    }
    if parse_sweep_verdict(log, sha).is_none() {
        // The script ended before its verdict step: name why when it says so.
        result.refusal = [
            ("refusing to start a remote run", "cloud_spend_refused"),
            ("another gate host is running", "cloud_gate_busy"),
            ("another cloud sweep is active", "cloud_gate_busy"),
            (
                "requested SHA is not the current rolling tip",
                "sweep_sha_not_rolling_tip",
            ),
        ]
        .iter()
        .find_map(|(needle, code)| log.contains(needle).then(|| (*code).to_string()))
        .or_else(|| Some("sweep_verdict_missing".into()));
    }
    // The wake carries a compact typed summary, never the log or the report.
    let incomplete_tail = (verdict == SweepVerdict::Incomplete).then(|| filtered_log_tail(log));
    result.failing_tests = Vec::new();
    result.failing_tests_omitted = 0;
    let class_count = |class: &str| {
        let suffix = format!(" [{class}]");
        new_failures
            .iter()
            .filter(|name| name.ends_with(&suffix))
            .count()
    };
    result.detail = Some(sweep_detail(
        new_count.max(new_failures.len()),
        (class_count("crash"), class_count("timeout")),
        known_failures.len(),
        sweep_report_lanes(&report_text),
        wall_secs,
        incomplete_tail,
    ));
    result.sweep = Some(CloudSweepResultV1 {
        verdict,
        sha: sha.to_string(),
        results_dir: results_dir.display().to_string(),
        new_failures,
        known_failures,
    });
    let state = if verdict == SweepVerdict::Green {
        JobState::Succeeded
    } else {
        JobState::Failed
    };
    (state, result)
}

/// Seconds from the job's launch (its creation when never held) to `now`.
fn job_wall_secs(job: &AgentJobV1, now: DateTime<Utc>) -> Option<i64> {
    let created =
        DateTime::parse_from_rfc3339(job.started_at.as_deref().unwrap_or(&job.created_at)).ok()?;
    Some((now - created.with_timezone(&Utc)).num_seconds().max(0))
}

/// The receipt a `candidate_receipt` job printed: the last `RECEIPT_JSON ` line
/// of the log, when it is a JSON object.
pub(crate) fn parse_receipt_line(log: &str) -> Option<serde_json::Value> {
    log.lines()
        .rev()
        .find_map(|line| line.trim_end().strip_prefix(RECEIPT_LINE_PREFIX))
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .filter(serde_json::Value::is_object)
}

/// The receipt a `scoped_test` job printed (#1638): the last
/// `SCOPED_TEST_RECEIPT ` line of the log, when it is a JSON object.
pub(crate) fn parse_scoped_receipt_line(log: &str) -> Option<serde_json::Value> {
    log.lines()
        .rev()
        .find_map(|line| line.trim_end().strip_prefix(SCOPED_RECEIPT_LINE_PREFIX))
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .filter(serde_json::Value::is_object)
}

/// The receipt a `test_no_run` sweep printed (#2692): the last
/// `SHARD_SWEEP_RECEIPT ` line of the log, when it is a JSON object.
pub(crate) fn parse_shard_sweep_receipt_line(log: &str) -> Option<serde_json::Value> {
    log.lines()
        .rev()
        .find_map(|line| {
            line.trim_end()
                .strip_prefix(SHARD_SWEEP_RECEIPT_LINE_PREFIX)
        })
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .filter(serde_json::Value::is_object)
}

/// One line for a shard sweep: `shard-sweep ok: store-01 exit 0 (0 warnings)`.
fn shard_sweep_summary(receipt: &serde_json::Value) -> String {
    let package = receipt
        .get("package")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let ok = receipt.get("ok").and_then(serde_json::Value::as_bool) == Some(true);
    let shards: Vec<String> = receipt
        .get("shards")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .map(|entry| {
            let name = entry
                .get("shard")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let code = entry
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .map_or("none".to_string(), |code| code.to_string());
            let warnings: u64 = entry
                .get("warnings")
                .and_then(serde_json::Value::as_object)
                .map_or(0, |counts| {
                    counts.values().filter_map(serde_json::Value::as_u64).sum()
                });
            format!("{name} exit {code} ({warnings} warnings)")
        })
        .collect();
    format!(
        "shard-sweep {} {package}: {}",
        if ok { "ok" } else { "failed" },
        shards.join(", ")
    )
}

/// A `test_no_run` build succeeds only with a clean exit and a receipt whose
/// `ok` is true (every shard built, every clean worked, the warning gate held).
/// The receipt rides in the result either way; a run that ended before the
/// script could print one is the typed refusal `shard_sweep_receipt_missing`.
fn classify_shard_sweep(
    exit_code: Option<i32>,
    log: &str,
    mut result: AgentJobResultV1,
) -> (JobState, AgentJobResultV1) {
    let receipt = parse_shard_sweep_receipt_line(log);
    let ok = receipt
        .as_ref()
        .and_then(|r| r.get("ok"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    if receipt.is_none() {
        result.refusal = Some("shard_sweep_receipt_missing".into());
    }
    let state = if exit_code == Some(0) && ok {
        JobState::Succeeded
    } else {
        JobState::Failed
    };
    // The typed receipt is the canonical result; `detail` never embeds it.
    result.detail = match &receipt {
        Some(receipt) => Some(shard_sweep_summary(receipt)),
        None => Some(tail(
            &log.lines()
                .filter(|line| !line.starts_with(SHARD_SWEEP_RECEIPT_LINE_PREFIX))
                .collect::<Vec<_>>()
                .join("\n"),
        ))
        .filter(|t| !t.is_empty()),
    };
    result.receipt = receipt;
    (state, result)
}

/// #2121: the cause line on a job whose unit ended without a status. Every
/// stop the daemon itself issues is logged (`log_unit_stop`) and settles the job
/// with its own refusal, so a job that reaches the lost path was ended by
/// someone else.
const UNIT_ENDED_OUTSIDE_RSID: &str = "the job unit ended without writing a status, and rsid did \
not stop it (no daemon stop was issued for this job): an external `systemctl stop` (for example a \
glob over `rsi-job-*`), a host restart or a kill ended it. This is not a test failure; re-run \
the job. Check `journalctl --user -u <unit>` for the stop time.";

/// A systemd service stopped by an external `systemctl stop` records the
/// terminating signal in its unit properties. Keep the raw result and status
/// in the job result so an interrupted scoped run is distinguishable from a
/// unit that vanished without an explanation.
fn external_stop_status(diagnostics: &str) -> Option<(String, i32)> {
    let properties: std::collections::HashMap<_, _> = diagnostics
        .split_whitespace()
        .filter_map(|part| part.split_once('='))
        .collect();
    let result = properties.get("Result")?;
    let status = properties.get("ExecMainStatus")?.parse::<i32>().ok()?;
    ((*result == "signal" && status > 0) || (*result == "success" && status == 15))
        .then(|| ((*result).to_string(), status))
}

fn stopped_scoped_test_result(
    log: &str,
    unit: &str,
    result: &str,
    status: i32,
) -> AgentJobResultV1 {
    let mut receipt = parse_scoped_receipt_line(log);
    if let Some(receipt) = receipt.as_mut().and_then(serde_json::Value::as_object_mut) {
        receipt.insert("ok".into(), false.into());
        receipt.insert("partial".into(), true.into());
        receipt.insert(
            "termination".into(),
            serde_json::json!({"unit": unit, "result": result, "exec_main_status": status}),
        );
    }
    AgentJobResultV1 {
        refusal: Some("stopped_externally".into()),
        detail: Some(format!(
            "scoped-test unit stopped externally: Result={result} ExecMainStatus={status}\n{}",
            tail(&log_without_receipt_lines(log))
        )),
        receipt,
        ..AgentJobResultV1::default()
    }
}

/// A project recipe that runs `scripts/scoped-test` (`scoped-test`,
/// `scoped-test-tmpfs`, #1795): it settles like a `scoped_test` job. Any other
/// recipe does so only when its log carries the script's receipt line.
fn is_scoped_recipe(p: &TestJobParams, log: &str) -> bool {
    p.recipe.as_deref().is_some_and(|name| {
        name.starts_with("scoped-test") || parse_scoped_receipt_line(log).is_some()
    })
}

/// The exact failing tests a scoped-test receipt names (#1795, #1770):
/// `<package> <test>`, with the target between them for a non-lib target.
fn scoped_receipt_failing_tests(receipt: &serde_json::Value) -> Vec<String> {
    let text = |item: &serde_json::Value, key: &str| {
        item.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let mut names: Vec<String> = receipt
        .get("failures")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| !text(item, "test").is_empty())
        .map(|item| {
            let (package, kind, target) = (
                text(item, "package"),
                text(item, "kind"),
                text(item, "target"),
            );
            let test = text(item, "test");
            if kind == "lib" || target.is_empty() {
                format!("{package} {test}")
            } else {
                format!("{package} {target} {test}")
            }
        })
        .collect();
    names.sort();
    names.dedup();
    names.truncate(MAX_FAILING_TESTS);
    names
}

/// A `scoped_test` job succeeds only with a clean exit and a receipt whose
/// `ok` is true. The receipt rides in the result either way; a run that ended
/// before the script could print one is the typed refusal
/// `scoped_test_receipt_missing`. `exit_code` and the log path are already on
/// the result and the job.
fn classify_scoped_test(
    exit_code: Option<i32>,
    log: &str,
    mut result: AgentJobResultV1,
) -> (JobState, AgentJobResultV1) {
    let receipt = parse_scoped_receipt_line(log);
    let ok = receipt
        .as_ref()
        .and_then(|r| r.get("ok"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    if receipt.is_none() {
        result.refusal = Some("scoped_test_receipt_missing".into());
    }
    // The receipt names the exact failures; the log scan stays the fallback.
    if let Some(names) = receipt
        .as_ref()
        .map(scoped_receipt_failing_tests)
        .filter(|names| !names.is_empty())
    {
        result.failing_tests = names;
        result.failing_tests_omitted = 0;
    }
    let state = if exit_code == Some(0) && ok {
        JobState::Succeeded
    } else {
        JobState::Failed
    };
    // The typed receipt is the one canonical result; `detail` never embeds its
    // serialization (#1914). A green run gets a one-line package verdict, any
    // other run keeps the bounded log tail minus the receipt line.
    result.detail = match (&receipt, state) {
        (Some(receipt), JobState::Succeeded) => scoped_receipt_summary(receipt),
        _ => Some(tail(&log_without_receipt_lines(log))).filter(|t| !t.is_empty()),
    };
    result.receipt = receipt;
    (state, result)
}

/// The log with every `SCOPED_TEST_RECEIPT` line removed.
fn log_without_receipt_lines(log: &str) -> String {
    log.lines()
        .filter(|line| {
            !line
                .trim_start()
                .starts_with(SCOPED_RECEIPT_LINE_PREFIX.trim_end())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `scoped-test passed: rsid 3 tests, rsid-store 1 tests`: the package verdicts
/// of a scoped-test receipt as one short line.
fn scoped_receipt_summary(receipt: &serde_json::Value) -> Option<String> {
    let packages: Vec<String> = receipt
        .get("packages")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .filter_map(|item| {
            let name = item.get("package")?.as_str()?;
            let status = item
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            Some(
                match item.get("completed").and_then(serde_json::Value::as_u64) {
                    Some(n) => format!("{name} {status} ({n} tests)"),
                    None => format!("{name} {status}"),
                },
            )
        })
        .collect();
    let verdict = if packages.is_empty() {
        "scoped-test passed".to_string()
    } else {
        format!("scoped-test passed: {}", packages.join(", "))
    };
    Some(verdict)
}

/// A `candidate_receipt` job succeeds only with a clean exit and a receipt
/// whose `ok` is true. The typed receipt rides in the result either way, and
/// `detail` is the one summary line (not the log tail).
fn classify_candidate_receipt(
    exit_code: Option<i32>,
    log: &str,
    mut result: AgentJobResultV1,
) -> (JobState, AgentJobResultV1) {
    let receipt = parse_receipt_line(log);
    let ok = receipt
        .as_ref()
        .and_then(|r| r.get("ok"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    result.detail = log
        .lines()
        .rev()
        .find(|line| line.starts_with("check-touched-shards "))
        .map(str::to_string)
        .or(result.detail);
    if receipt.is_none() {
        result.refusal = Some("candidate_receipt_missing".into());
    }
    result.receipt = receipt;
    let state = if exit_code == Some(0) && ok {
        JobState::Succeeded
    } else {
        JobState::Failed
    };
    (state, result)
}

/// Map a finished job to its terminal state and typed result.
pub(crate) fn classify(
    job: &AgentJobV1,
    exit_code: Option<i32>,
    log: &str,
) -> (JobState, AgentJobResultV1) {
    let failing = failing_test_report(log);
    let mut result = AgentJobResultV1 {
        exit_code,
        failing_tests: failing.names,
        failing_tests_omitted: failing.omitted,
        detail: Some(tail(log)).filter(|t| !t.is_empty()),
        ..AgentJobResultV1::default()
    };
    // cargo-slot may stop admission itself before the next daemon poll. Keep
    // that typed cause even for scoped tests that never reached their runner.
    if exit_code == Some(124)
        && let Some(phase) = log.lines().rev().find_map(parse_phase)
        && phase.text.starts_with("admission-timeout:")
    {
        result.refusal = Some(JOB_ADMISSION_TIMED_OUT.into());
        result.detail = Some(phase.text);
        result.receipt = parse_scoped_receipt_line(log);
        return (JobState::Failed, result);
    }
    if let JobParams::CloudSweep(p) = &job.params {
        let wall = job_wall_secs(job, Utc::now());
        return classify_cloud_sweep_timed(
            &p.sha,
            exit_code,
            log,
            result,
            &sweep_results_dir(&p.sha),
            wall,
        );
    }
    if let JobParams::Build(p) = &job.params
        && p.command == rsi_common::agent_jobs::BuildCommand::TestNoRun
    {
        return classify_shard_sweep(exit_code, log, result);
    }
    if let JobParams::Test(p) = &job.params
        && p.is_candidate_receipt()
    {
        return classify_candidate_receipt(exit_code, log, result);
    }
    if let JobParams::Test(p) = &job.params
        && (p.is_scoped_test() || is_scoped_recipe(p, log))
    {
        let (state, mut result) = classify_scoped_test(exit_code, log, result);
        // #1979: the receipt states the job timeout the run was held to.
        if let (Some(receipt), Some(minutes)) = (
            result.receipt.as_mut().and_then(|r| r.as_object_mut()),
            p.timeout_minutes,
        ) {
            receipt.insert("job_timeout_minutes".into(), minutes.into());
        }
        return (state, result);
    }
    let landing_source = match &job.params {
        JobParams::Landing(p) | JobParams::CloudGate(p) => Some(p.accepted.clone()),
        _ => None,
    };
    if let Some(source) = landing_source {
        let cwd = PathBuf::from(&job.cwd);
        let run =
            crate::rolling_queue::classify_run(exit_code, log, log, || landed_tip(&cwd, &source));
        result.landed_sha = run.outcome.landed_sha;
        result.refusal = run.outcome.refusal;
        if !run.outcome.failing_tests.is_empty() {
            result.failing_tests = run.outcome.failing_tests;
            result.failing_tests_omitted = 0;
        }
        let landed = result.landed_sha.is_some();
        return (
            if landed {
                JobState::Succeeded
            } else {
                JobState::Failed
            },
            result,
        );
    }
    let state = if exit_code == Some(0) {
        JobState::Succeeded
    } else {
        JobState::Failed
    };
    (state, result)
}

/// Whether the disk blocks under `dir` exceed `cap` bytes. Symlinks are not
/// followed; the walk stops as soon as the cap is passed.
fn scratch_exceeds(dir: &Path, cap: u64) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mut used: u64 = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            used = used.saturating_add(meta.blocks().saturating_mul(512));
            if used > cap {
                return true;
            }
            if meta.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    false
}

/// A running test/build job whose scratch is over `quota`: stop its unit and
/// return the failed settlement only after confirming it is inactive. `None`
/// when within quota or stopping is uncertain, preserving custody for retry.
fn scratch_quota_breach(
    job: &rsi_common::agent_jobs::AgentJobV1,
    runtime: &dyn JobRuntime,
    quota: u64,
) -> Option<(JobState, AgentJobResultV1)> {
    if !matches!(job.kind, JobKind::Test | JobKind::Build) {
        return None;
    }
    if !scratch_exceeds(&job_tmp_dir(Path::new(&job.log_path)), quota) {
        return None;
    }
    log_unit_stop(&job.unit_name, "job scratch over its disk quota");
    if let Err(error) = runtime.stop_unit(&job.unit_name) {
        tracing::warn!(unit = %job.unit_name, %error, "cannot stop over-quota agent job unit");
        return None;
    }
    if runtime.unit_active(&job.unit_name) {
        tracing::warn!(unit = %job.unit_name, "over-quota agent job unit is still active after stop");
        return None;
    }
    let result = AgentJobResultV1 {
        refusal: Some(JOB_SCRATCH_QUOTA_EXCEEDED.into()),
        detail: Some(format!(
            "job scratch (TMPDIR) exceeded {} GiB; the job was stopped and its scratch removed",
            quota / (1024 * MIB)
        )),
        ..AgentJobResultV1::default()
    };
    Some((JobState::Failed, result))
}

fn scoped_job(job: &AgentJobV1) -> bool {
    matches!(&job.params, JobParams::Test(p) if p.is_scoped_test())
}

fn scoped_admission_expired(job: &AgentJobV1, wall: u64, limit: u64) -> bool {
    wall > limit.saturating_sub(SCOPED_JOB_RUNTIME_RESERVE_SECS)
        && read_log_phases(&job.log_path).last().is_some_and(|phase| {
            phase.text.starts_with("slot-wait:")
                || phase.text.starts_with("artifact-lock-wait:")
                || phase.text.starts_with("sandbox-lock-wait:")
        })
}

/// Scoped jobs count waits in their wall budget; other jobs keep separate admission.
fn timed_out(job: &AgentJobV1, now: DateTime<Utc>) -> bool {
    job_timeout_secs(&job.params).is_some_and(|limit| {
        // Execution and wait durations cannot exceed total wall time. Avoid
        // scanning a potentially large log while all budgets are still safe.
        let allowance = admission_allowance_secs(&job.params);
        let safe_wall = if scoped_job(job) {
            limit
                .saturating_sub(SCOPED_JOB_RUNTIME_RESERVE_SECS)
                .min(JOB_START_WAIT_MAX_SECS)
        } else {
            // The start check below applies at the start cap, whatever the
            // admission allowance (#2149).
            limit.min(allowance).min(JOB_START_WAIT_MAX_SECS)
        };
        if job_wall_secs(job, now).is_some_and(|wall| wall <= safe_wall as i64) {
            return false;
        }
        let started = log_job_start(&job.log_path);
        if started == Some(None) {
            return job_wall_secs(job, now)
                .is_some_and(|wall| wall > JOB_START_WAIT_MAX_SECS as i64);
        }
        let wall = started
            .flatten()
            .map(|at| now.timestamp().saturating_sub(at))
            .or_else(|| job_wall_secs(job, now))
            .unwrap_or(0)
            .max(0) as u64;
        if scoped_job(job) {
            return wall > limit || scoped_admission_expired(job, wall, limit);
        }
        let waited = log_wait_secs(&job.log_path, now).unwrap_or(0);
        waited > allowance || wall.saturating_sub(waited) > limit
    })
}

/// #1756: seconds a job log spent waiting for a governor build slot
/// (`scripts/cargo-slot`) or the artifact lock, summed from its `rsi-phase`
/// lines. The separate admission cap does not truncate accounting. A wait with no grant yet
/// counts up to `now`. `None` when the log shows no such wait.
fn log_wait_secs(path: &str, now: DateTime<Utc>) -> Option<u64> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let (mut line, mut seen, mut total, mut pending) = (Vec::new(), false, 0_i64, None::<i64>);
    while reader.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
        if line.starts_with(b"rsi-phase ")
            && let Some(phase) = parse_phase(&String::from_utf8_lossy(&line))
        {
            if phase.text.starts_with("slot-wait:")
                || phase.text.starts_with("artifact-lock-wait:")
                || phase.text.starts_with("sandbox-lock-wait:")
            {
                seen = true;
                pending.get_or_insert(phase.at);
            } else if phase.text.starts_with("slot-granted:")
                || phase.text.starts_with("artifact-lock-held:")
                || phase.text.starts_with("sandbox-lock-held:")
            {
                if let Some(since) = pending.take() {
                    total += (phase.at - since).max(0);
                }
            }
        }
        line.clear();
    }
    if let Some(since) = pending {
        total += (now.timestamp() - since).max(0);
    }
    seen.then(|| u64::try_from(total).unwrap_or(0))
}

/// #192: what libtest printed over a whole job log. Cargo prints one `Running`
/// (or `Doc-tests`) line per test binary and each libtest binary ends with one
/// `test result: <ok|FAILED>. P passed; F failed; I ignored; ...` line. Only
/// `passed` and `failed` count as executed; `ignored`, `measured` and `filtered
/// out` ran nothing.
#[derive(Debug, Default, PartialEq, Eq)]
struct LibtestExecution {
    /// `Running ...` and `Doc-tests ...` lines: the binaries cargo started.
    binaries: u64,
    /// Parsed `test result:` lines.
    summaries: u64,
    /// Sum of `passed` and `failed` over the parsed summaries.
    executed: u64,
    /// A `test result:` line this parser could not read.
    unparsed: bool,
}

impl LibtestExecution {
    /// Reliable zero-execution evidence, never a guess: at least one summary,
    /// every started binary reported one (a custom harness, which prints none,
    /// leaves `summaries < binaries` and proves nothing), every summary read,
    /// and the totals are zero.
    fn proves_no_test_ran(&self) -> bool {
        !self.unparsed
            && self.summaries > 0
            && self.summaries >= self.binaries
            && self.executed == 0
    }
}

/// The longest line prefix the scan inspects; the rest of a longer line is
/// skipped unread, so a log of one huge line costs no memory.
const LIBTEST_SCAN_LINE_MAX: u64 = 4096;

/// Read one line (at most [`LIBTEST_SCAN_LINE_MAX`] bytes kept) into `line`.
/// `Ok(false)` only at a clean end of input; any read error, including one
/// while the rest of an over-long line is discarded, is returned so that a
/// partially read log is never mistaken for a whole one.
fn read_scan_line<R: std::io::BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> std::io::Result<bool> {
    use std::io::{BufRead, Read};
    line.clear();
    let first = (&mut *reader)
        .take(LIBTEST_SCAN_LINE_MAX)
        .read_until(b'\n', line)?;
    if first == 0 {
        return Ok(false);
    }
    let mut more = line.last() != Some(&b'\n');
    let mut rest = Vec::new();
    while more {
        rest.clear();
        let read = (&mut *reader)
            .take(LIBTEST_SCAN_LINE_MAX)
            .read_until(b'\n', &mut rest)?;
        more = read > 0 && rest.last() != Some(&b'\n');
    }
    Ok(true)
}

/// One `test result:` line's executed count (`passed` + `failed`), `None` when
/// either field is missing or not a number.
fn libtest_summary_executed(line: &str) -> Option<u64> {
    let counts = line.strip_prefix("test result:")?.split_once(". ")?.1;
    let field = |name: &str| {
        counts.split(';').find_map(|part| {
            part.trim()
                .strip_suffix(name)
                .and_then(|count| count.trim().parse::<u64>().ok())
        })
    };
    Some(field(" passed")?.saturating_add(field(" failed")?))
}

/// Stream a whole job log, never holding more than one bounded line. A read
/// error anywhere is an `Err`: the evidence is then incomplete and proves nothing.
fn libtest_execution(mut reader: impl std::io::BufRead) -> std::io::Result<LibtestExecution> {
    let mut seen = LibtestExecution::default();
    let mut line = Vec::new();
    while read_scan_line(&mut reader, &mut line)? {
        let text = line.trim_ascii_start();
        if text.starts_with(b"Running ") || text.starts_with(b"Doc-tests ") {
            seen.binaries += 1;
        } else if text.starts_with(b"test result:") {
            match libtest_summary_executed(String::from_utf8_lossy(text).trim_end()) {
                Some(executed) => {
                    seen.summaries += 1;
                    seen.executed = seen.executed.saturating_add(executed);
                }
                None => seen.unparsed = true,
            }
        }
    }
    Ok(seen)
}

/// #192: an ordinary package run with test-name filters: not a scoped_test,
/// recipe, shard, candidate receipt or QA lane, and not an unfiltered run.
fn is_filtered_package_test(params: &TestJobParams) -> bool {
    params.package.is_some()
        && !params.filters.is_empty()
        && params.scoped_test.is_none()
        && params.recipe.is_none()
        && params.shard.is_none()
        && params.candidate_receipt.is_none()
        && params.qa_lane.is_none()
}

/// #192: a filtered package job that exited 0 settles `failed`
/// `job_filter_matched_no_tests` when its whole log proves libtest ran no test
/// (a mistyped `exact` name matches nothing and cargo still exits 0). The
/// observed `exit_code` is kept. Any other outcome, and any log without that
/// reliable evidence (custom harness, unreadable log), keeps the settlement.
fn settle_unexecuted_filter(
    job: &AgentJobV1,
    settled: (JobState, AgentJobResultV1),
) -> (JobState, AgentJobResultV1) {
    let (state, mut result) = settled;
    let JobParams::Test(params) = &job.params else {
        return (state, result);
    };
    if state != JobState::Succeeded
        || result.exit_code != Some(0)
        || !is_filtered_package_test(params)
    {
        return (state, result);
    }
    let Ok(file) = std::fs::File::open(&job.log_path) else {
        return (state, result);
    };
    let proven = libtest_execution(std::io::BufReader::new(file))
        .is_ok_and(|seen| seen.proves_no_test_ran());
    if !proven {
        return (state, result);
    }
    let filters: String = params.filters.join(" ").chars().take(600).collect();
    let note = format!(
        "{JOB_FILTER_MATCHED_NO_TESTS}: cargo exited 0 but libtest ran no test (0 passed, 0 failed in every test binary) for filters [{filters}]; check the full module path and exact names. This is early feedback only, not acceptance."
    );
    result.refusal = Some(JOB_FILTER_MATCHED_NO_TESTS.into());
    result.detail = Some(match result.detail.take() {
        Some(tail) => format!("{note}\n{tail}"),
        None => note,
    });
    (JobState::Failed, result)
}

/// One `rsi-phase <epoch> <name>[: detail]` line from a job log (#1591).
#[derive(Debug, Clone, PartialEq, Eq)]
struct LogPhase {
    at: i64,
    text: String,
}

/// The last few phase lines of a job log, oldest first. Streams the whole log
/// (bounded by the job's log cap) because the phases may precede the tail.
fn read_log_phases(path: &str) -> Vec<LogPhase> {
    use std::io::BufRead;
    const KEEP: usize = 6;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut phases: std::collections::VecDeque<LogPhase> = std::collections::VecDeque::new();
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
        if let Some(phase) = parse_phase(&String::from_utf8_lossy(&line)) {
            if phases.len() == KEEP {
                phases.pop_front();
            }
            phases.push_back(phase);
        }
        line.clear();
    }
    phases.into()
}

fn parse_phase(line: &str) -> Option<LogPhase> {
    let rest = line.trim_end().strip_prefix("rsi-phase ")?;
    let (at, text) = rest.split_once(' ')?;
    Some(LogPhase {
        at: at.parse().ok()?,
        text: text.chars().take(300).collect(),
    })
}

/// Where the run was when it timed out: the last phases with their age, so
/// "20 minutes in a lock or slot wait" is visible without a log (#1591).
fn phase_summary(phases: &[LogPhase], now: DateTime<Utc>) -> String {
    let Some(last) = phases.last() else {
        return "no phase was logged: the unit's wrapper never wrote its job-start line, so the run stalled before it started (or the log predates phase logging)".into();
    };
    let age = |at: i64| (now.timestamp() - at).max(0);
    let waiting = last.text.contains("wait") && !last.text.contains("waited");
    let mut out = format!(
        "last phase `{}` began {}s before the timeout{}\nphases:",
        last.text,
        age(last.at),
        if waiting {
            " and the job was still waiting there"
        } else {
            ""
        }
    );
    for phase in phases {
        out.push_str(&format!("\n  -{}s {}", age(phase.at), phase.text));
    }
    out
}

/// Seconds without a `job-start` line before the daemon notes the delay in the
/// job's own log (#1832).
const START_WAIT_NOTE_SECS: i64 = 120;

/// #1832: did the unit's wrapper reach its command? `Some(false)` only when the
/// log has the daemon's `unit-launch` line (so it is a phase-logging log) and
/// no `job-start` in its head; `None` for a log that predates phase logging.
/// Reads at most the first 16 KiB: `job-start` follows the pre-launch lines.
fn log_job_started(path: &str) -> Option<bool> {
    log_job_start(path).map(|started| started.is_some())
}

/// None: legacy/unreadable log; Some(None): launch requested but not started;
/// Some(Some(epoch)): the wrapper started. Reads bounded pre-launch evidence.
fn log_job_start(path: &str) -> Option<Option<i64>> {
    use std::io::Read;
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(16 * 1024)
        .read_to_end(&mut head)
        .ok()?;
    let text = String::from_utf8_lossy(&head);
    let mut launched = false;
    for phase in text.lines().filter_map(parse_phase) {
        if phase.text == "job-start" {
            return Some(Some(phase.at));
        }
        launched |= phase.text.starts_with("unit-launch:");
    }
    launched.then_some(None)
}

/// #1832: a running job whose unit has not written `job-start` after
/// [`START_WAIT_NOTE_SECS`] gets one daemon-written `start-wait` line with the
/// unit's state, so the wait shows in its log while it happens.
fn note_start_wait(job: &AgentJobV1, runtime: &dyn JobRuntime, now: DateTime<Utc>) {
    let started = job.started_at.as_deref().unwrap_or(&job.created_at);
    let Ok(started) = DateTime::parse_from_rfc3339(started) else {
        return;
    };
    let waited = (now - started.with_timezone(&Utc)).num_seconds();
    if waited < START_WAIT_NOTE_SECS || log_job_started(&job.log_path) != Some(false) {
        return;
    }
    if std::fs::read_to_string(&job.log_path)
        .is_ok_and(|text| text.lines().any(|l| l.contains(" start-wait:")))
    {
        return;
    }
    let unit = runtime
        .unit_diagnostics(&job.unit_name)
        .unwrap_or_else(|| "unit state unavailable".into());
    append_phase(
        &job.log_path,
        now,
        &format!("start-wait: no job-start after {waited}s; {unit}"),
    );
}

/// #1832: the settlement of a job whose unit never wrote `job-start` before the
/// timeout: a start delay with the unit's state, not a test timeout.
fn start_timed_out_result(
    job: &AgentJobV1,
    now: DateTime<Utc>,
    unit: Option<&str>,
) -> AgentJobResultV1 {
    let minutes = JOB_START_WAIT_MAX_SECS / 60;
    AgentJobResultV1 {
        refusal: Some(JOB_START_TIMED_OUT.into()),
        detail: Some(format!(
            "the job's unit never started its command within the separate {minutes}-minute start admission limit (no `job-start` line in the log): a systemd start or admission delay, not a test timeout; the unit was stopped; resubmit, and check host load\nunit: {}\n{}",
            unit.unwrap_or("state unavailable"),
            phase_summary(&read_log_phases(&job.log_path), now)
        )),
        ..AgentJobResultV1::default()
    }
}

/// The typed `job_timed_out` settlement: failing tests, the last phase and the
/// log tail ride along so the owner sees how far the run got. A unit that never
/// started its command settles `job_start_timed_out` instead (#1832).
fn timed_out_result(job: &AgentJobV1, now: DateTime<Utc>, unit: Option<&str>) -> AgentJobResultV1 {
    if log_job_started(&job.log_path) == Some(false) {
        return start_timed_out_result(job, now, unit);
    }
    let log = read_log_tail(&job.log_path);
    let minutes = job_timeout_secs(&job.params).unwrap_or(0) / 60;
    let waited = log_wait_secs(&job.log_path, now);
    if scoped_job(job) {
        let wall = log_job_start(&job.log_path)
            .flatten()
            .map(|at| now.timestamp().saturating_sub(at).max(0) as u64)
            .unwrap_or_else(|| job_wall_secs(job, now).unwrap_or(0).max(0) as u64);
        let admission = scoped_admission_expired(job, wall, minutes * 60);
        return AgentJobResultV1 {
            refusal: Some(
                if admission {
                    JOB_ADMISSION_TIMED_OUT
                } else {
                    JOB_TIMED_OUT
                }
                .into(),
            ),
            detail: Some(format!(
                "scoped-test exhausted its {minutes}-minute wall budget{}; waits count against it; the unit was stopped\n{}",
                if admission {
                    " minus the 120s runtime reserve while waiting for admission"
                } else {
                    ""
                },
                phase_summary(&read_log_phases(&job.log_path), now)
            )),
            slot_wait_secs: waited,
            ..AgentJobResultV1::default()
        };
    }
    let allowance = admission_allowance_secs(&job.params);
    if waited.is_some_and(|secs| secs > allowance) {
        return AgentJobResultV1 {
            refusal: Some(JOB_ADMISSION_TIMED_OUT.into()),
            detail: Some(format!(
                "the job exhausted its separate {}-minute build admission limit; execution excludes this wait; the unit was stopped\n{}",
                allowance / 60,
                phase_summary(&read_log_phases(&job.log_path), now)
            )),
            slot_wait_secs: waited,
            ..AgentJobResultV1::default()
        };
    }
    let note = format!(
        "the test job ran past its {minutes}-minute timeout{} and its unit was stopped; narrow the change or its focused test filters (the scoped_test receipt names the plan), or ask your manager to raise timeout_minutes\n{}",
        waited.map_or(String::new(), |secs| format!(
            " (not counting {secs}s waiting for a build slot or sandbox/artifact lock)"
        )),
        phase_summary(&read_log_phases(&job.log_path), now)
    );
    let failing = failing_test_report(&log);
    AgentJobResultV1 {
        refusal: Some(JOB_TIMED_OUT.into()),
        failing_tests: failing.names,
        failing_tests_omitted: failing.omitted,
        detail: Some(if log.is_empty() {
            note
        } else {
            format!("{note}\n{}", tail(&log))
        }),
        slot_wait_secs: waited,
        ..AgentJobResultV1::default()
    }
}

/// #1337: a running job past its timeout: stop its unit and return the
/// failed settlement only after confirming it is inactive. `None` when within
/// its timeout or stopping is uncertain (the next poll retries).
fn timeout_breach(
    job: &AgentJobV1,
    runtime: &dyn JobRuntime,
    now: DateTime<Utc>,
) -> Option<(JobState, AgentJobResultV1)> {
    if !timed_out(job, now) {
        return None;
    }
    let unit = (log_job_started(&job.log_path) == Some(false))
        .then(|| runtime.unit_diagnostics(&job.unit_name))
        .flatten();
    log_unit_stop(&job.unit_name, "test job past its wall-clock timeout");
    if let Err(error) = runtime.stop_unit(&job.unit_name) {
        tracing::warn!(unit = %job.unit_name, %error, "cannot stop timed-out agent job unit");
        return None;
    }
    if runtime.unit_active(&job.unit_name) {
        tracing::warn!(unit = %job.unit_name, "timed-out agent job unit is still active after stop");
        return None;
    }
    Some((
        JobState::Failed,
        timed_out_result(job, now, unit.as_deref()),
    ))
}

/// Hub #2954: a running `build` job whose worktree HEAD moved, or whose clean
/// tree became dirty, since submit is stopped and settled `job_source_moved`
/// (the poll runs every few seconds, so this covers the slot wait, the slot
/// grant and the compile). `None` when the worktree is as recorded or the unit
/// could not be stopped (the next poll retries).
fn source_breach(
    job: &AgentJobV1,
    runtime: &dyn JobRuntime,
) -> Option<(JobState, AgentJobResultV1)> {
    let result = source::check(job, "while the job ran").err()?;
    log_unit_stop(&job.unit_name, "build job worktree moved since submit");
    if let Err(error) = runtime.stop_unit(&job.unit_name) {
        tracing::warn!(unit = %job.unit_name, %error, "cannot stop moved-source agent job unit");
        return None;
    }
    if runtime.unit_active(&job.unit_name) {
        tracing::warn!(unit = %job.unit_name, "moved-source agent job unit is still active after stop");
        return None;
    }
    Some((JobState::Failed, result))
}

/// #1337: the andon event for a test job stopped at its timeout.
fn timed_out_friction(job: &AgentJobV1) -> rsi_common::friction::NewFrictionEventV1 {
    rsi_common::friction::NewFrictionEventV1::new(
        rsi_common::friction::FrictionKind::RunawayProcess,
        &[&format!("job_{}", job.kind.as_str()), "timeout"],
    )
    .session(Some(job.owner_session_id))
    .evidence("job", job.id)
}

/// One poll over every `running` job: settle each whose unit finished (or
/// vanished). Returns the number of jobs settled. Each settle CASes in the
/// store, so overlapping polls or a restart reconcile never wake twice.
pub(crate) async fn poll_once(
    store: &Arc<tokio::sync::Mutex<Store>>,
    runtime: &Arc<dyn JobRuntime>,
    now: DateTime<Utc>,
) -> Result<usize> {
    poll_with_scratch_quota(store, runtime, now, JOB_SCRATCH_QUOTA_BYTES).await
}

/// [`poll_once`] with an explicit per-job scratch quota.
async fn poll_with_scratch_quota(
    store: &Arc<tokio::sync::Mutex<Store>>,
    runtime: &Arc<dyn JobRuntime>,
    now: DateTime<Utc>,
    scratch_quota: u64,
) -> Result<usize> {
    let running = {
        let store = Arc::clone(store);
        tokio::task::spawn_blocking(move || {
            crate::store_lock::blocking_lock(&store, "agent_jobs::list_running")
                .list_running_agent_jobs()
        })
        .await
        .map_err(|error| DaemonError::Process(format!("agent job list join: {error}")))??
    };
    let mut settled = 0;
    for row in running {
        let runtime = Arc::clone(runtime);
        let store = Arc::clone(store);
        let done = tokio::task::spawn_blocking(move || -> Result<bool> {
            let job = &row.job;
            // Check liveness before reading the status file: the wrapper writes
            // the status before the unit goes inactive, so an inactive unit
            // with no status really ended without one.
            let active = runtime.unit_active(&job.unit_name);
            let code = read_status(&row.status_path);
            let (mut state, result) = match code {
                // A status file is the wrapper's final write, but the service
                // may still be tearing down its children. Keep scratch until
                // the backend confirms that group has stopped.
                Some(_) if active => return Ok(false),
                Some(code) => {
                    let log = read_log_tail(&job.log_path);
                    settle_unexecuted_filter(job, classify(job, Some(code), &log))
                }
                None if active => {
                    note_start_wait(job, &*runtime, now);
                    match source_breach(job, &*runtime)
                        .or_else(|| timeout_breach(job, &*runtime, now))
                        .or_else(|| scratch_quota_breach(job, &*runtime, scratch_quota))
                    {
                        Some(settlement) => settlement,
                        None => return Ok(false),
                    }
                }
                // #1337: the unit cap (timeout plus grace) stopped it while the
                // daemon could not: still a timeout, not a lost job.
                None if timed_out(job, now) => (JobState::Failed, timed_out_result(job, now, None)),
                None => {
                    let created = DateTime::parse_from_rfc3339(
                        job.started_at.as_deref().unwrap_or(&job.created_at),
                    )
                    .map(|t| t.with_timezone(&Utc))
                    .unwrap_or(now);
                    if (now - created).num_seconds() < LAUNCH_GRACE_SECS {
                        return Ok(false);
                    }
                    if matches!(job.kind, JobKind::Test | JobKind::Build)
                        && log_job_started(&job.log_path) == Some(false)
                    {
                        let unit = runtime
                            .unit_diagnostics(&job.unit_name)
                            .unwrap_or_else(|| "unit state unavailable".into());
                        let result = AgentJobResultV1 {
                            refusal: Some(JOB_LAUNCH_FAILED.into()),
                            detail: Some(format!(
                                "the unit ended before its wrapper started; {unit}\n{}",
                                phase_summary(&read_log_phases(&job.log_path), now)
                            )),
                            ..AgentJobResultV1::default()
                        };
                        let settled =
                            crate::store_lock::blocking_lock(&store, "agent_jobs::settle")
                                .settle_agent_job_outcome(
                                    job.id,
                                    JobState::Failed,
                                    &result,
                                    job.wake == JobWake::Owner,
                                    Utc::now(),
                                )?;
                        if settled.is_some() {
                            cleanup_job_tmp(Path::new(&job.log_path));
                        }
                        return Ok(settled.is_some());
                    }
                    let log = read_log_tail(&job.log_path);
                    if matches!(&job.params, JobParams::Test(params) if params.is_scoped_test())
                        && let Some(diagnostics) = runtime.unit_diagnostics(&job.unit_name)
                        && let Some((result, status)) = external_stop_status(&diagnostics)
                    {
                        let result =
                            stopped_scoped_test_result(&log, &job.unit_name, &result, status);
                        let settled =
                            crate::store_lock::blocking_lock(&store, "agent_jobs::settle")
                                .settle_agent_job_outcome(
                                    job.id,
                                    JobState::Failed,
                                    &result,
                                    job.wake == JobWake::Owner,
                                    Utc::now(),
                                )?;
                        if settled.is_some() {
                            cleanup_job_tmp(Path::new(&job.log_path));
                        }
                        return Ok(settled.is_some());
                    }
                    let is_sweep = matches!(job.params, JobParams::CloudSweep(_));
                    let failing = if is_sweep {
                        FailingTests::default()
                    } else {
                        failing_test_report(&log)
                    };
                    let result = AgentJobResultV1 {
                        refusal: Some("unit_ended_without_status".into()),
                        failing_tests: failing.names,
                        failing_tests_omitted: failing.omitted,
                        detail: if is_sweep {
                            Some(sweep_detail(
                                0,
                                (0, 0),
                                0,
                                None,
                                job_wall_secs(job, now),
                                Some(filtered_log_tail(&log)),
                            ))
                        } else {
                            // #2121: say who did not stop it, so the owner
                            // knows this is not its test failing.
                            Some(format!("{UNIT_ENDED_OUTSIDE_RSID}\n{}", tail(&log)))
                        },
                        sweep: match &job.params {
                            JobParams::CloudSweep(p) => Some(CloudSweepResultV1 {
                                verdict: SweepVerdict::Incomplete,
                                sha: p.sha.clone(),
                                results_dir: sweep_results_dir(&p.sha).display().to_string(),
                                new_failures: Vec::new(),
                                known_failures: Vec::new(),
                            }),
                            _ => None,
                        },
                        ..AgentJobResultV1::default()
                    };
                    (JobState::Lost, result)
                }
            };
            let mut result = result;
            source::stamp_finish(job, &mut state, &mut result);
            if matches!(
                job.kind,
                rsi_common::agent_jobs::JobKind::Test | rsi_common::agent_jobs::JobKind::Build
            ) && result.slot_wait_secs.is_none()
            {
                result.slot_wait_secs = log_wait_secs(&job.log_path, now);
            }
            // #1006: a `wake: none` job settles without its per-job owner wake.
            let settled = {
                let store = crate::store_lock::blocking_lock(&store, "agent_jobs::settle");
                let settled = store.settle_agent_job_outcome(
                    job.id,
                    state,
                    &result,
                    job.wake == JobWake::Owner,
                    Utc::now(),
                )?;
                if settled.is_some() && result.refusal.as_deref() == Some(JOB_TIMED_OUT) {
                    crate::friction::note_locked(&store, &timed_out_friction(job));
                }
                settled
            };
            if matches!(
                job.kind,
                rsi_common::agent_jobs::JobKind::Test | rsi_common::agent_jobs::JobKind::Build
            ) {
                cleanup_job_tmp(Path::new(&job.log_path));
            }
            Ok(settled.is_some())
        })
        .await
        .map_err(|error| DaemonError::Process(format!("agent job poll join: {error}")))??;
        settled += usize::from(done);
    }
    Ok(settled)
}

/// Remove the scratch of every job that is no longer `running` but whose
/// scratch directory is still on disk: settlement commits before the scratch
/// is deleted, so a crash between the two would otherwise leak that job's
/// scratch forever. Scratch of a `running` job, or of a directory with no row,
/// is never touched. Returns the number of scratch directories removed.
/// `AgentCancelJob` (#1106): stop a running job's unit and settle it `failed`
/// with the typed refusal `job_cancelled`, silently (the owner made this call,
/// so no per-job wake). A job that already settled is returned unchanged with
/// `cancelled: false`. The settle is guarded on `state='running'`, so a poll
/// that settles the job first wins and the cancel reports what is stored.
#[cfg(test)]
pub(crate) fn cancel_job(
    store: &Store,
    runtime: &dyn JobRuntime,
    job_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(AgentJobV1, bool)> {
    cancel_job_as(store, runtime, job_id, now, None)
}

/// #2232: whether a manager that does not own `job` may stop it (#2213).
/// Only an ordinary worker-submitted `test` or `build` job qualifies. A job the
/// daemon owns is never stopped by a manager, whatever session the row is
/// attributed to, because stopping it breaks an obligation the manager does
/// not hold: a queue lander (`landing`), the cloud gate and sweep, a release
/// build (it feeds a deploy), and a full-library QA run: the recipe's own jobs
/// (the hub's and a satellite's guest run) and any job a bounded-QA obligation
/// is bound to (the real `bounded_qa_requested` binding, which a caller
/// cannot forge through a name or parameters). The owner's own `AgentCancelJob`
/// is unchanged.
///
/// # Errors
/// A persistence error reading the binding.
pub(crate) fn manager_may_cancel(store: &Store, job: &AgentJobV1) -> Result<bool> {
    let ordinary = match &job.params {
        JobParams::Test(_) => !is_library_qa(&job.params),
        JobParams::Build(build) => !build.release,
        JobParams::Landing(_) | JobParams::CloudGate(_) | JobParams::CloudSweep(_) => false,
    };
    Ok(ordinary && !store.bounded_qa_job_bound(job.id)?)
}

/// `cancel_job` with the canceller named (#2213): `Some(manager)` when the
/// owner's current manager stopped the job (the caller already authorized the
/// scope); the settled job's detail records the manager and the owner.
pub(crate) fn cancel_job_as(
    store: &Store,
    runtime: &dyn JobRuntime,
    job_id: Uuid,
    now: DateTime<Utc>,
    manager: Option<Uuid>,
) -> Result<(AgentJobV1, bool)> {
    let not_found = || DaemonError::InvalidParam(JOB_NOT_FOUND.into());
    let row = store.get_agent_job(job_id)?.ok_or_else(not_found)?;
    if row.job.state == JobState::Queued {
        // #1566: held behind a deploy drain, never launched: nothing to stop.
        let result = AgentJobResultV1 {
            refusal: Some(JOB_CANCELLED.into()),
            detail: Some(match manager {
                Some(manager) => format!(
                    "cancelled by manager {manager} (job owner {}) before the held job started",
                    row.job.owner_session_id
                ),
                None => "cancelled by the owner before the held job started".into(),
            }),
            ..AgentJobResultV1::default()
        };
        let settled =
            store.settle_agent_job_outcome(job_id, JobState::Failed, &result, false, now)?;
        let current = store.get_agent_job(job_id)?.ok_or_else(not_found)?;
        return Ok((current.job, settled.is_some()));
    }
    if row.job.state != JobState::Running {
        return Ok((row.job, false));
    }
    let reason = match manager {
        Some(manager) => StopReason::ManagerCancel(manager),
        None => StopReason::OwnerCancel,
    };
    stop_running_job(store, runtime, &row, now, &reason)
}

/// #3038: stop every ordinary `test`/`build` job `owner` still has queued or
/// running (the same jobs a manager may stop, `manager_may_cancel`; a
/// daemon-owned lander or QA run is left alone), silently like the owner's own
/// `AgentCancelJob`. A cancelling topology execution calls this for a node
/// session parked on a job, so the job does not outlive the node. Returns the
/// ids this call cancelled.
///
/// # Errors
/// A persistence error, or `job_cancel_unit_not_stopped` when a unit would not
/// stop (the job stays running so a retry stops it).
pub(crate) fn cancel_open_jobs_of_owner(
    store: &Store,
    runtime: &dyn JobRuntime,
    owner: Uuid,
    now: DateTime<Utc>,
) -> Result<Vec<Uuid>> {
    let mut cancelled = Vec::new();
    for row in store.list_open_agent_jobs_for_owner(owner)? {
        if !manager_may_cancel(store, &row.job)? {
            continue;
        }
        let (job, was_cancelled) = cancel_job_as(store, runtime, row.job.id, now, None)?;
        if was_cancelled {
            cancelled.push(job.id);
        }
    }
    Ok(cancelled)
}

/// Why a running job's unit is being stopped and how it settles.
enum StopReason {
    /// `AgentCancelJob`: `job_cancelled`, silently.
    OwnerCancel,
    /// #2213: the owner's current manager cancelled it; the detail names the
    /// manager and the owner. Silent like `OwnerCancel`.
    ManagerCancel(Uuid),
    /// #2048: a staged `interrupt_workers` deploy past its grace:
    /// `interrupted_by_deploy` with the deploy id, and the owner's normal
    /// job-terminal wake.
    Deploy(Uuid),
    /// #2278: a staged `interrupt_workers` deploy past its grace stops an
    /// ordinary test job and puts it back in the queue under the same id, owner,
    /// params and wake policy. It is held until the deploy settles and reruns on
    /// the new daemon; the owner's wake fires once, when the rerun ends.
    DeployRequeue(Uuid),
}

fn stop_running_job(
    store: &Store,
    runtime: &dyn JobRuntime,
    row: &AgentJobRow,
    now: DateTime<Utc>,
    reason: &StopReason,
) -> Result<(AgentJobV1, bool)> {
    let job_id = row.job.id;
    let not_found = || DaemonError::InvalidParam(JOB_NOT_FOUND.into());
    // Settle only once the unit is confirmed stopped; a failed stop leaves the
    // job running with its scratch so a repeat cancel retries.
    let unstopped = |detail: String| {
        DaemonError::Process(format!(
            "job_cancel_unit_not_stopped: unit {} was not stopped, the job is still running; retry the cancel: {detail}",
            row.job.unit_name
        ))
    };
    log_unit_stop(
        &row.job.unit_name,
        match reason {
            StopReason::OwnerCancel | StopReason::ManagerCancel(_) => "job cancelled",
            StopReason::Deploy(_) | StopReason::DeployRequeue(_) => "job interrupted by deploy",
        },
    );
    runtime.stop_unit(&row.job.unit_name).map_err(unstopped)?;
    if runtime.unit_active(&row.job.unit_name) {
        return Err(unstopped("the unit is still active".into()));
    }
    if let StopReason::DeployRequeue(deploy) = reason {
        if !store.requeue_agent_job_for_deploy(job_id, *deploy)? {
            // Settled while it was being stopped: nothing to put back.
            let current = store.get_agent_job(job_id)?.ok_or_else(not_found)?;
            return Ok((current.job, false));
        }
        // The stopped run left no outcome: drop its stale status and log so the
        // rerun's classification reads only its own output.
        let _ = std::fs::remove_file(&row.status_path);
        let _ = std::fs::write(&row.job.log_path, "");
        append_phase(
            &row.job.log_path,
            now,
            &format!(
                "queue-wait: stopped by deploy {deploy} after its grace; requeued, the unit reruns after the restart"
            ),
        );
        cleanup_job_tmp(Path::new(&row.job.log_path));
        let current = store.get_agent_job(job_id)?.ok_or_else(not_found)?;
        return Ok((current.job, true));
    }
    let log = read_log_tail(&row.job.log_path);
    let (refusal, note, deploy_id, wake) = match reason {
        StopReason::OwnerCancel => (
            JOB_CANCELLED,
            "cancelled by the owner; the unit was stopped".to_string(),
            None,
            false,
        ),
        StopReason::ManagerCancel(manager) => (
            JOB_CANCELLED,
            format!(
                "cancelled by manager {manager} (job owner {}); the unit was stopped",
                row.job.owner_session_id
            ),
            None,
            false,
        ),
        StopReason::Deploy(deploy) | StopReason::DeployRequeue(deploy) => (
            JOB_INTERRUPTED_BY_DEPLOY,
            "stopped by a staged deploy after its grace (interrupt_workers); resubmit once on the new daemon"
                .to_string(),
            Some(*deploy),
            row.job.wake == JobWake::Owner,
        ),
    };
    let failing = failing_test_report(&log);
    let result = AgentJobResultV1 {
        refusal: Some(refusal.into()),
        failing_tests: failing.names,
        failing_tests_omitted: failing.omitted,
        detail: Some(if log.is_empty() {
            note
        } else {
            format!("{note}\n{}", tail(&log))
        }),
        deploy_id,
        ..AgentJobResultV1::default()
    };
    let settled = store.settle_agent_job_outcome(job_id, JobState::Failed, &result, wake, now)?;
    if matches!(row.job.kind, JobKind::Test | JobKind::Build) {
        cleanup_job_tmp(Path::new(&row.job.log_path));
    }
    let current = store.get_agent_job(job_id)?.ok_or_else(not_found)?;
    Ok((current.job, settled.is_some()))
}

/// #2048: once a staged `interrupt_workers` deploy has waited
/// `DEPLOY_JOB_GRACE_SECS` since its request, stop every running `test` job
/// (`scoped_test` included) with `interrupted_by_deploy`, so the deploy stops
/// idling behind re-runnable verification and each owner's job-terminal wake
/// fires. `build`, `landing` and cloud jobs (and the queue's lander) are never
/// touched and keep blocking the quiet point. Returns the stopped job ids.
///
/// #2190: stopping test jobs gains nothing while the deploy is still blocked
/// by something it cannot preempt (an admitted or gating queue landing, a
/// running build), so none is stopped meanwhile and the grace runs from the
/// latest tick that saw such a blocker (from the request when none was seen).
/// New `test`/`build` jobs are held for the deploy meanwhile, so a job is only
/// ever stopped if it was already running when the deploy was staged.
///
/// #2176: a hub's full-library QA run on this satellite is a guest. Any staged
/// deploy stops it at once (no grace, no `interrupt_workers` needed, and even
/// while a landing blocks the deploy: QA is preemptible, only ordinary test
/// jobs are spared by #2190), and the hub defers that attempt as
/// `deploy_preempted` instead of the deploy timing out behind it.
pub(crate) fn cancel_test_jobs_for_deploy(
    store: &Store,
    runtime: &dyn JobRuntime,
    drain: &crate::deploy_drain::DeployDrain,
    now: DateTime<Utc>,
) -> Result<Vec<Uuid>> {
    let Some(deploy) = store.live_agent_deploy()? else {
        return Ok(Vec::new());
    };
    if deploy.state != rsi_common::agent_deploy::DeployState::Staged {
        return Ok(Vec::new());
    }
    let grace = chrono::Duration::seconds(
        i64::try_from(rsi_common::agent_deploy::DEPLOY_JOB_GRACE_SECS).unwrap_or(i64::MAX),
    );
    let grace_over = if deploy.owner_session_id.is_some()
        && store.agent_deploy_interrupt(deploy.id)?.requested
    {
        if store.deploy_unpreemptible_blockers()?.is_empty() {
            store
                .agent_deploy_created_at(deploy.id)?
                .is_some_and(|created| {
                    let grace_from = drain
                        .unpreemptible_blocked_at(deploy.id)
                        .map_or(created, |seen| seen.max(created));
                    now >= grace_from + grace
                })
        } else {
            drain.note_unpreemptible_blocked(deploy.id, now);
            false
        }
    } else {
        false
    };
    let mut stopped = Vec::new();
    for row in store.list_running_agent_jobs()? {
        if row.job.kind != JobKind::Test
            || !(grace_over || crate::rolling_queue::satellite_qa::is_hub_run(&row.job))
        {
            continue;
        }
        // #2278: only the hub's QA guest run settles failed (the hub defers it);
        // an ordinary test job is requeued so its owner's wake fires once, on the
        // rerun.
        let reason = if crate::rolling_queue::satellite_qa::is_hub_run(&row.job) {
            StopReason::Deploy(deploy.id)
        } else {
            StopReason::DeployRequeue(deploy.id)
        };
        match stop_running_job(store, runtime, &row, now, &reason) {
            Ok((_, true)) => stopped.push(row.job.id),
            Ok((_, false)) => {}
            Err(error) => {
                tracing::warn!(%error, job = %row.job.id, deploy = %deploy.id, "deploy job stop deferred");
            }
        }
    }
    Ok(stopped)
}

pub(crate) async fn sweep_terminal_scratch(
    store: &Arc<tokio::sync::Mutex<Store>>,
    jobs_dir: &Path,
) -> Result<usize> {
    let jobs_dir = jobs_dir.to_path_buf();
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let entries = match std::fs::read_dir(&jobs_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(DaemonError::Process(format!(
                    "agent job scratch scan: {error}"
                )));
            }
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("tmp") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| Uuid::parse_str(stem).ok())
            else {
                continue;
            };
            if !path.is_dir() {
                continue;
            }
            let row = store.blocking_lock().get_agent_job(id)?;
            let Some(row) = row else { continue };
            if !row.job.state.is_terminal() {
                continue;
            }
            cleanup_job_tmp(Path::new(&row.job.log_path));
            removed += 1;
        }
        Ok(removed)
    })
    .await
    .map_err(|error| DaemonError::Process(format!("agent job scratch sweep join: {error}")))?
}

/// The daemon task: reconcile once (the same poll settles jobs that finished
/// while rsid was down), then keep polling.
pub async fn run_agent_jobs_loop(
    store: Arc<tokio::sync::Mutex<Store>>,
    runtime: Arc<dyn JobRuntime>,
    drain: Arc<crate::deploy_drain::DeployDrain>,
) {
    let tools = JobTools::discover();
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut first = true;
    loop {
        interval.tick().await;
        match poll_once(&store, &runtime, Utc::now()).await {
            Ok(settled) if settled > 0 => {
                tracing::info!(settled, reconcile = first, "agent jobs settled");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "agent job poll deferred"),
        }
        {
            let guard = store.lock().await;
            match cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, Utc::now()) {
                Ok(stopped) if !stopped.is_empty() => {
                    tracing::info!(
                        ?stopped,
                        "test jobs stopped by a staged deploy after its grace"
                    );
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "deploy job grace check deferred"),
            }
        }
        match start_queued_jobs(&store, &runtime, &tools, &drain).await {
            Ok(started) if started > 0 => {
                tracing::info!(started, "queued agent jobs started after their hold ended");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "held agent job start deferred"),
        }
        match jobs_dir() {
            Ok(dir) => {
                match sweep_terminal_scratch(&store, &dir).await {
                    Ok(removed) if removed > 0 => {
                        tracing::info!(removed, "removed scratch left by settled agent jobs");
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "agent job scratch sweep deferred"),
                }
                // #2176: a settled hub QA run returns its build target.
                let target_jobs = dir.clone();
                match crate::rolling_queue::satellite_qa::reclaim_settled_runs(
                    &store,
                    &dir,
                    move || library_qa_target(&target_jobs),
                )
                .await
                {
                    Ok(Some(reclaimed)) => {
                        tracing::info!(
                            ?reclaimed,
                            "a settled hub QA run returned its build target"
                        );
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(%error, "hub QA target reclaim deferred"),
                }
                // #2509: a settled QA run leaves the target holding only what
                // that run touched; artifacts it superseded are removed.
                let trim_jobs = dir.clone();
                match crate::qa_target_trim::trim_settled_target(
                    &store,
                    &dir,
                    &crate::qa_target_trim::TRIM_PENDING,
                    move || library_qa_target(&trim_jobs),
                )
                .await
                {
                    Ok(Some(crate::qa_target_trim::TrimOutcome::Trimmed { units, bytes })) => {
                        tracing::info!(units, bytes, "QA target trimmed to the newest run");
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "QA target trim deferred"),
                }
            }
            Err(error) => tracing::warn!(%error, "agent job scratch sweep skipped"),
        }
        first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::agent_jobs::{AgentSubmitJobRequestV1, JOB_HELD_FOR_DEPLOY, JobKind};
    use std::sync::Mutex;

    fn tools() -> JobTools {
        JobTools {
            cargo_slot: PathBuf::from("/x/cargo-slot"),
            lander: PathBuf::from("/x/rsi-rolling-land"),
        }
    }

    fn params(kind: JobKind, value: serde_json::Value) -> JobParams {
        AgentSubmitJobRequestV1 {
            project_id: None,
            sandbox_session_id: None,
            kind,
            params: value,
            name: None,
            idempotency_key: None,
            worktree: None,
            wake: None,
        }
        .typed_params()
        .expect("valid params")
    }

    /// Records launches; the test decides what "the unit" did by writing the
    /// status file itself.
    #[derive(Default)]
    struct FakeRuntime {
        launched: Mutex<Vec<LaunchSpec>>,
        active: Mutex<bool>,
        fail_launch: Mutex<bool>,
        stopped: Mutex<Vec<String>>,
        diagnostics: Mutex<Option<String>>,
    }

    impl JobRuntime for FakeRuntime {
        fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
            if *self.fail_launch.lock().unwrap() {
                return Err("no systemd".into());
            }
            self.launched.lock().unwrap().push(spec.clone());
            *self.active.lock().unwrap() = true;
            Ok(())
        }

        fn unit_active(&self, _unit: &str) -> bool {
            *self.active.lock().unwrap()
        }

        fn stop_unit(&self, unit: &str) -> std::result::Result<(), String> {
            self.stopped.lock().unwrap().push(unit.to_string());
            *self.active.lock().unwrap() = false;
            Ok(())
        }

        fn unit_diagnostics(&self, _unit: &str) -> Option<String> {
            self.diagnostics.lock().unwrap().clone()
        }
    }

    fn ctx(owner: Uuid, cwd: &Path, key: Option<&str>) -> SubmitContext {
        SubmitContext {
            owner,
            project_id: None,
            cwd: cwd.to_path_buf(),
            name: Some("check".into()),
            params: params(
                JobKind::Build,
                serde_json::json!({"command":"check","workspace":true}),
            ),
            idempotency_key: key.map(str::to_string),
            wake: JobWake::Owner,
        }
    }

    fn open() -> (Arc<tokio::sync::Mutex<Store>>, tempfile::TempDir) {
        let store = Store::open_in_memory().expect("store");
        (Arc::new(tokio::sync::Mutex::new(store)), disk_fixture())
    }

    fn disk_fixture() -> tempfile::TempDir {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(rsi_common::manifest_dir!()).join("../../target"));
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("agent-jobs-")
            .tempdir_in(base.canonicalize().unwrap())
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn unsupported_job_platforms_refuse_before_persisting_or_launching() {
        let (store, dir) = open();
        let request = ctx(Uuid::new_v4(), dir.path(), None);
        let error = submit(
            &*store.blocking_lock(),
            &UnsupportedJobRuntime,
            &tools(),
            dir.path(),
            request,
            Utc::now(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(rsi_common::agent_jobs::JOB_PLATFORM_UNSUPPORTED)
        );
        assert!(
            store
                .blocking_lock()
                .list_running_agent_jobs()
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn launchd_accepts_package_build_tests_and_names_linux_workflow_refusals() {
        let runtime = launchd::LaunchdJobRuntime::default();
        for p in [
            params(
                JobKind::Build,
                serde_json::json!({"command":"check","workspace":true}),
            ),
            params(
                JobKind::Test,
                serde_json::json!({"package":"rsid","lib_only":true}),
            ),
        ] {
            assert_eq!(runtime.validate(&p), Ok(()));
        }
        for p in [
            params(JobKind::Test, serde_json::json!({"shard":"other-01"})),
            params(
                JobKind::Test,
                serde_json::json!({"candidate_receipt":"rsi/example"}),
            ),
            params(
                JobKind::Landing,
                serde_json::json!({"accepted":"a".repeat(40)}),
            ),
        ] {
            assert_eq!(
                runtime.validate(&p),
                Err(rsi_common::agent_jobs::JOB_PLATFORM_UNSUPPORTED)
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_uncertain_launch_keeps_its_running_row_and_scratch_for_recovery() {
        struct Uncertain;
        impl JobRuntime for Uncertain {
            fn launch(&self, _: &LaunchSpec) -> std::result::Result<(), String> {
                Err("controller unavailable after dispatch".into())
            }
            fn unit_active(&self, _: &str) -> bool {
                true
            }
            fn stop_unit(&self, _: &str) -> std::result::Result<(), String> {
                Err("controller unavailable".into())
            }
        }
        let (store, dir) = open();
        let store = store.blocking_lock();
        let owner = Uuid::new_v4();
        let error = submit(
            &store,
            &Uncertain,
            &tools(),
            dir.path(),
            ctx(owner, dir.path(), None),
            Utc::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("remains running"));
        let jobs = store.list_running_agent_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(job_tmp_dir(Path::new(&jobs[0].job.log_path)).is_dir());
        assert!(wakes_for(&store, owner).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_completed_wrapper_keeps_scratch_until_its_service_stops() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            ctx(owner, dir.path(), None),
            Utc::now(),
        )
        .unwrap();
        std::fs::write(&row.status_path, "0\n").unwrap();
        let scratch = job_tmp_dir(Path::new(&row.job.log_path));
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);
        assert!(scratch.is_dir());
        assert!(wakes_for(&*store.lock().await, owner).is_empty());
        *runtime.active.lock().unwrap() = false;
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 1);
        assert_eq!(wakes_for(&*store.lock().await, owner).len(), 1);
    }

    #[cfg(target_os = "macos")]
    async fn wait_for_launchd_completion(runtime: &dyn JobRuntime, row: &AgentJobRow) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while runtime.unit_active(&row.job.unit_name) || read_status(&row.status_path).is_none() {
            if std::time::Instant::now() > deadline {
                let _ = runtime.stop_unit(&row.job.unit_name);
                panic!(
                    "launchd completion timed out: {}",
                    read_log_tail(&row.job.log_path)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[cfg(target_os = "macos")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn macos_build_job_persists_status_logs_and_one_wake_across_restart() {
        let dir = disk_fixture();
        let cwd = dir.path().join("fixture");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::write(
            cwd.join("Cargo.toml"),
            "[package]\nname = 'rsi-job-smoke'\nversion = '0.1.0'\nedition = '2024'\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(cwd.join("src/lib.rs"), "pub fn smoke() -> u8 { 42 }\n").unwrap();
        let db = dir.path().join("jobs.db");
        let owner = Uuid::new_v4();
        let store = Store::open_test_fixture(&db).unwrap();
        let runtime = platform_job_runtime();
        let tools = JobTools {
            cargo_slot: PathBuf::from("/usr/bin/env"),
            ..tools()
        };
        let (row, _) = submit(
            &store,
            &*runtime,
            &tools,
            dir.path(),
            ctx(owner, &cwd, Some("mac-build")),
            Utc::now(),
        )
        .unwrap();
        // Destroy every in-memory object before completion: the reopened store
        // and a fresh runtime recover solely from durable row/service/status.
        drop(store);
        drop(runtime);
        let runtime = platform_job_runtime();
        wait_for_launchd_completion(&*runtime, &row).await;
        let store = Arc::new(tokio::sync::Mutex::new(Store::open(&db).unwrap()));
        assert_eq!(poll_once(&store, &runtime, Utc::now()).await.unwrap(), 1);
        assert_eq!(poll_once(&store, &runtime, Utc::now()).await.unwrap(), 0);
        let guard = store.lock().await;
        let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(job.state, JobState::Succeeded);
        assert_eq!(job.exit_code, Some(0));
        assert!(read_log_tail(&job.log_path).contains("rsi-job-smoke"));
        let wakes = wakes_for(&guard, owner);
        assert_eq!(wakes.len(), 1);
        assert!(wakes[0].message.contains(&job.id.to_string()));
    }

    #[cfg(target_os = "macos")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn macos_cancel_stops_only_its_service_and_timeout_stops_its_group() {
        let dir = disk_fixture();
        let runtime = platform_job_runtime();
        let make_spec = |timeout| LaunchSpec {
            unit_name: runtime.unit_name(Uuid::new_v4()),
            cwd: dir.path().to_path_buf(),
            log_path: dir.path().join(format!("{}.log", Uuid::new_v4())),
            status_path: dir.path().join(format!("{}.status", Uuid::new_v4())),
            command: JobCommand {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo running; while :; do echo tick >> \"$1\"; sleep 0.1; done".into(),
                    "rsi-job".into(),
                    dir.path()
                        .join(format!("{}.heartbeat", Uuid::new_v4()))
                        .display()
                        .to_string(),
                ],
                runtime_max_secs: timeout,
                stop_timeout_secs: 1,
                log_max_bytes: 4096,
                memory_max_gib: 1,
                cpu_quota_percent: 100,
                background: false,
                admission_max_secs: None,
            },
            build_environment: None,
        };
        let cancelled = make_spec(30);
        let other = make_spec(5);
        runtime.launch(&cancelled).unwrap();
        if let Err(error) = runtime.launch(&other) {
            runtime.stop_unit(&cancelled.unit_name).unwrap();
            panic!("second launch failed: {error}");
        }
        let started = std::time::Instant::now() + Duration::from_secs(3);
        while [&cancelled, &other]
            .iter()
            .any(|spec| !Path::new(spec.command.argv.last().unwrap()).is_file())
        {
            if std::time::Instant::now() > started {
                runtime.stop_unit(&cancelled.unit_name).unwrap();
                runtime.stop_unit(&other.unit_name).unwrap();
                panic!("job descendants did not start");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        runtime.stop_unit(&cancelled.unit_name).unwrap();
        assert!(!runtime.unit_active(&cancelled.unit_name));
        assert!(
            runtime.unit_active(&other.unit_name),
            "cancellation leaves the other job running"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while runtime.unit_active(&other.unit_name) {
            if std::time::Instant::now() > deadline {
                runtime.stop_unit(&other.unit_name).unwrap();
                panic!("job watchdog did not stop its service");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(read_status(other.status_path.to_str().unwrap()), None);
        for spec in [&cancelled, &other] {
            let path = Path::new(spec.command.argv.last().unwrap());
            let stopped = std::fs::read(path).unwrap_or_default();
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert_eq!(
                std::fs::read(path).unwrap_or_default(),
                stopped,
                "launchd stopped the job's descendant group"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn package_test_and_build_units_hold_the_shard_runner_artifact_lock_shared() {
        let jobs = disk_fixture();
        let store = Store::open_in_memory().unwrap();
        let runtime = FakeRuntime::default();
        for (kind, value, holds_lock) in [
            (
                JobKind::Test,
                serde_json::json!({"package":"rsid","filters":["agent_jobs"]}),
                true,
            ),
            (
                JobKind::Build,
                serde_json::json!({"command":"check","workspace":true}),
                true,
            ),
            // The runner takes (and finally cleans under) its own lock.
            (
                JobKind::Test,
                serde_json::json!({"shard":"other-01"}),
                false,
            ),
        ] {
            let mut context = ctx(Uuid::new_v4(), jobs.path(), None);
            context.params = params(kind, value);
            submit(&store, &runtime, &tools(), jobs.path(), context, Utc::now()).unwrap();
            let launches = runtime.launched.lock().unwrap();
            let spec = launches.last().unwrap();
            let environment = spec.build_environment.as_ref().unwrap();
            let args = systemd_run_args(spec, Some("/usr/bin"));
            let lock = environment.target_dir.join(ARTIFACT_LOCK_FILENAME);
            assert_eq!(
                environment.artifact_lock.as_deref(),
                holds_lock.then_some(lock.as_path())
            );
            let waiter = args.iter().position(|a| a == "rsi-lock");
            assert!(args.iter().any(|arg| arg == "--no-block"));
            #[cfg(target_os = "linux")]
            {
                let admission = args.iter().position(|arg| arg == "rsi-admission").unwrap();
                assert_eq!(args[admission - 1], ADMISSION_WAIT);
                assert_eq!(
                    args[admission + 1],
                    environment
                        .admission_lock
                        .as_ref()
                        .unwrap()
                        .display()
                        .to_string()
                );
                assert!(waiter.is_none_or(|waiter| admission < waiter));
            }
            assert_eq!(waiter.is_some(), holds_lock);
            let Some(waiter) = waiter else { continue };
            // The lock wraps the job's own argv: the waiter script takes the
            // lock shared (logging a contended wait) and then runs it.
            assert_eq!(args[waiter - 1], LOCK_WAIT);
            assert_eq!(
                args[waiter + 1..waiter + 3],
                [lock.to_str().unwrap(), "/x/cargo-slot"]
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_manager_release_build_runs_in_the_shared_release_target() {
        // #1218: the manager seat's release build uses one persistent,
        // daemon-owned target (so a deploy is incremental); a worker's release
        // build, and any dev build, keep the per-sandbox target.
        let jobs = disk_fixture();
        let data = jobs.path().join("rsi-home");
        let cwd = jobs.path().join("sandbox");
        std::fs::create_dir_all(cwd.join("target")).unwrap();
        std::fs::write(cwd.join("target/.rsi-build-env.json"),
            serde_json::json!({"env": {"RUSTC_WRAPPER": "/opt/rsi-build-rustc", "RSI_BUILD_SCCACHE": "/opt/sccache"}}).to_string()).unwrap();
        let release = params(
            JobKind::Build,
            serde_json::json!({"command":"build","workspace":true,"release":true}),
        );
        let dev = params(
            JobKind::Build,
            serde_json::json!({"command":"build","workspace":true}),
        );
        let prepare = |params: &JobParams, manager: bool, name: &str| {
            let log = jobs.path().join(format!("{name}.log"));
            prepare_build_environment_in(params, &cwd, &log, manager, &data, Some(&cwd))
                .unwrap()
                .unwrap()
        };
        let shared = crate::release_target::release_target_dir_in(&data);
        let manager_release = prepare(&release, true, "a");
        assert_eq!(manager_release.target_dir, shared);
        assert!(
            manager_release
                .recorded_env
                .contains(&("RSI_BUILD_SCCACHE".to_string(), "/opt/sccache".to_string()))
        );
        assert_eq!(
            manager_release.admission_lock.as_deref(),
            Some(shared.join(".rsi-job-admission.lock").as_path()),
            "release builds serialize on the target's own admission lock"
        );
        assert_eq!(prepare(&release, false, "b").target_dir, cwd.join("target"));
        assert_eq!(prepare(&dev, true, "c").target_dir, cwd.join("target"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn persistent_build_stamp_takes_precedence_over_legacy_target_record() {
        use crate::sandbox::execution_scratch::BUILD_ENV_RECORD;

        let dir = disk_fixture();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(
            target.join(BUILD_ENV_RECORD),
            br#"{"env":{"CARGO_INCREMENTAL":"1"}}"#,
        )
        .unwrap();
        // Existing sessions can still launch jobs before they resume and stamp
        // the new location.
        assert_eq!(
            recorded_build_env(&target),
            vec![("CARGO_INCREMENTAL".into(), "1".into())]
        );
        let persistent = dir.path().join(BUILD_ENV_RECORD);
        std::fs::write(
            &persistent,
            br#"{"env":{"CARGO_INCREMENTAL":"0","LD_PRELOAD":"/evil.so","RUSTC_WRAPPER":"bad\nwrapper"}}"#,
        )
        .unwrap();
        assert_eq!(
            recorded_build_env(&target),
            vec![("CARGO_INCREMENTAL".into(), "0".into())]
        );
        std::fs::write(&persistent, b"invalid json").unwrap();
        assert!(recorded_build_env(&target).is_empty());
        std::fs::remove_file(&persistent).unwrap();
        std::fs::create_dir(&persistent).unwrap();
        assert!(recorded_build_env(&target).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn queued_job_preserves_stamped_build_environment_after_cargo_clean() {
        use crate::config::{Config, RuntimeConfig};
        use crate::sandbox::execution_scratch::{BUILD_ENV_RECORD, SandboxExecutionScratch};

        let dir = disk_fixture();
        let cwd = dir.path().join("sandbox");
        std::fs::create_dir(&cwd).unwrap();
        // An isolated Cargo project: never clean a real sandbox or a pool.
        std::fs::write(
            cwd.join("Cargo.toml"),
            "[package]\nname = \"clean-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n[lib]\npath = \"lib.rs\"\n",
        )
        .unwrap();
        std::fs::write(cwd.join("lib.rs"), "").unwrap();
        let scratch = SandboxExecutionScratch::prepare_for_test(&cwd)
            .unwrap()
            .with_build_env(&RuntimeConfig::from_config(&Config::default()), true);
        let mut provider = tokio::process::Command::new("unused-provider");
        scratch.stamp_build_env(&mut provider);
        let stamped: std::collections::BTreeMap<_, _> = provider
            .as_std()
            .get_envs()
            .filter_map(|(name, value)| {
                Some((name.to_str()?.to_string(), value?.to_str()?.to_string()))
            })
            .collect();
        assert_eq!(
            stamped.get("CARGO_INCREMENTAL").map(String::as_str),
            Some("0")
        );
        let marker = scratch.target().join("disposable-build-output");
        std::fs::write(&marker, "fixture artifact").unwrap();

        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let mut rows = Vec::new();
        for key in ["clean", "successor"] {
            let mut context = ctx(owner, &cwd, Some(key));
            context.wake = JobWake::None;
            let (row, _) = submit(
                &*store.lock().await,
                &*runtime,
                &tools(),
                dir.path(),
                context,
                Utc::now(),
            )
            .unwrap();
            rows.push(row);
        }
        assert_eq!(rows[0].job.state, JobState::Running);
        assert_eq!(rows[1].job.state, JobState::Queued);
        assert_eq!(rows[1].job.held.as_deref(), Some(SANDBOX_JOB_QUEUED));
        let first = runtime.launched.lock().unwrap()[0].clone();
        let build = first.build_environment.as_ref().unwrap();
        assert_eq!(build.target_dir, scratch.target());
        assert_eq!(
            build
                .recorded_env
                .iter()
                .cloned()
                .collect::<std::collections::BTreeMap<_, _>>(),
            stamped
        );

        // Execute the first unit's clean with its actual launch environment.
        // FakeRuntime keeps the test independent of systemd/launchd; the queue
        // and its launch-time environment preparation are production paths.
        let mut clean = tokio::process::Command::new(
            std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        );
        clean
            .current_dir(&cwd)
            .args(["clean", "--offline"])
            .env("CARGO_TARGET_DIR", &build.target_dir)
            .env("TMPDIR", &build.tmp_dir)
            .envs(build.recorded_env.iter().cloned())
            .env_remove("RSI_SESSION_TOKEN")
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(30), clean.output())
            .await
            .expect("fixture cargo clean timed out")
            .expect("fixture cargo clean starts");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !marker.exists(),
            "cargo clean removed the fixture artifacts"
        );
        assert!(
            cwd.join(BUILD_ENV_RECORD).is_file(),
            "the authenticated stamp survives"
        );

        std::fs::write(&rows[0].status_path, "0\n").unwrap();
        *runtime.active.lock().unwrap() = false;
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 1);
        let drain = crate::deploy_drain::DeployDrain::new();
        assert_eq!(
            start_queued_jobs(&store, &dynamic, &tools(), &drain)
                .await
                .unwrap(),
            1
        );
        let successor = {
            let launches = runtime.launched.lock().unwrap();
            assert_eq!(launches.len(), 2);
            launches[1].clone()
        };
        assert_eq!(successor.unit_name, rows[1].job.unit_name);
        let next = successor.build_environment.as_ref().unwrap();
        assert_eq!(next.target_dir, build.target_dir);
        assert_eq!(next.recorded_env, build.recorded_env);
        let args = systemd_run_args(&successor, Some("/usr/bin"));
        assert!(args.contains(&format!(
            "--setenv=CARGO_TARGET_DIR={}",
            build.target_dir.display()
        )));
        for (name, value) in &stamped {
            assert!(
                args.contains(&format!("--setenv={name}={value}")),
                "{name} survives clean"
            );
        }
        let guard = store.lock().await;
        assert_eq!(
            guard
                .get_agent_job(rows[1].job.id)
                .unwrap()
                .unwrap()
                .job
                .state,
            JobState::Running
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn build_unit_inherits_the_session_shell_build_settings() {
        // #1741: a job unit builds with the settings its session's shell
        // received (recorded by the daemon in .rsi-build-env.json), so
        // it reuses the shell's artifacts and works in a pool-seeded target.
        let jobs = disk_fixture();
        let store = Store::open_in_memory().unwrap();
        let runtime = FakeRuntime::default();
        let cwd = jobs.path().join("sandbox");
        std::fs::create_dir_all(cwd.join("target")).unwrap();
        std::fs::write(
            cwd.join(".rsi-build-env.json"),
            serde_json::to_vec(&serde_json::json!({"version": 1, "env": {
                "CARGO_INCREMENTAL": "0",
                "CARGO_PROFILE_DEV_DEBUG": "line-tables-only",
                "RUSTC_WRAPPER": "/opt/rsi/rsi-build-rustc",
                "SCCACHE_DIR": "/home/u/.rsi/agent-sccache",
                "LD_PRELOAD": "/tmp/evil.so",
            }}))
            .unwrap(),
        )
        .unwrap();
        let mut context = ctx(Uuid::new_v4(), &cwd, None);
        context.params = params(
            JobKind::Build,
            serde_json::json!({"command":"build","workspace":true}),
        );
        submit(&store, &runtime, &tools(), jobs.path(), context, Utc::now()).unwrap();
        let launches = runtime.launched.lock().unwrap();
        let args = systemd_run_args(launches.last().unwrap(), Some("/usr/bin"));
        for expected in [
            "--setenv=CARGO_INCREMENTAL=0",
            "--setenv=CARGO_PROFILE_DEV_DEBUG=line-tables-only",
            "--setenv=RUSTC_WRAPPER=/opt/rsi/rsi-build-rustc",
            "--setenv=SCCACHE_DIR=/home/u/.rsi/agent-sccache",
        ] {
            assert!(
                args.iter().any(|arg| arg == expected),
                "{expected} in {args:?}"
            );
        }
        let target = format!("--setenv=CARGO_TARGET_DIR={}", cwd.join("target").display());
        assert!(args.iter().any(|arg| *arg == target));
        assert!(
            args.iter()
                .filter(|arg| arg.starts_with("--setenv="))
                .all(|arg| {
                    let name = arg["--setenv=".len()..].split('=').next().unwrap();
                    JOB_UNIT_ENV_NAMES.contains(&name)
                        || crate::sandbox::execution_scratch::RECORDED_BUILD_ENV.contains(&name)
                }),
            "only the shared unit names and recorded build settings pass: {args:?}"
        );
        // The private scratch comes from the unit, never from the record, and a
        // name the daemon does not stamp is not forwarded (#2629).
        let environment = launches.last().unwrap().build_environment.as_ref().unwrap();
        for expected in [
            format!("--setenv=TMPDIR={}", environment.tmp_dir.display()),
            format!(
                "--setenv=GIT_CEILING_DIRECTORIES={}",
                environment.tmp_dir.display()
            ),
        ] {
            assert!(args.contains(&expected), "{expected} in {args:?}");
        }
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("LD_PRELOAD") || arg.contains("evil.so")),
            "a foreign recorded name is not forwarded: {args:?}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn build_and_gate_units_receive_only_the_required_build_environment() {
        let jobs = disk_fixture();
        let store = Store::open_in_memory().unwrap();
        let runtime = FakeRuntime::default();
        let oid = "0123456789abcdef0123456789abcdef01234567";
        for (kind, value, build_env) in [
            (
                JobKind::Test,
                serde_json::json!({"package":"rsid","filters":["agent_jobs"]}),
                true,
            ),
            (
                JobKind::Build,
                serde_json::json!({"command":"check","workspace":true}),
                true,
            ),
            (JobKind::Landing, serde_json::json!({"accepted":oid}), true),
            (
                JobKind::CloudGate,
                serde_json::json!({"accepted":oid}),
                true,
            ),
            (JobKind::CloudSweep, serde_json::json!({"sha":oid}), false),
        ] {
            let cwd = jobs.path().join(format!("{kind:?}"));
            std::fs::create_dir(&cwd).unwrap();
            let mut context = ctx(Uuid::new_v4(), &cwd, None);
            context.params = params(kind, value);
            let (row, _) =
                submit(&store, &runtime, &tools(), jobs.path(), context, Utc::now()).unwrap();
            let launches = runtime.launched.lock().unwrap();
            let spec = launches.last().unwrap();
            let args = systemd_run_args(spec, Some("/usr/bin"));
            let env: Vec<_> = args
                .iter()
                .filter_map(|a| a.strip_prefix("--setenv="))
                .map(|a| a.split_once('=').unwrap())
                .collect();
            assert!(
                env.iter().all(|(key, _)| JOB_UNIT_ENV_NAMES.contains(key)),
                "only the shared unit names pass: {env:?}"
            );
            assert_eq!(env.iter().any(|(key, _)| *key == "TMPDIR"), build_env);
            assert_eq!(
                env.iter().any(|(key, _)| *key == "GIT_CEILING_DIRECTORIES"),
                build_env
            );
            assert_eq!(
                env.iter().any(|(key, _)| *key == "CARGO_TARGET_DIR"),
                build_env
            );
            if build_env {
                let environment = spec.build_environment.as_ref().unwrap();
                assert!(environment.tmp_dir.is_dir(), "scratch exists before launch");
                assert_eq!(
                    environment.tmp_dir,
                    job_tmp_dir(Path::new(&row.job.log_path))
                );
                assert!(environment.target_dir.is_absolute());
                // The lander validates that the target already exists and is
                // outside its TMPDIR before it starts any gates.
                assert_eq!(environment.target_dir, cwd.join("target"));
                assert!(environment.target_dir.is_dir());
                assert!(!environment.target_dir.starts_with(&environment.tmp_dir));
                assert!(args.contains(&format!(
                    "--setenv=TMPDIR={}",
                    environment.tmp_dir.display()
                )));
                // #2612: plain Git in the private TMPDIR cannot reach the
                // repository around the jobs directory (the operator's home).
                assert!(args.contains(&format!(
                    "--setenv=GIT_CEILING_DIRECTORIES={}",
                    environment.tmp_dir.display()
                )));
                assert!(args.contains(&format!(
                    "--setenv=CARGO_TARGET_DIR={}",
                    environment.target_dir.display()
                )));
                // Every build-scratch name the unit sets is a shared name and
                // reaches the command line with its value.
                for (name, value) in environment.scratch_env() {
                    assert!(JOB_UNIT_ENV_NAMES.contains(&name), "{name} is shared");
                    assert!(
                        args.contains(&format!("--setenv={name}={value}")),
                        "{name} in {args:?}"
                    );
                }
            }
        }
        let launches = runtime.launched.lock().unwrap();
        assert_ne!(
            launches[0].build_environment.as_ref().unwrap().tmp_dir,
            launches[1].build_environment.as_ref().unwrap().tmp_dir
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn cargo_target_resolution_keeps_sandbox_output_under_reclaim() {
        let root = disk_fixture();
        let cwd = root.path().join("worktree");
        std::fs::create_dir_all(cwd.join(".cargo")).unwrap();
        for config in ["config.toml", "config"] {
            std::fs::write(
                cwd.join(".cargo").join(config),
                "[build]\ntarget-dir = '/outside/review-target'\n",
            )
            .unwrap();
        }
        assert_eq!(cargo_target_dir(&cwd), cwd.join("target"));
        assert_eq!(
            job_target_dir(&cwd, Some(&cwd), root.path()).unwrap(),
            cwd.join("target")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn sandbox_subdirectories_and_sibling_sandboxes_reuse_their_sandbox_target() {
        let root = disk_fixture();
        let data = root.path().join("data");
        let base = root.path().join("sandboxes");
        let own = base.join("own");
        let sibling = base.join("sibling");
        let outside = root.path().join("integration");
        for dir in [
            own.join("crates/rsid"),
            sibling.join("crates/rsid"),
            outside.clone(),
            data.clone(),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let own = own.canonicalize().unwrap();
        let sibling = sibling.canonicalize().unwrap();
        let sandbox = Some(own.as_path());
        // The owner's root and any subdirectory use the sandbox's own target.
        for cwd in [own.clone(), own.join("crates/rsid")] {
            assert_eq!(
                job_target_dir(&cwd, sandbox, &data).unwrap(),
                own.join("target"),
                "{}",
                cwd.display()
            );
        }
        // Another worker's sandbox (root or subdirectory) uses that sandbox's target.
        for cwd in [sibling.clone(), sibling.join("crates/rsid")] {
            assert_eq!(
                job_target_dir(&cwd, sandbox, &data).unwrap(),
                sibling.join("target"),
                "{}",
                cwd.display()
            );
        }
        // A worktree outside every sandbox root is the only manager-target case.
        assert_eq!(
            enclosing_sandbox_root(&outside.canonicalize().unwrap(), &own, &data),
            None
        );
        // The daemon's default sandbox base counts as a sandbox root too, even
        // when the owner's sandbox lives elsewhere.
        let default_sandbox = data.join("sandboxes/other/crates");
        std::fs::create_dir_all(&default_sandbox).unwrap();
        let default_sandbox = default_sandbox.canonicalize().unwrap();
        assert_eq!(
            enclosing_sandbox_root(&default_sandbox, &own, &data),
            Some(data.join("sandboxes/other").canonicalize().unwrap())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn test_and_build_jobs_from_two_worktrees_build_in_their_own_targets() {
        let jobs = disk_fixture();
        let store = Store::open_in_memory().unwrap();
        let runtime = FakeRuntime::default();
        let mut targets = Vec::new();
        for name in ["worktree-a", "worktree-b"] {
            let worktree = jobs.path().join(name);
            std::fs::create_dir_all(&worktree).unwrap();
            for (kind, value) in [
                (
                    JobKind::Test,
                    serde_json::json!({"package":"rsid","filters":["agent_jobs"]}),
                ),
                (
                    JobKind::Build,
                    serde_json::json!({"command":"check","workspace":true}),
                ),
            ] {
                let mut context = ctx(Uuid::new_v4(), &worktree, None);
                context.params = params(kind, value);
                submit(&store, &runtime, &tools(), jobs.path(), context, Utc::now()).unwrap();
                let launches = runtime.launched.lock().unwrap();
                let spec = launches.last().unwrap();
                let args = systemd_run_args(spec, Some("/usr/bin"));
                assert!(args.contains(&format!(
                    "--setenv=CARGO_TARGET_DIR={}",
                    worktree.join("target").display()
                )));
                assert_eq!(
                    spec.build_environment.as_ref().unwrap().target_dir,
                    worktree.join("target")
                );
                targets.push(worktree.join("target"));
            }
        }
        assert_ne!(targets[0], targets[2]);
        assert_ne!(targets[1], targets[3]);
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn memory_backed_jobs_directory_is_refused_before_launch() {
        let base = Path::new("/dev/shm");
        if !base.is_dir()
            || nix::sys::statfs::statfs(base).unwrap().filesystem_type()
                != nix::sys::statfs::TMPFS_MAGIC
        {
            return;
        }
        let jobs = tempfile::tempdir_in(base).unwrap();
        let store = Store::open_in_memory().unwrap();
        let runtime = FakeRuntime::default();
        let owner = Uuid::new_v4();
        let error = submit(
            &store,
            &runtime,
            &tools(),
            jobs.path(),
            ctx(owner, jobs.path(), None),
            Utc::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("job TMPDIR is on tmpfs/ramfs"));
        assert!(runtime.launched.lock().unwrap().is_empty());
        let row = &store.list_agent_jobs(owner, 1).unwrap()[0];
        assert_eq!(row.job.state, JobState::Failed);
        assert!(!job_tmp_dir(Path::new(&row.job.log_path)).exists());
    }

    fn wakes_for(store: &Store, owner: Uuid) -> Vec<rsi_common::types::ScheduledJob> {
        store
            .list_scheduled_jobs()
            .expect("wakes")
            .into_iter()
            .filter(|job| job.wake_session_id == Some(owner))
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn recipe_jobs_persist_replay_reconcile_and_timeout_with_existing_wake_policy() {
        let (store, dir) = open();
        std::fs::create_dir(dir.path().join(".rsi")).unwrap();
        std::fs::write(dir.path().join(".rsi/jobs.toml"),
            "version = 1\n[recipes.gate]\nrunner = 'make'\ntarget = 'check-cpu'\ntimeout_minutes = 20\ncpu_quota_percent = 200\n").unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let now = Utc::now();
        let recipe_ctx = |key, wake| SubmitContext {
            params: params(
                JobKind::Test,
                serde_json::json!({"recipe":"gate","timeout_minutes":10}),
            ),
            wake,
            ..ctx(owner, dir.path(), Some(key))
        };
        let (finished, replayed) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            recipe_ctx("finished", JobWake::Owner),
            now,
        )
        .unwrap();
        assert!(!replayed);
        let (again, replayed) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            recipe_ctx("finished", JobWake::Owner),
            now,
        )
        .unwrap();
        assert!(replayed);
        assert_eq!(again.job.id, finished.job.id);
        assert_eq!(runtime.launched.lock().unwrap().len(), 1);
        // Reconciliation needs only the durable row and status, not the
        // submitting turn or a recipe manifest that may since have changed.
        std::fs::remove_file(dir.path().join(".rsi/jobs.toml")).unwrap();
        std::fs::write(&finished.status_path, "0\n").unwrap();
        *runtime.active.lock().unwrap() = false;
        assert_eq!(poll_once(&store, &dynamic, now).await.unwrap(), 1);
        assert_eq!(poll_once(&store, &dynamic, now).await.unwrap(), 0);
        {
            let guard = store.lock().await;
            let row = guard.get_agent_job(finished.job.id).unwrap().unwrap();
            assert_eq!(row.job.params, finished.job.params);
            assert_eq!(row.job.state, JobState::Succeeded);
            assert_eq!(row.job.result.unwrap().exit_code, Some(0));
            assert_eq!(wakes_for(&guard, owner).len(), 1);
        }
        std::fs::write(dir.path().join(".rsi/jobs.toml"),
            "version = 1\n[recipes.gate]\nrunner = 'make'\ntarget = 'check-cpu'\ntimeout_minutes = 20\ncpu_quota_percent = 200\n").unwrap();
        let (timed, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            recipe_ctx("timed", JobWake::None),
            now,
        )
        .unwrap();
        append_phase(&timed.job.log_path, now, "job-start");
        let later = now + chrono::Duration::minutes(11);
        assert_eq!(poll_once(&store, &dynamic, later).await.unwrap(), 1);
        let guard = store.lock().await;
        let row = guard.get_agent_job(timed.job.id).unwrap().unwrap();
        assert_eq!(row.job.state, JobState::Failed);
        assert_eq!(
            row.job.result.unwrap().refusal.as_deref(),
            Some(JOB_TIMED_OUT)
        );
        assert_eq!(
            wakes_for(&guard, owner).len(),
            1,
            "wake none remains silent for a batch predicate"
        );
        assert_eq!(*runtime.stopped.lock().unwrap(), vec![timed.job.unit_name]);
        assert!(!job_tmp_dir(Path::new(&timed.job.log_path)).exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_recipe_declaring_less_than_the_stamped_default_timeout_runs_at_its_declaration() {
        let store = Store::open_in_memory().unwrap();
        let dir = disk_fixture();
        std::fs::create_dir(dir.path().join(".rsi")).unwrap();
        std::fs::write(dir.path().join(".rsi/jobs.toml"),
            "version = 1\n[recipes.gate]\nrunner = 'make'\ntarget = 'gate'\ntimeout_minutes = 10\ncpu_quota_percent = 200\n").unwrap();
        let runtime = FakeRuntime::default();
        let owner = Uuid::new_v4();
        // The verb stamps the operator default (20) on a request naming none.
        let (row, _) = submit(
            &store,
            &runtime,
            &tools(),
            dir.path(),
            SubmitContext {
                params: params(
                    JobKind::Test,
                    serde_json::json!({"recipe":"gate","timeout_minutes":20}),
                ),
                ..ctx(owner, dir.path(), None)
            },
            Utc::now(),
        )
        .unwrap();
        assert_eq!(job_timeout_secs(&row.job.params), Some(600));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn recipe_refusals_do_not_record_or_launch_a_job() {
        let store = Store::open_in_memory().unwrap();
        let dir = disk_fixture();
        let runtime = FakeRuntime::default();
        let owner = Uuid::new_v4();
        let request = || SubmitContext {
            params: params(
                JobKind::Test,
                serde_json::json!({"recipe":"gate","timeout_minutes":20}),
            ),
            ..ctx(owner, dir.path(), None)
        };
        assert!(
            submit(
                &store,
                &runtime,
                &tools(),
                dir.path(),
                request(),
                Utc::now()
            )
            .is_err()
        );
        std::fs::create_dir(dir.path().join(".rsi")).unwrap();
        for manifest in [
            "version = 1\n[recipes]",
            "version = 2\n[recipes]",
            "version = 1\n[recipes.other]\nrunner = 'make'\ntarget = 'gate'\ntimeout_minutes = 20\ncpu_quota_percent = 200\n",
        ] {
            std::fs::write(dir.path().join(".rsi/jobs.toml"), manifest).unwrap();
            assert!(
                submit(
                    &store,
                    &runtime,
                    &tools(),
                    dir.path(),
                    request(),
                    Utc::now()
                )
                .is_err()
            );
        }
        // #1954: stale declarations and target aliases refuse before recording
        // or launching anything, and direct the caller to the rolling-tool path.
        for target in ["scoped-test", "scoped-test-tmpfs"] {
            for name in [target, "verify"] {
                std::fs::write(
                    dir.path().join(".rsi/jobs.toml"),
                    format!("version = 1\n[recipes.{name}]\nrunner = 'make'\ntarget = '{target}'\ntimeout_minutes = 20\ncpu_quota_percent = 200\n"),
                )
                .unwrap();
                let error = submit(
                    &store,
                    &runtime,
                    &tools(),
                    dir.path(),
                    SubmitContext {
                        params: params(JobKind::Test, serde_json::json!({"recipe":name})),
                        ..ctx(owner, dir.path(), None)
                    },
                    Utc::now(),
                )
                .unwrap_err();
                assert!(matches!(error, DaemonError::InvalidParam(code)
                    if code == rsi_common::agent_jobs::JOB_RECIPE_USE_SCOPED_TEST));
            }
        }
        assert!(store.list_agent_jobs(owner, 10).unwrap().is_empty());
        assert!(runtime.launched.lock().unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn package_test_target_selects_one_integration_harness_with_exact_filters() {
        let command = job_command(
            &tools(),
            &params(
                JobKind::Test,
                serde_json::json!({"package":"rsi-common","test_target":"manifest_dir_guard","exact":true,"filters":["tests::one","tests::two"]}),
            ),
            Path::new("/w/sandbox"),
            None,
        )
        .unwrap();
        assert_eq!(
            command.argv,
            [
                "/x/cargo-slot",
                "env",
                "-u",
                "RSI_PROCESS_OWNERSHIP_NAMESPACE",
                "cargo",
                "test",
                "-p",
                "rsi-common",
                "--test",
                "manifest_dir_guard",
                "--no-fail-fast",
                "--",
                "tests::one",
                "tests::two",
                "--exact"
            ]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn package_test_exact_matching_is_a_libtest_flag_and_preserves_default_argv() {
        let command = |value| {
            job_command(
                &tools(),
                &params(JobKind::Test, value),
                Path::new("/w/sandbox"),
                None,
            )
            .unwrap()
            .argv
        };
        for filters in [
            serde_json::json!([]),
            serde_json::json!(["module::tests::one", "module::tests::two"]),
        ] {
            for lib_only in [false, true] {
                let mut value =
                    serde_json::json!({"package":"rsi","filters":filters,"lib_only":lib_only});
                let default = command(value.clone());
                value["exact"] = serde_json::json!(false);
                assert_eq!(command(value.clone()), default);
                value["exact"] = serde_json::json!(true);
                let exact = command(value);
                let mut expected = default;
                if filters.as_array().unwrap().is_empty() {
                    expected.push("--".into());
                }
                expected.push("--exact".into());
                assert_eq!(exact, expected);
                let separator = exact.iter().position(|arg| arg == "--").unwrap();
                let mut libtest_args: Vec<String> =
                    serde_json::from_value(filters.clone()).unwrap();
                libtest_args.push("--exact".into());
                assert_eq!(exact[separator + 1..], libtest_args);
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn commands_are_fixed_argv_built_only_from_typed_fields() {
        let cwd = Path::new("/w/sandbox");
        let t = tools();
        let package = job_command(
            &t,
            &params(
                JobKind::Test,
                serde_json::json!({"package":"rsid","filters":["a::b","c"],"lib_only":true}),
            ),
            cwd,
            None,
        )
        .unwrap();
        assert_eq!(
            package.argv,
            [
                "/x/cargo-slot",
                "env",
                "-u",
                "RSI_PROCESS_OWNERSHIP_NAMESPACE",
                "cargo",
                "test",
                "-p",
                "rsid",
                "--lib",
                "--no-fail-fast",
                "--",
                "a::b",
                "c"
            ]
        );
        // #1106: no filters runs every test of the package, still without
        // stopping at the first failing binary.
        let all = job_command(
            &t,
            &params(JobKind::Test, serde_json::json!({"package":"rsid"})),
            cwd,
            None,
        )
        .unwrap();
        assert_eq!(
            all.argv[4..],
            ["cargo", "test", "-p", "rsid", "--no-fail-fast"]
        );
        let empty = job_command(
            &t,
            &params(
                JobKind::Test,
                serde_json::json!({"package":"rsid","filters":[]}),
            ),
            cwd,
            None,
        )
        .unwrap();
        assert_eq!(empty.argv, all.argv);
        let shard = job_command(
            &t,
            &params(
                JobKind::Test,
                serde_json::json!({"shard":"other-01","filterset":"test(x)"}),
            ),
            cwd,
            None,
        )
        .unwrap();
        assert_eq!(shard.argv[4], "scripts/run-rsid-test-shards.sh");
        assert_eq!(
            shard.argv[5..],
            ["shard", "other-01", "--filterset", "test(x)"]
        );
        let build = job_command(
            &t,
            &params(
                JobKind::Build,
                serde_json::json!({"command":"check","workspace":true,"all_targets":true}),
            ),
            cwd,
            None,
        )
        .unwrap();
        assert_eq!(
            build.argv[5..],
            ["check", "--workspace", "--all-targets", "--keep-going"]
        );
        let build = job_command(
            &t,
            &params(
                JobKind::Build,
                serde_json::json!({"command":"build","package":"rsid","release":true}),
            ),
            cwd,
            None,
        )
        .unwrap();
        assert_eq!(build.argv[5..], ["build", "-p", "rsid", "--release"]);
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let landing = job_command(
            &t,
            &params(
                JobKind::Landing,
                serde_json::json!({"accepted":oid,"test_filters":["rsid=agent_jobs"]}),
            ),
            cwd,
            None,
        )
        .unwrap();
        assert_eq!(landing.argv[..2], ["/x/cargo-slot", "/x/rsi-rolling-land"]);
        assert_eq!(
            landing.argv[2..],
            [
                "--repo",
                "/w/sandbox",
                "--remote",
                "origin",
                "--accepted",
                oid,
                "--test-filter",
                "rsid=agent_jobs"
            ]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn cloud_gate_runs_the_daemons_embedded_script_never_the_callers_tree() {
        let sandbox = tempfile::tempdir().unwrap();
        let jobs = tempfile::tempdir().unwrap();
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let cloud = params(JobKind::CloudGate, serde_json::json!({"accepted":oid}));
        assert!(
            job_command(&tools(), &cloud, sandbox.path(), None).is_err(),
            "no trusted script"
        );
        // A hostile script in the caller's tree is never chosen.
        std::fs::create_dir_all(sandbox.path().join("scripts")).unwrap();
        std::fs::write(
            sandbox.path().join("scripts/cloud-gate.sh"),
            "#!/bin/sh\nevil\n",
        )
        .unwrap();
        assert!(job_command(&tools(), &cloud, sandbox.path(), None).is_err());
        let script = materialize_trusted_gate(jobs.path()).unwrap();
        assert!(script.starts_with(jobs.path()));
        let command = job_command(&tools(), &cloud, sandbox.path(), Some(&script)).unwrap();
        assert_eq!(command.argv[0], script.display().to_string());
        assert!(!command.argv[0].starts_with(&sandbox.path().display().to_string()));
        assert_eq!(command.argv[1], "--");
        assert!(command.stop_timeout_secs >= 900, "destroy trap needs time");
        // The Terraform the script applies sits beside it, from the same embedded bytes.
        let tf = script
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("infra/aws/gate/main.tf");
        assert_eq!(
            std::fs::read_to_string(tf).unwrap(),
            include_str!("../../../infra/aws/gate/main.tf")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn cloud_sweep_runs_the_daemons_embedded_script_never_the_callers_tree() {
        let sandbox = tempfile::tempdir().unwrap();
        let jobs = tempfile::tempdir().unwrap();
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let sweep = params(JobKind::CloudSweep, serde_json::json!({"sha":oid}));
        assert!(
            job_command(&tools(), &sweep, sandbox.path(), None).is_err(),
            "no trusted script"
        );
        // A hostile script in the caller's tree is never chosen, and passing
        // it as the gate script's sibling does not make it trusted either.
        std::fs::create_dir_all(sandbox.path().join("scripts")).unwrap();
        for name in ["cloud-sweep.sh", "cloud-gate.sh"] {
            std::fs::write(
                sandbox.path().join("scripts").join(name),
                "#!/bin/sh\nevil\n",
            )
            .unwrap();
        }
        assert!(job_command(&tools(), &sweep, sandbox.path(), None).is_err());
        let gate = materialize_trusted_gate(jobs.path()).unwrap();
        let command = job_command(&tools(), &sweep, sandbox.path(), Some(&gate)).unwrap();
        let script = PathBuf::from(&command.argv[0]);
        assert!(script.starts_with(jobs.path()));
        assert!(!script.starts_with(sandbox.path()));
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            include_str!("../../../scripts/cloud-sweep.sh")
        );
        assert_eq!(
            command.argv[1..],
            [
                "cloud".to_string(),
                oid.to_string(),
                "--repo".to_string(),
                sandbox.path().display().to_string(),
                "--mirror".to_string(),
                jobs.path().join("sweep-mirror.git").display().to_string()
            ]
        );
        assert!(
            command.stop_timeout_secs >= 20 * 60,
            "the EXIT trap destroys the host inside the stop timeout"
        );
        // Every local file the cloud mode reads is embedded beside the script.
        for (relative, embedded) in [
            (
                "scripts/cloud-gate.sh",
                include_str!("../../../scripts/cloud-gate.sh"),
            ),
            (
                "scripts/cloud-spend.py",
                include_str!("../../../scripts/cloud-spend.py"),
            ),
            (
                "scripts/cloud-sweep-verdict.py",
                include_str!("../../../scripts/cloud-sweep-verdict.py"),
            ),
            (
                "scripts/cloud-sweep-excerpts.sh",
                include_str!("../../../scripts/cloud-sweep-excerpts.sh"),
            ),
            (
                "infra/aws/gate/main.tf",
                include_str!("../../../infra/aws/gate/main.tf"),
            ),
        ] {
            let path = script.parent().unwrap().parent().unwrap().join(relative);
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                embedded,
                "{relative}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_sweep_verdict_is_the_exact_machine_line_and_never_defaults_to_green() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let other = "fedcba9876543210fedcba9876543210fedcba98";
        let line = |tail: &str| format!("noise\nVERDICT {tail}\nspend line\n");
        assert_eq!(
            parse_sweep_verdict_line(&line(&format!("GREEN {sha} new=0")), sha),
            Some((SweepVerdict::Green, 0))
        );
        assert_eq!(
            parse_sweep_verdict_line(&line(&format!("RED {sha} new=2")), sha),
            Some((SweepVerdict::Red, 2))
        );
        assert_eq!(
            parse_sweep_verdict_line(&line(&format!("INCOMPLETE {sha} new=0")), sha),
            Some((SweepVerdict::Incomplete, 0))
        );
        assert_eq!(
            parse_sweep_verdict_line(&format!("VERDICT GREEN {sha} new=0\r\n"), sha),
            Some((SweepVerdict::Green, 0)),
            "a CRLF ending is not a token"
        );
        // Anything after or around the grammar is not a verdict: the parser
        // accepts only `VERDICT <STATE> <sha> new=<n>`.
        for bad in [
            format!("VERDICT GREEN {sha} arbitrary-suffix"),
            format!("VERDICT GREEN {sha}"),
            format!("VERDICT GREEN {sha} (known only: #1, #2)"),
            format!("VERDICT GREEN {sha} new=0 extra"),
            format!("VERDICT GREEN {sha} new=1"),
            format!("VERDICT GREEN {sha} new=00x"),
            format!("VERDICT GREEN {sha} new="),
            format!("VERDICT GREEN {sha} new=-0"),
            format!("VERDICT GREEN {sha}  new=0"),
            format!("VERDICT RED {sha} new=0"),
            format!("VERDICT RED {sha}"),
            format!("VERDICT RED {sha} new=99999999999"),
            format!("VERDICT GREEN {other} new=0"),
            format!("VERDICT PASS {sha} new=0"),
            "VERDICT GREEN".to_string(),
            format!("VERDICT GREEN {sha} new=0\nVERDICT nonsense"),
            format!("VERDICT GREEN {sha} new=0\nVERDICT GREEN {sha} arbitrary"),
            format!("- `VERDICT GREEN {sha} new=0`"),
            format!("  VERDICT GREEN {sha} new=0"),
            String::new(),
            "no verdict here".to_string(),
        ] {
            assert_eq!(parse_sweep_verdict_line(&bad, sha), None, "{bad:?}");
            assert_eq!(parse_sweep_verdict(&bad, sha), None, "{bad:?}");
        }
        assert_eq!(
            parse_sweep_verdict(
                &format!("VERDICT GREEN {sha} new=0\nVERDICT RED {sha} new=1\n"),
                sha
            ),
            Some(SweepVerdict::Red),
            "the final VERDICT line decides"
        );
    }

    fn sweep_job_result(
        sha: &str,
        exit: Option<i32>,
        log: &str,
        qa: Option<&str>,
    ) -> (JobState, AgentJobResultV1) {
        let results = tempfile::tempdir().unwrap();
        if let Some(qa) = qa {
            std::fs::write(results.path().join("QA.md"), qa).unwrap();
        }
        classify_cloud_sweep(
            sha,
            exit,
            log,
            AgentJobResultV1 {
                exit_code: exit,
                ..AgentJobResultV1::default()
            },
            results.path(),
        )
    }

    /// The lane labels a complete sweep reports: every rsid shard plus the
    /// fixed lanes `scripts/cloud-sweep.sh remote` runs.
    fn sweep_lane_labels() -> Vec<String> {
        sweep_expected_lanes()
    }

    fn sweep_lane_table(labels: &[String]) -> String {
        let rows: String = labels
            .iter()
            .map(|label| format!("| {label} | 10 | 0 | 1 | 0 |\n"))
            .collect();
        format!("| Lane | Pass | Fail | Skip | Exit |\n| --- | ---: | ---: | ---: | ---: |\n{rows}")
    }

    fn sweep_qa_with_labels(sha: &str, labels: &[String], machine: &str) -> String {
        format!(
            "# Cloud QA sweep\n\nTip SHA: `{sha}`  \nLanes: {}  \n\n{}\n## Verdict\n\n{machine}\n",
            labels.len(),
            sweep_lane_table(labels),
        )
    }

    /// A complete collected report for `sha`: the tip line, earlier failures
    /// that are not in the verdict, the verdict section and the machine line.
    fn sweep_qa(sha: &str, verdict_lines: &str, machine: &str) -> String {
        format!(
            "# Cloud QA sweep\n\nTip SHA: `{sha}`  \nLanes: {}  \n\n{}\n- `old::not_in_the_verdict_section`\n\n## Verdict\n\n{verdict_lines}\n{machine}\n",
            sweep_lane_labels().len(),
            sweep_lane_table(&sweep_lane_labels()),
        )
    }

    /// #1120: a crashed (SIGABRT) or timed-out test is named in
    /// `new_failures` with its class, the verdict stays RED, and the detail
    /// says `crash` / `timeout`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn sweep_names_crashed_and_timed_out_tests_with_their_class() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let red_line = format!("VERDICT RED {sha} new=3");
        let qa = sweep_qa(
            sha,
            concat!(
                "- NEW `rsid session::h2_rotation_successor_interleaving_matrix_preserves_one_authority_projection` [crash]\n",
                "- NEW `rsid store::tests::slow_one` [timeout] (name-only match #9; signature unverified)\n",
                "- NEW `rsid b::plain_failure`\n",
                "- KNOWN #7 `rsid a::known_crash` [crash]\n",
            ),
            &red_line,
        );
        let (state, result) =
            sweep_job_result(sha, Some(0), &format!("cat QA.md\n{red_line}\n"), Some(&qa));
        assert_eq!(state, JobState::Failed);
        let detail = result.detail.clone().unwrap();
        let sweep = result.sweep.unwrap();
        assert_eq!(sweep.verdict, SweepVerdict::Red);
        assert_eq!(
            sweep.new_failures,
            [
                "rsid session::h2_rotation_successor_interleaving_matrix_preserves_one_authority_projection [crash]",
                "rsid store::tests::slow_one [timeout]",
                "rsid b::plain_failure",
            ]
        );
        assert_eq!(sweep.known_failures, ["#7 rsid a::known_crash [crash]"]);
        assert!(
            detail.contains("new=3; known=1; crash=1; timeout=1"),
            "{detail}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn sweep_results_are_typed_green_red_incomplete_or_refused() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let red_line = format!("VERDICT RED {sha} new=2");
        let qa = sweep_qa(
            sha,
            "- KNOWN #1016 `rsid a::known`\n- NEW `rsid b::new_one`\n- NEW `rsid c::new_two` (name-only match #9; signature unverified)\n",
            &red_line,
        );
        // RED with failures.
        let (state, red) =
            sweep_job_result(sha, Some(0), &format!("cat QA.md\n{red_line}\n"), Some(&qa));
        assert_eq!(state, JobState::Failed);
        let sweep = red.sweep.unwrap();
        assert_eq!(sweep.verdict, SweepVerdict::Red);
        assert_eq!(sweep.sha, sha);
        assert!(sweep.results_dir.len() > 1);
        assert_eq!(sweep.new_failures, ["rsid b::new_one", "rsid c::new_two"]);
        assert_eq!(sweep.known_failures, ["#1016 rsid a::known"]);
        assert_eq!(red.refusal, None);

        // GREEN (known failures only) succeeds.
        let green_line = format!("VERDICT GREEN {sha} new=0");
        let known_only = sweep_qa(
            sha,
            "Known only: #1016\n- KNOWN #1016 `rsid a::known`\n",
            &green_line,
        );
        let (state, green) = sweep_job_result(
            sha,
            Some(0),
            &format!("Known only: #1016\n{green_line}\n"),
            Some(&known_only),
        );
        assert_eq!(state, JobState::Succeeded);
        let sweep = green.sweep.unwrap();
        assert_eq!(sweep.verdict, SweepVerdict::Green);
        assert!(sweep.new_failures.is_empty());
        assert_eq!(sweep.known_failures, ["#1016 rsid a::known"]);
        assert_eq!(green.refusal, None);

        // GREEN with a nonzero exit, or NEW failures in the report, is not GREEN.
        let new_in_report = sweep_qa(sha, "- NEW `rsid b::new_one`\n", &green_line);
        for (exit, report) in [(Some(1), &known_only), (Some(0), &new_in_report)] {
            let (state, result) =
                sweep_job_result(sha, exit, &format!("{green_line}\n"), Some(report));
            assert_eq!(state, JobState::Failed);
            assert_eq!(result.sweep.unwrap().verdict, SweepVerdict::Incomplete);
        }

        // INCOMPLETE, a missing line and a malformed line never become GREEN.
        for log in [
            format!("VERDICT INCOMPLETE {sha} new=0\n"),
            "the script died\n".to_string(),
            "VERDICT GREEN\n".to_string(),
            format!("VERDICT GREEN {sha} arbitrary-suffix\n"),
            String::new(),
        ] {
            let (state, result) = sweep_job_result(sha, Some(0), &log, Some(&known_only));
            assert_eq!(state, JobState::Failed, "{log:?}");
            assert_eq!(result.sweep.unwrap().verdict, SweepVerdict::Incomplete);
        }
    }

    /// #1085: the sweep wake is a compact typed summary. The log's ssh
    /// warnings and the whole QA.md never reach the wake or the result detail.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_sweep_wake_is_a_compact_summary_without_ssh_warnings_or_the_report() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let ssh = "** WARNING: connection is not using a post-quantum key exchange algorithm.\n\
                   ** This session may be vulnerable to \"store now, decrypt later\" attacks.\n\
                   ** The server may need to be upgraded. See https://openssh.com/pq.html\n\
                   Warning: Permanently added '10.0.0.7' (ED25519) to the list of known hosts.\n";
        let job_for = |result: &AgentJobResultV1| {
            let job = AgentJobV1 {
                id: Uuid::new_v4(),
                kind: JobKind::CloudSweep,
                name: None,
                state: JobState::Failed,
                owner_session_id: Uuid::new_v4(),
                unit_name: "u".into(),
                cwd: "/tmp".into(),
                log_path: "/tmp/job.log".into(),
                params: params(JobKind::CloudSweep, serde_json::json!({"sha":sha})),
                exit_code: result.exit_code,
                result: None,
                created_at: Utc::now().to_rfc3339(),
                started_at: None,
                finished_at: None,
                held: None,
                wake: JobWake::Owner,
            };
            crate::store::agent_jobs::wake_message(&job, JobState::Failed, result)
        };
        let fixtures = [
            (
                "GREEN",
                format!("VERDICT GREEN {sha} new=0"),
                "Known only: #1016\n- KNOWN #1016 `rsid a::known`\n".to_string(),
                "new=0; known=1; lanes=23; wall=12m05s",
            ),
            (
                "RED",
                format!("VERDICT RED {sha} new=2"),
                "- KNOWN #1016 `rsid a::known`\n- NEW `rsid b::new_one`\n- NEW `rsid c::new_two`\n"
                    .to_string(),
                "new=2; known=1; lanes=23; wall=12m05s",
            ),
        ];
        for (label, line, verdict_lines, expect_detail) in fixtures {
            let qa = sweep_qa(sha, &verdict_lines, &line);
            let log = format!("{ssh}{ssh}{qa}\n{line}\n");
            let results = tempfile::tempdir().unwrap();
            std::fs::write(results.path().join("QA.md"), &qa).unwrap();
            let (_, result) = classify_cloud_sweep_timed(
                sha,
                Some(0),
                &log,
                AgentJobResultV1 {
                    exit_code: Some(0),
                    detail: Some(tail(&log)),
                    ..AgentJobResultV1::default()
                },
                results.path(),
                Some(725),
            );
            let wake = job_for(&result);
            assert!(wake.contains(&format!("verdict={label}")), "{wake}");
            assert!(wake.contains(&format!("sha={sha}")), "{wake}");
            assert!(wake.contains("results_dir="), "{wake}");
            assert!(wake.contains("log=/tmp/job.log"), "{wake}");
            assert!(wake.contains(&format!("detail={expect_detail}")), "{wake}");
            assert!(
                wake.len() < 700,
                "{label} wake is {} bytes: {wake}",
                wake.len()
            );
            for noise in [
                "post-quantum",
                "Permanently added",
                "Cloud QA sweep",
                "| Lane |",
            ] {
                assert!(
                    !wake.contains(noise),
                    "{label} wake carries {noise}: {wake}"
                );
            }
        }

        // An INCOMPLETE sweep names its cause through a filtered, bounded tail.
        let log = format!("{ssh}CLOUD SWEEP running\nthe script died\n");
        let (_, result) = classify_cloud_sweep(
            sha,
            Some(1),
            &log,
            AgentJobResultV1::default(),
            tempfile::tempdir().unwrap().path(),
        );
        let detail = result.detail.unwrap();
        assert!(detail.contains("the script died"), "{detail}");
        assert!(!detail.contains("post-quantum") && !detail.contains("Permanently added"));
        assert!(detail.len() < 700, "{detail}");
    }

    /// Review #1077 (blocker): the sweep's fetch-and-bundle step never runs the
    /// caller's repository configuration. The caller's tree holds a hostile
    /// `core.sshCommand`, `core.fsmonitor`, hooks, a filter driver and a
    /// `GIT_SSH_COMMAND` in the environment; none of them may run, for a local
    /// origin, an ssh origin and an `ext::` origin alike.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_sweep_bundle_step_never_runs_the_callers_git_configuration() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        let work = tempfile::tempdir().unwrap();
        let root = work.path();
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .current_dir(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let origin = root.join("origin.git");
        git(root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        let seed = root.join("seed");
        git(
            root,
            &["init", "-q", "-b", "rolling", seed.to_str().unwrap()],
        );
        std::fs::write(seed.join("a.txt"), "a\n").unwrap();
        git(&seed, &["add", "a.txt"]);
        git(&seed, &["commit", "-q", "-m", "tip"]);
        git(
            &seed,
            &["push", "-q", origin.to_str().unwrap(), "rolling:rolling"],
        );
        let sha = git(&seed, &["rev-parse", "HEAD"]);

        // The caller's repository: hostile config, hooks and filter.
        let caller = root.join("caller");
        git(
            root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                caller.to_str().unwrap(),
            ],
        );
        let marks = root.join("marks");
        std::fs::create_dir(&marks).unwrap();
        let hostile = |name: &str| -> String {
            let path = root.join(format!("hostile-{name}"));
            std::fs::write(
                &path,
                format!("#!/bin/sh\necho ran > {}/{name}\nexit 1\n", marks.display()),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.display().to_string()
        };
        let hooks = root.join("hooks");
        std::fs::create_dir(&hooks).unwrap();
        for hook in [
            "reference-transaction",
            "post-checkout",
            "post-commit",
            "post-merge",
            "pre-auto-gc",
        ] {
            let path = hooks.join(hook);
            std::fs::write(
                &path,
                format!("#!/bin/sh\necho ran > {}/hook-{hook}\n", marks.display()),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git(
            &caller,
            &["config", "core.sshCommand", &hostile("sshCommand")],
        );
        git(
            &caller,
            &["config", "core.fsmonitor", &hostile("fsmonitor")],
        );
        git(
            &caller,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        git(
            &caller,
            &["config", "filter.evil.clean", &hostile("filter-clean")],
        );
        git(
            &caller,
            &["config", "filter.evil.smudge", &hostile("filter-smudge")],
        );
        git(&caller, &["config", "protocol.ext.allow", "always"]);
        std::fs::write(caller.join(".gitattributes"), "* filter=evil\n").unwrap();
        let ran = |name: &str| marks.join(name).exists();

        // Control: the same configuration does run when git itself is used
        // in the caller's tree, so the assertions below can fail.
        git(
            &caller,
            &["remote", "set-url", "origin", "ssh://127.0.0.1:1/nope"],
        );
        let control = Command::new("git")
            .current_dir(&caller)
            .args(["fetch", "origin"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(!control.status.success());
        assert!(ran("sshCommand"), "the fixture's core.sshCommand is live");
        let _ = Command::new("git")
            .current_dir(&caller)
            .arg("status")
            .output();
        assert!(ran("fsmonitor"), "the fixture's core.fsmonitor is live");
        std::fs::remove_dir_all(&marks).unwrap();
        std::fs::create_dir(&marks).unwrap();

        let jobs = root.join("jobs");
        std::fs::create_dir(&jobs).unwrap();
        let gate = materialize_trusted_gate(&jobs).unwrap();
        let script = gate.with_file_name("cloud-sweep.sh");
        let mirror = jobs.join("sweep-mirror.git");
        let bundle_step = |out: &Path| {
            Command::new(&script)
                .args(["bundle", &sha])
                .arg(out)
                .args(["--repo"])
                .arg(&caller)
                .args(["--mirror"])
                .arg(&mirror)
                .arg("--allow-local-root")
                .arg(root)
                .current_dir(&caller)
                .env("HOME", root)
                .env("GIT_SSH_COMMAND", hostile("env-ssh"))
                .output()
                .unwrap()
        };

        // A local origin: the exact tip is bundled from the daemon-owned mirror.
        git(
            &caller,
            &["remote", "set-url", "origin", origin.to_str().unwrap()],
        );
        let out = root.join("tip.bundle");
        let done = bundle_step(&out);
        assert!(done.status.success(), "{done:?}");
        let heads = git(root, &["bundle", "list-heads", out.to_str().unwrap()]);
        assert!(
            heads.contains(&format!("{sha} refs/remotes/origin/rolling")),
            "{heads}"
        );
        assert!(mirror.join("HEAD").is_file(), "the mirror is daemon-owned");

        // An ssh origin and an `ext::` origin never run the caller's programs
        // (they fail: no server, and `ext::` is refused).
        for url in [
            "ssh://127.0.0.1:1/nope".to_string(),
            format!("ext::sh -c 'echo ran > {}/ext-transport'", marks.display()),
        ] {
            git(&caller, &["remote", "set-url", "origin", &url]);
            let failed = bundle_step(&root.join("never.bundle"));
            assert!(!failed.status.success(), "{url}");
        }
        // A tip that moved on is refused with the script's stable message.
        std::fs::write(seed.join("b.txt"), "b\n").unwrap();
        git(&seed, &["add", "b.txt"]);
        git(&seed, &["commit", "-q", "-m", "next"]);
        git(
            &seed,
            &["push", "-q", origin.to_str().unwrap(), "rolling:rolling"],
        );
        git(
            &caller,
            &["remote", "set-url", "origin", origin.to_str().unwrap()],
        );
        let stale = bundle_step(&root.join("stale.bundle"));
        assert!(!stale.status.success());
        assert!(
            String::from_utf8_lossy(&stale.stderr)
                .contains("requested SHA is not the current rolling tip")
        );

        // Review round 3 (#1077 blocker): a planted mirror is never trusted.
        // Its config selects `remote.evil.uploadpack` and `core.sshCommand`; a
        // caller whose origin URL is the remote *name* `evil` (or a valid
        // origin behind a symlinked or wrongly-moded mirror) must not run them.
        let tip = git(&seed, &["rev-parse", "HEAD"]);
        let plant = |name: &str| -> PathBuf {
            let planted = root.join(format!("planted-{name}.git"));
            git(root, &["init", "-q", "--bare", planted.to_str().unwrap()]);
            for (key, value) in [
                ("remote.evil.url", origin.to_str().unwrap().to_string()),
                ("remote.evil.uploadpack", hostile("uploadpack")),
                ("remote.origin.uploadpack", hostile("origin-uploadpack")),
                ("core.sshCommand", hostile("mirror-sshCommand")),
                ("core.fsmonitor", hostile("mirror-fsmonitor")),
            ] {
                git(&planted, &["config", key, &value]);
            }
            planted
        };
        let bundle_tip = |mirror: &Path, out: &Path| {
            Command::new(&script)
                .args(["bundle", &tip])
                .arg(out)
                .args(["--repo"])
                .arg(&caller)
                .args(["--mirror"])
                .arg(mirror)
                .arg("--allow-local-root")
                .arg(root)
                .current_dir(&caller)
                .env("HOME", root)
                .output()
                .unwrap()
        };
        // (a) The remote named `evil` as the origin URL, over a symlinked mirror.
        git(&caller, &["remote", "set-url", "origin", "evil"]);
        let symlinked = jobs.join("symlinked-mirror.git");
        std::os::unix::fs::symlink(plant("symlink"), &symlinked).unwrap();
        let refused = bundle_tip(&symlinked, &root.join("evil.bundle"));
        assert!(!refused.status.success(), "{refused:?}");
        assert!(!root.join("evil.bundle").exists());
        // (b) A valid origin behind a symlinked mirror: the symlink is dropped
        // and the mirror is rebuilt from scratch, so nothing planted runs.
        git(
            &caller,
            &["remote", "set-url", "origin", origin.to_str().unwrap()],
        );
        let rebuilt = bundle_tip(&symlinked, &root.join("rebuilt.bundle"));
        assert!(rebuilt.status.success(), "{rebuilt:?}");
        let meta = std::fs::symlink_metadata(&symlinked).unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink());
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        assert!(
            !std::fs::read_to_string(symlinked.join("config"))
                .unwrap()
                .contains("uploadpack")
        );
        // (c) A real but wrongly-moded directory with a planted config, and one
        // with the right mode but a planted config, are rebuilt too.
        for (name, mode) in [("loose", 0o755), ("planted", 0o700)] {
            let dir = plant(name);
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            let done = bundle_tip(&dir, &root.join(format!("{name}.bundle")));
            assert!(done.status.success(), "{name}: {done:?}");
            assert!(
                !std::fs::read_to_string(dir.join("config"))
                    .unwrap()
                    .contains("uploadpack"),
                "{name}"
            );
        }
        // (d0) Ordinary origins are accepted (round 4: the https/ssh pattern
        // once demanded a literal `-`), the hostile ones refused.
        let check = |url: &str| {
            Command::new(&script)
                .args(["check-origin", url])
                .env("HOME", root)
                .status()
                .unwrap()
                .success()
        };
        for url in [
            "https://github.com/example/repo.git",
            "https://github.com/example/repo",
            "ssh://git@github.com/example/repo.git",
            "ssh://git@github.com:22/example/repo.git",
            "git@github.com:owner/repo.git",
        ] {
            assert!(check(url), "accepted: {url}");
        }
        for url in [
            "evil",
            "-oProxyCommand=x",
            "ext::sh -c true",
            "http://example.com/x.git",
            "ssh://-oProxyCommand=x/y",
            "https://-host/x",
            "git@github.com:-x",
            "/etc/passwd",
            "file:///etc",
        ] {
            assert!(!check(url), "refused: {url}");
        }
        // (d) Hostile origin URLs are refused before any fetch.
        for url in [
            "evil",
            "-oProxyCommand=x",
            "ext::sh -c true",
            "file:///etc",
            "http://example.com/x.git",
        ] {
            git(&caller, &["config", "remote.origin.url", url]);
            let done = bundle_tip(&jobs.join("url-mirror.git"), &root.join("url.bundle"));
            assert!(!done.status.success(), "{url}");
            assert!(
                String::from_utf8_lossy(&done.stderr).contains("refusing origin URL"),
                "{url}: {done:?}"
            );
        }

        let leaked: Vec<_> = std::fs::read_dir(&marks)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            leaked.is_empty(),
            "no program of the caller's configuration ran, but these did: {leaked:?}"
        );
    }

    /// Review round 3 (#1077 major): GREEN needs a structurally complete report,
    /// not a report that merely contains the SHA and a verdict. A stub with no
    /// lane table, a second Tip SHA header, a line after the verdict and fewer
    /// lanes than the sweep runs are all INCOMPLETE.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_green_needs_every_lane_row_one_tip_header_and_the_verdict_last() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let other = "fedcba9876543210fedcba9876543210fedcba98";
        let green = format!("VERDICT GREEN {sha} new=0");
        let log = format!("{green}\n");
        let good = sweep_qa(sha, "- no failing test names\n", &green);
        let (state, ok) = sweep_job_result(sha, Some(0), &log, Some(&good));
        assert_eq!(state, JobState::Succeeded, "the complete report is GREEN");
        assert_eq!(ok.sweep.unwrap().verdict, SweepVerdict::Green);

        let mut fewer = sweep_lane_labels();
        fewer.pop();
        let short_table = format!(
            "# Cloud QA sweep\n\nTip SHA: `{sha}`  \nLanes: {}  \n\n{}\n## Verdict\n\n{green}\n",
            fewer.len(),
            sweep_lane_table(&fewer)
        );
        let mut missing_fixed = sweep_lane_labels();
        missing_fixed.retain(|label| label != "rsi-common");
        missing_fixed.push("rsid-other-17".into());
        let swapped = good.replace(
            "rsid-other-05 | 10 | 0 | 1 | 0 |",
            "rsid-other-05 | 10 | 0 | 1 |",
        );
        let cases: Vec<(&str, String)> = vec![
            (
                "the stub report",
                format!("Tip SHA: `{sha}`\n## Verdict\n{green}\n"),
            ),
            (
                "a second Tip SHA header",
                good.replace("\nLanes:", &format!("\nTip SHA: `{sha}`\nLanes:")),
            ),
            (
                "a second Tip SHA header for another sha",
                good.replace("\nLanes:", &format!("\nTip SHA: `{other}`\nLanes:")),
            ),
            (
                "a line after the verdict",
                format!("{good}Tip SHA: `{sha}`\n"),
            ),
            ("trailing text after the verdict", format!("{good}done\n")),
            ("a lane count below the expected lanes", short_table),
            (
                "Lanes: header above the rows present",
                good.replacen("Lanes: 23", "Lanes: 99", 1),
            ),
            (
                "a fixed lane replaced by an extra shard",
                sweep_qa_with_labels(sha, &missing_fixed, &green),
            ),
            ("an incomplete lane row", swapped),
            (
                "an inline Tip SHA header",
                good.replace(
                    &format!("Tip SHA: `{sha}`"),
                    &format!("prefix Tip SHA: `{sha}`"),
                ),
            ),
        ];
        for (name, qa) in cases {
            let (state, result) = sweep_job_result(sha, Some(0), &log, Some(&qa));
            assert_eq!(state, JobState::Failed, "{name}");
            assert_eq!(
                result.sweep.unwrap().verdict,
                SweepVerdict::Incomplete,
                "{name}"
            );
            assert!(result.refusal.is_some(), "{name} names why");
        }
    }

    /// Review round 4 (#1077): GREEN names exactly the expected lanes, has one
    /// verdict section, lists no NEW entry, and is consistent with the lane
    /// exit codes the way the QA.md writer decides them; a report the real
    /// writer produces is still GREEN.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_green_mirrors_the_writers_rule_and_the_real_writers_report_is_green() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let green = format!("VERDICT GREEN {sha} new=0");
        let log = format!("{green}\n");
        let is_green = |qa: &str| {
            let (state, result) = sweep_job_result(sha, Some(0), &log, Some(qa));
            let verdict = result.sweep.unwrap().verdict;
            assert_eq!(state == JobState::Succeeded, verdict == SweepVerdict::Green);
            verdict == SweepVerdict::Green
        };
        let labels = sweep_lane_labels();
        // The lane list is the crate manifest's shards plus the fixed lanes.
        assert_eq!(labels.len(), 16 + SWEEP_FIXED_LANES.len());
        assert!(labels.contains(&"rsid-store-04".to_string()));

        // Positive: a report shaped like the real writer's for a sweep whose
        // one nonzero lane has only KNOWN reds (real lane table and layout).
        let mut table = sweep_lane_table(&labels);
        table = table.replace(
            "| rsid-bins | 10 | 0 | 1 | 0 |",
            "| rsid-bins | 152 | 1 | 2 | 100 |",
        );
        let real_shape = format!(
            "# Cloud QA sweep \u{2014} {sha}\n\nTip SHA: `{sha}`  \nStarted: 2026-09-30T11:39:41Z  \nFinished: 2026-09-30T12:18:59Z  \nWall: 39.3 min  \nLanes: {}  \nFailing names needing signature review: 1\n\n{table}\nSeed set: absent; classification unavailable\nUnclassified failing names (seed names alone do not verify failure signatures):\n- `tests::a_known_red`\nFLAKE lines:\n- none observed\n\n## Verdict\n\n- KNOWN #1079 `tests::a_known_red`\nKnown only: #1079\n\n{green}\n",
            labels.len()
        );
        assert!(
            is_green(&real_shape),
            "known reds under a nonzero lane exit"
        );
        // The same nonzero exit with no KNOWN entry is what the writer calls RED.
        assert!(!is_green(&real_shape.replace(
            "- KNOWN #1079 `tests::a_known_red`\nKnown only: #1079\n",
            "- no failing test names\n"
        )));

        // Negative: invented labels of the right count; an unknown extra lane;
        // a duplicated lane; two Verdict sections (the first lists a NEW); a NEW
        // entry under a GREEN line; a harness-failure or incomplete marker.
        let invented: Vec<String> = (0..labels.len())
            .map(|n| format!("rsid-invented-{n:02}"))
            .collect();
        let mut extra = labels.clone();
        extra.push("rsid-extra-99".into());
        let mut duplicated = labels.clone();
        duplicated[0] = duplicated[1].clone();
        let good = sweep_qa_with_labels(sha, &labels, &green);
        assert!(is_green(&good), "the plain complete report");
        let two_verdicts = good.replace(
            "## Verdict\n",
            "## Verdict\n\n- NEW `rsid late::new_failure`\n\nVERDICT RED {sha} new=1\n\n## Verdict\n",
        ).replace("{sha}", sha);
        let cases: Vec<(&str, String)> = vec![
            (
                "invented shard labels",
                sweep_qa_with_labels(sha, &invented, &green),
            ),
            (
                "an unknown extra lane",
                sweep_qa_with_labels(sha, &extra, &green),
            ),
            (
                "a duplicated lane",
                sweep_qa_with_labels(sha, &duplicated, &green),
            ),
            ("two Verdict sections", two_verdicts),
            (
                "a NEW entry under a GREEN line",
                good.replace("## Verdict\n", "## Verdict\n\n- NEW `rsid b::new`\n"),
            ),
            (
                "a harness failure",
                good.replace(
                    "## Verdict\n",
                    "## Verdict\n\n- `rsid-bins:build_or_harness_failure`\n",
                ),
            ),
            (
                "an Incomplete marker",
                good.replace("## Verdict\n", "## Verdict\n\nIncomplete: Lanes: 0\n"),
            ),
            (
                "a nonzero lane exit with no KNOWN entry",
                good.replace(
                    "| rsid-bins | 10 | 0 | 1 | 0 |",
                    "| rsid-bins | 10 | 1 | 1 | 100 |",
                ),
            ),
        ];
        for (name, qa) in cases {
            assert!(!is_green(&qa), "{name}");
        }
    }

    /// The report the cloud host's real writers produce (`cloud-sweep-report.py`
    /// then `cloud-sweep-verdict.py`) for a clean sweep is GREEN, and the same
    /// sweep with a lane that exited nonzero and no named failure is not.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_report_from_the_real_writers_is_green_when_clean() {
        use std::process::Command;
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let scripts = Path::new(rsi_common::manifest_dir!()).join("../../scripts");
        let write_report = |bad_lane: bool| -> String {
            let dir = tempfile::tempdir().unwrap();
            let result = dir.path().join(sha);
            for sub in ["status", "logs"] {
                std::fs::create_dir_all(result.join(sub)).unwrap();
            }
            for lane in sweep_expected_lanes() {
                let code = if bad_lane && lane == "rsi" { 101 } else { 0 };
                std::fs::write(
                    result.join(format!("status/{lane}.tsv")),
                    format!("{lane}\t{code}\n"),
                )
                .unwrap();
                std::fs::write(
                    result.join(format!("logs/{lane}.log")),
                    "test result: ok. 4 passed; 0 failed; 0 ignored\n",
                )
                .unwrap();
            }
            let report = Command::new("python3")
                .arg(scripts.join("cloud-sweep-report.py"))
                .args([sha, "2026-09-30T00:00:00Z", "2026-09-30T00:10:00Z"])
                .arg(&result)
                .arg(dir.path().join("no-seeds"))
                .output()
                .unwrap();
            assert!(report.status.success(), "{report:?}");
            std::fs::write(result.join("QA.md"), &report.stdout).unwrap();
            let verdict = Command::new("python3")
                .arg(scripts.join("cloud-sweep-verdict.py"))
                .arg(&result)
                .args(["--classifier", "/nonexistent-classifier"])
                .env("HOME", dir.path())
                .output()
                .unwrap();
            assert!(verdict.status.success(), "{verdict:?}");
            std::fs::read_to_string(result.join("QA.md")).unwrap()
        };
        let clean = write_report(false);
        let last = clean.trim_end().lines().last().unwrap().to_string();
        assert_eq!(last, format!("VERDICT GREEN {sha} new=0"), "{clean}");
        let (state, ok) = sweep_job_result(sha, Some(0), &format!("{last}\n"), Some(&clean));
        assert_eq!(state, JobState::Succeeded, "{clean}");
        assert_eq!(ok.sweep.unwrap().verdict, SweepVerdict::Green);
        // A lane that exited nonzero with no named failure: the writer says
        // INCOMPLETE, so the log's line is not GREEN either; and a GREEN line
        // forced over that report is refused.
        let broken = write_report(true);
        assert!(broken.contains("build_or_harness_failure"), "{broken}");
        let forced = broken
            .replace("VERDICT INCOMPLETE", "VERDICT GREEN")
            .replace("new=0", "new=0");
        let (state, result) = sweep_job_result(
            sha,
            Some(0),
            &format!("VERDICT GREEN {sha} new=0\n"),
            Some(&forced),
        );
        assert_eq!(state, JobState::Failed);
        assert_eq!(result.sweep.unwrap().verdict, SweepVerdict::Incomplete);
    }

    /// Review #1077: GREEN rests on a complete, readable, exact report for this
    /// SHA. A missing or unreadable report, an oversized one (whose later NEW
    /// failures would be cut off), a wrong SHA, a missing verdict section and a
    /// NEW count that disagrees with the line are INCOMPLETE, never GREEN.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_green_needs_a_complete_report_whose_new_count_agrees() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let other = "fedcba9876543210fedcba9876543210fedcba98";
        let green_line = format!("VERDICT GREEN {sha} new=0");
        let log = format!("{green_line}\n");
        let good = sweep_qa(sha, "- KNOWN #7 `rsid a::known`\n", &green_line);
        let (state, ok) = sweep_job_result(sha, Some(0), &log, Some(&good));
        assert_eq!(state, JobState::Succeeded);
        assert_eq!(ok.sweep.unwrap().verdict, SweepVerdict::Green);

        // A NEW failure past the 2 MiB read cap: the old reader cut it off and
        // reported GREEN. The report is now oversized, so it is INCOMPLETE.
        let padding = format!("- KNOWN #7 `{}`\n", "p".repeat(200));
        let mut hidden = sweep_qa(sha, &padding.repeat(11_000), &green_line);
        assert!(hidden.len() as u64 > 2 * MIB);
        hidden.push_str("- NEW `rsid late::new_failure`\n");
        let mut reports: Vec<(&str, Option<String>)> = vec![
            ("oversized", Some(hidden)),
            ("missing", None),
            (
                "another sha",
                Some(sweep_qa(other, "- KNOWN #7 `rsid a::known`\n", &green_line)),
            ),
            (
                "no verdict section",
                Some(format!(
                    "# Cloud QA sweep\n\nTip SHA: `{sha}`\n{green_line}\n"
                )),
            ),
            (
                "the report's own line says RED",
                Some(sweep_qa(sha, "", &format!("VERDICT RED {sha} new=1"))),
            ),
        ];
        // More NEW names than the bounded list holds: the count is still exact.
        let many: String = (0..SWEEP_MAX_FAILURES + 5)
            .map(|i| format!("- NEW `rsid many::n{i}`\n"))
            .collect();
        reports.push(("many NEW", Some(sweep_qa(sha, &many, &green_line))));
        for (name, qa) in reports {
            let (state, result) = sweep_job_result(sha, Some(0), &log, qa.as_deref());
            assert_eq!(state, JobState::Failed, "{name}");
            assert_eq!(
                result.sweep.unwrap().verdict,
                SweepVerdict::Incomplete,
                "{name}"
            );
            assert!(result.refusal.is_some(), "{name} names why");
        }

        // QA.md that cannot be read (a directory) is INCOMPLETE too.
        let results = tempfile::tempdir().unwrap();
        std::fs::create_dir(results.path().join("QA.md")).unwrap();
        let (state, unreadable) = classify_cloud_sweep(
            sha,
            Some(0),
            &log,
            AgentJobResultV1::default(),
            results.path(),
        );
        assert_eq!(state, JobState::Failed);
        assert_eq!(
            unreadable.refusal.as_deref(),
            Some("sweep_report_unreadable")
        );
        assert_eq!(unreadable.sweep.unwrap().verdict, SweepVerdict::Incomplete);

        // A RED whose count matches the report, even past the bounded list,
        // stays RED with the bounded names.
        let red_line = format!("VERDICT RED {sha} new={}", SWEEP_MAX_FAILURES + 5);
        let (state, red) = sweep_job_result(
            sha,
            Some(0),
            &format!("{red_line}\n"),
            Some(&sweep_qa(sha, &many, &red_line)),
        );
        assert_eq!(state, JobState::Failed);
        let sweep = red.sweep.unwrap();
        assert_eq!(sweep.verdict, SweepVerdict::Red);
        assert_eq!(sweep.new_failures.len(), SWEEP_MAX_FAILURES);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_sweep_the_spend_guard_refuses_ends_with_a_typed_refusal() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        for (log, code) in [
            (
                "cloud-spend: at or above the daily cap, refusing to start a remote run\n",
                "cloud_spend_refused",
            ),
            (
                "cloud-spend: at or above the stop line, refusing to start a remote run\n",
                "cloud_spend_refused",
            ),
            (
                "cloud-gate: another gate host is running\n",
                "cloud_gate_busy",
            ),
            (
                "requested SHA is not the current rolling tip\n",
                "sweep_sha_not_rolling_tip",
            ),
            ("something else\n", "sweep_verdict_missing"),
        ] {
            let (state, result) = sweep_job_result(sha, Some(4), log, None);
            assert_eq!(state, JobState::Failed);
            assert_eq!(result.refusal.as_deref(), Some(code), "{log}");
            assert_eq!(result.sweep.unwrap().verdict, SweepVerdict::Incomplete);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_finished_sweep_wakes_the_owner_once_with_the_typed_verdict() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let mut c = ctx(owner, dir.path(), None);
        c.params = params(JobKind::CloudSweep, serde_json::json!({"sha":sha}));
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            c,
            Utc::now(),
        )
        .unwrap();
        let launched = runtime.launched.lock().unwrap()[0].clone();
        assert!(launched.command.argv[0].starts_with(&dir.path().display().to_string()));
        assert!(launched.build_environment.is_none());

        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);
        std::fs::write(&row.status_path, "0\n").unwrap();
        std::fs::write(
            &row.job.log_path,
            format!("CLOUD SWEEP running\nVERDICT INCOMPLETE {sha} new=0\nspend line\n"),
        )
        .unwrap();
        *runtime.active.lock().unwrap() = false;
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 1);
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);

        let guard = store.lock().await;
        let wakes = wakes_for(&guard, owner);
        assert_eq!(wakes.len(), 1, "one completion wake");
        assert_eq!(wakes[0].wake_mode, rsi_common::types::WakeMode::Resume);
        assert!(wakes[0].message.contains("cloud_sweep job"));
        assert!(wakes[0].message.contains("verdict=INCOMPLETE"));
        assert!(wakes[0].message.contains(&format!("sha={sha}")));
        let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(
            job.result.unwrap().sweep.unwrap().verdict,
            SweepVerdict::Incomplete
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_sweep_whose_unit_vanished_is_lost_and_incomplete() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let mut c = ctx(owner, dir.path(), None);
        c.params = params(JobKind::CloudSweep, serde_json::json!({"sha":sha}));
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            c,
            Utc::now(),
        )
        .unwrap();
        *runtime.active.lock().unwrap() = false;
        let later = Utc::now() + chrono::Duration::seconds(LAUNCH_GRACE_SECS + 5);
        assert_eq!(poll_once(&store, &dynamic, later).await.unwrap(), 1);
        let guard = store.lock().await;
        let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(job.state, JobState::Lost);
        assert_eq!(
            job.result.unwrap().sweep.unwrap().verdict,
            SweepVerdict::Incomplete
        );
        assert_eq!(wakes_for(&guard, owner).len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_symlinked_trusted_gate_is_repaired_and_never_written_or_run_through() {
        use std::os::unix::fs::symlink;
        let jobs = tempfile::tempdir().unwrap();
        let sandbox = tempfile::tempdir().unwrap();
        // The agent plants trusted-gate as a link into its sandbox, with a
        // hostile script already in place there.
        std::fs::create_dir_all(sandbox.path().join("scripts")).unwrap();
        let hostile = sandbox.path().join("scripts/cloud-gate.sh");
        std::fs::write(&hostile, "#!/bin/sh\nevil\n").unwrap();
        symlink(sandbox.path(), jobs.path().join("trusted-gate")).unwrap();
        let script = materialize_trusted_gate(jobs.path()).unwrap();
        assert!(
            std::fs::canonicalize(&script)
                .unwrap()
                .starts_with(std::fs::canonicalize(jobs.path()).unwrap())
        );
        assert!(!script.starts_with(sandbox.path()));
        assert!(
            std::fs::symlink_metadata(jobs.path().join("trusted-gate"))
                .unwrap()
                .file_type()
                .is_dir()
        );
        // Nothing was written into the link target and its hostile script is untouched.
        assert_eq!(
            std::fs::read_to_string(&hostile).unwrap(),
            "#!/bin/sh\nevil\n"
        );
        assert!(!sandbox.path().join("infra").exists());
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            include_str!("../../../scripts/cloud-gate.sh")
        );
        // A symlinked sub-directory and a symlinked script file are repaired too.
        std::fs::remove_dir_all(jobs.path().join("trusted-gate/infra")).unwrap();
        symlink(sandbox.path(), jobs.path().join("trusted-gate/infra")).unwrap();
        std::fs::remove_file(&script).unwrap();
        symlink(&hostile, &script).unwrap();
        let script = materialize_trusted_gate(jobs.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            include_str!("../../../scripts/cloud-gate.sh")
        );
        assert!(!sandbox.path().join("aws").exists());
        assert_eq!(
            std::fs::read_to_string(&hostile).unwrap(),
            "#!/bin/sh\nevil\n"
        );
        // A symlinked jobs directory is refused outright.
        let linked = tempfile::tempdir().unwrap();
        let link = linked.path().join("jobs");
        symlink(sandbox.path(), &link).unwrap();
        assert!(materialize_trusted_gate(&link).is_err());
        assert!(!sandbox.path().join("trusted-gate").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn materializing_the_trusted_gate_repairs_a_tampered_copy() {
        let jobs = tempfile::tempdir().unwrap();
        let script = materialize_trusted_gate(jobs.path()).unwrap();
        std::fs::write(&script, "#!/bin/sh\nevil\n").unwrap();
        let again = materialize_trusted_gate(jobs.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(again).unwrap(),
            include_str!("../../../scripts/cloud-gate.sh")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_unit_is_a_user_service_with_the_fixed_wrapper_and_no_agent_shell() {
        let spec = LaunchSpec {
            unit_name: "rsi-job-1".into(),
            cwd: PathBuf::from("/w/sandbox"),
            log_path: PathBuf::from("/j/1.log"),
            status_path: PathBuf::from("/j/1.status"),
            command: JobCommand {
                argv: vec!["cargo".into(), "check; rm -rf /".into()],
                runtime_max_secs: 60,
                stop_timeout_secs: 900,
                log_max_bytes: 1000,
                memory_max_gib: 3,
                cpu_quota_percent: 250,
                background: false,
                admission_max_secs: None,
            },
            build_environment: None,
        };
        let args = systemd_run_args(&spec, Some("/usr/bin"));
        // #2119: a unit with no build environment holds no cargo build.
        assert!(!args.iter().any(|arg| arg.starts_with("--slice")));
        assert_eq!(args[0], "--user");
        assert!(args.contains(&"--unit=rsi-job-1".to_string()));
        // #2127: nested units name their job through this variable.
        assert!(args.contains(&"--setenv=RSI_JOB_ID=1".to_string()));
        // Only background work changes the default CPU weight (#2112).
        assert!(args.iter().all(|a| !a.starts_with("--property=CPUWeight=")));
        assert!(args.contains(&"--working-directory=/w/sandbox".to_string()));
        assert!(args.contains(&"--property=TimeoutStopSec=900".to_string()));
        assert!(args.contains(&"--property=MemoryMax=3G".to_string()));
        assert!(args.contains(&"--property=CPUQuota=250%".to_string()));
        let separator = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            args[separator + 1..separator + 4],
            ["/bin/sh", "-c", WRAPPER]
        );
        // Agent-derived words are trailing positional arguments, never part of
        // the shell text.
        assert_eq!(
            args[separator + 4..],
            [
                "rsi-job",
                "/j/1.log",
                "/j/1.status",
                "1000",
                "cargo",
                "check; rm -rf /"
            ]
        );
        assert!(!WRAPPER.contains("rm -rf"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_oversized_log_is_capped_and_fails_visibly_even_when_the_job_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let (log, status) = (dir.path().join("j.log"), dir.path().join("j.status"));
        let out = Command::new("/bin/sh")
            .args(["-c", WRAPPER, "rsi-job"])
            .arg(&log)
            .arg(&status)
            .arg("100")
            .args(["sh", "-c", "yes | head -c 100000; exit 0"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(read_status(status.to_str().unwrap()), Some(153));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.len() < 300, "log stayed bounded: {}", text.len());
        assert!(text.contains("log reached the 100-byte cap"));
        // Under the cap a zero exit stays zero and the log is untouched. Each
        // job has a fresh log (the wrapper appends), so start from none.
        std::fs::remove_file(&log).unwrap();
        let out = Command::new("/bin/sh")
            .args(["-c", WRAPPER, "rsi-job"])
            .arg(&log)
            .arg(&status)
            .arg("100")
            .args(["sh", "-c", "echo fine"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(read_status(status.to_str().unwrap()), Some(0));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.starts_with("rsi-phase "), "{text}");
        assert!(text.contains(" job-start\n"), "{text}");
        assert_eq!(text.lines().last(), Some("fine"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn every_kind_gets_bounded_log_memory_and_cpu() {
        let cwd = Path::new("/w/sandbox");
        let jobs = tempfile::tempdir().unwrap();
        let gate = materialize_trusted_gate(jobs.path()).unwrap();
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let cases = [
            params(
                JobKind::Test,
                serde_json::json!({"package":"rsid","filters":["a"]}),
            ),
            params(
                JobKind::Build,
                serde_json::json!({"command":"check","workspace":true}),
            ),
            params(JobKind::Landing, serde_json::json!({"accepted":oid})),
        ];
        for case in &cases {
            let c = job_command(&tools(), case, cwd, None).unwrap();
            assert_eq!((c.log_max_bytes, c.memory_max_gib), (64 * MIB, 24));
            assert!(c.cpu_quota_percent >= 100);
        }
        let cloud = params(JobKind::CloudGate, serde_json::json!({"accepted":oid}));
        let c = job_command(&tools(), &cloud, cwd, Some(&gate)).unwrap();
        assert_eq!(
            (c.log_max_bytes, c.memory_max_gib, c.cpu_quota_percent),
            (32 * MIB, 2, 100)
        );
        let sweep = params(JobKind::CloudSweep, serde_json::json!({"sha":oid}));
        let c = job_command(&tools(), &sweep, cwd, Some(&gate)).unwrap();
        assert_eq!(c.log_max_bytes, 32 * MIB);
        assert!(c.memory_max_gib >= 2 && c.cpu_quota_percent >= 100);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_wrapper_records_the_exit_status_and_log_and_keeps_arguments_inert() {
        let dir = tempfile::tempdir().unwrap();
        let (log, status) = (dir.path().join("j.log"), dir.path().join("j.status"));
        // #1608: the daemon's pre-launch phase line survives the wrapper.
        std::fs::write(&log, "rsi-phase 1 unit-launch: requesting service x\n").unwrap();
        let out = Command::new("/bin/sh")
            .args(["-c", WRAPPER, "rsi-job"])
            .arg(&log)
            .arg(&status)
            .arg("1000")
            .args(["sh", "-c", "echo hello; exit 7"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(read_status(status.to_str().unwrap()), Some(7));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.starts_with("rsi-phase 1 unit-launch:"), "{text}");
        assert!(text.contains(" job-start\n"), "{text}");
        assert_eq!(text.lines().last(), Some("hello"));
        // A hostile argument is data, not shell text.
        let canary = dir.path().join("pwned");
        let hostile = format!("x; touch {}", canary.display());
        Command::new("/bin/sh")
            .args(["-c", WRAPPER, "rsi-job"])
            .arg(&log)
            .arg(&status)
            .arg("1000")
            .args(["echo", &hostile])
            .output()
            .unwrap();
        assert!(!canary.exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_empty_log_timeout_says_no_phase_was_logged() {
        let summary = phase_summary(&[], Utc::now());
        assert!(summary.contains("no phase was logged"), "{summary}");
    }

    /// #1591: a contended artifact lock is logged before the unit blocks, and
    /// the held line says how long it waited; the job then runs normally.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_contended_artifact_lock_wait_is_logged_with_its_duration() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");
        std::fs::write(&lock, "").unwrap();
        let mut holder = Command::new("flock")
            .args(["--exclusive", lock.to_str().unwrap(), "sleep", "2"])
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let out = Command::new("/bin/sh")
            .args(["-c", LOCK_WAIT, "rsi-lock"])
            .arg(&lock)
            .args(["echo", "ran"])
            .output()
            .unwrap();
        holder.wait().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{text}");
        assert!(text.contains(" artifact-lock-wait: "), "{text}");
        assert!(text.contains(" artifact-lock-held: waited "), "{text}");
        assert_eq!(text.lines().last(), Some("ran"));
        let phases: Vec<LogPhase> = text.lines().filter_map(parse_phase).collect();
        assert_eq!(phases.len(), 2, "{text}");
        assert!(phases[1].at >= phases[0].at);
        // Uncontended: only the held line, waited 0s.
        let out = Command::new("/bin/sh")
            .args(["-c", LOCK_WAIT, "rsi-lock"])
            .arg(&lock)
            .args(["echo", "ran"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(!text.contains("artifact-lock-wait"), "{text}");
        assert!(text.contains("waited 0s"), "{text}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn sweep_removes_scratch_of_terminal_jobs_left_by_a_crash_but_never_running_jobs() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let submit_one = |key: &str| {
            let store = store.clone();
            let runtime = runtime.clone();
            let cwd = dir.path().to_path_buf();
            let key = key.to_string();
            async move {
                let guard = store.lock().await;
                submit(
                    &guard,
                    &*runtime,
                    &tools(),
                    &cwd,
                    ctx(Uuid::new_v4(), &cwd, Some(&key)),
                    Utc::now(),
                )
                .unwrap()
                .0
            }
        };
        let running = submit_one("running").await;
        let crashed = submit_one("crashed").await;
        let running_scratch = job_tmp_dir(Path::new(&running.job.log_path));
        let crashed_scratch = job_tmp_dir(Path::new(&crashed.job.log_path));
        std::fs::write(crashed_scratch.join("artifact"), "leaked").unwrap();
        let orphan = dir.path().join(format!("{}.tmp", Uuid::new_v4()));
        std::fs::create_dir(&orphan).unwrap();

        // Crash between settlement and scratch deletion: the row is terminal,
        // the scratch is still there.
        store
            .lock()
            .await
            .settle_agent_job(
                crashed.job.id,
                JobState::Failed,
                &AgentJobResultV1::default(),
                false,
                Utc::now(),
            )
            .unwrap();
        assert!(crashed_scratch.exists());

        assert_eq!(sweep_terminal_scratch(&store, dir.path()).await.unwrap(), 1);
        assert!(!crashed_scratch.exists(), "terminal job scratch is removed");
        assert!(running_scratch.exists(), "a running job's scratch is kept");
        assert!(orphan.exists(), "a directory with no job row is left alone");
        assert_eq!(sweep_terminal_scratch(&store, dir.path()).await.unwrap(), 0);
        drop(dynamic);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn oversized_scratch_fails_the_running_job_stops_its_unit_and_removes_the_scratch() {
        struct Stopping {
            inner: FakeRuntime,
            stopped: Mutex<Vec<String>>,
        }
        impl JobRuntime for Stopping {
            fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
                self.inner.launch(spec)
            }
            fn unit_active(&self, unit: &str) -> bool {
                self.inner.unit_active(unit)
            }
            fn stop_unit(&self, unit: &str) -> std::result::Result<(), String> {
                self.stopped.lock().unwrap().push(unit.to_string());
                *self.inner.active.lock().unwrap() = false;
                Ok(())
            }
        }
        let (store, dir) = open();
        let runtime = Arc::new(Stopping {
            inner: FakeRuntime::default(),
            stopped: Mutex::new(Vec::new()),
        });
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            ctx(owner, dir.path(), None),
            Utc::now(),
        )
        .unwrap();
        let scratch = job_tmp_dir(Path::new(&row.job.log_path));
        std::fs::create_dir(scratch.join("nested")).unwrap();
        std::fs::write(scratch.join("nested/blob"), vec![7u8; 256 * 1024]).unwrap();

        // Within the quota: still running, scratch kept, unit untouched.
        assert_eq!(
            poll_with_scratch_quota(&store, &dynamic, Utc::now(), 8 * 1024 * 1024)
                .await
                .unwrap(),
            0
        );
        assert!(scratch.exists() && runtime.stopped.lock().unwrap().is_empty());

        // Above the quota: stopped, failed with the typed refusal, scratch gone.
        assert_eq!(
            poll_with_scratch_quota(&store, &dynamic, Utc::now(), 64 * 1024)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            *runtime.stopped.lock().unwrap(),
            [row.job.unit_name.clone()]
        );
        assert!(!scratch.exists(), "over-quota scratch is removed");
        let settled = store
            .lock()
            .await
            .get_agent_job(row.job.id)
            .unwrap()
            .unwrap();
        assert_eq!(settled.job.state, JobState::Failed);
        assert_eq!(
            settled.job.result.unwrap().refusal.as_deref(),
            Some(JOB_SCRATCH_QUOTA_EXCEEDED)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    async fn assert_quota_stop_preserves_custody_until_retry(fail_stop: bool, kind: JobKind) {
        struct Stubborn {
            inner: FakeRuntime,
            uncertain: Mutex<bool>,
            fail_stop: bool,
        }
        impl JobRuntime for Stubborn {
            fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
                self.inner.launch(spec)
            }
            fn unit_active(&self, unit: &str) -> bool {
                self.inner.unit_active(unit)
            }
            fn stop_unit(&self, unit: &str) -> std::result::Result<(), String> {
                if *self.uncertain.lock().unwrap() {
                    if self.fail_stop {
                        return Err("service controller stop failed".into());
                    }
                    return Ok(());
                }
                self.inner.stop_unit(unit)
            }
        }
        let (store, dir) = open();
        let runtime = Arc::new(Stubborn {
            inner: FakeRuntime::default(),
            uncertain: Mutex::new(true),
            fail_stop,
        });
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let mut context = ctx(owner, dir.path(), None);
        if kind == JobKind::Test {
            context.params = params(kind, serde_json::json!({"package":"rsid","filters":[]}));
        }
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            context,
            Utc::now(),
        )
        .unwrap();
        let scratch = job_tmp_dir(Path::new(&row.job.log_path));
        let artifact = scratch.join("blob");
        let contents = vec![7u8; 256 * 1024];
        std::fs::write(&artifact, &contents).unwrap();

        // An error or a successful stop that leaves the service active must
        // keep the running row, artifacts and wake queue unchanged on retry.
        for _ in 0..2 {
            assert_eq!(
                poll_with_scratch_quota(&store, &dynamic, Utc::now(), 64 * 1024)
                    .await
                    .unwrap(),
                0
            );
            let guard = store.lock().await;
            assert_eq!(
                guard.get_agent_job(row.job.id).unwrap().unwrap().job,
                row.job
            );
            assert_eq!(guard.list_running_agent_jobs().unwrap().len(), 1);
            assert!(wakes_for(&guard, owner).is_empty());
            assert_eq!(std::fs::read(&artifact).unwrap(), contents);
            assert!(runtime.unit_active(&row.job.unit_name));
        }

        // Reconciliation retries the stop, then settles and wakes once.
        *runtime.uncertain.lock().unwrap() = false;
        assert_eq!(
            poll_with_scratch_quota(&store, &dynamic, Utc::now(), 64 * 1024)
                .await
                .unwrap(),
            1
        );
        assert!(!runtime.unit_active(&row.job.unit_name));
        assert!(!scratch.exists());
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);
        let guard = store.lock().await;
        let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(
            job.result.unwrap().refusal.as_deref(),
            Some(JOB_SCRATCH_QUOTA_EXCEEDED)
        );
        assert_eq!(wakes_for(&guard, owner).len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn oversized_scratch_failed_stop_preserves_custody_until_successful_retry() {
        for kind in [JobKind::Build, JobKind::Test] {
            assert_quota_stop_preserves_custody_until_retry(true, kind).await;
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn oversized_scratch_lingering_service_preserves_custody_until_successful_retry() {
        for kind in [JobKind::Build, JobKind::Test] {
            assert_quota_stop_preserves_custody_until_retry(false, kind).await;
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn completion_wakes_the_owner_exactly_once_across_repeated_polls() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let (row, replayed) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            ctx(owner, dir.path(), None),
            Utc::now(),
        )
        .unwrap();
        assert!(!replayed);
        assert_eq!(runtime.launched.lock().unwrap().len(), 1);

        let scratch = job_tmp_dir(Path::new(&row.job.log_path));
        std::fs::write(scratch.join("test-artifact"), "temporary").unwrap();

        // Still running: no settle, no wake.
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);
        assert!(wakes_for(&*store.lock().await, owner).is_empty());
        assert!(scratch.exists());

        // The unit finishes.
        std::fs::write(&row.status_path, "0\n").unwrap();
        std::fs::write(&row.job.log_path, "Finished dev profile\n").unwrap();
        *runtime.active.lock().unwrap() = false;
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 1);
        assert!(!scratch.exists());
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);

        let guard = store.lock().await;
        let wakes = wakes_for(&guard, owner);
        assert_eq!(wakes.len(), 1, "one wake per job");
        assert_eq!(wakes[0].wake_mode, rsi_common::types::WakeMode::Resume);
        assert!(wakes[0].message.contains("succeeded"));
        assert!(wakes[0].message.contains(&row.job.log_path));
        let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(job.state, JobState::Succeeded);
        assert_eq!(job.exit_code, Some(0));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_wake_none_job_settles_silently_and_stays_readable() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let mut silent = ctx(owner, dir.path(), Some("silent"));
        silent.wake = JobWake::None;
        let (quiet, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            silent,
            Utc::now(),
        )
        .unwrap();
        let (loud, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            ctx(owner, dir.path(), Some("loud")),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(quiet.job.wake, JobWake::None);
        assert_eq!(loud.job.wake, JobWake::Owner);

        for row in [&quiet, &loud] {
            std::fs::write(&row.status_path, "3\n").unwrap();
            std::fs::write(&row.job.log_path, "error: boom\n").unwrap();
        }
        *runtime.active.lock().unwrap() = false;
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 1);
        let drain = crate::deploy_drain::DeployDrain::new();
        assert_eq!(
            start_queued_jobs(&store, &dynamic, &tools(), &drain)
                .await
                .unwrap(),
            1
        );
        *runtime.active.lock().unwrap() = false;
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 1);
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);

        let guard = store.lock().await;
        let wakes = wakes_for(&guard, owner);
        assert_eq!(wakes.len(), 1, "only the owner-wake job wakes");
        assert!(wakes[0].message.contains(&loud.job.id.to_string()));
        // The silent job still settled, and its result is readable.
        let read = guard.get_agent_job(quiet.job.id).unwrap().unwrap().job;
        assert_eq!(read.state, JobState::Failed);
        assert_eq!(read.exit_code, Some(3));
        assert_eq!(read.wake, JobWake::None);
        let listed = guard.list_agent_jobs(owner, 10).unwrap();
        assert!(listed.iter().any(|row| row.job.id == quiet.job.id
            && row.job.state == JobState::Failed
            && row.job.wake == JobWake::None));
        // The same key with a different wake policy is a conflicting replay.
        let mut changed = ctx(owner, dir.path(), Some("silent"));
        changed.wake = JobWake::Owner;
        assert!(submit(&guard, &*runtime, &tools(), dir.path(), changed, Utc::now()).is_err());
    }

    fn build_in(repo: &Path, key: &str, extra: serde_json::Value) -> SubmitContext {
        let mut params = serde_json::json!({"command":"check","workspace":true});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let mut request = ctx(Uuid::new_v4(), repo, Some(key));
        request.params = self::params(JobKind::Build, params);
        request
    }

    fn repo_git(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Submit a build in `repo`, let the unit finish with `exit`, and settle it.
    async fn finish_build(
        repo: &Path,
        jobs: &Path,
        mutate: impl FnOnce(&Path),
        extra: serde_json::Value,
    ) -> AgentJobV1 {
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            jobs,
            build_in(repo, "b", extra),
            Utc::now(),
        )
        .unwrap();
        mutate(repo);
        std::fs::write(&row.status_path, "0\n").unwrap();
        *runtime.active.lock().unwrap() = false;
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 1);
        store
            .lock()
            .await
            .get_agent_job(row.job.id)
            .unwrap()
            .unwrap()
            .job
    }

    /// Hub #2954: an unchanged worktree records its HEAD in the receipt.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_build_job_receipt_carries_the_unchanged_source_head() {
        let (repo, _first, second) = source::tests::repo();
        let jobs = disk_fixture();
        let job = finish_build(repo.path(), jobs.path(), |_| {}, serde_json::json!({})).await;
        assert_eq!(job.state, JobState::Succeeded);
        let result = job.result.unwrap();
        assert_eq!(result.source_head.as_deref(), Some(second.as_str()));
        assert_eq!(result.source_dirty, Some(false));
        assert_eq!(result.refusal, None);
    }

    /// Hub #2954: HEAD moving between submit and finish fails the job and
    /// names both SHAs; the build's own exit code stays.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_build_whose_head_moved_before_finish_fails_job_source_moved() {
        let (repo, _first, second) = source::tests::repo();
        let jobs = disk_fixture();
        let mut moved_to = String::new();
        let job = finish_build(
            repo.path(),
            jobs.path(),
            |repo| {
                repo_git(repo, &["commit", "-q", "--allow-empty", "-m", "three"]);
                moved_to = repo_git(repo, &["rev-parse", "HEAD"]);
            },
            serde_json::json!({}),
        )
        .await;
        assert_eq!(job.state, JobState::Failed);
        let result = job.result.unwrap();
        assert_eq!(result.refusal.as_deref(), Some("job_source_moved"));
        let detail = result.detail.unwrap();
        assert!(
            detail.contains(&second) && detail.contains(&moved_to),
            "{detail}"
        );
        assert_eq!(result.source_head.as_deref(), Some(moved_to.as_str()));
        assert_eq!(result.exit_code, Some(0), "the build's own exit code stays");
    }

    /// Hub #2954: a clean tree that became dirty fails; a tree dirty from the
    /// start is built as is.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_build_whose_tree_became_dirty_fails_but_a_dirty_start_is_built_as_is() {
        let (repo, _first, second) = source::tests::repo();
        let jobs = disk_fixture();
        let job = finish_build(
            repo.path(),
            jobs.path(),
            |repo| std::fs::write(repo.join("a.txt"), "edited mid-build").unwrap(),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(job.state, JobState::Failed);
        let result = job.result.unwrap();
        assert_eq!(result.refusal.as_deref(), Some("job_source_moved"));
        assert!(result.detail.unwrap().contains(&second));
        assert_eq!(result.source_dirty, Some(true));

        // Dirty at submit: the same edit is not a transition.
        let jobs = disk_fixture();
        let job = finish_build(repo.path(), jobs.path(), |_| {}, serde_json::json!({})).await;
        assert_eq!(job.state, JobState::Succeeded);
        let result = job.result.unwrap();
        assert_eq!(result.source_head.as_deref(), Some(second.as_str()));
        assert_eq!(result.source_dirty, Some(true));
    }

    /// Hub #2954: the poll catches a move while the unit still runs (slot wait
    /// or compile), stops the unit and settles the job.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_running_build_whose_head_moves_is_stopped_and_settled() {
        let (repo, _first, second) = source::tests::repo();
        let jobs = disk_fixture();
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            jobs.path(),
            build_in(repo.path(), "r", serde_json::json!({})),
            Utc::now(),
        )
        .unwrap();
        append_phase(
            &row.job.log_path,
            Utc::now(),
            "slot-wait: queued (build position 2): busy",
        );
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);
        repo_git(
            repo.path(),
            &["commit", "-q", "--allow-empty", "-m", "three"],
        );
        let moved = repo_git(repo.path(), &["rev-parse", "HEAD"]);
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 1);
        assert_eq!(
            runtime.stopped.lock().unwrap().len(),
            1,
            "the unit was stopped"
        );
        let job = store
            .lock()
            .await
            .get_agent_job(row.job.id)
            .unwrap()
            .unwrap()
            .job;
        assert_eq!(job.state, JobState::Failed);
        let result = job.result.unwrap();
        assert_eq!(result.refusal.as_deref(), Some("job_source_moved"));
        let detail = result.detail.unwrap();
        assert!(
            detail.contains(&second) && detail.contains(&moved),
            "{detail}"
        );
    }

    /// Hub #2954: a held job whose worktree moved while it waited fails at its
    /// start instead of launching.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_held_build_whose_head_moved_fails_at_start_without_launching() {
        let (repo, _first, second) = source::tests::repo();
        let jobs = disk_fixture();
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let (row, _) = submit_with_hold(
            &*store.lock().await,
            &*runtime,
            &tools(),
            jobs.path(),
            build_in(repo.path(), "h", serde_json::json!({})),
            true,
            Utc::now(),
        )
        .unwrap();
        repo_git(
            repo.path(),
            &["commit", "-q", "--allow-empty", "-m", "three"],
        );
        let drain = crate::deploy_drain::DeployDrain::new();
        assert_eq!(
            start_queued_jobs(&store, &dynamic, &tools(), &drain)
                .await
                .unwrap(),
            0
        );
        assert!(runtime.launched.lock().unwrap().is_empty());
        let job = store
            .lock()
            .await
            .get_agent_job(row.job.id)
            .unwrap()
            .unwrap()
            .job;
        assert_eq!(job.state, JobState::Failed);
        let result = job.result.unwrap();
        assert_eq!(result.refusal.as_deref(), Some("job_source_moved"));
        assert!(result.detail.unwrap().contains(&second));
    }

    /// Hub #2954: `params.sha` pins the expected HEAD and is refused up front.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_build_sha_pin_that_is_not_the_worktree_head_is_refused_at_submit() {
        let (repo, first, second) = source::tests::repo();
        let jobs = disk_fixture();
        let (store, _) = open();
        let runtime = FakeRuntime::default();
        let refused = submit(
            &*store.blocking_lock(),
            &runtime,
            &tools(),
            jobs.path(),
            build_in(repo.path(), "p1", serde_json::json!({"sha": first})),
            Utc::now(),
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains("job_source_sha_mismatch"), "{refused}");
        assert!(
            refused.contains(&first) && refused.contains(&second),
            "{refused}"
        );
        assert!(
            runtime.launched.lock().unwrap().is_empty(),
            "nothing launched"
        );
        assert!(
            store
                .blocking_lock()
                .list_running_agent_jobs()
                .unwrap()
                .is_empty(),
            "no row recorded"
        );
        let (row, _) = submit(
            &*store.blocking_lock(),
            &runtime,
            &tools(),
            jobs.path(),
            build_in(repo.path(), "p2", serde_json::json!({"sha": second})),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(row.job.state, JobState::Running);
    }

    /// Hub #3025: a build whose `<id>.source` record cannot be written is
    /// refused with a typed error before any row exists or unit launches.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_build_whose_source_record_cannot_be_written_is_refused_at_submit() {
        let (repo, _first, _second) = source::tests::repo();
        let jobs = disk_fixture();
        let (store, _) = open();
        let runtime = FakeRuntime::default();
        // A jobs directory that does not exist makes the sidecar write fail.
        let missing = jobs.path().join("not-a-directory");
        let refused = submit(
            &*store.blocking_lock(),
            &runtime,
            &tools(),
            &missing,
            build_in(repo.path(), "r1", serde_json::json!({})),
            Utc::now(),
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains("job_source_record_failed"), "{refused}");
        assert!(refused.contains(".source"), "{refused}");
        assert!(
            runtime.launched.lock().unwrap().is_empty(),
            "nothing launched"
        );
        assert!(
            store
                .blocking_lock()
                .list_running_agent_jobs()
                .unwrap()
                .is_empty(),
            "no row recorded"
        );
    }

    /// Hub #3025: an idempotent replay leaves no orphan record of its own.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_replayed_build_submit_leaves_no_orphan_source_record() {
        let (repo, _first, _second) = source::tests::repo();
        let jobs = disk_fixture();
        let (store, _) = open();
        let runtime = FakeRuntime::default();
        let owner = Uuid::new_v4();
        let request = |key: &str| {
            let mut request = build_in(repo.path(), key, serde_json::json!({}));
            request.owner = owner;
            request
        };
        let (_row, replayed) = submit(
            &*store.blocking_lock(),
            &runtime,
            &tools(),
            jobs.path(),
            request("same"),
            Utc::now(),
        )
        .unwrap();
        assert!(!replayed);
        let (_row, replayed) = submit(
            &*store.blocking_lock(),
            &runtime,
            &tools(),
            jobs.path(),
            request("same"),
            Utc::now(),
        )
        .unwrap();
        assert!(replayed);
        let records = std::fs::read_dir(jobs.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "source"))
            .count();
        assert_eq!(records, 1, "only the accepted job has a record");
    }

    /// Hub #3025: a build with no submit-time record is never stamped with the
    /// finish-time HEAD, even when the worktree moved during the job.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_build_with_no_source_record_is_not_stamped_with_the_finish_head() {
        let (repo, _first, _second) = source::tests::repo();
        let jobs = disk_fixture();
        let job = finish_build(
            repo.path(),
            jobs.path(),
            |repo| {
                for entry in std::fs::read_dir(jobs.path()).unwrap().flatten() {
                    if entry.path().extension().is_some_and(|ext| ext == "source") {
                        std::fs::remove_file(entry.path()).unwrap();
                    }
                }
                repo_git(repo, &["commit", "-q", "--allow-empty", "-m", "three"]);
            },
            serde_json::json!({}),
        )
        .await;
        let result = job.result.unwrap();
        assert_eq!(result.source_head, None, "no submit-time head, no stamp");
        assert_eq!(result.source_dirty, None);
    }

    /// #2172: a nextest failure is named once, `binary test`, without the
    /// progress counter and without the bare echo of its captured libtest
    /// output; a libtest-only failure keeps its bare name.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn nextest_failures_are_named_once_without_the_progress_counter() {
        let log = "\
        PASS [   0.4s] ( 58/102) rsid a::tests::fine
        FAIL [   0.2s] ( 59/102) rsid session::tests::red
  stdout ───

    running 1 test
    test session::tests::red ... FAILED

    failures:

    failures:
        session::tests::red

  stderr ───
        FAIL [   0.4s] (5774/8220) rsid::bin/rsid tests::timed
    test tests::timed ... FAILED
        FAIL [   0.1s] ( 70/93 ) rsid-store store::tests::late
test libtest::only ... FAILED
        FAIL [   0.1s] (not a counter) rsid odd::name
  Cancelling due to test failure
        FAIL [   0.2s] ( 59/102) rsid session::tests::red
";
        assert_eq!(
            failing_test_report(log).names,
            [
                "(not a counter) rsid odd::name",
                "libtest::only",
                "rsid session::tests::red",
                "rsid-store store::tests::late",
                "rsid::bin/rsid tests::timed",
            ]
        );
        assert_eq!(
            failing_test_report("        PASS [   0.4s] ( 1/2) rsid a::b\n"),
            FailingTests::default()
        );
    }

    /// #185: a nextest run's live and final status sections name each timeout
    /// once beside the failures, one terminal outcome per exact binary+test
    /// (the latest status wins), the same test path in two binaries stays two
    /// names, and the omitted count agrees with nextest's own summary.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn nextest_timeouts_are_named_once_per_binary_beside_the_failures() {
        let live_and_final = "\
        FAIL [   1.7s] ( 3176/8223) rsid session::tests::red
        FAIL [  10.1s] ( 5776/8223) rsid::bin/rsid tests::flip
        FAIL [   0.4s] ( 5777/8223) rsid-store tests::same_name
     TIMEOUT [ 240.1s] ( 8103/8223) rsid-store store::tests::slow
     TIMEOUT [ 240.0s] ( 8104/8223) rsid::bin/rsid tests::same_name
     TIMEOUT [ 240.0s] ( 8105/8223) rsid::bin/rsid tests::flip
    test session::tests::red ... FAILED
test libtest::only ... FAILED
────────────
     Summary [3715.8s] 8223 tests run: 8201 passed (95 slow, 1 leaky), 2 failed, 3 timed out, 22 skipped
        FAIL [   1.7s] ( 3176/8223) rsid session::tests::red
        FAIL [   0.4s] ( 5777/8223) rsid-store tests::same_name
     TIMEOUT [ 240.1s] ( 8103/8223) rsid-store store::tests::slow
     TIMEOUT [ 240.0s] ( 8104/8223) rsid::bin/rsid tests::same_name
     TIMEOUT [ 240.0s] ( 8105/8223) rsid::bin/rsid tests::flip
error: test run failed
";
        let report = failing_test_report(live_and_final);
        assert_eq!(
            report.names,
            [
                "TIMEOUT rsid-store store::tests::slow",
                "TIMEOUT rsid::bin/rsid tests::flip",
                "TIMEOUT rsid::bin/rsid tests::same_name",
                "libtest::only",
                "rsid session::tests::red",
                "rsid-store tests::same_name",
            ],
            "tests::flip is one timeout (FAIL then TIMEOUT), not a failure and a timeout"
        );
        assert_eq!(report.omitted, 0, "the summary's 5 are all named");

        // A summary that counts more than the log names is not a clean run.
        let cut_tail = "\
     TIMEOUT [ 240.1s] ( 8103/8223) rsid-store store::tests::slow
     Summary [3715.8s] 8223 tests run: 8201 passed, 3 failed, 2 timed out, 22 skipped
";
        let report = failing_test_report(cut_tail);
        assert_eq!(report.names, ["TIMEOUT rsid-store store::tests::slow"]);
        assert_eq!(report.omitted, 4);
    }

    /// #185: names past the bound are counted, never silently dropped.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn failing_tests_past_the_bound_are_counted_as_omitted() {
        let log: String = (0..MAX_FAILING_TESTS + 8)
            .map(|n| format!("     TIMEOUT [  60.0s] ({n}/99) rsid t::slow_{n:03}\n"))
            .collect();
        let report = failing_test_report(&log);
        assert_eq!(report.names.len(), MAX_FAILING_TESTS);
        assert_eq!(report.omitted, 8);
        assert!(
            report
                .names
                .iter()
                .all(|name| name.starts_with("TIMEOUT rsid t::slow_"))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_failed_test_run_reports_failing_tests_to_the_owner() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let mut c = ctx(owner, dir.path(), None);
        c.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":["x"]}),
        );
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            c,
            Utc::now(),
        )
        .unwrap();
        std::fs::write(&row.status_path, "101\n").unwrap();
        std::fs::write(
            &row.job.log_path,
            "test store::tests::a ... FAILED\n        FAIL [   0.2s] rsid b::c\n",
        )
        .unwrap();
        *runtime.active.lock().unwrap() = false;
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 1);
        assert!(!job_tmp_dir(Path::new(&row.job.log_path)).exists());
        let guard = store.lock().await;
        let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(job.state, JobState::Failed);
        let result = job.result.unwrap();
        assert_eq!(result.exit_code, Some(101));
        assert_eq!(result.failing_tests, ["rsid b::c", "store::tests::a"]);
        assert!(
            wakes_for(&guard, owner)[0]
                .message
                .contains("failing_tests=")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn cancelling_a_running_job_stops_its_unit_and_settles_it_cancelled_once() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let mut c = ctx(owner, dir.path(), None);
        c.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[]}),
        );
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            c,
            Utc::now(),
        )
        .unwrap();
        std::fs::write(&row.job.log_path, "test a::b ... ok\n").unwrap();

        let guard = store.lock().await;
        let (job, cancelled) = cancel_job(&guard, &*runtime, row.job.id, Utc::now()).unwrap();
        assert!(cancelled);
        assert_eq!(job.state, JobState::Failed);
        let result = job.result.clone().unwrap();
        assert_eq!(result.refusal.as_deref(), Some(JOB_CANCELLED));
        assert!(result.detail.unwrap().contains("cancelled by the owner"));
        assert_eq!(
            *runtime.stopped.lock().unwrap(),
            [row.job.unit_name.clone()]
        );
        assert!(!job_tmp_dir(Path::new(&row.job.log_path)).exists());
        // The owner made the call: no per-job wake is queued.
        assert!(wakes_for(&guard, owner).is_empty());

        // A repeat cancel changes nothing and stops nothing more.
        let (again, cancelled_again) =
            cancel_job(&guard, &*runtime, row.job.id, Utc::now()).unwrap();
        assert!(!cancelled_again);
        assert_eq!(again, job);
        assert_eq!(runtime.stopped.lock().unwrap().len(), 1);
        drop(guard);
        // The poll has nothing left to settle for it.
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);
    }

    /// #3038: a cancelling topology execution stops the node session's open
    /// jobs, and only that session's.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn cancelling_a_sessions_open_jobs_stops_only_its_own_ordinary_jobs() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let submit_for = |who: Uuid, key: &str| {
            let mut c = ctx(who, dir.path(), Some(key));
            c.params = params(
                JobKind::Test,
                serde_json::json!({"package":"rsid","filters":[]}),
            );
            let guard = store.blocking_lock();
            submit(&guard, &*runtime, &tools(), dir.path(), c, Utc::now())
                .unwrap()
                .0
        };
        let mine = submit_for(owner, "mine");
        let foreign = submit_for(other, "foreign");

        let guard = store.blocking_lock();
        let cancelled = cancel_open_jobs_of_owner(&guard, &*runtime, owner, Utc::now()).unwrap();
        assert_eq!(cancelled, vec![mine.job.id]);
        assert_eq!(
            guard.get_agent_job(mine.job.id).unwrap().unwrap().job.state,
            JobState::Failed
        );
        assert_eq!(
            guard
                .get_agent_job(foreign.job.id)
                .unwrap()
                .unwrap()
                .job
                .state,
            JobState::Running
        );
        // Nothing is left open for the session: a repeat cancels nothing.
        assert!(
            cancel_open_jobs_of_owner(&guard, &*runtime, owner, Utc::now())
                .unwrap()
                .is_empty()
        );
    }

    /// #2048: a deploy row owned by a fresh manager session, staged at `at`.
    fn stage_deploy(store: &Store, interrupt_workers: bool, at: DateTime<Utc>) -> Uuid {
        stage_deploy_waiting(store, interrupt_workers, at, 3600, Uuid::new_v4(), &[])
    }

    /// [`stage_deploy`] with the deploy's `max_wait_secs`.
    fn stage_deploy_waiting(
        store: &Store,
        interrupt_workers: bool,
        at: DateTime<Utc>,
        max_wait_secs: u32,
        id: Uuid,
        manifest: &[rsi_common::agent_deploy::DeployBinaryV1],
    ) -> Uuid {
        let owner = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO sessions (id, provider, query, working_dir, status, created_at, \
                 updated_at, session_kind) VALUES (?1,'Claude','q','/tmp','Running',?2,?2,'Task')",
                rusqlite::params![
                    owner.to_string(),
                    at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();

        store
            .insert_agent_deploy(
                &crate::store::agent_deploys::NewDeploy {
                    id,
                    owner_session_id: owner,
                    idempotency_key: "k",
                    sha: "0123456789abcdef0123456789abcdef01234567",
                    fingerprint: crate::store::agent_deploys::deploy_fingerprint_with(
                        "0123456789abcdef0123456789abcdef01234567",
                        "x",
                        max_wait_secs,
                        interrupt_workers,
                    ),
                    manifest,
                    max_wait_secs,
                    interrupt_workers,
                },
                at,
            )
            .unwrap();
        id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_staged_interrupt_deploy_stops_test_jobs_after_its_grace_once_no_build_blocks_it() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let drain = crate::deploy_drain::DeployDrain::new();
        let (tester, builder) = (Uuid::new_v4(), Uuid::new_v4());
        let mut test_ctx = ctx(tester, dir.path(), None);
        test_ctx.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[]}),
        );
        let guard = store.lock().await;
        let t0 = Utc::now();
        let (test_job, _) = submit(&guard, &*runtime, &tools(), dir.path(), test_ctx, t0).unwrap();
        let (build_job, _) = submit(
            &guard,
            &*runtime,
            &tools(),
            dir.path(),
            ctx(builder, dir.path(), None),
            t0,
        )
        .unwrap();
        assert_eq!(test_job.job.state, JobState::Running);
        assert_eq!(build_job.job.state, JobState::Running);
        let deploy = stage_deploy(&guard, true, t0);
        let grace = i64::try_from(rsi_common::agent_deploy::DEPLOY_JOB_GRACE_SECS).unwrap();
        let state_of = |id: Uuid| guard.get_agent_job(id).unwrap().unwrap().job.state;

        // #2190: the build job is something the deploy cannot preempt, so
        // stopping the test job gains nothing: it is left alone, past the
        // grace as well.
        let early = t0 + chrono::Duration::seconds(grace - 1);
        let late = t0 + chrono::Duration::seconds(grace + 1);
        for at in [early, late] {
            assert!(
                cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, at)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(state_of(test_job.job.id), JobState::Running);
        }
        assert_eq!(
            guard
                .deploy_quiet_blockers_with(Uuid::new_v4(), false)
                .unwrap(),
            ["job_running"]
        );

        // The build ends. The grace restarts from the last tick that saw it
        // blocked, so the test job still gets a full grace.
        guard
            .settle_agent_job(
                build_job.job.id,
                JobState::Succeeded,
                &AgentJobResultV1::default(),
                false,
                late,
            )
            .unwrap();
        let inside = late + chrono::Duration::seconds(grace - 1);
        assert!(
            cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, inside)
                .unwrap()
                .is_empty()
        );
        assert_eq!(state_of(test_job.job.id), JobState::Running);

        // Past that grace the test job is stopped and requeued under its own id
        // (#2278): held for the deploy, no wake yet, rerun on the new daemon.
        let after = late + chrono::Duration::seconds(grace + 1);
        let stopped = cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, after).unwrap();
        assert_eq!(stopped, [test_job.job.id]);
        let requeued = guard.get_agent_job(test_job.job.id).unwrap().unwrap().job;
        assert_eq!(requeued.state, JobState::Queued);
        assert_eq!(requeued.held.as_deref(), Some("held_for_deploy"));
        assert_eq!(requeued.owner_session_id, tester);
        assert_eq!(requeued.wake, JobWake::Owner);
        assert_eq!(requeued.params, test_job.job.params);
        assert!(requeued.result.is_none());
        assert!(
            wakes_for(&guard, tester).is_empty(),
            "the owner's wake waits for the rerun"
        );
        assert_eq!(
            *runtime.stopped.lock().unwrap(),
            [test_job.job.unit_name.clone()]
        );
        // The build job was never stopped: it ran to its own end.
        assert_eq!(state_of(build_job.job.id), JobState::Succeeded);
        assert!(
            guard
                .deploy_quiet_blockers_with(Uuid::new_v4(), false)
                .unwrap()
                .is_empty()
        );

        // The receipt source names the stopped job; a repeat stops nothing more.
        assert_eq!(
            guard.agent_deploy_interrupt(deploy).unwrap().cancelled_jobs,
            [test_job.job.id]
        );
        assert!(
            cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, after)
                .unwrap()
                .is_empty()
        );
    }

    /// A deploy service whose restart is a counter, installing into the temp
    /// dir, and the manifest of one staged `rsid` binary for deploy `id`.
    fn restart_counting_service(
        dir: &Path,
        id: Uuid,
    ) -> (
        crate::deploy::DeployService,
        Arc<std::sync::atomic::AtomicUsize>,
        Vec<rsi_common::agent_deploy::DeployBinaryV1>,
    ) {
        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        let (source, install) = (dir.join("deploy-src"), dir.join("deploy-install"));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(source.join("rsid"), b"new-rsid").unwrap();
        std::fs::write(install.join("rsid"), b"old-rsid").unwrap();
        let service = crate::deploy::DeployService::new(
            install,
            vec![dir.to_path_buf()],
            Box::new(|| true),
            Arc::new(|_: &Path| Ok((SHA.into(), 999))),
        );
        let restarts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&restarts);
        service.set_restart_trigger(Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        let manifest = service
            .stage_plan()
            .stage_binaries(id, source.to_str().unwrap(), SHA, 1)
            .unwrap();
        (service, restarts, manifest)
    }

    fn test_ctx(owner: Uuid, dir: &Path) -> SubmitContext {
        let mut c = ctx(owner, dir, None);
        c.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[]}),
        );
        c
    }

    /// #2278: a staged `interrupt_workers` deploy on a busy hub (a running test
    /// job, a new submission every few seconds) restarts inside its bound. The
    /// stream is held, never started or lost; the running job is stopped after
    /// its grace and requeued under its own id, so its owner's single wake
    /// fires when the rerun ends on the new daemon.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_deploy_with_a_running_job_and_a_stream_of_submissions_restarts_within_its_bound() {
        use crate::deploy::{GateState, PollOutcome, poll_once};
        const BOUND_SECS: i64 = 900;
        const STEP_SECS: i64 = 20;
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let drain = crate::deploy_drain::DeployDrain::new();
        drain.set_hold_cap_secs(60);
        let deploy = Uuid::new_v4();
        let (service, restarts, manifest) = restart_counting_service(dir.path(), deploy);
        let tester = Uuid::new_v4();
        let t0 = Utc::now();
        let (running, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            test_ctx(tester, dir.path()),
            t0,
        )
        .unwrap();
        assert_eq!(running.job.state, JobState::Running);
        stage_deploy_waiting(
            &*store.lock().await,
            true,
            t0,
            BOUND_SECS as u32,
            deploy,
            &manifest,
        );
        let grace = i64::try_from(rsi_common::agent_deploy::DEPLOY_JOB_GRACE_SECS).unwrap();

        let mut gate = GateState::default();
        let mut streamed = Vec::new();
        let mut restarted_at = None;
        for tick in 0..=BOUND_SECS / STEP_SECS {
            let at = t0 + chrono::Duration::seconds(tick * STEP_SECS);
            // The deploy runner and the jobs loop run before this tick's submit,
            // as in the daemon (the poll engages the hold new submits meet).
            let outcome = poll_once(&store, &service, at, &mut gate, &drain, true)
                .await
                .unwrap();
            if let PollOutcome::Restarting(id) = outcome {
                assert_eq!(id, deploy);
                restarted_at = Some(tick * STEP_SECS);
                break;
            }
            {
                let guard = store.lock().await;
                cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, at).unwrap();
                // A new submission, held by the drain exactly as the verb does.
                let owner = Uuid::new_v4();
                let hold = drain.holds_job_start(Some(owner), true);
                assert!(hold, "the deploy holds new job starts (tick {tick})");
                let (row, _) = submit_with_hold(
                    &guard,
                    &*runtime,
                    &tools(),
                    dir.path(),
                    test_ctx(owner, dir.path()),
                    hold,
                    at,
                )
                .unwrap();
                assert_eq!(row.job.state, JobState::Queued);
                streamed.push(row.job.id);
            }
            assert_eq!(
                start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                    .await
                    .unwrap(),
                0,
                "no held job starts while the deploy waits (tick {tick})"
            );
        }

        // Restarted just after the grace and the confirming quiet polls, well
        // inside the bound, with one restart fired.
        let restarted_at = restarted_at.expect("the deploy reached its restart");
        assert!(restarted_at > grace, "not before the grace: {restarted_at}");
        assert!(
            restarted_at <= grace + 60 && restarted_at < BOUND_SECS,
            "restarted at {restarted_at}s"
        );
        assert_eq!(restarts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!streamed.is_empty());

        let guard = store.lock().await;
        // Nothing was lost: the running job and the whole stream are queued.
        let requeued = guard.get_agent_job(running.job.id).unwrap().unwrap().job;
        assert_eq!(requeued.state, JobState::Queued);
        assert_eq!(requeued.owner_session_id, tester);
        assert_eq!(requeued.wake, JobWake::Owner);
        for id in &streamed {
            let job = guard.get_agent_job(*id).unwrap().unwrap().job;
            assert_eq!(job.state, JobState::Queued);
            assert_eq!(job.held.as_deref(), Some("held_for_deploy"));
        }
        assert_eq!(
            runtime.launched.lock().unwrap().len(),
            1,
            "only the first run"
        );
        // The outcome names the requeued job; the owner is not woken yet.
        assert_eq!(
            guard.agent_deploy_interrupt(deploy).unwrap().cancelled_jobs,
            [running.job.id]
        );
        assert!(wakes_for(&guard, tester).is_empty());
        drop(guard);

        // The new daemon: a fresh drain (the retry window of the old process
        // does not survive a restart; startup restores it only for a timed-out
        // row, #2552), so the hold is gone and the held jobs start. The rerun's
        // end wakes the owner exactly once.
        let drain = crate::deploy_drain::DeployDrain::new();
        drain.sync(None, true, Utc::now());
        let started = start_queued_jobs(&store, &job_runtime, &tools(), &drain)
            .await
            .unwrap();
        assert!(started >= 1, "the held jobs start once the hold is gone");
        let rerun = store
            .lock()
            .await
            .get_agent_job(running.job.id)
            .unwrap()
            .unwrap();
        assert_eq!(rerun.job.state, JobState::Running);
        std::fs::write(&rerun.status_path, "0\n").unwrap();
        *runtime.active.lock().unwrap() = false;
        poll_once_jobs(&store, &job_runtime).await;
        let guard = store.lock().await;
        assert!(
            guard
                .get_agent_job(running.job.id)
                .unwrap()
                .unwrap()
                .job
                .state
                .is_terminal()
        );
        assert_eq!(
            wakes_for(&guard, tester).len(),
            1,
            "one wake, from the rerun"
        );
    }

    /// The jobs loop's settle pass, over the fake runtime.
    async fn poll_once_jobs(store: &Arc<tokio::sync::Mutex<Store>>, runtime: &Arc<dyn JobRuntime>) {
        poll_once(store, runtime, Utc::now()).await.unwrap();
    }

    /// #2278: a staged `interrupt_workers` deploy that times out behind
    /// something it cannot preempt (a running build) does not release its held
    /// jobs into a race with the retry: new submissions stay held for the retry
    /// window, and the retry's own hold takes them over.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_timed_out_interrupt_deploy_keeps_jobs_held_for_its_retry() {
        use crate::deploy::{GateState, PollOutcome, poll_once};
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let drain = crate::deploy_drain::DeployDrain::new();
        drain.set_hold_cap_secs(60);
        let first = Uuid::new_v4();
        let (service, restarts, manifest) = restart_counting_service(dir.path(), first);
        let t0 = Utc::now();
        let builder = Uuid::new_v4();
        let (build, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            ctx(builder, dir.path(), None),
            t0,
        )
        .unwrap();
        assert_eq!(build.job.state, JobState::Running);
        stage_deploy_waiting(&*store.lock().await, true, t0, 60, first, &manifest);
        let mut gate = GateState::default();
        assert!(matches!(
            poll_once(&store, &service, t0, &mut gate, &drain, true)
                .await
                .unwrap(),
            PollOutcome::Waiting(_)
        ));
        let deadline = t0 + chrono::Duration::seconds(61);
        assert_eq!(
            poll_once(&store, &service, deadline, &mut gate, &drain, true)
                .await
                .unwrap(),
            PollOutcome::Settled(first, rsi_common::agent_deploy::DeployState::TimedOut)
        );
        assert_eq!(restarts.load(std::sync::atomic::Ordering::SeqCst), 0);

        // The deploy is gone but its job hold is not: a job submitted right now
        // is held, whoever owns it, so an immediate retry meets the same set.
        for has_parent in [true, false] {
            assert!(
                drain.holds_job_start(Some(Uuid::new_v4()), has_parent),
                "held after the timeout (parent: {has_parent})"
            );
        }
        let guard = store.lock().await;
        let owner = Uuid::new_v4();
        let (held, _) = submit_with_hold(
            &guard,
            &*runtime,
            &tools(),
            dir.path(),
            test_ctx(owner, dir.path()),
            drain.holds_job_start(Some(owner), true),
            deadline,
        )
        .unwrap();
        assert_eq!(held.job.state, JobState::Queued);
        // The retry takes the hold over without a gap.
        let retry = stage_deploy_waiting(&guard, true, deadline, 900, Uuid::new_v4(), &[]);
        drop(guard);
        assert!(matches!(
            poll_once(&store, &service, deadline, &mut gate, &drain, true)
                .await
                .unwrap(),
            PollOutcome::Waiting(_)
        ));
        assert!(drain.holds_job_start(Some(Uuid::new_v4()), true));
        assert_eq!(
            store
                .lock()
                .await
                .get_agent_job(held.job.id)
                .unwrap()
                .unwrap()
                .job
                .state,
            JobState::Queued
        );
        let _ = retry;
    }

    /// #2190: a staged deploy blocked by an admitted or gating queue landing
    /// (which it cannot preempt) leaves running test jobs alone, however long
    /// that lasts; only once the landing settles do they get their grace and
    /// are then stopped `interrupted_by_deploy`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_deploy_blocked_by_a_gating_landing_leaves_running_test_jobs_alone_until_it_settles()
    {
        use crate::store::rolling_queue::NewQueueEntry;
        use rsi_common::rolling_queue::{
            RollingQueueBinding, RollingQueueEntryState, RollingQueueOutcome,
        };
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let drain = crate::deploy_drain::DeployDrain::new();
        let tester = Uuid::new_v4();
        let mut test_ctx = ctx(tester, dir.path(), None);
        test_ctx.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[]}),
        );
        let guard = store.lock().await;
        let t0 = Utc::now();
        let (job, _) = submit(&guard, &*runtime, &tools(), dir.path(), test_ctx, t0).unwrap();
        let deploy = stage_deploy(&guard, true, t0);
        let grace = i64::try_from(rsi_common::agent_deploy::DEPLOY_JOB_GRACE_SECS).unwrap();
        let landing = {
            guard
                .enqueue_rolling_queue_source(
                    &NewQueueEntry {
                        project_id: None,
                        repo_path: "/tmp/repo".into(),
                        source_commit: "b".repeat(40),
                        source_session_id: tester,
                        owner_epic_id: None,
                        binding: RollingQueueBinding::Unbound,
                        work_key: None,
                        migration_version: None,
                        hot_files: vec![],
                        test_filters: vec![],
                        fixes_tests: Vec::new(),
                        idempotency_key: "land".into(),
                    },
                    t0,
                )
                .unwrap();
            guard
                .claim_next_rolling_queue_entry(t0)
                .unwrap()
                .unwrap()
                .entry
                .id
        };
        assert_eq!(
            guard.deploy_unpreemptible_blockers().unwrap(),
            ["landing_in_progress"]
        );

        // Ticks across an hour: the landing gates, the test job is untouched.
        for minutes in [1, 6, 30, 59] {
            let at = t0 + chrono::Duration::minutes(minutes);
            assert!(
                cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, at)
                    .unwrap()
                    .is_empty()
            );
        }
        let last_blocked = t0 + chrono::Duration::minutes(59);
        assert_eq!(drain.unpreemptible_blocked_at(deploy), Some(last_blocked));
        assert_eq!(
            guard.get_agent_job(job.job.id).unwrap().unwrap().job.state,
            JobState::Running
        );
        assert!(runtime.stopped.lock().unwrap().is_empty());
        assert!(wakes_for(&guard, tester).is_empty());

        // The landing settles. The job still gets its grace from there.
        guard
            .settle_rolling_queue_entry(
                landing,
                RollingQueueEntryState::Failed,
                &RollingQueueOutcome::default(),
                last_blocked,
            )
            .unwrap();
        assert!(guard.deploy_unpreemptible_blockers().unwrap().is_empty());
        let inside = last_blocked + chrono::Duration::seconds(grace - 1);
        assert!(
            cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, inside)
                .unwrap()
                .is_empty()
        );
        let after = last_blocked + chrono::Duration::seconds(grace + 1);
        assert_eq!(
            cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, after).unwrap(),
            [job.job.id]
        );
        // #2278: the stopped job is back in the queue under its own id, and its
        // owner is not woken until the rerun ends.
        let requeued = guard.get_agent_job(job.job.id).unwrap().unwrap().job;
        assert_eq!(requeued.state, JobState::Queued);
        let job_wakes = wakes_for(&guard, tester)
            .into_iter()
            .filter(|wake| wake.message.contains(&job.job.id.to_string()))
            .count();
        assert_eq!(job_wakes, 0, "no wake until the rerun ends");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_deploy_without_interrupt_workers_never_stops_test_jobs() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let mut test_ctx = ctx(Uuid::new_v4(), dir.path(), None);
        test_ctx.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[]}),
        );
        let guard = store.lock().await;
        let t0 = Utc::now();
        let drain = crate::deploy_drain::DeployDrain::new();
        let (job, _) = submit(&guard, &*runtime, &tools(), dir.path(), test_ctx, t0).unwrap();
        stage_deploy(&guard, false, t0);
        let late = t0 + chrono::Duration::hours(1);
        assert!(
            cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, late)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            guard.get_agent_job(job.job.id).unwrap().unwrap().job.state,
            JobState::Running
        );
    }

    /// #2176: a running hub full-library QA run, as the satellite owns it.
    fn insert_hub_qa_run(store: &Store, dir: &Path, at: DateTime<Utc>) -> Uuid {
        use crate::store::agent_jobs::NewAgentJob;
        let hub_run = Uuid::new_v4();
        store
            .insert_agent_job(
                &NewAgentJob {
                    id: hub_run,
                    owner_session_id: Uuid::new_v4(),
                    project_id: None,
                    name: Some(format!("Hub full library QA {}", "a".repeat(40))),
                    params: params(
                        JobKind::Test,
                        serde_json::json!({"recipe":LIBRARY_QA_RECIPE,"timeout_minutes":180}),
                    ),
                    cwd: dir.display().to_string(),
                    unit_name: format!("qa-{hub_run}"),
                    log_path: dir.join("qa.log").display().to_string(),
                    status_path: dir.join("qa.status").display().to_string(),
                    idempotency_key: Some(format!("hub-library-qa:{}", Uuid::new_v4())),
                    wake: JobWake::None,
                },
                at,
            )
            .unwrap();
        hub_run
    }

    /// #2176: a hub's full-library QA run on this satellite is a guest. Any
    /// staged deploy stops it on the next pass, with no `interrupt_workers`
    /// and no grace, as `interrupted_by_deploy` (the hub defers that attempt
    /// as `deploy_preempted`), so it stops blocking the quiet point. An
    /// ordinary test job under the same deploy keeps running.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_staged_deploy_stops_a_hub_qa_run_at_once_without_interrupt_or_grace() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let drain = crate::deploy_drain::DeployDrain::new();
        let mut test_ctx = ctx(Uuid::new_v4(), dir.path(), None);
        test_ctx.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[]}),
        );
        let guard = store.lock().await;
        let t0 = Utc::now();
        let (plain, _) = submit(&guard, &*runtime, &tools(), dir.path(), test_ctx, t0).unwrap();
        let hub_run = insert_hub_qa_run(&guard, dir.path(), t0);
        let deploy = stage_deploy(&guard, false, t0);
        let stopped = cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, t0).unwrap();
        assert_eq!(stopped, [hub_run]);
        let settled = guard.get_agent_job(hub_run).unwrap().unwrap().job;
        assert_eq!(settled.state, JobState::Failed);
        let result = settled.result.unwrap();
        assert_eq!(result.refusal.as_deref(), Some(JOB_INTERRUPTED_BY_DEPLOY));
        assert_eq!(result.deploy_id, Some(deploy));
        assert_eq!(
            guard
                .get_agent_job(plain.job.id)
                .unwrap()
                .unwrap()
                .job
                .state,
            JobState::Running,
            "an ordinary test job keeps the #2048 rule"
        );
        assert!(
            cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, t0)
                .unwrap()
                .is_empty()
        );
    }

    /// #2176 with #2190: QA is preemptible, so a hub QA run is stopped at once
    /// even while a gating landing blocks the deploy; only the ordinary test
    /// job is spared until the landing settles and its grace passes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_hub_qa_run_is_stopped_at_once_even_while_a_gating_landing_blocks_the_deploy() {
        use crate::store::rolling_queue::NewQueueEntry;
        use rsi_common::rolling_queue::{
            RollingQueueBinding, RollingQueueEntryState, RollingQueueOutcome,
        };
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let drain = crate::deploy_drain::DeployDrain::new();
        let tester = Uuid::new_v4();
        let mut test_ctx = ctx(tester, dir.path(), None);
        test_ctx.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[]}),
        );
        let guard = store.lock().await;
        let t0 = Utc::now();
        let (plain, _) = submit(&guard, &*runtime, &tools(), dir.path(), test_ctx, t0).unwrap();
        let hub_run = insert_hub_qa_run(&guard, dir.path(), t0);
        stage_deploy(&guard, true, t0);
        guard
            .enqueue_rolling_queue_source(
                &NewQueueEntry {
                    project_id: None,
                    repo_path: "/tmp/repo".into(),
                    source_commit: "b".repeat(40),
                    source_session_id: tester,
                    owner_epic_id: None,
                    binding: RollingQueueBinding::Unbound,
                    work_key: None,
                    migration_version: None,
                    hot_files: vec![],
                    test_filters: vec![],
                    fixes_tests: Vec::new(),
                    idempotency_key: "land".into(),
                },
                t0,
            )
            .unwrap();
        let landing = guard
            .claim_next_rolling_queue_entry(t0)
            .unwrap()
            .unwrap()
            .entry
            .id;
        assert_eq!(
            guard.deploy_unpreemptible_blockers().unwrap(),
            ["landing_in_progress"]
        );

        // The landing blocks the deploy: QA goes at once, the test job stays.
        let blocked = t0 + chrono::Duration::hours(1);
        let stopped = cancel_test_jobs_for_deploy(&guard, &*runtime, &drain, blocked).unwrap();
        assert_eq!(stopped, [hub_run]);
        let state_of = |id: Uuid| guard.get_agent_job(id).unwrap().unwrap().job.state;
        assert_eq!(state_of(hub_run), JobState::Failed);
        assert_eq!(state_of(plain.job.id), JobState::Running);
        assert_eq!(
            drain.unpreemptible_blocked_at(guard.live_agent_deploy().unwrap().unwrap().id),
            Some(blocked)
        );

        // The landing settles; the test job then gets its grace and is stopped.
        guard
            .settle_rolling_queue_entry(
                landing,
                RollingQueueEntryState::Failed,
                &RollingQueueOutcome::default(),
                blocked,
            )
            .unwrap();
        let grace = i64::try_from(rsi_common::agent_deploy::DEPLOY_JOB_GRACE_SECS).unwrap();
        assert!(
            cancel_test_jobs_for_deploy(
                &guard,
                &*runtime,
                &drain,
                blocked + chrono::Duration::seconds(grace - 1)
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            cancel_test_jobs_for_deploy(
                &guard,
                &*runtime,
                &drain,
                blocked + chrono::Duration::seconds(grace + 1)
            )
            .unwrap(),
            [plain.job.id]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_cancel_whose_unit_stop_fails_or_stays_active_leaves_the_job_running_and_retries() {
        /// `stop_unit` fails while `failing`; with `lingers` it reports `Ok`
        /// yet the unit stays active.
        struct Stubborn {
            inner: FakeRuntime,
            failing: Mutex<bool>,
            lingers: Mutex<bool>,
        }
        impl JobRuntime for Stubborn {
            fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
                self.inner.launch(spec)
            }
            fn unit_active(&self, unit: &str) -> bool {
                self.inner.unit_active(unit)
            }
            fn stop_unit(&self, unit: &str) -> std::result::Result<(), String> {
                if *self.failing.lock().unwrap() {
                    return Err("systemctl stop exited 1".into());
                }
                if *self.lingers.lock().unwrap() {
                    return Ok(());
                }
                self.inner.stop_unit(unit)
            }
        }
        let (store, dir) = open();
        let runtime = Stubborn {
            inner: FakeRuntime::default(),
            failing: Mutex::new(true),
            lingers: Mutex::new(false),
        };
        let mut c = ctx(Uuid::new_v4(), dir.path(), None);
        c.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[]}),
        );
        let (row, _) = submit(
            &*store.lock().await,
            &runtime,
            &tools(),
            dir.path(),
            c,
            Utc::now(),
        )
        .unwrap();
        let tmp = job_tmp_dir(Path::new(&row.job.log_path));
        assert!(tmp.exists());

        let guard = store.lock().await;
        for (failing, lingers) in [(true, false), (false, true)] {
            *runtime.failing.lock().unwrap() = failing;
            *runtime.lingers.lock().unwrap() = lingers;
            let error = cancel_job(&guard, &runtime, row.job.id, Utc::now()).unwrap_err();
            assert!(
                error.to_string().contains("job_cancel_unit_not_stopped"),
                "{error}"
            );
            let stored = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
            assert_eq!(stored.state, JobState::Running);
            assert!(tmp.exists(), "scratch kept while the unit may be alive");
        }
        // A repeat cancel retries and succeeds once the unit really stops.
        *runtime.lingers.lock().unwrap() = false;
        let (job, cancelled) = cancel_job(&guard, &runtime, row.job.id, Utc::now()).unwrap();
        assert!(cancelled);
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.result.unwrap().refusal.as_deref(), Some(JOB_CANCELLED));
        assert!(!tmp.exists());
    }

    fn engage_drain(drain: &crate::deploy_drain::DeployDrain) {
        let row = crate::store::agent_deploys::DeployRow {
            id: Uuid::new_v4(),
            owner_session_id: Some(Uuid::new_v4()),
            sha: "0".repeat(40),
            manifest: Vec::new(),
            state: rsi_common::agent_deploy::DeployState::Staged,
            reason: None,
            deadline_at: Utc::now() + chrono::Duration::seconds(600),
            operator: false,
            forced: false,
        };
        drain.sync(Some(&row), true, Utc::now());
        assert!(drain.is_draining());
    }

    fn held_test_job(
        store: &Store,
        runtime: &dyn JobRuntime,
        dir: &Path,
        owner: Uuid,
    ) -> AgentJobRow {
        let mut c = ctx(owner, dir, None);
        c.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[]}),
        );
        let (row, replayed) =
            submit_with_hold(store, runtime, &tools(), dir, c, true, Utc::now()).unwrap();
        assert!(!replayed);
        row
    }

    /// #2190: stage an `interrupt_workers` deploy at `staged` and engage what its
    /// runner derives each tick: the worker-start hold already over (cap 60 s) and
    /// the job hold, which is not bounded by that cap.
    fn engage_job_hold(
        store: &Store,
        drain: &crate::deploy_drain::DeployDrain,
        staged: DateTime<Utc>,
    ) -> crate::store::agent_deploys::DeployRow {
        let id = stage_deploy(store, true, staged);
        let row = store.live_agent_deploy().unwrap().unwrap();
        assert_eq!(row.id, id);
        drain.set_hold_cap_secs(60);
        let now = Utc::now();
        drain.sync_until(
            Some(&row),
            Some(staged + chrono::Duration::seconds(60)),
            true,
            now,
        );
        drain.hold_jobs_until_settled(&row, now);
        row
    }

    /// #2524: a host at or above its thermal ceiling starts no cargo job; the
    /// job is held `held_for_thermal` with no unit, keeps no admission clock and
    /// starts once the host is below the ceiling. Non-cargo jobs are not held.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_hot_host_holds_cargo_jobs_until_it_cools() {
        use rsid_core::thermal_guard::ThermalHold;
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let drain = crate::deploy_drain::DeployDrain::new();
        let owner = Uuid::new_v4();
        crate::host_pressure::TEST_THERMAL_HOLD.with(|hot| {
            hot.set(Some(Some(ThermalHold {
                temperature_c: 92.5,
                ceiling_c: 85,
            })));
        });

        let mut c = ctx(owner, dir.path(), None);
        c.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[]}),
        );
        let submitted = Utc::now() - chrono::Duration::minutes(30);
        let (job, replayed) = submit_with_hold(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            c,
            false,
            submitted,
        )
        .unwrap();
        assert!(!replayed);
        assert_eq!(job.job.state, JobState::Queued);
        assert_eq!(job.job.held.as_deref(), Some(JOB_HELD_FOR_THERMAL));
        assert!(runtime.launched.lock().unwrap().is_empty(), "no unit");
        let log = std::fs::read_to_string(&job.job.log_path).unwrap();
        assert!(
            log.contains("held for thermal") && log.contains("92.5"),
            "{log}"
        );

        // Still hot: it stays queued past the ordinary admission cap, not failed.
        for _ in 0..2 {
            assert_eq!(
                start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                    .await
                    .unwrap(),
                0
            );
        }
        {
            let guard = store.lock().await;
            let stored = guard.get_agent_job(job.job.id).unwrap().unwrap().job;
            assert_eq!(stored.state, JobState::Queued, "held, not timed out");
            assert_eq!(stored.held.as_deref(), Some(JOB_HELD_FOR_THERMAL));
        }

        // Cooled: it starts.
        crate::host_pressure::TEST_THERMAL_HOLD.with(|hot| hot.set(Some(None)));
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            1
        );
        assert_eq!(runtime.launched.lock().unwrap().len(), 1);
        let stored = store
            .lock()
            .await
            .get_agent_job(job.job.id)
            .unwrap()
            .unwrap()
            .job;
        assert_eq!(stored.state, JobState::Running);
        assert_eq!(stored.held, None);
        crate::host_pressure::TEST_THERMAL_HOLD.with(|hot| hot.set(None));
    }

    /// #2937: a host with no governor client (macOS) holds a cargo job while
    /// its build slots are full or memory is low: `held_for_capacity`, no unit,
    /// and the admission clock keeps running (the job fails visibly if the host
    /// never frees up). It starts once capacity returns. Linux never gates.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_full_host_holds_cargo_jobs_until_capacity_frees() {
        use crate::host_memory::{JobCapacityHold, TEST_CAPACITY_HOLD};
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let drain = crate::deploy_drain::DeployDrain::new();
        let owner = Uuid::new_v4();
        TEST_CAPACITY_HOLD.with(|hold| {
            hold.set(Some(Some(JobCapacityHold::Memory {
                available_gb: 2.5,
                min_gb: 4,
            })));
        });

        let mut c = ctx(owner, dir.path(), None);
        c.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[]}),
        );
        let (job, replayed) = submit_with_hold(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            c,
            false,
            Utc::now(),
        )
        .unwrap();
        assert!(!replayed);
        assert_eq!(job.job.state, JobState::Queued);
        assert_eq!(job.job.held.as_deref(), Some(JOB_HELD_FOR_CAPACITY));
        assert!(runtime.launched.lock().unwrap().is_empty(), "no unit");
        let log = std::fs::read_to_string(&job.job.log_path).unwrap();
        assert!(
            log.contains("held for capacity") && log.contains("2.5 GiB"),
            "{log}"
        );

        // Still full: it stays queued.
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .lock()
                .await
                .get_agent_job(job.job.id)
                .unwrap()
                .unwrap()
                .job
                .state,
            JobState::Queued
        );

        // Capacity returned: it starts.
        TEST_CAPACITY_HOLD.with(|hold| hold.set(Some(None)));
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            1
        );
        assert_eq!(runtime.launched.lock().unwrap().len(), 1);
        let stored = store
            .lock()
            .await
            .get_agent_job(job.job.id)
            .unwrap()
            .unwrap()
            .job;
        assert_eq!((stored.state, stored.held), (JobState::Running, None));
        TEST_CAPACITY_HOLD.with(|hold| hold.set(None));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn jobs_submitted_while_a_staged_interrupt_deploy_waits_are_held_and_start_once_it_settles()
     {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let drain = crate::deploy_drain::DeployDrain::new();
        let now = Utc::now();
        let row = engage_job_hold(
            &*store.lock().await,
            &drain,
            now - chrono::Duration::minutes(30),
        );
        let (first_owner, second_owner) = (Uuid::new_v4(), Uuid::new_v4());

        // The worker-start hold is long over, yet a test job is held: the
        // deploy would stop it after its grace. That covers the deploy's own
        // caller and a parentless session too.
        assert!(!drain.is_draining(), "the capped worker hold is over");
        assert!(!drain.holds(Some(first_owner), true));
        assert!(drain.holds_job_start(Some(first_owner), true));
        assert!(drain.holds_job_start(row.owner_session_id, true));
        assert!(drain.holds_job_start(Some(first_owner), false));
        assert_eq!(drain.job_release_by(), Some(row.deadline_at));

        // Submitted long enough ago to be past the admission cap: the hold,
        // not the job, has been waiting, so no timeout has run.
        let submitted = now - chrono::Duration::minutes(25);
        let mut jobs = Vec::new();
        for owner in [first_owner, second_owner] {
            let mut c = ctx(owner, dir.path(), None);
            c.params = params(
                JobKind::Test,
                serde_json::json!({"package":"rsid","filters":[]}),
            );
            let (job, replayed) = submit_with_hold(
                &*store.lock().await,
                &*runtime,
                &tools(),
                dir.path(),
                c,
                drain.holds_job_start(Some(owner), true),
                submitted,
            )
            .unwrap();
            assert!(!replayed);
            assert_eq!(job.job.state, JobState::Queued);
            assert_eq!(job.job.held.as_deref(), Some(JOB_HELD_FOR_DEPLOY));
            assert_eq!(job.job.started_at, None);
            jobs.push(job);
        }
        assert!(runtime.launched.lock().unwrap().is_empty(), "no unit");
        assert!(admission_expired(
            &jobs[0].job,
            now,
            admission_cap_secs(&drain),
            submitted
        ));
        for _ in 0..2 {
            assert_eq!(
                start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                    .await
                    .unwrap(),
                0
            );
        }
        assert!(runtime.launched.lock().unwrap().is_empty());
        {
            let guard = store.lock().await;
            for job in &jobs {
                let stored = guard.get_agent_job(job.job.id).unwrap().unwrap().job;
                assert_eq!(stored.state, JobState::Queued, "held, not timed out");
                assert_eq!(stored.started_at, None);
            }
            assert!(wakes_for(&guard, first_owner).is_empty());
            assert!(wakes_for(&guard, second_owner).is_empty());
        }

        // The deploy settles without timing out (the runner finds no live deploy
        // and drops its retry window, `reconcile_retry_hold`, #2551): both start,
        // in submission order, and their timeout clock begins only now.
        drain.sync(None, true, Utc::now());
        drain.clear_retry_hold(row.id);
        assert!(!drain.holds_job_start(Some(first_owner), true));
        assert!(drain.job_release_by().is_none());
        let released = Utc::now();
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            2
        );
        let launched = runtime.launched.lock().unwrap().clone();
        assert_eq!(
            launched
                .iter()
                .map(|spec| spec.unit_name.clone())
                .collect::<Vec<_>>(),
            jobs.iter()
                .map(|job| job.job.unit_name.clone())
                .collect::<Vec<_>>()
        );
        {
            let guard = store.lock().await;
            for job in &jobs {
                let stored = guard.get_agent_job(job.job.id).unwrap().unwrap().job;
                assert_eq!(stored.state, JobState::Running);
                assert_eq!(stored.held, None);
                let started = DateTime::parse_from_rfc3339(&stored.started_at.unwrap()).unwrap();
                assert!(
                    started.with_timezone(&Utc) >= released,
                    "clock starts at launch"
                );
            }
            assert!(wakes_for(&guard, first_owner).is_empty(), "no wake yet");
        }

        // Each job's one wake fires at its real terminal state.
        for job in &jobs {
            std::fs::write(&job.status_path, "0\n").unwrap();
        }
        *runtime.active.lock().unwrap() = false;
        assert_eq!(
            poll_once(&store, &job_runtime, Utc::now()).await.unwrap(),
            2
        );
        assert_eq!(
            poll_once(&store, &job_runtime, Utc::now()).await.unwrap(),
            0
        );
        let guard = store.lock().await;
        for (job, owner) in jobs.iter().zip([first_owner, second_owner]) {
            assert_eq!(
                guard.get_agent_job(job.job.id).unwrap().unwrap().job.state,
                JobState::Succeeded
            );
            assert_eq!(wakes_for(&guard, owner).len(), 1, "exactly one wake");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_abandoned_or_expired_deploy_releases_its_held_jobs() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let drain = crate::deploy_drain::DeployDrain::new();
        let row = engage_job_hold(
            &*store.lock().await,
            &drain,
            Utc::now() - chrono::Duration::minutes(30),
        );
        let owner = Uuid::new_v4();
        let held = held_test_job(&*store.lock().await, &*runtime, dir.path(), owner);
        assert_eq!(held.job.held.as_deref(), Some(JOB_HELD_FOR_DEPLOY));
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            0
        );

        // The owner abandons the deploy: its hold releases at once.
        drain.release(row.id);
        assert!(!drain.holds_job_start(Some(owner), true));
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            1
        );
        assert_eq!(runtime.launched.lock().unwrap().len(), 1);

        // #2551: a deploy that reaches its deadline unsettled does not release its
        // jobs there: the hold continues into the retry window (the one clock
        // boundary, the deadline) and ends by itself only when the window does.
        drain.hold_jobs_until_settled(&row, Utc::now());
        assert!(drain.holds_job_start(Some(owner), true));
        let window_end = crate::deploy_drain::retry_hold_until(row.deadline_at);
        let ms = chrono::Duration::milliseconds(1);
        for at in [
            row.deadline_at - ms,
            row.deadline_at,
            row.deadline_at + ms,
            window_end - ms,
        ] {
            assert!(
                drain.holds_job_start_at(Some(owner), true, at),
                "still held at {at}"
            );
            assert!(drain.holds_job_start_at(None, false, at), "{at}");
        }
        assert!(!drain.holds_job_start_at(Some(owner), true, window_end));
    }

    /// #2551: the jobs loop never admits a held job between the staged deploy's
    /// deadline and the runner's timeout settlement. The deploy here is past its
    /// deadline and still unsettled (`staged`), as when the runner has not polled.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_jobs_loop_admits_nothing_between_the_deadline_and_the_timeout_poll() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let drain = crate::deploy_drain::DeployDrain::new();
        let owner = Uuid::new_v4();
        let held = held_test_job(&*store.lock().await, &*runtime, dir.path(), owner);
        let row = crate::store::agent_deploys::DeployRow {
            id: Uuid::new_v4(),
            owner_session_id: Some(Uuid::new_v4()),
            sha: "0".repeat(40),
            manifest: Vec::new(),
            state: rsi_common::agent_deploy::DeployState::Staged,
            reason: None,
            deadline_at: Utc::now() + chrono::Duration::milliseconds(300),
            operator: false,
            forced: false,
        };
        drain.hold_jobs_until_settled(&row, Utc::now());
        // Before the deadline: held by the staged hold.
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            0
        );
        // Past the deadline, the staged hold's own clock has expired but the
        // retry hold carries on: still nothing starts.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(Utc::now() > row.deadline_at);
        assert!(drain.job_release_by().is_some_and(|end| end > Utc::now()));
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            0,
            "the job stays held past the deadline"
        );
        assert!(runtime.launched.lock().unwrap().is_empty());
        let queued = store
            .lock()
            .await
            .get_agent_job(held.job.id)
            .unwrap()
            .unwrap();
        assert_eq!(queued.job.state, JobState::Queued);
        assert_eq!(queued.job.held.as_deref(), Some(JOB_HELD_FOR_DEPLOY));
        // The deploy is cancelled: its hold goes at once.
        drain.release(row.id);
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            1
        );
    }

    /// #2552: a daemon that restarts after a staged `interrupt_workers` deploy
    /// timed out restores the rest of the retry window from the durable row
    /// before the jobs loop admits anything. The window ends at its original
    /// time (deadline + `DEPLOY_JOB_RETRY_HOLD_SECS`) however late the restart is.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_restart_after_a_timed_out_interrupt_deploy_restores_the_original_retry_window() {
        use crate::deploy::{GateState, PollOutcome, poll_once};
        let dir = disk_fixture();
        let path = dir.path().join("retry-window.db");
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let first = Uuid::new_v4();
        let (service, _restarts, manifest) = restart_counting_service(dir.path(), first);
        let t0 = Utc::now();
        let store = Arc::new(tokio::sync::Mutex::new(
            Store::open_test_fixture(&path).unwrap(),
        ));
        // A running build keeps the deploy from reaching its quiet point.
        let (build, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            ctx(Uuid::new_v4(), dir.path(), None),
            t0,
        )
        .unwrap();
        assert_eq!(build.job.state, JobState::Running);
        stage_deploy_waiting(&*store.lock().await, true, t0, 60, first, &manifest);
        let held_owner = Uuid::new_v4();
        let held = held_test_job(&*store.lock().await, &*runtime, dir.path(), held_owner);

        let drain = crate::deploy_drain::DeployDrain::new();
        drain.set_hold_cap_secs(60);
        let mut gate = GateState::default();
        let timed_out = t0 + chrono::Duration::seconds(61);
        assert!(matches!(
            poll_once(&store, &service, t0, &mut gate, &drain, true)
                .await
                .unwrap(),
            PollOutcome::Waiting(_)
        ));
        assert_eq!(
            poll_once(&store, &service, timed_out, &mut gate, &drain, true)
                .await
                .unwrap(),
            PollOutcome::Settled(first, rsi_common::agent_deploy::DeployState::TimedOut)
        );
        let deadline = store
            .lock()
            .await
            .get_agent_deploy(first)
            .unwrap()
            .unwrap()
            .deadline_at;
        let window_end = crate::deploy_drain::retry_hold_until(deadline);
        drop(store);

        // The unplanned restart: a fresh drain, the same durable database.
        let store = Arc::new(tokio::sync::Mutex::new(Store::open(&path).unwrap()));
        let drain = crate::deploy_drain::DeployDrain::new();
        let config = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let started =
            crate::deploy::start_admission_after_restore(&store, &drain, &config, || async {
                start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                    .await
                    .unwrap()
            })
            .await
            .unwrap();
        assert_eq!(started, 0, "the restored window holds the first admission");
        assert!(
            runtime.launched.lock().unwrap().len() <= 1,
            "only the build"
        );
        assert_eq!(drain.retry_hold_owner(), Some(first));
        // 20 s after the timeout the rest of the window is still held, to the
        // original end and not a second past it.
        let ms = chrono::Duration::milliseconds(1);
        for at in [
            deadline + chrono::Duration::seconds(20),
            deadline + chrono::Duration::seconds(299),
            window_end - ms,
        ] {
            assert!(drain.holds_job_start_at(Some(held_owner), true, at), "{at}");
            assert!(drain.holds_job_start_at(None, false, at), "{at}");
        }
        assert!(!drain.holds_job_start_at(Some(held_owner), true, window_end));
        // A second restore, later, derives the same end: a restart never extends.
        crate::deploy::restore_drain_at_startup(
            &store,
            &drain,
            &config,
            deadline + chrono::Duration::seconds(250),
        )
        .await
        .unwrap();
        assert!(!drain.holds_job_start_at(Some(held_owner), true, window_end));
        let queued = store
            .lock()
            .await
            .get_agent_job(held.job.id)
            .unwrap()
            .unwrap();
        assert_eq!(queued.job.state, JobState::Queued);

        // A deploy that settled without timing out leaves no window to restore.
        let succeeded = {
            let guard = store.lock().await;
            let later = t0 + chrono::Duration::seconds(2);
            let id = stage_deploy_waiting(&guard, true, later, 60, Uuid::new_v4(), &manifest);
            guard
                .settle_agent_deploy(
                    id,
                    rsi_common::agent_deploy::DeployState::Succeeded,
                    None,
                    later,
                )
                .unwrap();
            id
        };
        let fresh = crate::deploy_drain::DeployDrain::new();
        crate::deploy::restore_drain_at_startup(&store, &fresh, &config, Utc::now())
            .await
            .unwrap();
        assert_eq!(fresh.retry_hold_owner(), None, "deploy {succeeded}");
        assert!(!fresh.holds_job_start_at(Some(held_owner), true, deadline));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_held_job_survives_store_reopen_and_starts_once_across_restarts() {
        let dir = disk_fixture();
        let path = dir.path().join("held-job.db");
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let store = Store::open_test_fixture(&path).unwrap();
        let row = held_test_job(&store, &*runtime, dir.path(), Uuid::new_v4());
        drop(store);

        let store = Arc::new(tokio::sync::Mutex::new(Store::open(&path).unwrap()));
        let drain = crate::deploy_drain::DeployDrain::new();
        engage_drain(&drain);
        let restored = store
            .lock()
            .await
            .get_agent_job(row.job.id)
            .unwrap()
            .unwrap();
        assert_eq!(restored.job.state, JobState::Queued);
        assert_eq!(restored.job.held.as_deref(), Some("held_for_deploy"));
        assert_eq!(restored.job.started_at, None);
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            0
        );
        drain.sync(None, true, Utc::now());
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            1
        );
        drop(store);

        let store = Arc::new(tokio::sync::Mutex::new(Store::open(&path).unwrap()));
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            0
        );
        let restored = store
            .lock()
            .await
            .get_agent_job(row.job.id)
            .unwrap()
            .unwrap();
        assert_eq!(restored.job.state, JobState::Running);
        assert!(restored.job.started_at.is_some());
        let launched = runtime.launched.lock().unwrap();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].unit_name, row.job.unit_name);
    }

    /// #2201: a daemon that restarts while a deploy is staged restores the hold
    /// from the durable row before the jobs loop's first admission, so a job
    /// queued behind the hold stays queued instead of starting and being stopped
    /// by the deploy after its grace. Both holds: a staged `interrupt_workers`
    /// deploy (every job start) and a plain one (a worker's job start).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_restart_restores_the_staged_deploy_hold_before_the_jobs_loop_admits_a_held_job() {
        for interrupt_workers in [true, false] {
            let dir = disk_fixture();
            let path = dir.path().join("restart-order.db");
            let runtime = Arc::new(FakeRuntime::default());
            let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
            let owner = Uuid::new_v4();
            let store = Store::open_test_fixture(&path).unwrap();
            let row = held_test_job(&store, &*runtime, dir.path(), owner);
            let deploy = stage_deploy(&store, interrupt_workers, Utc::now());
            drop(store);

            // The restarted daemon: durable state survives, the drain does not.
            let store = Arc::new(tokio::sync::Mutex::new(Store::open(&path).unwrap()));
            let drain = crate::deploy_drain::DeployDrain::new();
            assert!(
                !drain.holds_job_start(Some(owner), true),
                "a fresh drain holds nothing: the window the startup order must close"
            );
            let config =
                crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
            let started =
                crate::deploy::start_admission_after_restore(&store, &drain, &config, || async {
                    start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                        .await
                        .unwrap()
                })
                .await
                .unwrap();
            assert_eq!(started, 0, "interrupt_workers {interrupt_workers}");
            assert!(runtime.launched.lock().unwrap().is_empty());
            let queued = store
                .lock()
                .await
                .get_agent_job(row.job.id)
                .unwrap()
                .unwrap();
            assert_eq!(queued.job.state, JobState::Queued);
            assert_eq!(queued.job.held.as_deref(), Some("held_for_deploy"));
            assert_eq!(queued.job.started_at, None);

            // The deploy settles; the hold is derived from the row, so the job
            // starts exactly once.
            store
                .lock()
                .await
                .settle_agent_deploy(
                    deploy,
                    rsi_common::agent_deploy::DeployState::Succeeded,
                    None,
                    Utc::now(),
                )
                .unwrap();
            // The daemon restarts again: a fresh drain, so the staged deploy's
            // retry window (installed with its hold) is not carried over.
            let drain = crate::deploy_drain::DeployDrain::new();
            crate::deploy::restore_drain_at_startup(&store, &drain, &config, Utc::now())
                .await
                .unwrap();
            assert_eq!(
                start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(runtime.launched.lock().unwrap().len(), 1);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_held_job_is_queued_without_a_unit_and_starts_when_the_drain_ends() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let drain = crate::deploy_drain::DeployDrain::new();
        engage_drain(&drain);
        let owner = Uuid::new_v4();
        let row = held_test_job(&*store.lock().await, &*runtime, dir.path(), owner);
        assert_eq!(row.job.state, JobState::Queued);
        assert_eq!(row.job.held.as_deref(), Some("held_for_deploy"));
        assert!(runtime.launched.lock().unwrap().is_empty());
        // #1608: the held job's log already says where it waits.
        let held_phases = read_log_phases(&row.job.log_path);
        assert_eq!(held_phases.len(), 1, "{held_phases:?}");
        assert!(
            held_phases[0]
                .text
                .starts_with("queue-wait: held by the deploy drain"),
            "{held_phases:?}"
        );
        // A queued job is neither polled as running nor swept as terminal.
        assert!(
            store
                .lock()
                .await
                .list_running_agent_jobs()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            poll_once(&store, &job_runtime, Utc::now()).await.unwrap(),
            0
        );

        // Still draining: nothing starts.
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            0
        );
        assert!(runtime.launched.lock().unwrap().is_empty());

        // The hold ends: the job starts once, stamped, with the same unit.
        drain.sync(None, true, Utc::now());
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            0,
            "a second poll never double-launches"
        );
        let launched = runtime.launched.lock().unwrap().clone();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].unit_name, row.job.unit_name);
        let stored = store
            .lock()
            .await
            .get_agent_job(row.job.id)
            .unwrap()
            .unwrap()
            .job;
        assert_eq!(stored.state, JobState::Running);
        assert_eq!(stored.held, None);
        assert!(stored.started_at.is_some());
        // The launch phase follows the hold phase, before any wrapper output.
        let phases = read_log_phases(&row.job.log_path);
        assert_eq!(phases.len(), 2, "{phases:?}");
        assert!(phases[0].text.starts_with("queue-wait:"), "{phases:?}");
        assert!(
            phases[1]
                .text
                .starts_with("unit-launch: requesting service"),
            "{phases:?}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_submitted_job_logs_its_launch_phase_before_the_unit_exists() {
        let store = Store::open_in_memory().expect("store");
        let dir = disk_fixture();
        let runtime = FakeRuntime::default();
        let mut c = ctx(Uuid::new_v4(), dir.path(), None);
        c.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[]}),
        );
        let (row, _) = submit(&store, &runtime, &tools(), dir.path(), c, Utc::now()).unwrap();
        // The fake runtime never runs the wrapper, so the daemon's line is the
        // only evidence, and the summary names it as the last phase.
        let phases = read_log_phases(&row.job.log_path);
        assert_eq!(phases.len(), 1, "{phases:?}");
        assert_eq!(
            phases[0].text,
            format!("unit-launch: requesting service {}", row.job.unit_name)
        );
        assert!(phase_summary(&phases, Utc::now()).contains("last phase `unit-launch"));
        let log = std::fs::read_to_string(&row.job.log_path).unwrap();
        assert!(log.lines().any(
            |line| line == format!("rsi-build-target {}", dir.path().join("target").display())
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_held_job_cancels_without_a_unit_and_never_starts() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let drain = crate::deploy_drain::DeployDrain::new();
        engage_drain(&drain);
        let row = held_test_job(&*store.lock().await, &*runtime, dir.path(), Uuid::new_v4());
        let (job, cancelled) =
            cancel_job(&*store.lock().await, &*runtime, row.job.id, Utc::now()).unwrap();
        assert!(cancelled);
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.result.unwrap().refusal.as_deref(), Some(JOB_CANCELLED));
        assert!(runtime.stopped.lock().unwrap().is_empty());
        drain.sync(None, true, Utc::now());
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            0
        );
        assert!(runtime.launched.lock().unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_held_job_whose_launch_fails_settles_failed_and_wakes_its_owner() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let drain = crate::deploy_drain::DeployDrain::new();
        let owner = Uuid::new_v4();
        engage_drain(&drain);
        let row = held_test_job(&*store.lock().await, &*runtime, dir.path(), owner);
        *runtime.fail_launch.lock().unwrap() = true;
        drain.sync(None, true, Utc::now());
        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            0
        );
        let guard = store.lock().await;
        let stored = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(stored.state, JobState::Failed);
        assert_eq!(
            stored.result.unwrap().refusal.as_deref(),
            Some(JOB_LAUNCH_FAILED)
        );
        assert_eq!(wakes_for(&guard, owner).len(), 1, "the owner is told");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_wall_clock_limit_counts_from_the_launch_not_from_the_hold() {
        let (store, dir) = open();
        let runtime = FakeRuntime::default();
        let row = held_test_job(&store.blocking_lock(), &runtime, dir.path(), Uuid::new_v4());
        let mut job = row.job;
        let JobParams::Test(test) = &mut job.params else {
            unreachable!("a test job");
        };
        test.timeout_minutes = Some(30);
        job.created_at = "2020-01-01T00:00:00.000000000Z".into();
        let now = Utc::now();
        assert!(timed_out(&job, now), "never started: counts from creation");
        job.started_at = Some(now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
        assert!(
            !timed_out(&job, now),
            "a job held for hours has its whole limit once it starts"
        );
    }

    /// #1921: a peer can use its full run budget without expiring the rest of
    /// its batch. The durable FIFO survives restart and launches one at a time.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn same_sandbox_peers_queue_before_unit_launch_without_spending_admission() {
        let dir = disk_fixture();
        let db = dir.path().join("peers.db");
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let old = Utc::now() - chrono::Duration::minutes(30);
        let store = Store::open_test_fixture(&db).unwrap();
        let mut rows = Vec::new();
        for key in ["first", "second", "third"] {
            let mut c = timed_test_ctx(owner, dir.path(), Some(20));
            c.idempotency_key = Some(key.into());
            c.wake = JobWake::None;
            let (row, replayed) =
                submit(&store, &*runtime, &tools(), dir.path(), c.clone(), old).unwrap();
            assert!(!replayed);
            let (again, replayed) =
                submit(&store, &*runtime, &tools(), dir.path(), c, old).unwrap();
            assert!(replayed);
            assert_eq!(again.job.id, row.job.id);
            rows.push(row);
        }
        assert_eq!(rows[0].job.state, JobState::Running);
        for row in &rows[1..] {
            assert_eq!(row.job.state, JobState::Queued);
            assert_eq!(row.job.held.as_deref(), Some(SANDBOX_JOB_QUEUED));
            assert_eq!(row.job.started_at, None);
            assert!(!job_tmp_dir(Path::new(&row.job.log_path)).exists());
        }
        assert_eq!(runtime.launched.lock().unwrap().len(), 1);
        // The batch is thirty minutes old, but its live holder is fifteen
        // minutes into a twenty-minute run and is still writing output.
        let running_since = Utc::now() - chrono::Duration::minutes(15);
        append_phase(&rows[0].job.log_path, running_since, "job-start");
        append_phase(
            &rows[0].job.log_path,
            running_since,
            "slot-granted: running",
        );
        append_job_log(&rows[0].job.log_path, "test progress continues\n");
        drop(store);
        let store = Arc::new(tokio::sync::Mutex::new(Store::open(&db).unwrap()));
        let drain = crate::deploy_drain::DeployDrain::new();
        assert_eq!(
            start_queued_jobs(&store, &dynamic, &tools(), &drain)
                .await
                .unwrap(),
            0
        );
        assert_eq!(runtime.launched.lock().unwrap().len(), 1);

        for (index, row) in rows.iter().enumerate() {
            std::fs::write(&row.status_path, "0\n").unwrap();
            *runtime.active.lock().unwrap() = false;
            assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 1);
            let guard = store.lock().await;
            let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
            assert_eq!(job.state, JobState::Succeeded);
            assert_eq!(job.result.unwrap().exit_code, Some(0));
            drop(guard);
            let expected = usize::from(index + 1 < rows.len());
            assert_eq!(
                start_queued_jobs(&store, &dynamic, &tools(), &drain)
                    .await
                    .unwrap(),
                expected
            );
            if expected == 1 {
                let guard = store.lock().await;
                let next = guard
                    .get_agent_job(rows[index + 1].job.id)
                    .unwrap()
                    .unwrap()
                    .job;
                assert_eq!(next.state, JobState::Running);
                assert_eq!(next.held, None);
                assert!(next.started_at.is_some());
                let started = Utc::now();
                append_phase(&next.log_path, started, "job-start");
                append_phase(&next.log_path, started, "slot-granted: running");
                assert!(!timed_out(&next, started + chrono::Duration::minutes(19)));
                assert!(timed_out(&next, started + chrono::Duration::minutes(21)));
            }
        }
        assert_eq!(
            runtime
                .launched
                .lock()
                .unwrap()
                .iter()
                .map(|spec| &spec.unit_name)
                .collect::<Vec<_>>(),
            rows.iter()
                .map(|row| &row.job.unit_name)
                .collect::<Vec<_>>()
        );
        assert!(wakes_for(&*store.lock().await, owner).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn queued_peers_fail_with_the_dead_or_stalled_holder_identity() {
        for active in [false, true] {
            let (store, dir) = open();
            let runtime = Arc::new(FakeRuntime::default());
            let dynamic: Arc<dyn JobRuntime> = runtime.clone();
            let owner = Uuid::new_v4();
            let old = Utc::now() - chrono::Duration::minutes(30);
            let mut rows = Vec::new();
            for _ in 0..3 {
                rows.push(
                    submit(
                        &*store.lock().await,
                        &*runtime,
                        &tools(),
                        dir.path(),
                        ctx(owner, dir.path(), None),
                        old,
                    )
                    .unwrap()
                    .0,
                );
            }
            let holder = &rows[0];
            std::fs::File::options()
                .write(true)
                .open(&holder.job.log_path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old.into()))
                .unwrap();
            *runtime.active.lock().unwrap() = active;
            let drain = crate::deploy_drain::DeployDrain::new();
            // Even an unhealthy peer cannot bypass a deploy hold (#1969).
            engage_drain(&drain);
            assert_eq!(
                start_queued_jobs(&store, &dynamic, &tools(), &drain)
                    .await
                    .unwrap(),
                0
            );
            for row in &rows[1..] {
                assert_eq!(
                    store
                        .lock()
                        .await
                        .get_agent_job(row.job.id)
                        .unwrap()
                        .unwrap()
                        .job
                        .state,
                    JobState::Queued
                );
            }
            drain.sync(None, true, Utc::now());
            assert_eq!(
                start_queued_jobs(&store, &dynamic, &tools(), &drain)
                    .await
                    .unwrap(),
                0
            );
            // Settlement and wake are idempotent on the next poll.
            assert_eq!(
                start_queued_jobs(&store, &dynamic, &tools(), &drain)
                    .await
                    .unwrap(),
                0
            );
            let guard = store.lock().await;
            for row in &rows[1..] {
                let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
                assert_eq!(job.state, JobState::Failed);
                assert_eq!(job.started_at, None);
                let result = job.result.unwrap();
                assert_eq!(
                    result.refusal.as_deref(),
                    Some(if active {
                        JOB_SANDBOX_HOLDER_STALLED
                    } else {
                        JOB_SANDBOX_HOLDER_DEAD
                    })
                );
                let detail = result.detail.unwrap();
                assert!(detail.contains(&holder.job.id.to_string()), "{detail}");
                assert!(detail.contains(&holder.job.unit_name), "{detail}");
                assert!(detail.contains(&holder.job.log_path), "{detail}");
                assert!(
                    detail.contains("execution timeout never started"),
                    "{detail}"
                );
            }
            assert_eq!(
                guard
                    .get_agent_job(holder.job.id)
                    .unwrap()
                    .unwrap()
                    .job
                    .state,
                JobState::Running
            );
            assert_eq!(wakes_for(&guard, owner).len(), 2);
            assert_eq!(runtime.launched.lock().unwrap().len(), 1);
            assert!(runtime.stopped.lock().unwrap().is_empty());
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn sandbox_holder_health_respects_progress_launch_grace_and_completion() {
        let store = Store::open_in_memory().unwrap();
        let dir = disk_fixture();
        let runtime = FakeRuntime::default();
        let now = Utc::now();
        let (row, _) = submit(
            &store,
            &runtime,
            &tools(),
            dir.path(),
            ctx(Uuid::new_v4(), dir.path(), None),
            now,
        )
        .unwrap();
        // Pin the log's progress clock to the fixture's `now`: submit stamps
        // the real mtime a few ms later, which would shave the stall boundary.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&row.job.log_path)
            .unwrap()
            .set_modified(now.into())
            .unwrap();
        *runtime.active.lock().unwrap() = false;
        assert!(
            sandbox_holder_failure(&row, &runtime, now + chrono::Duration::seconds(59)).is_none()
        );
        assert_eq!(
            sandbox_holder_failure(&row, &runtime, now + chrono::Duration::seconds(61))
                .unwrap()
                .refusal
                .as_deref(),
            Some(JOB_SANDBOX_HOLDER_DEAD)
        );
        *runtime.active.lock().unwrap() = true;
        assert!(
            sandbox_holder_failure(&row, &runtime, now + chrono::Duration::seconds(599)).is_none()
        );
        assert_eq!(
            sandbox_holder_failure(&row, &runtime, now + chrono::Duration::seconds(601))
                .unwrap()
                .refusal
                .as_deref(),
            Some(JOB_SANDBOX_HOLDER_STALLED)
        );
        // A completed wrapper awaits normal reconciliation, regardless of age
        // or service state; it must not fail the next peer.
        std::fs::write(&row.status_path, "0\n").unwrap();
        *runtime.active.lock().unwrap() = false;
        assert!(sandbox_holder_failure(&row, &runtime, now + chrono::Duration::hours(1)).is_none());

        // Output cannot extend a holder's own execution budget.
        let (timed, _) = submit(
            &store,
            &runtime,
            &tools(),
            dir.path(),
            timed_test_ctx(Uuid::new_v4(), dir.path(), Some(20)),
            now - chrono::Duration::minutes(21),
        )
        .unwrap();
        append_phase(
            &timed.job.log_path,
            now - chrono::Duration::minutes(21),
            "job-start",
        );
        append_job_log(&timed.job.log_path, "still writing output\n");
        let result = sandbox_holder_failure(&timed, &runtime, now).unwrap();
        assert_eq!(result.refusal.as_deref(), Some(JOB_SANDBOX_HOLDER_STALLED));
        assert!(result.detail.unwrap().contains("budget is exhausted"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn cancelling_a_queued_peer_unblocks_its_successor_after_the_running_peer() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let mut rows = Vec::new();
        for _ in 0..3 {
            rows.push(
                submit(
                    &*store.lock().await,
                    &*runtime,
                    &tools(),
                    dir.path(),
                    ctx(owner, dir.path(), None),
                    Utc::now(),
                )
                .unwrap()
                .0,
            );
        }
        let (cancelled, did_cancel) =
            cancel_job(&*store.lock().await, &*runtime, rows[1].job.id, Utc::now()).unwrap();
        assert!(did_cancel);
        assert_eq!(
            cancelled.result.unwrap().refusal.as_deref(),
            Some(JOB_CANCELLED)
        );
        assert!(runtime.stopped.lock().unwrap().is_empty());
        let drain = crate::deploy_drain::DeployDrain::new();
        assert_eq!(
            start_queued_jobs(&store, &dynamic, &tools(), &drain)
                .await
                .unwrap(),
            0
        );
        cancel_job(&*store.lock().await, &*runtime, rows[0].job.id, Utc::now()).unwrap();
        assert_eq!(
            start_queued_jobs(&store, &dynamic, &tools(), &drain)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            runtime.launched.lock().unwrap().last().unwrap().unit_name,
            rows[2].job.unit_name
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn independent_job_owners_and_worktrees_can_launch_concurrently() {
        let dir = disk_fixture();
        let store = Store::open_in_memory().unwrap();
        let runtime = FakeRuntime::default();
        let owner = Uuid::new_v4();
        let other = dir.path().join("other-worktree");
        std::fs::create_dir(&other).unwrap();
        for (owner, cwd) in [
            (owner, dir.path()),
            (owner, other.as_path()),
            (Uuid::new_v4(), dir.path()),
        ] {
            let (row, _) = submit(
                &store,
                &runtime,
                &tools(),
                dir.path(),
                ctx(owner, cwd, None),
                Utc::now(),
            )
            .unwrap();
            assert_eq!(row.job.state, JobState::Running);
        }
        assert_eq!(runtime.launched.lock().unwrap().len(), 3);
    }

    /// #2149: the async full-library QA run has no caller waiting on it, so its
    /// build admission lasts as long as its own job timeout (not the ordinary
    /// ten minutes) and is accounted apart from execution, which keeps its
    /// full timeout. (#2211 gave every ordinary test job the same allowance;
    /// the 5-minute job below is the one still at the ten-minute floor.)
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_library_qa_job_waits_for_build_admission_up_to_its_own_timeout() {
        let (store, dir) = open();
        let runtime = FakeRuntime::default();
        let mut job =
            held_test_job(&store.blocking_lock(), &runtime, dir.path(), Uuid::new_v4()).job;
        let ordinary_params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid-core","timeout_minutes":180}),
        );
        let qa_params = params(
            JobKind::Test,
            serde_json::json!({"recipe":LIBRARY_QA_RECIPE,"timeout_minutes":180}),
        );
        let short_params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid-core","timeout_minutes":5}),
        );
        assert_eq!(
            admission_allowance_secs(&short_params),
            JOB_SLOT_WAIT_ALLOWANCE_SECS
        );
        assert_eq!(admission_allowance_secs(&ordinary_params), 180 * 60);
        // The budget equals the job timeout.
        assert_eq!(admission_allowance_secs(&qa_params), 180 * 60);
        let t0 = Utc::now() - chrono::Duration::minutes(400);
        let at = |minutes: i64| t0 + chrono::Duration::minutes(minutes);
        job.started_at = Some(t0.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
        job.log_path = dir.path().join("qa-wait.log").display().to_string();
        let write = |text: String| std::fs::write(&job.log_path, text).unwrap();
        let start = t0.timestamp();
        let mut ordinary = job.clone();
        ordinary.params = short_params;
        job.params = qa_params;
        // 45 minutes queued for a slot: the ordinary job is over its cap, the
        // QA job is still inside its own.
        write(format!(
            "rsi-phase {start} job-start\nrsi-phase {start} slot-wait: queued (build position 2): busy\n"
        ));
        assert!(timed_out(&ordinary, at(45)));
        assert!(!timed_out(&job, at(45)));
        assert!(!timed_out(&job, at(179)));
        // Past its timeout the wait settles as a capacity refusal.
        assert!(timed_out(&job, at(181)));
        let result = timed_out_result(&job, at(181), None);
        assert_eq!(result.refusal.as_deref(), Some(JOB_ADMISSION_TIMED_OUT));
        assert!(
            result
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("180-minute"),
            "{result:?}"
        );
        // A 120-minute wait that was granted leaves the full 180 minutes to run.
        write(format!(
            "rsi-phase {start} job-start\nrsi-phase {start} slot-wait: queued (build position 2): busy\nrsi-phase {} slot-granted: waited 7200s (build); running\n",
            start + 120 * 60
        ));
        assert!(!timed_out(&job, at(300)));
        assert!(timed_out(&job, at(301)));
        assert_eq!(
            timed_out_result(&job, at(301), None).refusal.as_deref(),
            Some(JOB_TIMED_OUT)
        );
        // The unit's runtime cap carries both the execution and the admission budget.
        let dir = disk_fixture();
        std::fs::create_dir(dir.path().join(".rsi")).unwrap();
        std::fs::write(dir.path().join(".rsi/jobs.toml"),
            "version = 1\n[recipes.rolling-library-qa]\nrunner = 'make'\ntarget = 'rolling-library-qa'\ntimeout_minutes = 180\ncpu_quota_percent = 200\n").unwrap();
        let command = job_command(&tools(), &job.params, dir.path(), None).unwrap();
        assert_eq!(
            command.runtime_max_secs,
            2 * 180 * 60 + JOB_TIMEOUT_UNIT_GRACE_SECS
        );
    }

    /// #2211: an ordinary test job queued behind other jobs on the one-slot
    /// governor waits for its turn up to its own timeout, not a fixed ten
    /// minutes, and the wait never spends its execution budget.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_ordinary_test_job_waits_for_its_slot_up_to_its_own_timeout() {
        let (store, dir) = open();
        let runtime = FakeRuntime::default();
        let mut job =
            held_test_job(&store.blocking_lock(), &runtime, dir.path(), Uuid::new_v4()).job;
        job.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":["agent_jobs"],"timeout_minutes":20}),
        );
        let t0 = Utc::now() - chrono::Duration::minutes(120);
        let at = |minutes: i64| t0 + chrono::Duration::minutes(minutes);
        job.started_at = Some(t0.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
        job.log_path = dir.path().join("queued.log").display().to_string();
        let write = |text: String| std::fs::write(&job.log_path, text).unwrap();
        let start = t0.timestamp();
        assert_eq!(admission_allowance_secs(&job.params), 20 * 60);
        // Queued for 15 minutes: the old fixed ten-minute clock would have
        // failed this job; it is still waiting inside its own timeout.
        write(format!(
            "rsi-phase {start} job-start\nrsi-phase {start} slot-wait: governor wait: queued (build position 2)\n"
        ));
        assert!(!timed_out(&job, at(11)));
        assert!(!timed_out(&job, at(19)));
        // Past its timeout without a slot it settles as a capacity refusal.
        assert!(timed_out(&job, at(21)));
        let result = timed_out_result(&job, at(21), None);
        assert_eq!(result.refusal.as_deref(), Some(JOB_ADMISSION_TIMED_OUT));
        assert!(result.detail.unwrap().contains("20-minute"));
        // A 15-minute wait that was granted leaves the full 20 minutes to run.
        write(format!(
            "rsi-phase {start} job-start\nrsi-phase {start} slot-wait: governor wait: queued\nrsi-phase {} slot-granted: waited 900s (build); running\n",
            start + 15 * 60
        ));
        assert!(!timed_out(&job, at(34)));
        assert!(timed_out(&job, at(36)));
        assert_eq!(
            timed_out_result(&job, at(36), None).refusal.as_deref(),
            Some(JOB_TIMED_OUT)
        );
    }

    /// #2211: a `build` job (a manager's release build for a deploy) has no
    /// timeout, so it gets a fixed capacity wait rather than losing a
    /// ten-minute race against the worker test jobs ahead of it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_build_job_waits_for_its_slot_far_longer_than_ten_minutes() {
        let build = params(
            JobKind::Build,
            serde_json::json!({"command":"build","workspace":true,"release":true}),
        );
        assert_eq!(admission_allowance_secs(&build), BUILD_ADMISSION_WAIT_SECS);
        assert!(BUILD_ADMISSION_WAIT_SECS > JOB_SLOT_WAIT_ALLOWANCE_SECS);
        let dir = disk_fixture();
        let command = job_command(&tools(), &build, dir.path(), None).unwrap();
        // cargo-slot's own deadline and the unit's runtime cap carry the wait.
        assert_eq!(command.admission_max_secs, Some(BUILD_ADMISSION_WAIT_SECS));
        assert_eq!(
            command.runtime_max_secs,
            2 * 3600 + BUILD_ADMISSION_WAIT_SECS - JOB_SLOT_WAIT_ALLOWANCE_SECS
        );
        let spec = LaunchSpec {
            unit_name: "rsi-job-build".into(),
            cwd: dir.path().to_owned(),
            log_path: dir.path().join("build.log"),
            status_path: dir.path().join("build.status"),
            command,
            build_environment: None,
        };
        assert!(systemd_run_args(&spec, None).contains(&format!(
            "--setenv=RSI_CARGO_ADMISSION_MAX_SEC={BUILD_ADMISSION_WAIT_SECS}"
        )));
    }

    /// #2211: only unscoped test and build jobs widen the slot wait: a scoped
    /// test counts waits in its wall budget, a short test keeps the ten-minute
    /// floor and a landing is untouched.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn only_unscoped_cargo_jobs_widen_the_governor_wait() {
        let dir = disk_fixture();
        let widened = |value: serde_json::Value| {
            job_command(&tools(), &params(JobKind::Test, value), dir.path(), None)
                .unwrap()
                .admission_max_secs
        };
        assert_eq!(
            widened(serde_json::json!({"package":"rsid","timeout_minutes":45})),
            Some(45 * 60)
        );
        assert_eq!(
            widened(serde_json::json!({"package":"rsid","timeout_minutes":5})),
            None
        );
        assert_eq!(widened(serde_json::json!({"package":"rsid"})), None);
        assert_eq!(
            widened(serde_json::json!({
                "scoped_test":{"base":"origin/rolling"}, "timeout_minutes":60,
            })),
            None
        );
    }

    /// #1756: time queued for a governor build slot or the artifact lock never
    /// counts against the run budget, and is reported on its own.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn slot_and_lock_waits_do_not_count_against_the_job_timeout() {
        let (store, dir) = open();
        let runtime = FakeRuntime::default();
        let row = held_test_job(&store.blocking_lock(), &runtime, dir.path(), Uuid::new_v4());
        let mut job = row.job;
        let JobParams::Test(test) = &mut job.params else {
            unreachable!("a test job");
        };
        test.timeout_minutes = Some(20);
        let now = Utc::now();
        let t0 = now.timestamp() - 35 * 60;
        job.started_at = Some(
            (now - chrono::Duration::minutes(35))
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        );
        job.log_path = dir.path().join("waits.log").display().to_string();
        let write = |text: String| std::fs::write(&job.log_path, text).unwrap();
        // 580 s queued for a slot, within the ten-minute admission cap, then running.
        write(format!(
            "rsi-phase {t0} job-start\nrsi-phase {t0} slot-wait: asking the governor for a build slot\nrsi-phase {} slot-wait: still waiting 60s: busy\nrsi-phase {} slot-granted: waited 580s (build); running\n",
            t0 + 60,
            t0 + 580
        ));
        assert_eq!(log_wait_secs(&job.log_path, now), Some(580));
        // 35 minutes of wall time, 580 s of it queued: over 25 minutes running is over 20.
        assert!(timed_out(&job, now));
        // 29 minutes in, the same wait leaves under 20 running minutes: within the limit.
        let early = now - chrono::Duration::minutes(6);
        assert!(!timed_out(&job, early));
        // An open wait consumes admission only, until its separate cap expires.
        write(format!(
            "rsi-phase {t0} job-start\nrsi-phase {t0} slot-wait: queued (build position 2): busy\n"
        ));
        let within_admission = DateTime::from_timestamp(t0 + 599, 0).unwrap();
        assert_eq!(log_wait_secs(&job.log_path, within_admission), Some(599));
        assert!(!timed_out(&job, within_admission));
        assert_eq!(log_wait_secs(&job.log_path, now), Some(35 * 60));
        assert!(timed_out(&job, now));
        // The allowance bounds how long a stuck queue can extend the timeout.
        let later = now + chrono::Duration::hours(2);
        assert_eq!(log_wait_secs(&job.log_path, later), Some(155 * 60));
        assert!(timed_out(&job, later));
        // No wait phase, no wait report.
        write(format!("rsi-phase {t0} job-start\n"));
        assert_eq!(log_wait_secs(&job.log_path, now), None);
        // The artifact lock counts as a wait too.
        write(format!(
            "rsi-phase {t0} artifact-lock-wait: /tmp/lock\nrsi-phase {} artifact-lock-held: waited 90s\n",
            t0 + 90
        ));
        assert_eq!(log_wait_secs(&job.log_path, now), Some(90));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_job_still_queued_long_after_its_hold_ended_fails_job_admission_timed_out() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let drain = crate::deploy_drain::DeployDrain::new();
        drain.set_hold_cap_secs(600);
        assert_eq!(admission_cap_secs(&drain), 600 + JOB_ADMISSION_MARGIN_SECS);
        let owner = Uuid::new_v4();
        let mut c = ctx(owner, dir.path(), None);
        c.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[],"timeout_minutes":20}),
        );
        // Submitted an hour ago and never launched, and no hold has engaged
        // since: past 10 min + margin since the hold ended (#2190: the clock
        // runs from the later of the submission and the hold's last engagement).
        let old = Utc::now() - chrono::Duration::hours(1);
        drain.note_job_hold_engaged(old);
        let (row, _) = submit_with_hold(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            c,
            true,
            old,
        )
        .unwrap();
        assert_eq!(row.job.state, JobState::Queued);
        // A fresh queued job beside it is within its allowance and launches.
        let fresh = held_test_job(&*store.lock().await, &*runtime, dir.path(), owner);

        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            1,
            "the expired job is failed, not launched; the fresh one starts"
        );
        let guard = store.lock().await;
        let stored = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(stored.state, JobState::Failed);
        let result = stored.result.unwrap();
        assert_eq!(result.refusal.as_deref(), Some(JOB_ADMISSION_TIMED_OUT));
        assert_ne!(result.refusal.as_deref(), Some(JOB_TIMED_OUT));
        assert!(
            result
                .detail
                .unwrap()
                .contains("execution timeout never started")
        );
        assert_eq!(wakes_for(&guard, owner).len(), 1, "the owner is told once");
        assert_eq!(
            guard
                .get_agent_job(fresh.job.id)
                .unwrap()
                .unwrap()
                .job
                .state,
            JobState::Running
        );
        assert_eq!(runtime.launched.lock().unwrap().len(), 1);
        drop(guard);
        // A second pass settles nothing more and wakes nobody again.
        start_queued_jobs(&store, &job_runtime, &tools(), &drain)
            .await
            .unwrap();
        assert_eq!(wakes_for(&*store.lock().await, owner).len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_admission_cap_defaults_when_the_drain_is_uncapped() {
        let drain = crate::deploy_drain::DeployDrain::new();
        assert_eq!(
            admission_cap_secs(&drain),
            i64::try_from(rsi_common::agent_deploy::DEPLOY_DRAIN_HOLD_DEFAULT_SECS).unwrap()
                + JOB_ADMISSION_MARGIN_SECS
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn cancelling_an_unknown_job_is_job_not_found() {
        let (store, _dir) = open();
        let runtime = FakeRuntime::default();
        let guard = store.blocking_lock();
        let error = cancel_job(&guard, &runtime, Uuid::new_v4(), Utc::now()).unwrap_err();
        assert!(error.to_string().contains(JOB_NOT_FOUND), "{error}");
        assert!(runtime.stopped.lock().unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn restart_reconcile_settles_a_finished_unit_once_and_leaves_a_live_one() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let owner = Uuid::new_v4();
        let (finished, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            ctx(owner, dir.path(), None),
            Utc::now(),
        )
        .unwrap();
        let (live, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            ctx(Uuid::new_v4(), dir.path(), None),
            Utc::now(),
        )
        .unwrap();
        // rsid "restarts": a fresh runtime handle knows only what the units did.
        // One unit finished and recorded its status while rsid was down.
        std::fs::write(&finished.status_path, "0\n").unwrap();
        struct PerUnit(String);
        impl JobRuntime for PerUnit {
            fn launch(&self, _: &LaunchSpec) -> std::result::Result<(), String> {
                Ok(())
            }
            fn unit_active(&self, unit: &str) -> bool {
                unit == self.0
            }
        }
        let after_restart: Arc<dyn JobRuntime> = Arc::new(PerUnit(live.job.unit_name.clone()));
        assert_eq!(
            poll_once(&store, &after_restart, Utc::now()).await.unwrap(),
            1
        );
        assert!(!job_tmp_dir(Path::new(&finished.job.log_path)).exists());
        assert!(job_tmp_dir(Path::new(&live.job.log_path)).is_dir());
        // The reconcile and the periodic loop overlap: still one wake.
        assert_eq!(
            poll_once(&store, &after_restart, Utc::now()).await.unwrap(),
            0
        );
        let guard = store.lock().await;
        assert_eq!(wakes_for(&guard, owner).len(), 1);
        assert_eq!(
            guard
                .get_agent_job(finished.job.id)
                .unwrap()
                .unwrap()
                .job
                .state,
            JobState::Succeeded
        );
        assert_eq!(
            guard.get_agent_job(live.job.id).unwrap().unwrap().job.state,
            JobState::Running
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_vanished_unit_without_status_is_lost_after_the_launch_grace() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            ctx(owner, dir.path(), None),
            Utc::now(),
        )
        .unwrap();
        *runtime.active.lock().unwrap() = false;
        append_phase(&row.job.log_path, Utc::now(), "job-start");
        // Within the grace window a not-yet-visible unit is not called lost.
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);
        let later = Utc::now() + chrono::Duration::seconds(LAUNCH_GRACE_SECS + 5);
        assert_eq!(poll_once(&store, &dynamic, later).await.unwrap(), 1);
        assert!(!job_tmp_dir(Path::new(&row.job.log_path)).exists());
        assert_eq!(poll_once(&store, &dynamic, later).await.unwrap(), 0);
        let guard = store.lock().await;
        let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(job.state, JobState::Lost);
        let result = job.result.unwrap();
        assert_eq!(result.refusal.as_deref(), Some("unit_ended_without_status"));
        // #2121: the owner is told rsid did not stop it.
        assert!(
            result
                .detail
                .as_deref()
                .unwrap()
                .contains("rsid did not stop it")
        );
        assert_eq!(wakes_for(&guard, owner).len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_externally_stopped_scoped_test_keeps_its_partial_receipt() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let mut request = ctx(owner, dir.path(), None);
        request.params = params(
            JobKind::Test,
            serde_json::json!({"scoped_test":{"base":"origin/rolling"}}),
        );
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            request,
            Utc::now(),
        )
        .unwrap();
        *runtime.active.lock().unwrap() = false;
        *runtime.diagnostics.lock().unwrap() = Some("Result=signal ExecMainStatus=15".into());
        append_phase(&row.job.log_path, Utc::now(), "job-start");
        let receipt = serde_json::json!({
            "schema": 1,
            "base": "origin/rolling",
            "filters": ["rsid=store::"],
            "packages": [{"package":"rsid", "status":"passed", "completed":94}],
            "ok": false,
            "exit_code": 1,
        });
        append_job_log(
            &row.job.log_path,
            &format!("SCOPED_TEST_RECEIPT {receipt}\n"),
        );
        let later = Utc::now() + chrono::Duration::seconds(LAUNCH_GRACE_SECS + 5);
        assert_eq!(poll_once(&store, &dynamic, later).await.unwrap(), 1);
        let guard = store.lock().await;
        let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(job.state, JobState::Failed);
        let result = job.result.unwrap();
        assert_eq!(result.refusal.as_deref(), Some("stopped_externally"));
        assert!(
            result
                .detail
                .as_deref()
                .unwrap()
                .contains("Result=signal ExecMainStatus=15")
        );
        let receipt = result.receipt.unwrap();
        assert_eq!(receipt["packages"][0]["completed"], 94);
        assert_eq!(receipt["partial"], true);
        assert_eq!(receipt["termination"]["exec_main_status"], 15);
        assert_eq!(wakes_for(&guard, owner).len(), 1);
    }

    fn timed_test_ctx(owner: Uuid, cwd: &Path, timeout: Option<u32>) -> SubmitContext {
        let mut value =
            serde_json::json!({"package":"rsid","filters":["agent_jobs"],"lib_only":true});
        if let Some(minutes) = timeout {
            value["timeout_minutes"] = minutes.into();
        }
        SubmitContext {
            params: params(JobKind::Test, value),
            ..ctx(owner, cwd, None)
        }
    }

    /// #1337: a test job's unit cap follows its timeout (plus the grace that
    /// lets the daemon settle it first); an untimed job keeps the 3 h cap.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_timed_test_job_caps_its_unit_at_the_timeout_plus_grace() {
        let cwd = Path::new("/w");
        let timed = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","timeout_minutes":20}),
        );
        assert_eq!(job_timeout_secs(&timed), Some(1200));
        let command = job_command(&tools(), &timed, cwd, None).unwrap();
        // The unit cap also carries the slot-wait allowance (#1756): the daemon,
        // which can tell a wait from a run, decides the timeout first.
        // #2211: the queue wait is bounded by the job's own timeout, and the
        // unit cap carries it on top of the execution budget.
        assert_eq!(
            command.runtime_max_secs,
            1200 + JOB_TIMEOUT_UNIT_GRACE_SECS + admission_allowance_secs(&timed)
        );
        assert_eq!(command.admission_max_secs, Some(1200));
        // The timeout is a daemon field, never a cargo argument.
        assert!(!command.argv.iter().any(|arg| arg.contains("timeout")));
        let untimed = params(JobKind::Test, serde_json::json!({"package":"rsid"}));
        assert_eq!(job_timeout_secs(&untimed), None);
        assert_eq!(
            job_command(&tools(), &untimed, cwd, None)
                .unwrap()
                .runtime_max_secs,
            3 * 3600
        );
    }

    /// #1337: past its timeout a running test job's unit is stopped and the
    /// job settles `failed` with `job_timed_out`, once, waking its owner and
    /// recording a `runaway_process` andon event; within it nothing happens.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_test_job_past_its_timeout_is_stopped_and_fails_job_timed_out() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            timed_test_ctx(owner, dir.path(), Some(20)),
            Utc::now(),
        )
        .unwrap();
        let started = Utc::now().timestamp();
        let granted = started + 120;
        std::fs::write(
            &row.job.log_path,
            format!(
                "rsi-phase {started} job-start\nrsi-phase {started} slot-wait: queued (build position 0): all 4 build slots busy (4 in use); held by pid 7 (make scoped-test)\nrsi-phase {granted} slot-granted: waited 120s (build); running\ntest store::slow_one ... FAILED\n"
            ),
        )
        .unwrap();
        let within = Utc::now() + chrono::Duration::minutes(19);
        assert_eq!(poll_once(&store, &dynamic, within).await.unwrap(), 0);
        assert!(runtime.stopped.lock().unwrap().is_empty());
        // 21 minutes of wall time is only 19 of running once the 2-minute slot
        // wait is excluded (#1756).
        let waited = Utc::now() + chrono::Duration::minutes(21);
        assert_eq!(poll_once(&store, &dynamic, waited).await.unwrap(), 0);
        assert!(runtime.stopped.lock().unwrap().is_empty());
        let past = Utc::now() + chrono::Duration::minutes(23);
        assert_eq!(poll_once(&store, &dynamic, past).await.unwrap(), 1);
        assert_eq!(
            *runtime.stopped.lock().unwrap(),
            vec![row.job.unit_name.clone()]
        );
        assert_eq!(poll_once(&store, &dynamic, past).await.unwrap(), 0);
        let guard = store.lock().await;
        let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(job.state, JobState::Failed);
        let result = job.result.unwrap();
        assert_eq!(result.refusal.as_deref(), Some(JOB_TIMED_OUT));
        assert_eq!(result.slot_wait_secs, Some(120), "reported separately");
        assert!(
            result
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("20-minute timeout")
                    && detail.contains("not counting 120s")),
            "{result:?}"
        );
        // The last phase and how long the job sat in it ride in the result.
        let detail = result.detail.as_deref().unwrap();
        assert!(
            detail.contains("last phase `slot-granted: waited 120s (build); running`"),
            "{detail}"
        );
        assert!(detail.contains("began 126"), "{detail}");
        assert_eq!(wakes_for(&guard, owner).len(), 1);
        let rollup = guard
            .friction_rollup(
                &rsi_common::friction::ListFrictionRollupRequestV1::default(),
                Utc::now() + chrono::Duration::minutes(30),
            )
            .unwrap();
        assert!(
            rollup
                .rows
                .iter()
                .any(|r| r.signature == "runaway_process:job_test:timeout"),
            "{rollup:?}"
        );
    }

    /// #1832: a unit that never wrote `job-start` shows a `start-wait` line
    /// with the unit's state while it waits, and past the timeout settles
    /// `job_start_timed_out` (a start delay with its cause), not `job_timed_out`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_unit_that_never_starts_is_a_start_delay_not_a_test_timeout() {
        struct Stalled(FakeRuntime);
        impl JobRuntime for Stalled {
            fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
                self.0.launch(spec)
            }
            fn unit_active(&self, unit: &str) -> bool {
                self.0.unit_active(unit)
            }
            fn stop_unit(&self, unit: &str) -> std::result::Result<(), String> {
                self.0.stop_unit(unit)
            }
            fn unit_diagnostics(&self, _unit: &str) -> Option<String> {
                Some("ActiveState=activating SubState=start".into())
            }
        }
        let (store, dir) = open();
        let runtime = Arc::new(Stalled(FakeRuntime::default()));
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            timed_test_ctx(owner, dir.path(), Some(20)),
            Utc::now(),
        )
        .unwrap();
        let noted = Utc::now() + chrono::Duration::minutes(3);
        assert_eq!(poll_once(&store, &dynamic, noted).await.unwrap(), 0);
        let text = std::fs::read_to_string(&row.job.log_path).unwrap();
        assert!(
            text.contains("start-wait: no job-start after")
                && text.contains("ActiveState=activating"),
            "{text}"
        );
        // One note, not one per poll.
        assert_eq!(poll_once(&store, &dynamic, noted).await.unwrap(), 0);
        let text = std::fs::read_to_string(&row.job.log_path).unwrap();
        assert_eq!(text.matches("start-wait:").count(), 1, "{text}");
        let within = Utc::now() + chrono::Duration::minutes(9);
        assert_eq!(poll_once(&store, &dynamic, within).await.unwrap(), 0);
        let past = Utc::now() + chrono::Duration::seconds(JOB_START_WAIT_MAX_SECS as i64 + 1);
        assert_eq!(poll_once(&store, &dynamic, past).await.unwrap(), 1);
        let job = store
            .lock()
            .await
            .get_agent_job(row.job.id)
            .unwrap()
            .unwrap()
            .job;
        assert_eq!(job.state, JobState::Failed);
        let result = job.result.unwrap();
        assert_eq!(result.refusal.as_deref(), Some(JOB_START_TIMED_OUT));
        let detail = result.detail.unwrap();
        assert!(
            detail.contains("start or admission delay, not a test timeout")
                && detail.contains("ActiveState=activating"),
            "{detail}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_unit_ending_before_job_start_reports_launch_failure_with_phase() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let now = Utc::now();
        let owner = Uuid::new_v4();
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            timed_test_ctx(owner, dir.path(), Some(5)),
            now,
        )
        .unwrap();
        *runtime.active.lock().unwrap() = false;
        let past = now + chrono::Duration::seconds(LAUNCH_GRACE_SECS + 1);
        assert_eq!(poll_once(&store, &dynamic, past).await.unwrap(), 1);
        assert_eq!(poll_once(&store, &dynamic, past).await.unwrap(), 0);
        let guard = store.lock().await;
        let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(job.state, JobState::Failed);
        let result = job.result.unwrap();
        assert_eq!(result.refusal.as_deref(), Some(JOB_LAUNCH_FAILED));
        let detail = result.detail.unwrap();
        assert!(
            detail.contains("unit ended before its wrapper started"),
            "{detail}"
        );
        assert!(detail.contains("last phase `unit-launch:"), "{detail}");
        assert_eq!(wakes_for(&guard, owner).len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_delayed_batch_gets_its_full_execution_budget_after_admission() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let now = Utc::now();
        let mut rows = Vec::new();
        for _ in 0..8 {
            let (row, _) = submit(
                &*store.lock().await,
                &*runtime,
                &tools(),
                dir.path(),
                timed_test_ctx(Uuid::new_v4(), dir.path(), Some(5)),
                now,
            )
            .unwrap();
            rows.push(row);
        }
        // Independent owners, larger than launch/build capacity: accepted services can wait
        // longer than the five-minute execution limit without spending it.
        let launched = now + chrono::Duration::minutes(4);
        assert_eq!(poll_once(&store, &dynamic, launched).await.unwrap(), 0);
        for row in &rows {
            append_phase(&row.job.log_path, launched, "job-start");
            append_phase(
                &row.job.log_path,
                launched,
                "sandbox-lock-wait: earlier job owns the target",
            );
        }
        let admitted = launched + chrono::Duration::minutes(6);
        assert_eq!(poll_once(&store, &dynamic, admitted).await.unwrap(), 0);
        for row in &rows {
            append_phase(
                &row.job.log_path,
                admitted,
                "sandbox-lock-held: waited 360s",
            );
            append_phase(&row.job.log_path, admitted, "slot-granted: running");
        }
        assert_eq!(
            poll_once(&store, &dynamic, admitted + chrono::Duration::minutes(4))
                .await
                .unwrap(),
            0
        );
        let past = admitted + chrono::Duration::minutes(6);
        assert_eq!(poll_once(&store, &dynamic, past).await.unwrap(), rows.len());
        let guard = store.lock().await;
        for row in &rows {
            let result = guard
                .get_agent_job(row.job.id)
                .unwrap()
                .unwrap()
                .job
                .result
                .unwrap();
            assert_eq!(result.refusal.as_deref(), Some(JOB_TIMED_OUT));
            assert_eq!(result.slot_wait_secs, Some(360));
            assert!(
                result
                    .detail
                    .unwrap()
                    .contains("last phase `slot-granted: running`")
            );
        }
        assert_eq!(
            rows.iter()
                .map(|row| wakes_for(&guard, row.job.owner_session_id).len())
                .sum::<usize>(),
            rows.len()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn build_admission_timeout_retains_the_wait_phase_and_duration() {
        for phase in [
            "slot-wait: governor wait: queued (build position 2)",
            "slot-wait: local load wait: load 14.96 >= 2",
            "slot-wait: memory wait (governor): available 15 GB < 16",
            "slot-wait: memory wait (local): available 15 GB",
            "slot-wait: local slot wait: all 4 build slots busy",
            "artifact-lock-wait: held by the shard runner",
            "sandbox-lock-wait: waiting before cargo admission",
        ] {
            let (store, dir) = open();
            let runtime = Arc::new(FakeRuntime::default());
            let dynamic: Arc<dyn JobRuntime> = runtime.clone();
            let now = Utc::now();
            let owner = Uuid::new_v4();
            let (row, _) = submit(
                &*store.lock().await,
                &*runtime,
                &tools(),
                dir.path(),
                timed_test_ctx(owner, dir.path(), Some(5)),
                now,
            )
            .unwrap();
            append_phase(&row.job.log_path, now, "job-start");
            append_phase(&row.job.log_path, now, phase);
            // Admission can exceed the execution budget, but never ten minutes.
            let within = now + chrono::Duration::seconds(599);
            assert_eq!(poll_once(&store, &dynamic, within).await.unwrap(), 0);
            assert!(*runtime.active.lock().unwrap());
            let past = now + chrono::Duration::seconds(601);
            assert_eq!(poll_once(&store, &dynamic, past).await.unwrap(), 1);
            assert_eq!(poll_once(&store, &dynamic, past).await.unwrap(), 0);
            assert!(!*runtime.active.lock().unwrap());
            let guard = store.lock().await;
            let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
            assert_eq!(job.state, JobState::Failed);
            let result = job.result.unwrap();
            assert_eq!(result.refusal.as_deref(), Some(JOB_ADMISSION_TIMED_OUT));
            assert_eq!(result.slot_wait_secs, Some(601));
            let detail = result.detail.unwrap();
            assert!(
                detail.contains("separate 10-minute build admission limit"),
                "{detail}"
            );
            assert!(
                detail.contains(&format!("last phase `{phase}`")),
                "{detail}"
            );
            assert_eq!(wakes_for(&guard, owner).len(), 1);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn scoped_job_admission_uses_its_wall_budget_and_retains_runtime_reserve() {
        let (store, dir) = open();
        let runtime = FakeRuntime::default();
        let mut job =
            held_test_job(&store.blocking_lock(), &runtime, dir.path(), Uuid::new_v4()).job;
        job.params = params(
            JobKind::Test,
            serde_json::json!({
                "scoped_test": {"base": "origin/rolling"}, "timeout_minutes": 60,
            }),
        );
        let now = Utc::now();
        job.started_at = Some(now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
        append_phase(&job.log_path, now, "job-start");
        append_phase(&job.log_path, now, "slot-wait: all 4 build slots busy");
        assert!(!timed_out(&job, now + chrono::Duration::seconds(601)));
        assert!(!timed_out(&job, now + chrono::Duration::seconds(3479)));
        let past = now + chrono::Duration::seconds(3481);
        assert!(timed_out(&job, past));
        let result = timed_out_result(&job, past, None);
        assert_eq!(result.refusal.as_deref(), Some(JOB_ADMISSION_TIMED_OUT));
        assert_eq!(result.slot_wait_secs, Some(3481));
        assert!(result.detail.unwrap().contains("120s runtime reserve"));
        append_phase(
            &job.log_path,
            now + chrono::Duration::seconds(3479),
            "slot-granted: running",
        );
        assert!(!timed_out(&job, past));
        // A granted slot does not extend the job past its overall wall deadline.
        let expired = now + chrono::Duration::seconds(3601);
        assert!(timed_out(&job, expired));
        assert_eq!(
            timed_out_result(&job, expired, None).refusal.as_deref(),
            Some(JOB_TIMED_OUT)
        );
        // Even a short job's admission ends at the reserve, before its execution cap.
        job.params = params(
            JobKind::Test,
            serde_json::json!({
                "scoped_test": {"base": "origin/rolling"}, "timeout_minutes": 5,
            }),
        );
        append_phase(&job.log_path, now, "sandbox-lock-wait: busy");
        assert!(!timed_out(&job, now + chrono::Duration::seconds(179)));
        assert!(timed_out(&job, now + chrono::Duration::seconds(181)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn cargo_slot_admission_timeout_is_typed_before_a_scoped_receipt_exists() {
        let (store, dir) = open();
        let runtime = FakeRuntime::default();
        let mut ctx = timed_test_ctx(Uuid::new_v4(), dir.path(), Some(20));
        ctx.params = params(
            JobKind::Test,
            serde_json::json!({
                "scoped_test": {"base": "origin/rolling", "head": "HEAD"},
                "timeout_minutes": 20,
            }),
        );
        let (row, _) = submit(
            &*store.blocking_lock(),
            &runtime,
            &tools(),
            dir.path(),
            ctx,
            Utc::now(),
        )
        .unwrap();
        let log = "rsi-phase 1 slot-wait: local load wait: load 14.96 >= 2\nrsi-phase 601 admission-timeout: local load wait after 600s (limit 600s): load 14.96 >= 2\n";
        let (state, result) = classify(&row.job, Some(124), log);
        assert_eq!(state, JobState::Failed);
        assert_eq!(result.refusal.as_deref(), Some(JOB_ADMISSION_TIMED_OUT));
        assert_eq!(result.exit_code, Some(124));
        assert!(
            result
                .detail
                .unwrap()
                .contains("local load wait after 600s")
        );
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn sandbox_admission_serializes_a_batch_and_releases_after_each_job() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("admission.lock");
        let active = dir.path().join("active");
        let mut children = Vec::new();
        for _ in 0..6 {
            children.push(
                Command::new("/bin/sh")
                    .args(["-c", ADMISSION_WAIT, "rsi-admission"])
                    .arg(&lock)
                    .args([
                        "/bin/sh",
                        "-c",
                        "mkdir \"$1\" || exit 91; sleep 0.1; rmdir \"$1\"",
                        "job",
                    ])
                    .arg(&active)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
        }
        let mut waits = 0;
        for child in children {
            let out = child.wait_with_output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "{text}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(text.contains("sandbox-lock-held:"), "{text}");
            waits += usize::from(text.contains("sandbox-lock-wait:"));
        }
        assert!(
            waits > 0,
            "the batch queued and every command ran exclusively"
        );
    }

    /// #1832/#1830: the wrapper's bounded writer must not hold lines back: a
    /// long-running job's `job-start` and early output are in the log while it
    /// is still running.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_wrapper_log_shows_lines_while_the_job_is_still_running() {
        let dir = tempfile::tempdir().unwrap();
        let (log, status) = (dir.path().join("j.log"), dir.path().join("j.status"));
        let mut child = Command::new("/bin/sh")
            .args(["-c", WRAPPER, "rsi-job"])
            .arg(&log)
            .arg(&status)
            .arg("100000")
            .args(["sh", "-c", "echo early-line; sleep 5"])
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        let mut text = String::new();
        while std::time::Instant::now() < deadline {
            text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains("early-line") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(child.try_wait().unwrap().is_none(), "still running");
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            text.contains(" job-start\n") && text.contains("early-line"),
            "{text}"
        );
    }

    /// #1337: a timed job whose unit cap stopped it while the daemon was down
    /// is a timeout, not a lost job.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_timed_job_stopped_by_its_unit_cap_settles_timed_out_not_lost() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            timed_test_ctx(owner, dir.path(), Some(5)),
            Utc::now(),
        )
        .unwrap();
        append_phase(&row.job.log_path, Utc::now(), "job-start");
        *runtime.active.lock().unwrap() = false;
        let past = Utc::now() + chrono::Duration::minutes(11);
        assert_eq!(poll_once(&store, &dynamic, past).await.unwrap(), 1);
        assert!(runtime.stopped.lock().unwrap().is_empty());
        let job = store
            .lock()
            .await
            .get_agent_job(row.job.id)
            .unwrap()
            .unwrap()
            .job;
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.result.unwrap().refusal.as_deref(), Some(JOB_TIMED_OUT));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_job_outlives_the_session_that_submitted_it() {
        // The owner session ends (its row goes terminal) before the unit
        // finishes; the job is untouched and still wakes the owner on finish.
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let dynamic: Arc<dyn JobRuntime> = runtime.clone();
        let owner = Uuid::new_v4();
        let (row, _) = submit(
            &*store.lock().await,
            &*runtime,
            &tools(),
            dir.path(),
            ctx(owner, dir.path(), None),
            Utc::now(),
        )
        .unwrap();
        // Nothing about session state is consulted while the unit runs.
        for _ in 0..3 {
            assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);
        }
        assert_eq!(
            store
                .lock()
                .await
                .get_agent_job(row.job.id)
                .unwrap()
                .unwrap()
                .job
                .state,
            JobState::Running
        );
        std::fs::write(&row.status_path, "0\n").unwrap();
        *runtime.active.lock().unwrap() = false;
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 1);
        assert_eq!(wakes_for(&*store.lock().await, owner).len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_launch_failure_is_reported_synchronously_and_never_wakes() {
        let store = Store::open_in_memory().unwrap();
        let dir = disk_fixture();
        let runtime = FakeRuntime::default();
        *runtime.fail_launch.lock().unwrap() = true;
        let oid = "0123456789abcdef0123456789abcdef01234567";
        for (kind, value) in [
            (JobKind::Test, serde_json::json!({"package":"rsid"})),
            (
                JobKind::Build,
                serde_json::json!({"command":"check","workspace":true}),
            ),
            (JobKind::Landing, serde_json::json!({"accepted":oid})),
            (JobKind::CloudGate, serde_json::json!({"accepted":oid})),
        ] {
            let owner = Uuid::new_v4();
            let mut context = ctx(owner, dir.path(), None);
            context.params = params(kind, value);
            let error =
                submit(&store, &runtime, &tools(), dir.path(), context, Utc::now()).unwrap_err();
            assert!(error.to_string().contains(JOB_LAUNCH_FAILED));
            assert!(store.list_running_agent_jobs().unwrap().is_empty());
            let jobs = store.list_agent_jobs(owner, 10).unwrap();
            assert_eq!(jobs[0].job.state, JobState::Failed);
            assert!(!job_tmp_dir(Path::new(&jobs[0].job.log_path)).exists());
            assert!(wakes_for(&store, owner).is_empty());
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_idempotent_retry_returns_the_original_job_and_launches_once() {
        let store = Store::open_in_memory().unwrap();
        let dir = disk_fixture();
        let runtime = FakeRuntime::default();
        let owner = Uuid::new_v4();
        let first = submit(
            &store,
            &runtime,
            &tools(),
            dir.path(),
            ctx(owner, dir.path(), Some("k")),
            Utc::now(),
        )
        .unwrap();
        let second = submit(
            &store,
            &runtime,
            &tools(),
            dir.path(),
            ctx(owner, dir.path(), Some("k")),
            Utc::now(),
        )
        .unwrap();
        assert!(!first.1 && second.1);
        assert_eq!(first.0.job.id, second.0.job.id);
        assert_eq!(runtime.launched.lock().unwrap().len(), 1);
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// #1099: a candidate receipt runs the fixed wrapper with the ref only, and
    /// settles with the typed receipt in the result and the summary line in the
    /// detail; the wake carries the receipt without the per-file shard map.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_candidate_receipt_job_returns_the_typed_receipt_in_its_result_and_wake() {
        let p = params(
            JobKind::Test,
            serde_json::json!({"candidate_receipt":"rsi/abc-123"}),
        );
        let command = job_command(&tools(), &p, Path::new("/tmp"), None).unwrap();
        assert_eq!(
            command.argv,
            ["scripts/candidate-receipt.sh", "rsi/abc-123"]
        );
        let receipt = serde_json::json!({
            "ok": true, "head": "h", "base": "b", "merge_clean": true,
            "shards": {"compiled_ok": ["store-02"], "map": {"store-02": ["a.rs"]}},
            "migrations": {"new": [151]},
        });
        let summary = "check-touched-shards OK head=h base=b merge_clean=true";
        let log = format!("noise\n{summary}\nRECEIPT_JSON {receipt}\n");
        let job = AgentJobV1 {
            id: Uuid::new_v4(),
            kind: JobKind::Test,
            name: None,
            state: JobState::Running,
            owner_session_id: Uuid::new_v4(),
            unit_name: "u".into(),
            cwd: "/tmp".into(),
            log_path: "/tmp/job.log".into(),
            params: p,
            exit_code: None,
            result: None,
            created_at: Utc::now().to_rfc3339(),
            started_at: None,
            finished_at: None,
            held: None,
            wake: JobWake::Owner,
        };
        let (state, result) = classify(&job, Some(0), &log);
        assert_eq!(state, JobState::Succeeded);
        assert_eq!(result.receipt.as_ref(), Some(&receipt));
        assert_eq!(result.detail.as_deref(), Some(summary));
        let wake = crate::store::agent_jobs::wake_message(&job, state, &result);
        assert!(wake.contains("\"compiled_ok\":[\"store-02\"]"), "{wake}");
        assert!(wake.contains("\"migrations\":{\"new\":[151]}"), "{wake}");
        assert!(!wake.contains("a.rs"), "{wake}");

        // A red receipt still returns the typed receipt but settles failed;
        // no receipt line is a typed refusal.
        let red = log.replace("\"ok\":true", "\"ok\":false");
        let (state, result) = classify(&job, Some(1), &red);
        assert_eq!(state, JobState::Failed);
        assert_eq!(result.receipt.unwrap()["ok"], false);
        let (state, result) = classify(&job, Some(0), "no receipt\n");
        assert_eq!(state, JobState::Failed);
        assert_eq!(result.refusal.as_deref(), Some("candidate_receipt_missing"));
        assert!(result.receipt.is_none());
    }

    fn sweep_job(p: JobParams) -> AgentJobV1 {
        AgentJobV1 {
            id: Uuid::new_v4(),
            kind: JobKind::Build,
            name: None,
            state: JobState::Running,
            owner_session_id: Uuid::new_v4(),
            unit_name: "u".into(),
            cwd: "/tmp".into(),
            log_path: "/tmp/job.log".into(),
            params: p,
            exit_code: None,
            result: None,
            created_at: Utc::now().to_rfc3339(),
            started_at: None,
            finished_at: None,
            held: None,
            wake: JobWake::Owner,
        }
    }

    /// #2692: a `test_no_run` build runs the sweep script in the build slot with
    /// only the validated package, feature and shard names; the daemon never
    /// takes a command line, and the unit is budgeted like any build.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_test_no_run_build_runs_the_shard_sweep_script_in_the_build_slot() {
        let shards: Vec<String> = (1..=13).map(|n| format!("other-{n:02}")).collect();
        let sweep = params(
            JobKind::Build,
            serde_json::json!({"command":"test_no_run","package":"rsid","shards":shards}),
        );
        let command = job_command(&tools(), &sweep, Path::new("/tmp"), None).unwrap();
        assert_eq!(
            command.argv[..7],
            [
                "/x/cargo-slot",
                "env",
                "-u",
                STRIP_NAMESPACE_ENV,
                "scripts/shard-feature-sweep",
                "--package",
                "rsid"
            ]
        );
        let tail: Vec<&str> = command.argv[7..].iter().map(String::as_str).collect();
        assert_eq!(tail.len(), 26);
        assert_eq!(tail[..4], ["--shard", "other-01", "--shard", "other-02"]);
        assert_eq!(tail[24..], ["--shard", "other-13"]);
        // Thirteen sequential builds get more than a plain build's two hours,
        // and the unit's cap stays within the cloud sweep's eight.
        assert!(command.runtime_max_secs > 2 * 3600);
        // The unit cap also carries the build-admission wait, so the sweep's own
        // allowance is checked before that widening.
        let JobParams::Build(build) = &sweep else {
            unreachable!("a build job");
        };
        assert_eq!(shard_sweep_runtime_max_secs(build), 3600 + 13 * 1800);
        assert!(shard_sweep_runtime_max_secs(build) <= 8 * 3600);

        let one = params(
            JobKind::Build,
            serde_json::json!({"command":"test_no_run","package":"rsid-store",
                "no_default_features":true,"features":["test-shard-store-01"]}),
        );
        let command = job_command(&tools(), &one, Path::new("/tmp"), None).unwrap();
        assert_eq!(
            command.argv[5..],
            [
                "--package",
                "rsid-store",
                "--no-default-features",
                "--feature",
                "test-shard-store-01"
            ]
        );
        assert!(command.runtime_max_secs >= 2 * 3600);
        assert!(
            runs_in_build_slice(&one),
            "a shard build is held to the hub build budget"
        );
    }

    /// #2692: the sweep settles on its own receipt, which rides in the result
    /// with per-shard exit codes; a missing or red receipt never reads green.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_shard_sweep_settles_on_its_receipt_with_per_shard_results() {
        let p = params(
            JobKind::Build,
            serde_json::json!({"command":"test_no_run","package":"rsid","shards":["store-01","bad"]}),
        );
        let job = sweep_job(p);
        let receipt = serde_json::json!({
            "ok": false, "partial": false, "package": "rsid",
            "shards": [
                {"shard":"store-01","exit_code":0,"warnings":{"rsid":2}},
                {"shard":"bad","exit_code":101,"warnings":{"rsid":0}},
            ],
            "warnings": null,
        });
        let log = format!("noise\nSHARD_SWEEP_RECEIPT {receipt}\n");
        let (state, result) = classify(&job, Some(1), &log);
        assert_eq!(state, JobState::Failed);
        assert_eq!(result.receipt.as_ref(), Some(&receipt));
        assert_eq!(
            result.detail.as_deref(),
            Some(
                "shard-sweep failed rsid: store-01 exit 0 (2 warnings), bad exit 101 (0 warnings)"
            )
        );
        assert!(result.refusal.is_none());

        let green = log.replace("\"ok\":false", "\"ok\":true");
        let (state, result) = classify(&job, Some(0), &green);
        assert_eq!(state, JobState::Succeeded);
        assert!(result.detail.unwrap().starts_with("shard-sweep ok rsid: "));
        // A clean exit with a red receipt, and a receipt with a failing exit,
        // both settle failed.
        assert_eq!(classify(&job, Some(0), &log).0, JobState::Failed);
        assert_eq!(classify(&job, Some(1), &green).0, JobState::Failed);

        let (state, result) = classify(&job, Some(0), "no receipt\n");
        assert_eq!(state, JobState::Failed);
        assert_eq!(
            result.refusal.as_deref(),
            Some("shard_sweep_receipt_missing")
        );
        assert!(result.receipt.is_none());
        assert_eq!(result.detail.as_deref(), Some("no receipt"));
    }

    /// #1938: the job runs the resolved tool copy by its own path and names it
    /// to the script through `NAME=value` entries placed after `env`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_scoped_test_job_runs_the_resolved_tool_copy_and_names_it() {
        let p = rsi_common::agent_jobs::ScopedTestParams {
            base: "origin/rolling".into(),
            head: None,
        };
        let rev = "a".repeat(40);
        let tool = scoped_tool::ScopedTool::Rolling {
            script: PathBuf::from("/cache/aaaa/scripts/scoped-test"),
            rev: rev.clone(),
            files: vec!["scoped-test".into(), "new-helper.py".into()],
        };
        let argv = scoped_test_command(&tools(), &p, Some(20), &tool).argv;
        assert_eq!(
            argv[3..8],
            [
                "RSI_SCOPED_TEST_TOOL_SOURCE=rolling".to_string(),
                format!("RSI_SCOPED_TEST_TOOL_REV={rev}"),
                "RSI_SCOPED_TEST_TOOL_FILES=scoped-test,new-helper.py".to_string(),
                "/cache/aaaa/scripts/scoped-test".to_string(),
                "--base".to_string(),
            ]
        );
        let candidate = scoped_tool::ScopedTool::Candidate {
            files: vec!["scoped-test".into(), "new-helper.py".into()],
        };
        let argv = scoped_test_command(&tools(), &p, Some(20), &candidate).argv;
        assert_eq!(argv[3], "RSI_SCOPED_TEST_TOOL_SOURCE=candidate");
        assert_eq!(
            argv[4],
            "RSI_SCOPED_TEST_TOOL_FILES=scoped-test,new-helper.py"
        );
        assert_eq!(argv[5], "scripts/scoped-test");
    }

    /// #161: a requested per-job CPU budget lowers the unit quota, the scoped-test
    /// `--cpu-quota` and the plain cargo job's thread counts before the unit
    /// starts. It never raises what the daemon derived, and omitted changes nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_requested_cpu_budget_lowers_unit_quota_and_parallelism_but_never_raises_them() {
        let cwd = Path::new("/w/sandbox");
        let scoped = |extra: serde_json::Value| {
            let mut value = serde_json::json!({"scoped_test":{"base":"origin/rolling"}});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            params(JobKind::Test, value)
        };
        let cpu_flag = |command: &JobCommand| {
            let at = command
                .argv
                .iter()
                .position(|a| a == "--cpu-quota")
                .unwrap();
            command.argv[at + 1].parse::<u64>().unwrap()
        };
        let base = job_command(&tools(), &scoped(serde_json::json!({})), cwd, None).unwrap();
        let ceiling = base.cpu_quota_percent;
        // Requested 200: the outer unit quota and the nested script budget agree.
        let low = job_command(
            &tools(),
            &scoped(serde_json::json!({"cpu_quota_percent":200})),
            cwd,
            None,
        )
        .unwrap();
        assert_eq!(low.cpu_quota_percent, ceiling.min(200));
        assert_eq!(cpu_flag(&low), cpu_flag(&base).min(ceiling).min(200));
        // The scoped script's cargo-slot holder gets the matching build jobs,
        // after the `env -u NAME` prefix; omitting the request names none.
        let jobs = |command: &JobCommand| {
            command
                .argv
                .iter()
                .position(|a| a.starts_with("RSI_JOB_BUILD_JOBS="))
        };
        assert_eq!(jobs(&base), None);
        let at = jobs(&low).unwrap();
        assert_eq!(&low.argv[..3], &base.argv[..3]);
        assert!(
            at >= 3
                && low.argv[at]
                    .ends_with(&format!("={}", (low.cpu_quota_percent / 100).clamp(1, 4)))
        );
        let one = job_command(
            &tools(),
            &scoped(serde_json::json!({"cpu_quota_percent":100})),
            cwd,
            None,
        )
        .unwrap();
        assert_eq!(one.argv[jobs(&one).unwrap()], "RSI_JOB_BUILD_JOBS=1");
        // A request above the derived ceiling cannot raise it.
        let high = job_command(
            &tools(),
            &scoped(serde_json::json!({"cpu_quota_percent":1600})),
            cwd,
            None,
        )
        .unwrap();
        assert_eq!(
            (high.cpu_quota_percent, cpu_flag(&high)),
            (ceiling, cpu_flag(&base))
        );
        // The unit the systemd command starts carries the lowered quota.
        let spec = LaunchSpec {
            unit_name: "rsi-job-fixture".into(),
            cwd: cwd.to_owned(),
            log_path: "/tmp/fixture.log".into(),
            status_path: "/tmp/fixture.status".into(),
            command: low.clone(),
            build_environment: None,
        };
        assert!(
            systemd_run_args(&spec, None)
                .contains(&format!("--property=CPUQuota={}%", ceiling.min(200)))
        );

        // Plain cargo jobs: same unit quota rule, matching thread counts, and the
        // launch argv after the env prefix is the unchanged cargo-slot command.
        for (kind, value) in [
            (JobKind::Test, serde_json::json!({"package":"rsid"})),
            (
                JobKind::Build,
                serde_json::json!({"command":"check","workspace":true}),
            ),
        ] {
            let omitted = job_command(&tools(), &params(kind, value.clone()), cwd, None).unwrap();
            assert!(
                !omitted
                    .argv
                    .iter()
                    .any(|a| a.starts_with("RSI_JOB_BUILD_JOBS="))
            );
            let mut requested = value;
            requested["cpu_quota_percent"] = serde_json::json!(200);
            let command = job_command(&tools(), &params(kind, requested), cwd, None).unwrap();
            let quota = omitted.cpu_quota_percent.min(200);
            assert_eq!(command.cpu_quota_percent, quota);
            let threads = (quota / 100).clamp(1, 4);
            assert_eq!(
                command.argv[..4],
                [
                    "env".to_string(),
                    format!("RSI_JOB_BUILD_JOBS={threads}"),
                    format!("RUST_TEST_THREADS={threads}"),
                    format!("NEXTEST_TEST_THREADS={threads}"),
                ]
            );
            assert_eq!(command.argv[4..], omitted.argv[..]);
            assert!(threads <= 2);
        }
    }

    /// #2087: the ceiling is half the host threads between 2 and 12 cores.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_scoped_test_cpu_ceiling_scales_with_the_host_within_bounds() {
        // At the default 50% share (#2119) the budget is half the host.
        let default = rsid_core::build_slice::HUB_BUILD_CPU_PERCENT_DEFAULT;
        assert_eq!(scoped_test_cpu_ceiling_for(1, default), 100);
        assert_eq!(scoped_test_cpu_ceiling_for(4, default), 200);
        assert_eq!(scoped_test_cpu_ceiling_for(16, default), 800);
        assert_eq!(scoped_test_cpu_ceiling_for(32, default), 1200);
        assert_eq!(scoped_test_cpu_ceiling_for(128, default), 1200);
        // The whole host as budget leaves the #2087 ceiling alone.
        assert_eq!(scoped_test_cpu_ceiling_for(4, 100), 200);
        assert_eq!(scoped_test_cpu_ceiling_for(32, 100), 1200);
    }

    /// #2119: a smaller hub build share lowers every job's own ceiling, so the
    /// threads N concurrent jobs derive fit the slice budget they share.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_hub_build_share_caps_each_jobs_ceiling() {
        assert_eq!(scoped_test_cpu_ceiling_for(32, 25), 800);
        assert_eq!(scoped_test_cpu_ceiling_for(32, 10), 300);
        assert_eq!(scoped_test_cpu_ceiling_for(8, 10), 100);
        // The default cargo job ceiling (1600%) is capped the same way.
        for (threads, percent, cores) in [
            (32usize, 50u32, 16u64),
            (32, 25, 8),
            (32, 10, 3),
            (8, 50, 4),
        ] {
            assert_eq!(
                rsid_core::build_slice::per_job_ceiling_percent(
                    threads,
                    percent,
                    CARGO_JOB_CPU_QUOTA_PERCENT
                ),
                cores * 100,
                "{threads} threads at {percent}%"
            );
        }
    }

    /// #2119: cargo build and test units are started in `rsi-build.slice`, with
    /// the budget-capped CPU ceiling; units that hold no cargo build are not.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn build_and_test_units_carry_the_hub_build_slice_and_the_budgeted_ceiling() {
        let dir = disk_fixture();
        std::fs::create_dir(dir.path().join(".rsi")).unwrap();
        std::fs::write(dir.path().join(".rsi/jobs.toml"),
            "version = 1\n[recipes.rolling-library-qa]\nrunner = 'make'\ntarget = 'rolling-library-qa'\ntimeout_minutes = 180\ncpu_quota_percent = 200\n").unwrap();
        let env = |dir: &Path| BuildEnvironment {
            tmp_dir: dir.join("tmp"),
            target_dir: dir.join("target"),
            artifact_lock: None,
            admission_lock: None,
            recorded_env: Vec::new(),
        };
        let budget_cores = rsid_core::build_slice::budget_cores(
            host_threads(),
            rsid_core::build_slice::current_percent(),
        );
        for params in [
            params(
                JobKind::Test,
                serde_json::json!({"scoped_test":{"base":"origin/rolling"}}),
            ),
            params(
                JobKind::Test,
                serde_json::json!({"recipe":"rolling-library-qa", "timeout_minutes":180}),
            ),
            params(JobKind::Test, serde_json::json!({"package":"rsid-core"})),
            params(
                JobKind::Build,
                serde_json::json!({"command":"check","workspace":true}),
            ),
        ] {
            assert!(runs_in_build_slice(&params));
            let command = job_command(&tools(), &params, dir.path(), None).unwrap();
            assert!(
                command.cpu_quota_percent <= budget_cores * 100,
                "{params:?}: {}% over the {budget_cores}-core budget",
                command.cpu_quota_percent
            );
            let quota = command.cpu_quota_percent;
            if let Some(at) = command.argv.iter().position(|arg| arg == "--cpu-quota") {
                // scripts/scoped-test sizes its test threads inside this ceiling.
                assert_eq!(command.argv[at + 1], quota.to_string());
            }
            if let Some(threads) = command
                .argv
                .iter()
                .find_map(|arg| arg.strip_prefix("RSI_LIBRARY_QA_TEST_THREADS="))
            {
                assert_eq!(threads, (quota / 100).to_string());
            }
            let spec = LaunchSpec {
                unit_name: "rsi-job-budget".into(),
                cwd: dir.path().to_owned(),
                log_path: dir.path().join("budget.log"),
                status_path: dir.path().join("budget.status"),
                command,
                build_environment: Some(env(dir.path())),
            };
            let args = systemd_run_args(&spec, None);
            assert!(
                args.contains(&"--slice=rsi-build.slice".to_string()),
                "{params:?}: {args:?}"
            );
            assert!(args.contains(&format!("--property=CPUQuota={quota}%")));
        }
    }

    /// #2282: a build unit launched by a process that is not the hub daemon
    /// with the operator's setting loaded (here a test build, which never is)
    /// reads the shared slices but never writes them: `true` stands in for
    /// systemctl, `show` prints nothing so the slices look unconfigured, and an
    /// ungated launch would apply the default share and say `applied`.
    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_job_launch_never_writes_the_shared_slices_without_the_hub_daemons_permission() {
        let dir = disk_fixture();
        let runtime = SystemdJobRuntime {
            systemd_run: PathBuf::from("/usr/bin/true"),
            systemctl: PathBuf::from("/usr/bin/true"),
        };
        let spec = LaunchSpec {
            unit_name: "rsi-job-slice-gate".into(),
            cwd: dir.path().to_owned(),
            log_path: dir.path().join("slice-gate.log"),
            status_path: dir.path().join("slice-gate.status"),
            command: JobCommand {
                argv: vec!["cargo".into(), "check".into()],
                runtime_max_secs: 60,
                stop_timeout_secs: 900,
                log_max_bytes: 1000,
                memory_max_gib: 3,
                cpu_quota_percent: 250,
                background: false,
                admission_max_secs: None,
            },
            build_environment: Some(BuildEnvironment {
                tmp_dir: dir.path().join("tmp"),
                target_dir: dir.path().join("target"),
                artifact_lock: None,
                admission_lock: None,
                recorded_env: Vec::new(),
            }),
        };
        runtime.ensure_build_slice(&spec);
        let line = std::fs::read_to_string(&spec.log_path).unwrap();
        assert!(
            line.starts_with("rsi-build-slice rsi-build.slice unavailable "),
            "{line}"
        );
        assert!(line.contains("test seam"), "{line}");
        assert!(!line.contains("applied"), "{line}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn full_library_qa_unit_and_test_threads_use_the_host_cpu_ceiling() {
        let dir = disk_fixture();
        std::fs::create_dir(dir.path().join(".rsi")).unwrap();
        // Even an old declaration of 200 must use the host-derived policy.
        std::fs::write(dir.path().join(".rsi/jobs.toml"),
            "version = 1\n[recipes.rolling-library-qa]\nrunner = 'make'\ntarget = 'rolling-library-qa'\ntimeout_minutes = 180\ncpu_quota_percent = 200\n").unwrap();
        let p = params(
            JobKind::Test,
            serde_json::json!({
                "recipe":"rolling-library-qa", "timeout_minutes":180,
            }),
        );
        let command = job_command(&tools(), &p, dir.path(), None).unwrap();
        let ceiling = scoped_test_cpu_ceiling_percent(host_threads());
        assert_eq!(command.cpu_quota_percent, ceiling);
        // #2149: cargo-slot's admission wait is the job's timeout, set on
        // cargo-slot's own environment ahead of it.
        assert_eq!(
            &command.argv[..3],
            ["env", "RSI_CARGO_ADMISSION_MAX_SEC=10800", "/x/cargo-slot"]
        );
        assert_eq!(
            command.argv[6],
            format!("RSI_LIBRARY_QA_TEST_THREADS={}", ceiling / 100)
        );
        assert_eq!(command.argv[7], "make");
        let spec = LaunchSpec {
            unit_name: "full-library-qa-fixture".into(),
            cwd: dir.path().to_owned(),
            log_path: dir.path().join("qa.log"),
            status_path: dir.path().join("qa.status"),
            command,
            build_environment: None,
        };
        let args = systemd_run_args(&spec, None);
        assert!(args.contains(&format!("--property=CPUQuota={ceiling}%")));
        // #2112: it yields to the gates it follows up, inside that ceiling.
        assert!(args.contains(&"--property=CPUWeight=20".to_string()));
        assert!(args.contains(&"--property=Nice=10".to_string()));
    }

    /// #1638: the scoped-test form runs the fixed script with the two refs and
    /// budgets derived from the timeout, and settles with its typed receipt.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_scoped_test_job_runs_the_fixed_script_and_records_its_receipt() {
        let mut p = params(
            JobKind::Test,
            serde_json::json!({"scoped_test":{"base":"origin/rolling","head":"HEAD"}}),
        );
        let JobParams::Test(test) = &mut p else {
            panic!("test");
        };
        test.timeout_minutes = Some(20);
        let command = job_command(&tools(), &p, Path::new("/tmp"), None).unwrap();
        let ceiling = scoped_test_cpu_ceiling_percent(host_threads());
        let expected_quota = ceiling.to_string();
        let tail: Vec<&str> = command.argv.iter().skip(3).map(String::as_str).collect();
        assert_eq!(
            tail,
            [
                "scripts/scoped-test",
                "--base",
                "origin/rolling",
                "--head",
                "HEAD",
                "--runtime-max-sec",
                "1080",
                "--total-max-sec",
                "1140",
                "--job-max-sec",
                "1200",
                "--cargo-slot",
                "/x/cargo-slot",
                "--cpu-quota",
                expected_quota.as_str(),
            ]
        );
        assert_eq!(command.cpu_quota_percent, ceiling);
        assert_eq!(
            command.runtime_max_secs,
            20 * 60 + JOB_TIMEOUT_UNIT_GRACE_SECS
        );

        // #1979: at the 60-minute scoped default two 25-minute packages fit.
        let set_minutes = |p: &mut JobParams, minutes: u32| {
            let JobParams::Test(test) = p else {
                panic!("test");
            };
            test.timeout_minutes = Some(minutes);
        };
        set_minutes(&mut p, 60);
        let long = job_command(&tools(), &p, Path::new("/tmp"), None).unwrap();
        assert!(
            long.argv
                .windows(2)
                .any(|w| w == ["--runtime-max-sec", "3480"])
        );
        assert!(
            long.argv
                .windows(2)
                .any(|w| w == ["--total-max-sec", "3540"])
        );
        set_minutes(&mut p, 20);

        let receipt = serde_json::json!({
            "schema": 1, "base": "origin/rolling", "head": "h", "filters": ["rsid=x"],
            "log_dir": "/tmp/rsi-scoped-test-abc", "dry_run": false,
            "packages": [{"package": "rsid", "status": "passed", "completed": 3}],
            "exit_code": 0, "ok": true,
        });
        let log = format!(
            "noise\nscoped-test: rsid: passed (3 completed tests)\nSCOPED_TEST_RECEIPT {receipt}\n"
        );
        let job = AgentJobV1 {
            id: Uuid::new_v4(),
            kind: JobKind::Test,
            name: None,
            state: JobState::Running,
            owner_session_id: Uuid::new_v4(),
            unit_name: "u".into(),
            cwd: "/tmp".into(),
            log_path: "/tmp/job.log".into(),
            params: p,
            exit_code: None,
            result: None,
            created_at: Utc::now().to_rfc3339(),
            started_at: None,
            finished_at: None,
            held: None,
            wake: JobWake::Owner,
        };
        let (state, result) = classify(&job, Some(0), &log);
        assert_eq!(state, JobState::Succeeded);
        assert_eq!(result.exit_code, Some(0));
        // #1979: the receipt states the job timeout the run was held to.
        let mut stamped = receipt.clone();
        stamped["job_timeout_minutes"] = 20.into();
        assert_eq!(result.receipt.as_ref(), Some(&stamped));
        // #1914: a green run's detail is one verdict line, never the receipt.
        assert_eq!(
            result.detail.as_deref(),
            Some("scoped-test passed: rsid passed (3 tests)")
        );
        let wake = crate::store::agent_jobs::wake_message(&job, state, &result);
        assert!(wake.contains("exit_code=0"), "{wake}");
        assert!(
            wake.contains("\"log_dir\":\"/tmp/rsi-scoped-test-abc\""),
            "{wake}"
        );
        assert!(wake.contains("log=/tmp/job.log"), "{wake}");

        // A red receipt settles failed with its receipt; a clean exit with a
        // red receipt is still failed; no receipt is a typed refusal.
        let red = log.replace("\"ok\":true", "\"ok\":false");
        let (state, result) = classify(&job, Some(0), &red);
        assert_eq!(state, JobState::Failed);
        // #1914: the failed run keeps its log context without the receipt JSON.
        let detail = result.detail.clone().unwrap();
        assert!(
            detail.contains("rsid: passed (3 completed tests)"),
            "{detail}"
        );
        assert!(!detail.contains("SCOPED_TEST_RECEIPT"), "{detail}");
        assert!(!detail.contains("log_dir"), "{detail}");
        assert_eq!(result.receipt.unwrap()["ok"], false);
        let (state, result) = classify(&job, Some(1), "no receipt\n");
        assert_eq!(state, JobState::Failed);
        assert_eq!(
            result.refusal.as_deref(),
            Some("scoped_test_receipt_missing")
        );
        assert!(result.receipt.is_none());
        assert_eq!(result.detail.as_deref(), Some("no receipt"));

        // The reply no longer carries the receipt twice.
        let (_, result) = classify(&job, Some(0), &log);
        let bytes = serde_json::to_string(&result).unwrap().len();
        let doubled = log.len() + serde_json::to_string(&receipt).unwrap().len();
        assert!(bytes < doubled, "{bytes} >= {doubled}");
    }

    /// #1795/#1770: a durable `scoped-test` recipe settles with the script's
    /// typed receipt and the exact failing test identities, so a manager never
    /// reads the log; a recipe without a receipt keeps the plain exit-code rule.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_scoped_test_recipe_job_returns_its_receipt_and_failing_test_names() {
        let job_for = |recipe: &str| AgentJobV1 {
            id: Uuid::new_v4(),
            kind: JobKind::Test,
            name: None,
            state: JobState::Running,
            owner_session_id: Uuid::new_v4(),
            unit_name: "u".into(),
            cwd: "/tmp".into(),
            log_path: "/tmp/job.log".into(),
            params: params(JobKind::Test, serde_json::json!({ "recipe": recipe })),
            exit_code: None,
            result: None,
            created_at: Utc::now().to_rfc3339(),
            started_at: None,
            finished_at: None,
            held: None,
            wake: JobWake::Owner,
        };
        let receipt = |ok: bool, failures: serde_json::Value| {
            serde_json::json!({
                "schema": 1, "base": "origin/rolling", "head": "h", "filters": [],
                "packages": [{"package": "rsid", "status": "failed", "completed": 3}],
                "failures": failures, "exit_code": i32::from(!ok), "ok": ok,
            })
        };

        let failing = receipt(
            false,
            serde_json::json!([
                {"package": "rsid", "kind": "lib",
                 "target": "rsid", "test": "satellite::policy_link_applies"},
                {"package": "rsid-store", "kind": "test",
                 "target": "migrations", "test": "v9::replays"},
            ]),
        );
        let log = format!("noise\nSCOPED_TEST_RECEIPT {failing}\n");
        let job = job_for("scoped-test");
        let (state, result) = classify(&job, Some(2), &log);
        assert_eq!(state, JobState::Failed);
        assert_eq!(result.receipt.as_ref(), Some(&failing));
        assert_eq!(result.detail.as_deref(), Some("noise"));
        assert_eq!(
            result.failing_tests,
            [
                "rsid satellite::policy_link_applies",
                "rsid-store migrations v9::replays"
            ]
        );
        let wake = crate::store::agent_jobs::wake_message(&job, state, &result);
        assert!(wake.contains("satellite::policy_link_applies"), "{wake}");

        // The tmpfs recipe settles the same way; a green receipt succeeds.
        let green = receipt(true, serde_json::json!([]));
        let log = format!("SCOPED_TEST_RECEIPT {green}\n");
        let (state, result) = classify(&job_for("scoped-test-tmpfs"), Some(0), &log);
        assert_eq!(state, JobState::Succeeded);
        assert_eq!(result.receipt.as_ref(), Some(&green));
        assert!(result.failing_tests.is_empty());

        // A scoped-test recipe that died before its receipt is a typed refusal.
        let (state, result) = classify(&job, Some(1), "killed\n");
        assert_eq!(state, JobState::Failed);
        assert_eq!(
            result.refusal.as_deref(),
            Some("scoped_test_receipt_missing")
        );

        // An unrelated recipe without the receipt line is unchanged.
        let (state, result) = classify(&job_for("gate"), Some(0), "ok\n");
        assert_eq!(state, JobState::Succeeded);
        assert!(result.receipt.is_none() && result.refusal.is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_directory_is_the_callers_sandbox_or_a_managers_same_repo_worktree() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "-b", "rolling"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let integ = root.path().join("integration");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                integ.to_str().unwrap(),
                "-b",
                "integ",
            ],
        );
        let other = root.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        git(&other, &["init", "-q", "-b", "rolling"]);
        let plain = root.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();

        // Default: the caller's own sandbox.
        let own = resolve_cwd(Some(&main), &plain, false, None).unwrap();
        assert_eq!(own, main.canonicalize().unwrap());
        // No sandbox: nothing to run in.
        assert!(resolve_cwd(None, &plain, true, None).is_err());
        // A worker may not name another directory.
        assert!(resolve_cwd(Some(&main), &plain, false, Some(&integ)).is_err());
        // A manager may name a worktree of its own repository...
        let ok = resolve_cwd(Some(&main), &plain, true, Some(&integ)).unwrap();
        assert_eq!(ok, integ.canonicalize().unwrap());
        // ...but not another repository, a plain directory or a missing path.
        assert!(resolve_cwd(Some(&main), &plain, true, Some(&other)).is_err());
        assert!(resolve_cwd(Some(&main), &plain, true, Some(&plain)).is_err());
        assert!(resolve_cwd(Some(&main), &plain, true, Some(&root.path().join("nope"))).is_err());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn libtest_summary(passed: u64, failed: u64, ignored: u64, filtered: u64) -> String {
        format!(
            "test result: ok. {passed} passed; {failed} failed; {ignored} ignored; 0 measured; {filtered} filtered out; finished in 0.00s\n"
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn libtest_binary(label: &str, summary: &str) -> String {
        format!("     Running {label} (target/debug/deps/x)\n\nrunning 0 tests\n\n{summary}\n")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn scan(log: &str) -> LibtestExecution {
        libtest_execution(std::io::Cursor::new(log.as_bytes().to_vec())).unwrap()
    }

    /// #192: only reliable summaries prove that no test ran. Filtered-out and
    /// ignored tests are not executions; a started binary without a summary (a
    /// custom harness) or an unreadable summary proves nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn libtest_summaries_prove_zero_execution_only_when_every_binary_reports() {
        let zero = libtest_summary(0, 0, 0, 12);
        assert!(scan(&libtest_binary("unittests src/lib.rs", &zero)).proves_no_test_ran());
        // Ignored and filtered-out tests ran nothing.
        let ignored = libtest_summary(0, 0, 3, 9);
        assert!(scan(&libtest_binary("unittests src/lib.rs", &ignored)).proves_no_test_ran());
        // A doc-test binary that also reports zero.
        let both = format!(
            "{}{}",
            libtest_binary("unittests src/lib.rs", &zero),
            libtest_binary("Doc-tests x", &zero)
                .replace("     Running Doc-tests x", "   Doc-tests x")
        );
        let seen = scan(&both);
        assert_eq!((seen.binaries, seen.summaries, seen.executed), (2, 2, 0));
        assert!(seen.proves_no_test_ran());

        // One executed test, or one failed test, is an execution.
        for executed in [libtest_summary(1, 0, 0, 11), libtest_summary(0, 1, 0, 11)] {
            assert!(!scan(&libtest_binary("unittests src/lib.rs", &executed)).proves_no_test_ran());
        }
        // An earlier positive binary and a later zero one is not zero.
        let mixed = format!(
            "{}{}",
            libtest_binary("unittests src/lib.rs", &libtest_summary(1, 0, 0, 11)),
            libtest_binary("tests/other.rs", &zero)
        );
        assert!(!scan(&mixed).proves_no_test_ran());
        // A custom harness prints no summary: two binaries, one summary.
        let custom = format!(
            "{}     Running tests/custom.rs (target/debug/deps/custom)\n",
            libtest_binary("unittests src/lib.rs", &zero)
        );
        assert!(!scan(&custom).proves_no_test_ran());
        // No evidence at all, and an unreadable summary, prove nothing.
        assert!(!scan("").proves_no_test_ran());
        assert!(!scan("     Running tests/custom.rs (x)\n").proves_no_test_ran());
        assert!(!scan("test result: ok. many passed; 0 failed\n").proves_no_test_ran());
        assert!(!scan("test result: ok.\n").proves_no_test_ran());
        // A line far past the scan bound is skipped without hiding the next one.
        let long = format!("{}\n{zero}", "x".repeat(3 * LIBTEST_SCAN_LINE_MAX as usize));
        assert!(scan(&long).proves_no_test_ran());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    /// A log reader that yields `data` and then fails at `error_at` (or ends
    /// cleanly when `None`).
    struct FailingLog {
        data: Vec<u8>,
        pos: usize,
        error_at: Option<usize>,
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    impl std::io::Read for FailingLog {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            use std::io::BufRead;
            let available = self.fill_buf()?;
            let n = available.len().min(out.len());
            out[..n].copy_from_slice(&available[..n]);
            self.consume(n);
            Ok(n)
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    impl std::io::BufRead for FailingLog {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            let end = self
                .error_at
                .unwrap_or(self.data.len())
                .min(self.data.len());
            if self.pos >= end && self.error_at.is_some() {
                return Err(std::io::Error::other("injected log read error"));
            }
            Ok(&self.data[self.pos.min(end)..end])
        }

        fn consume(&mut self, amount: usize) {
            self.pos += amount;
        }
    }

    /// #192: a read error at any point, even after a valid zero summary or while
    /// the rest of an over-long line is being discarded, leaves the log
    /// incomplete: no zero-execution proof, and a clean end of input is unchanged.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_log_read_error_never_proves_that_no_test_ran() {
        let zero = libtest_binary("unittests src/lib.rs", &libtest_summary(0, 0, 0, 12));
        let reader = |data: &[u8], error_at: Option<usize>| FailingLog {
            data: data.to_vec(),
            pos: 0,
            error_at,
        };

        // Clean end of input after the zero summary: proven, as before.
        let clean = libtest_execution(reader(zero.as_bytes(), None)).unwrap();
        assert!(clean.proves_no_test_ran());
        // The same bytes, then a read error instead of the end: no proof.
        assert!(libtest_execution(reader(zero.as_bytes(), Some(zero.len()))).is_err());
        // An error before the summary is also an error, not an empty log.
        assert!(libtest_execution(reader(zero.as_bytes(), Some(8))).is_err());

        // An error while the remainder of an over-long line is discarded.
        let long = LIBTEST_SCAN_LINE_MAX as usize;
        let mut data = zero.clone().into_bytes();
        let start = data.len();
        data.extend(std::iter::repeat_n(b'x', 3 * long));
        data.push(b'\n');
        let whole = libtest_execution(reader(&data, None)).unwrap();
        assert!(whole.proves_no_test_ran(), "clean EOF after a long line");
        assert!(libtest_execution(reader(&data, Some(start + 2 * long))).is_err());
        // The first chunk of the long line is the part that is kept; an error
        // right after it is still an error.
        assert!(libtest_execution(reader(&data, Some(start + long))).is_err());

        // The settlement keeps the ordinary result when the log cannot be read
        // to its end: a directory opens but fails on read.
        let dir = crate::test_support::disk_backed_tempdir("filter-log-read-error");
        let job = filtered_job(
            params(
                JobKind::Test,
                serde_json::json!({"package":"rsi-common","lib_only":true,"exact":true,"filters":["a::tests::typo"]}),
            ),
            dir.path(),
        );
        let (state, result) = settle_unexecuted_filter(&job, classify(&job, Some(0), ""));
        assert_eq!((state, result.refusal), (JobState::Succeeded, None));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn filtered_job(params: JobParams, log_path: &Path) -> AgentJobV1 {
        AgentJobV1 {
            id: Uuid::new_v4(),
            kind: JobKind::Test,
            name: None,
            state: JobState::Running,
            owner_session_id: Uuid::new_v4(),
            unit_name: "u".into(),
            cwd: "/tmp".into(),
            log_path: log_path.display().to_string(),
            params,
            exit_code: None,
            result: None,
            created_at: Utc::now().to_rfc3339(),
            started_at: None,
            finished_at: None,
            held: None,
            wake: JobWake::Owner,
        }
    }

    /// Settle the way the jobs loop does: classify the bounded tail, then apply
    /// the whole-log filter check.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn settle_logged(job: &AgentJobV1, code: i32, log: &str) -> (JobState, AgentJobResultV1) {
        std::fs::write(&job.log_path, log).unwrap();
        settle_unexecuted_filter(
            job,
            classify(job, Some(code), &read_log_tail(&job.log_path)),
        )
    }

    /// #192: a filtered package job that exited 0 but provably ran no test
    /// settles `failed` `job_filter_matched_no_tests` with its exit code kept;
    /// every other shape keeps the ordinary settlement.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_filtered_package_job_that_ran_no_test_fails_job_filter_matched_no_tests() {
        let dir = crate::test_support::disk_backed_tempdir("filter-matched-no-tests");
        let log_path = dir.path().join("job.log");
        let filtered = |extra: serde_json::Value| {
            let mut value =
                serde_json::json!({"package":"rsi-common","lib_only":true,"exact":true});
            value["filters"] = serde_json::json!(["agent_control_schema::tests::typo"]);
            if let Some(extra) = extra.as_object() {
                value.as_object_mut().unwrap().extend(extra.clone());
            }
            filtered_job(params(JobKind::Test, value), &log_path)
        };
        let zero = libtest_binary("unittests src/lib.rs", &libtest_summary(0, 0, 0, 40));
        let one = libtest_binary("unittests src/lib.rs", &libtest_summary(1, 0, 0, 39));

        // Mistyped exact name: exit 0 retained, state failed, typed refusal.
        let (state, result) = settle_logged(&filtered(serde_json::json!({})), 0, &zero);
        assert_eq!(state, JobState::Failed);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.refusal.as_deref(), Some(JOB_FILTER_MATCHED_NO_TESTS));
        let detail = result.detail.unwrap();
        assert!(detail.starts_with(JOB_FILTER_MATCHED_NO_TESTS), "{detail}");
        assert!(
            detail.contains("agent_control_schema::tests::typo"),
            "{detail}"
        );
        assert!(
            detail.contains("test result: ok. 0 passed"),
            "tail kept: {detail}"
        );

        // One executed test succeeds.
        let (state, result) = settle_logged(&filtered(serde_json::json!({})), 0, &one);
        assert_eq!((state, result.refusal), (JobState::Succeeded, None));

        // An earlier positive summary pushed out of the 256 KiB tail by later
        // output, followed by a zero-test binary, stays successful.
        let noisy = format!(
            "{one}{}{zero}",
            "     Compiling noise v0.0.0\n".repeat(LOG_TAIL_READ_BYTES as usize / 10)
        );
        assert!(noisy.len() as u64 > 2 * LOG_TAIL_READ_BYTES);
        let (state, result) = settle_logged(&filtered(serde_json::json!({})), 0, &noisy);
        assert_eq!((state, result.refusal), (JobState::Succeeded, None));
        assert!(
            !read_log_tail(&log_path.display().to_string()).contains("1 passed"),
            "positive left the tail"
        );
        // The same log without the positive binary is still zero.
        let zero_noisy = noisy.replace(&one, "");
        let (state, result) = settle_logged(&filtered(serde_json::json!({})), 0, &zero_noisy);
        assert_eq!(state, JobState::Failed);
        assert_eq!(result.refusal.as_deref(), Some(JOB_FILTER_MATCHED_NO_TESTS));

        // Empty filters are an unfiltered run: unchanged.
        let all = filtered_job(
            params(
                JobKind::Test,
                serde_json::json!({"package":"rsi-common","lib_only":true}),
            ),
            &log_path,
        );
        let (state, result) = settle_logged(&all, 0, &zero);
        assert_eq!((state, result.refusal), (JobState::Succeeded, None));

        // A non-zero exit keeps the ordinary failure and its exit code.
        let (state, result) = settle_logged(&filtered(serde_json::json!({})), 101, &zero);
        assert_eq!(
            (state, result.exit_code, result.refusal),
            (JobState::Failed, Some(101), None)
        );

        // No or unreadable summary evidence (custom harness, empty or missing
        // log) is conservative.
        for log in ["", "     Running tests/custom.rs (x)\nrunning 1 test\n"] {
            let (state, result) = settle_logged(&filtered(serde_json::json!({})), 0, log);
            assert_eq!(
                (state, result.refusal),
                (JobState::Succeeded, None),
                "{log:?}"
            );
        }
        std::fs::remove_file(&log_path).unwrap();
        let ordinary = filtered(serde_json::json!({}));
        let (state, result) = settle_unexecuted_filter(&ordinary, classify(&ordinary, Some(0), ""));
        assert_eq!((state, result.refusal), (JobState::Succeeded, None));

        // Scoped, recipe, shard and candidate runs never take this path.
        for other in [
            serde_json::json!({"scoped_test":{"base":"origin/rolling"}}),
            serde_json::json!({"recipe":"check-cpu","filters":["rsid=a"]}),
            serde_json::json!({"shard":"store-01"}),
            serde_json::json!({"candidate_receipt":"rsi/abc"}),
        ] {
            let job = filtered_job(params(JobKind::Test, other), &log_path);
            assert!(!matches!(&job.params, JobParams::Test(p) if is_filtered_package_test(p)));
        }
    }
}
