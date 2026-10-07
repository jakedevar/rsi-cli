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
use crate::rolling_queue::{failing_tests, landed_tip, tail};
use crate::store::Store;
use crate::store::agent_jobs::{AgentJobRow, NewAgentJob};
use chrono::{DateTime, Utc};
use rsi_common::agent_jobs::{
    AgentJobResultV1, AgentJobV1, BuildJobParams, CloudSweepResultV1, JOB_ADMISSION_TIMED_OUT,
    JOB_CANCELLED, JOB_DIR_NOT_ALLOWED, JOB_LAUNCH_FAILED, JOB_NOT_FOUND, JOB_TIMED_OUT, JobKind,
    JobParams, JobState, JobWake, LandingJobParams, SWEEP_MAX_FAILURES, SweepVerdict,
    TestJobParams,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

#[cfg(any(target_os = "macos", test))]
mod launchd;

mod recipe;

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

/// #1611: how long past the deploy drain's hold cap a job may stay `queued`
/// (unlaunched) before it is failed `job_admission_timed_out`. The drain
/// releases a hold by its cap at the latest, so a job still queued past
/// cap + margin is stuck, not waiting.
const JOB_ADMISSION_MARGIN_SECS: i64 = 5 * 60;

/// #1611: the longest a job may wait unlaunched, from the drain's hold cap
/// (the default cap when the drain is uncapped) plus the margin.
fn admission_cap_secs(drain: &crate::deploy_drain::DeployDrain) -> i64 {
    let hold = drain.hold_cap().map_or(
        i64::try_from(rsi_common::agent_deploy::DEPLOY_DRAIN_HOLD_DEFAULT_SECS).unwrap_or(600),
        |cap| cap.num_seconds(),
    );
    hold.saturating_add(JOB_ADMISSION_MARGIN_SECS)
}

/// #1611: whether a queued job has waited unlaunched past `cap_secs`.
fn admission_expired(job: &AgentJobV1, now: DateTime<Utc>, cap_secs: i64) -> bool {
    DateTime::parse_from_rfc3339(&job.created_at)
        .is_ok_and(|created| (now - created.with_timezone(&Utc)).num_seconds() > cap_secs)
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
}

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

fn build_command(tools: &JobTools, p: &BuildJobParams) -> Vec<String> {
    let mut args = vec![p.command.as_str().to_string()];
    match &p.package {
        Some(package) => args.extend([s("-p"), package.clone()]),
        None => args.push(s("--workspace")),
    }
    if p.all_targets {
        args.push(s("--all-targets"));
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
    if let JobParams::Test(test) = params
        && test.recipe.is_some()
    {
        return recipe::command(tools, test, cwd);
    }
    let (argv, runtime_max_secs, stop_timeout_secs) = match params {
        JobParams::Test(p) => (
            test_command(tools, p),
            job_timeout_secs(params).map_or(3 * 3600, |secs| secs + JOB_TIMEOUT_UNIT_GRACE_SECS),
            60,
        ),
        JobParams::Build(p) => (build_command(tools, p), 2 * 3600, 60),
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
        JobParams::Test(_) | JobParams::Build(_) | JobParams::Landing(_) => (64 * MIB, 24, 1600),
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
}

/// Lock file `scripts/run-rsid-test-shards.sh` keeps in the Cargo target
/// directory; every job that uses the target holds it shared.
const ARTIFACT_LOCK_FILENAME: &str = ".rsid-test-shards.lock";

fn job_tmp_dir(log_path: &Path) -> PathBuf {
    log_path.with_extension("tmp")
}

/// Resolve the target directory a job builds in, without starting
/// Cargo under the store lock. A directory-scoped config (`<ancestor>/.cargo`,
/// relative to its parent) that sets `build.target-dir` wins, so an operator can
/// point a whole tree elsewhere. The daemon's own `CARGO_TARGET_DIR` is ignored
/// too (a dev daemon started inside a session inherits that session's). The
/// global Cargo home config is ignored on purpose: it names one shared target
/// for every worktree, and Cargo decides freshness by mtime, so jobs from
/// different worktrees would reuse each other's artifacts (#1090). Without a
/// scoped override a job builds in `<cwd>/target`.
fn cargo_target_dir(cwd: &Path, cargo_home: Option<&Path>) -> std::io::Result<PathBuf> {
    let config_dirs: Vec<PathBuf> = cwd
        .ancestors()
        .map(|p| p.join(".cargo"))
        .filter(|dir| Some(dir.as_path()) != cargo_home)
        .collect();
    for dir in config_dirs {
        // Cargo prefers the legacy filename when both are present.
        let legacy = dir.join("config");
        let config = if legacy.exists() {
            legacy
        } else {
            dir.join("config.toml")
        };
        let text = match std::fs::read_to_string(&config) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let value: toml::Value = toml::from_str(&text).map_err(|_| {
            std::io::Error::other(format!("cannot parse Cargo config {}", config.display()))
        })?;
        if let Some(target) = value.get("build").and_then(|v| v.get("target-dir")) {
            let target = target
                .as_str()
                .ok_or_else(|| std::io::Error::other("Cargo build.target-dir must be a string"))?;
            return Ok(dir.parent().unwrap_or(cwd).join(target));
        }
    }
    Ok(cwd.join("target"))
}

fn prepare_build_environment(
    params: &JobParams,
    cwd: &Path,
    log: &Path,
) -> std::io::Result<Option<BuildEnvironment>> {
    if !matches!(
        params,
        JobParams::Test(_) | JobParams::Build(_) | JobParams::Landing(_) | JobParams::CloudGate(_)
    ) {
        return Ok(None);
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")));
    let target_dir = cargo_target_dir(cwd, cargo_home.as_deref())?;
    let tmp_dir = job_tmp_dir(log);
    let runs_shard_runner = matches!(params, JobParams::Test(t) if t.shard.is_some());
    let artifact_lock = (!runs_shard_runner).then(|| target_dir.join(ARTIFACT_LOCK_FILENAME));
    if artifact_lock.is_some() {
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
            target_dir,
            artifact_lock,
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
const WRAPPER: &str = r#"log=$1; status=$2; cap=$3; shift 3
( PYTHONUNBUFFERED=1; export PYTHONUNBUFFERED; printf 'rsi-phase %s job-start\n' "$(date +%s)"; "$@" 2>&1 </dev/null; printf '%s\n' "$?" >"$status.code" ) | head -c "$cap" >>"$log"
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

/// The `systemd-run` command line for one unit. Separate from `launch` so the
/// exact arguments are testable without systemd.
pub(crate) fn systemd_run_args(spec: &LaunchSpec, path_env: Option<&str>) -> Vec<String> {
    let mut args = vec![
        s("--user"),
        s("--quiet"),
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
    if let Some(path) = path_env {
        args.push(format!("--setenv=PATH={path}"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        args.push(format!("--setenv=HOME={}", home.to_string_lossy()));
    }
    if let Some(env) = &spec.build_environment {
        args.push(format!("--setenv=TMPDIR={}", env.tmp_dir.display()));
        args.push(format!(
            "--setenv=CARGO_TARGET_DIR={}",
            env.target_dir.display()
        ));
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

impl JobRuntime for SystemdJobRuntime {
    fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
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
/// launched; [`start_queued_jobs`] launches it when the drain ends.
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
    let id = Uuid::new_v4();
    let new = NewAgentJob {
        id,
        owner_session_id: ctx.owner,
        project_id: ctx.project_id,
        name: ctx.name,
        params: ctx.params,
        cwd: ctx.cwd.display().to_string(),
        unit_name: runtime.unit_name(id),
        log_path: jobs_dir.join(format!("{id}.log")).display().to_string(),
        status_path: jobs_dir.join(format!("{id}.status")).display().to_string(),
        idempotency_key: ctx.idempotency_key,
        wake: ctx.wake,
    };
    let state = if hold {
        JobState::Queued
    } else {
        JobState::Running
    };
    let (row, replayed) = store.insert_agent_job_in_state(&new, now, state)?;
    if replayed {
        return Ok((row, replayed));
    }
    if hold {
        append_phase(
            &new.log_path,
            now,
            "queue-wait: held by the deploy drain; the unit has not been launched",
        );
        return Ok((row, false));
    }
    launch_recorded(store, runtime, &new, command, false)?;
    Ok((row, false))
}

/// #1608: append one `rsi-phase <epoch> <text>` line to a job's log from the
/// daemon, before the unit's wrapper exists, so a job that never starts (held
/// by a deploy drain, or stuck launching) still shows where it waited. Best
/// effort: evidence must never fail a submit. The wrapper appends after it.
fn append_phase(log_path: &str, now: DateTime<Utc>, text: &str) {
    use std::io::Write;
    let line = format!("rsi-phase {} {text}\n", now.timestamp());
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
        let build_environment =
            prepare_build_environment(&new.params, &cwd, Path::new(&new.log_path))
                .map_err(|error| format!("cannot prepare job build environment: {error}"))?;
        let spec = LaunchSpec {
            unit_name: new.unit_name.clone(),
            cwd,
            log_path: PathBuf::from(&new.log_path),
            status_path: PathBuf::from(&new.status_path),
            command,
            build_environment,
        };
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

/// #1566: launch every `queued` job whose owner the deploy drain no longer
/// holds, oldest first. The start CASes `queued -> running` before the unit is
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
        tokio::task::spawn_blocking(move || store.blocking_lock().list_queued_agent_jobs())
            .await
            .map_err(|error| DaemonError::Process(format!("queued job list join: {error}")))??
    };
    let mut started = 0;
    let admission_cap = admission_cap_secs(drain);
    for row in queued {
        if admission_expired(&row.job, Utc::now(), admission_cap) {
            let store = store.lock().await;
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
        let owner = row.job.owner_session_id;
        let has_parent = {
            let store = store.lock().await;
            store
                .get_session(owner)?
                .is_none_or(|session| session.parent_id.is_some())
        };
        if drain.holds(Some(owner), has_parent) {
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
            let store = store.blocking_lock();
            let command = match job_command(&tools, &new.params, Path::new(&new.cwd), None) {
                Ok(command) => command,
                Err(error) => {
                    let result = AgentJobResultV1 {
                        detail: Some(tail(&error.to_string())),
                        refusal: Some(JOB_LAUNCH_FAILED.into()),
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

/// Failing test names from libtest (`test x ... FAILED`) and nextest
/// (`FAIL [ 0.1s] crate name`) output.
fn failing_test_names(text: &str) -> Vec<String> {
    let mut names = failing_tests(text);
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("FAIL [")
            && let Some((_, name)) = rest.split_once(']')
        {
            let name = name.trim();
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names.truncate(MAX_FAILING_TESTS);
    names
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

/// NEW and KNOWN failing test names from the last `## Verdict` section of
/// `QA.md` (lines `- NEW <name>` and `- KNOWN #n <name>`, the name in
/// backticks), each list bounded.
pub(crate) fn parse_sweep_failures(report: &str) -> (Vec<String>, Vec<String>) {
    let (new, known, _) = parse_sweep_report(report);
    (new, known)
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
    let mut result = AgentJobResultV1 {
        exit_code,
        failing_tests: failing_test_names(log),
        detail: Some(tail(log)).filter(|t| !t.is_empty()),
        ..AgentJobResultV1::default()
    };
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
    if let JobParams::Test(p) = &job.params
        && p.is_candidate_receipt()
    {
        return classify_candidate_receipt(exit_code, log, result);
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

/// #1337: whether a job is past its wall-clock timeout at `now`.
fn timed_out(job: &AgentJobV1, now: DateTime<Utc>) -> bool {
    job_timeout_secs(&job.params).is_some_and(|limit| {
        job_wall_secs(job, now).is_some_and(|wall| u64::try_from(wall).unwrap_or(0) > limit)
    })
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

/// The typed `job_timed_out` settlement: failing tests, the last phase and the
/// log tail ride along so the owner sees how far the run got.
fn timed_out_result(job: &AgentJobV1, now: DateTime<Utc>) -> AgentJobResultV1 {
    let log = read_log_tail(&job.log_path);
    let minutes = job_timeout_secs(&job.params).unwrap_or(0) / 60;
    let note = format!(
        "the test job ran past its {minutes}-minute timeout and its unit was stopped; narrow the filters (scripts/check-touched-shards) or ask your manager to raise timeout_minutes\n{}",
        phase_summary(&read_log_phases(&job.log_path), now)
    );
    AgentJobResultV1 {
        refusal: Some(JOB_TIMED_OUT.into()),
        failing_tests: failing_test_names(&log),
        detail: Some(if log.is_empty() {
            note
        } else {
            format!("{note}\n{}", tail(&log))
        }),
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
    log_unit_stop(&job.unit_name, "test job past its wall-clock timeout");
    if let Err(error) = runtime.stop_unit(&job.unit_name) {
        tracing::warn!(unit = %job.unit_name, %error, "cannot stop timed-out agent job unit");
        return None;
    }
    if runtime.unit_active(&job.unit_name) {
        tracing::warn!(unit = %job.unit_name, "timed-out agent job unit is still active after stop");
        return None;
    }
    Some((JobState::Failed, timed_out_result(job, now)))
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
        tokio::task::spawn_blocking(move || store.blocking_lock().list_running_agent_jobs())
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
            let (state, result) = match code {
                // A status file is the wrapper's final write, but the service
                // may still be tearing down its children. Keep scratch until
                // the backend confirms that group has stopped.
                Some(_) if active => return Ok(false),
                Some(code) => {
                    let log = read_log_tail(&job.log_path);
                    classify(job, Some(code), &log)
                }
                None if active => match timeout_breach(job, &*runtime, now)
                    .or_else(|| scratch_quota_breach(job, &*runtime, scratch_quota))
                {
                    Some(settlement) => settlement,
                    None => return Ok(false),
                },
                // #1337: the unit cap (timeout plus grace) stopped it while the
                // daemon could not: still a timeout, not a lost job.
                None if timed_out(job, now) => (JobState::Failed, timed_out_result(job, now)),
                None => {
                    let created = DateTime::parse_from_rfc3339(
                        job.started_at.as_deref().unwrap_or(&job.created_at),
                    )
                    .map(|t| t.with_timezone(&Utc))
                    .unwrap_or(now);
                    if (now - created).num_seconds() < LAUNCH_GRACE_SECS {
                        return Ok(false);
                    }
                    let log = read_log_tail(&job.log_path);
                    let is_sweep = matches!(job.params, JobParams::CloudSweep(_));
                    let result = AgentJobResultV1 {
                        refusal: Some("unit_ended_without_status".into()),
                        failing_tests: if is_sweep {
                            Vec::new()
                        } else {
                            failing_test_names(&log)
                        },
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
                            Some(tail(&log)).filter(|t| !t.is_empty())
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
            // #1006: a `wake: none` job settles without its per-job owner wake.
            let settled = {
                let store = store.blocking_lock();
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
pub(crate) fn cancel_job(
    store: &Store,
    runtime: &dyn JobRuntime,
    job_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(AgentJobV1, bool)> {
    let not_found = || DaemonError::InvalidParam(JOB_NOT_FOUND.into());
    let row = store.get_agent_job(job_id)?.ok_or_else(not_found)?;
    if row.job.state == JobState::Queued {
        // #1566: held behind a deploy drain, never launched: nothing to stop.
        let result = AgentJobResultV1 {
            refusal: Some(JOB_CANCELLED.into()),
            detail: Some("cancelled by the owner before the held job started".into()),
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
    // Settle only once the unit is confirmed stopped; a failed stop leaves the
    // job running with its scratch so a repeat cancel retries.
    let unstopped = |detail: String| {
        DaemonError::Process(format!(
            "job_cancel_unit_not_stopped: unit {} was not stopped, the job is still running; retry the cancel: {detail}",
            row.job.unit_name
        ))
    };
    log_unit_stop(&row.job.unit_name, "job cancelled");
    runtime.stop_unit(&row.job.unit_name).map_err(unstopped)?;
    if runtime.unit_active(&row.job.unit_name) {
        return Err(unstopped("the unit is still active".into()));
    }
    let log = read_log_tail(&row.job.log_path);
    let note = "cancelled by the owner; the unit was stopped";
    let result = AgentJobResultV1 {
        refusal: Some(JOB_CANCELLED.into()),
        failing_tests: failing_test_names(&log),
        detail: Some(if log.is_empty() {
            note.to_string()
        } else {
            format!("{note}\n{}", tail(&log))
        }),
        ..AgentJobResultV1::default()
    };
    let settled = store.settle_agent_job_outcome(job_id, JobState::Failed, &result, false, now)?;
    if matches!(row.job.kind, JobKind::Test | JobKind::Build) {
        cleanup_job_tmp(Path::new(&row.job.log_path));
    }
    let current = store.get_agent_job(job_id)?.ok_or_else(not_found)?;
    Ok((current.job, settled.is_some()))
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
        match start_queued_jobs(&store, &runtime, &tools, &drain).await {
            Ok(started) if started > 0 => {
                tracing::info!(started, "held agent jobs started after the deploy drain");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "held agent job start deferred"),
        }
        match jobs_dir() {
            Ok(dir) => match sweep_terminal_scratch(&store, &dir).await {
                Ok(removed) if removed > 0 => {
                    tracing::info!(removed, "removed scratch left by settled agent jobs");
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "agent job scratch sweep deferred"),
            },
            Err(error) => tracing::warn!(%error, "agent job scratch sweep skipped"),
        }
        first = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::agent_jobs::{AgentSubmitJobRequestV1, JobKind};
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
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
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
        let store = Store::open(&db).unwrap();
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
                env.iter()
                    .all(|(key, _)| ["PATH", "HOME", "TMPDIR", "CARGO_TARGET_DIR"].contains(key))
            );
            assert_eq!(env.iter().any(|(key, _)| *key == "TMPDIR"), build_env);
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
                assert!(args.contains(&format!(
                    "--setenv=CARGO_TARGET_DIR={}",
                    environment.target_dir.display()
                )));
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
    fn cargo_target_resolution_honours_scoped_config_and_ignores_the_global_one() {
        // The runner's TMPDIR can be inside HOME, where ancestor Cargo config
        // would override this test's synthetic global config.
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let cwd = root.path().join("worktree");
        let home = root.path().join("cargo-home");
        std::fs::create_dir_all(cwd.join(".cargo")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(
            cargo_target_dir(&cwd, Some(&home)).unwrap(),
            cwd.join("target")
        );
        // The global config names one shared target: never used (#1090).
        std::fs::write(
            home.join("config.toml"),
            "[build]\ntarget-dir = 'shared-target'\n",
        )
        .unwrap();
        assert_eq!(
            cargo_target_dir(&cwd, Some(&home)).unwrap(),
            cwd.join("target")
        );
        // Even when the worktree sits under the Cargo home's parent and the
        // global dir is one of its ancestors' `.cargo`.
        let nested = root.path().join("cargo-home-parent/wt");
        std::fs::create_dir_all(&nested).unwrap();
        let global = root.path().join("cargo-home-parent/.cargo");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("config.toml"),
            "[build]\ntarget-dir = 'shared-target'\n",
        )
        .unwrap();
        assert_eq!(
            cargo_target_dir(&nested, Some(&global)).unwrap(),
            nested.join("target")
        );
        std::fs::write(
            cwd.join(".cargo/config.toml"),
            "[build]\ntarget-dir = 'local-target'\n",
        )
        .unwrap();
        assert_eq!(
            cargo_target_dir(&cwd, Some(&home)).unwrap(),
            cwd.join("local-target")
        );
        std::fs::write(
            cwd.join(".cargo/config"),
            "[build]\ntarget-dir = 'legacy-target'\n",
        )
        .unwrap();
        assert_eq!(
            cargo_target_dir(&cwd, Some(&home)).unwrap(),
            cwd.join("legacy-target")
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
        assert!(store.list_agent_jobs(owner, 10).unwrap().is_empty());
        assert!(runtime.launched.lock().unwrap().is_empty());
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
        assert_eq!(build.argv[5..], ["check", "--workspace", "--all-targets"]);
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
        let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts");
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
            },
            build_environment: None,
        };
        let args = systemd_run_args(&spec, Some("/usr/bin"));
        assert_eq!(args[0], "--user");
        assert!(args.contains(&"--unit=rsi-job-1".to_string()));
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
        let owner = Uuid::new_v4();
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
                    ctx(owner, &cwd, Some(&key)),
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
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 2);
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_held_job_survives_store_reopen_and_starts_once_across_restarts() {
        let dir = disk_fixture();
        let path = dir.path().join("held-job.db");
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let store = Store::open(&path).unwrap();
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
        assert_eq!(restored.job.held.as_deref(), Some("deploy_draining"));
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
        assert_eq!(row.job.held.as_deref(), Some("deploy_draining"));
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_job_held_past_the_admission_cap_fails_job_admission_timed_out() {
        let (store, dir) = open();
        let runtime = Arc::new(FakeRuntime::default());
        let job_runtime: Arc<dyn JobRuntime> = runtime.clone();
        let drain = crate::deploy_drain::DeployDrain::new();
        engage_drain(&drain);
        drain.set_hold_cap_secs(600);
        assert_eq!(admission_cap_secs(&drain), 600 + JOB_ADMISSION_MARGIN_SECS);
        let owner = Uuid::new_v4();
        let mut c = ctx(owner, dir.path(), None);
        c.params = params(
            JobKind::Test,
            serde_json::json!({"package":"rsid","filters":[],"timeout_minutes":20}),
        );
        // Submitted an hour ago and never launched: past 10 min + margin.
        let old = Utc::now() - chrono::Duration::hours(1);
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
        // A fresh held job beside it keeps waiting.
        let fresh = held_test_job(&*store.lock().await, &*runtime, dir.path(), owner);

        assert_eq!(
            start_queued_jobs(&store, &job_runtime, &tools(), &drain)
                .await
                .unwrap(),
            0,
            "an expired job is failed, not launched"
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
            JobState::Queued
        );
        assert!(runtime.launched.lock().unwrap().is_empty());
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
            ctx(owner, dir.path(), None),
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
        // Within the grace window a not-yet-visible unit is not called lost.
        assert_eq!(poll_once(&store, &dynamic, Utc::now()).await.unwrap(), 0);
        let later = Utc::now() + chrono::Duration::seconds(LAUNCH_GRACE_SECS + 5);
        assert_eq!(poll_once(&store, &dynamic, later).await.unwrap(), 1);
        assert!(!job_tmp_dir(Path::new(&row.job.log_path)).exists());
        assert_eq!(poll_once(&store, &dynamic, later).await.unwrap(), 0);
        let guard = store.lock().await;
        let job = guard.get_agent_job(row.job.id).unwrap().unwrap().job;
        assert_eq!(job.state, JobState::Lost);
        assert_eq!(
            job.result.unwrap().refusal.as_deref(),
            Some("unit_ended_without_status")
        );
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
        assert_eq!(command.runtime_max_secs, 1200 + JOB_TIMEOUT_UNIT_GRACE_SECS);
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
        std::fs::write(
            &row.job.log_path,
            format!(
                "rsi-phase {started} job-start\nrsi-phase {started} slot-wait: queued (build position 0): all 4 build slots busy (4 in use); held by pid 7 (make scoped-test)\ntest store::slow_one ... FAILED\n"
            ),
        )
        .unwrap();
        let within = Utc::now() + chrono::Duration::minutes(19);
        assert_eq!(poll_once(&store, &dynamic, within).await.unwrap(), 0);
        assert!(runtime.stopped.lock().unwrap().is_empty());
        let past = Utc::now() + chrono::Duration::minutes(21);
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
        assert!(
            result
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("20-minute timeout")),
            "{result:?}"
        );
        // The last phase and how long the job sat in it ride in the result.
        let detail = result.detail.as_deref().unwrap();
        assert!(
            detail.contains("last phase `slot-wait: queued (build position 0)")
                && detail.contains("held by pid 7 (make scoped-test)")
                && detail.contains("still waiting there"),
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
}
