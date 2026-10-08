//! Closed daemon command catalog (#635, plan §3.2).
//!
//! An author names one [`CatalogOp`] with typed params; the daemon builds the
//! argv and env and derives the effect class. Nothing here accepts an argv,
//! an env entry, a script path or an effect class from a topology.
//!
//! What v1 does NOT enforce (plan §V, #645): catalog ops run repository code
//! (`build.rs`, tests) as the daemon user with no jail and no network or
//! host-socket isolation. In-sandbox writes outside `target/` and `.rsi-tmp/`
//! are detected after the fact (`sandbox_unchanged`); writes outside the
//! sandbox are neither prevented nor detected.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use rsi_common::types::CatalogOp;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::{DaemonError, Result};
use crate::process_control::{
    CaptureLimits, OverflowBehavior, ProcessContainment, capture_bounded_with_spawn,
};

/// The daemon-derived effect class of every v1 catalog op.
pub(crate) const EFFECT_CLASS_CHECK: &str = "check";
/// Wall time of one catalog op (plan §2.5).
pub(crate) const CATALOG_OP_WALL_TIME: Duration = Duration::from_secs(60 * 60);
/// Bytes of stdout/stderr tail kept per op (plan §3 output shape).
pub(crate) const OUTPUT_TAIL_BYTES: usize = 16 * 1024;
/// Environment every op runs with (plan §3.2); `CARGO_TARGET_DIR` is added
/// per sandbox.
pub(crate) const COMMAND_ENV: &[(&str, &str)] = &[
    ("CARGO_BUILD_JOBS", "6"),
    ("CARGO_PROFILE_DEV_DEBUG", "line-tables-only"),
    ("CARGO_NET_OFFLINE", "true"),
];
/// Daemon-owned scratch the postcondition ignores.
pub(crate) const SCRATCH_DIRS: &[&str] = &["target", ".rsi-tmp"];

/// The daemon derives the class; no author field can change it.
pub(crate) const fn effect_class(op: &CatalogOp) -> &'static str {
    match op {
        CatalogOp::CargoTestFocused { .. }
        | CatalogOp::CargoClippyCrate { .. }
        | CatalogOp::CargoCheckCrate { .. }
        | CatalogOp::RollingBaseline { .. } => EFFECT_CLASS_CHECK,
    }
}

/// One daemon-built cargo invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Invocation {
    pub(crate) label: String,
    pub(crate) args: Vec<String>,
}

fn check(krate: &str) -> Invocation {
    Invocation {
        label: format!("cargo_check_crate:{krate}"),
        args: ["check", "-p", krate, "--all-targets", "--offline"]
            .map(str::to_owned)
            .to_vec(),
    }
}

fn clippy(krate: &str) -> Invocation {
    Invocation {
        label: format!("cargo_clippy_crate:{krate}"),
        args: ["clippy", "-p", krate, "--all-targets", "--offline"]
            .map(str::to_owned)
            .to_vec(),
    }
}

fn test(krate: &str, filter: Option<&str>, lib_only: bool) -> Invocation {
    let mut args = vec!["test".to_owned(), "-p".to_owned(), krate.to_owned()];
    if lib_only {
        args.push("--lib".into());
    }
    args.push("--offline".into());
    if let Some(filter) = filter {
        args.push(filter.to_owned());
    }
    args.push("--".into());
    args.push("--test-threads=8".into());
    Invocation {
        label: format!("cargo_test_focused:{krate}"),
        args,
    }
}

/// The fixed argv sequence of one op (program `cargo`).
pub(crate) fn invocations(op: &CatalogOp) -> Vec<Invocation> {
    match op {
        CatalogOp::CargoTestFocused {
            krate,
            filter,
            lib_only,
        } => vec![test(krate, filter.as_deref(), *lib_only)],
        CatalogOp::CargoClippyCrate { krate } => vec![clippy(krate)],
        CatalogOp::CargoCheckCrate { krate } => vec![check(krate)],
        // Fixed daemon-owned sequence; content to be agreed with Epic E (#629).
        CatalogOp::RollingBaseline { .. } => op
            .crates()
            .into_iter()
            .flat_map(|krate| [check(krate), clippy(krate), test(krate, None, true)])
            .collect(),
    }
}

/// Env of one op run in `sandbox`.
pub(crate) fn command_env(sandbox: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = COMMAND_ENV
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    env.push((
        "CARGO_TARGET_DIR".into(),
        sandbox.join("target").display().to_string(),
    ));
    env
}

/// Package names of the repository's workspace members.
pub(crate) async fn workspace_members(repo: &Path) -> Result<HashSet<String>> {
    let mut command = Command::new("cargo");
    command
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .current_dir(repo);
    let limits = CaptureLimits::new(
        16 * 1024 * 1024,
        64 * 1024,
        Duration::from_secs(60),
        OverflowBehavior::Error,
        ProcessContainment::Group,
    );
    let output =
        crate::process_control::capture_bounded(command, limits, &CancellationToken::new())
            .await
            .map_err(|error| DaemonError::Process(format!("cargo metadata failed: {error:?}")))?;
    if !output.status.success() {
        return Err(DaemonError::InvalidParam(format!(
            "cargo metadata failed in the execution repository: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    Ok(metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|package| package["name"].as_str().map(str::to_owned))
        .collect())
}

/// The result of one op run.
#[derive(Clone, Debug, Default)]
pub(crate) struct CommandOutcome {
    pub(crate) exit_code: i32,
    pub(crate) duration_ms: u64,
    pub(crate) stdout_tail: String,
    pub(crate) stderr_tail: String,
    /// Per-invocation report for multi-step ops (`rolling_baseline`).
    pub(crate) report: Option<serde_json::Value>,
    pub(crate) timed_out: bool,
}

impl CommandOutcome {
    /// Typed downstream output (plan §3: `{op, exit_code, duration_ms,
    /// stdout_tail, stderr_tail, report?}`).
    pub(crate) fn output(&self, op: &CatalogOp) -> serde_json::Value {
        let mut output = serde_json::json!({
            "op": op.name(),
            "exit_code": self.exit_code,
            "duration_ms": self.duration_ms,
            "stdout_tail": self.stdout_tail,
            "stderr_tail": self.stderr_tail,
        });
        if let Some(report) = &self.report {
            output["report"] = report.clone();
        }
        output
    }
}

fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(OUTPUT_TAIL_BYTES);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Run every invocation of `op` in `sandbox`, each in its own process group,
/// publishing the live group id through `pgid`.
pub(crate) async fn run(
    program: &OsString,
    sandbox: &Path,
    op: &CatalogOp,
    pgid: &AtomicI32,
    cancel: &CancellationToken,
) -> CommandOutcome {
    let started = Instant::now();
    let steps = invocations(op);
    let multi = steps.len() > 1;
    let mut report = Vec::new();
    let mut outcome = CommandOutcome::default();
    let mut failed = false;
    for step in steps {
        let remaining = CATALOG_OP_WALL_TIME.saturating_sub(started.elapsed());
        let mut command = Command::new(program);
        command
            .args(&step.args)
            .current_dir(sandbox)
            .envs(command_env(sandbox));
        let limits = CaptureLimits::new(
            OUTPUT_TAIL_BYTES,
            OUTPUT_TAIL_BYTES,
            remaining,
            OverflowBehavior::RetainTail,
            ProcessContainment::Group,
        );
        let step_started = Instant::now();
        let captured = capture_bounded_with_spawn(command, limits, cancel, |mut command| {
            let child = command
                .spawn()
                .map_err(|error| crate::process_control::CaptureError::Spawn(error.to_string()))?;
            if let Some(id) = child.id().and_then(|id| i32::try_from(id).ok()) {
                pgid.store(id, Ordering::SeqCst);
            }
            Ok(child)
        })
        .await;
        let (code, stdout, stderr, timed_out) = match captured {
            Ok(captured) => (
                captured.status.code().unwrap_or(-1),
                tail(&captured.stdout),
                tail(&captured.stderr),
                captured.timed_out,
            ),
            Err(error) => (-1, String::new(), format!("{error:?}"), false),
        };
        let step_ms = u64::try_from(step_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        report.push(serde_json::json!({
            "step": step.label,
            "exit_code": code,
            "duration_ms": step_ms,
        }));
        // The first failure's tails explain the result; otherwise the last.
        if !failed {
            outcome.exit_code = code;
            outcome.stdout_tail = stdout;
            outcome.stderr_tail = stderr;
            outcome.timed_out = timed_out;
            failed = code != 0 || timed_out;
        }
        if timed_out || cancel.is_cancelled() {
            break;
        }
    }
    outcome.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if multi {
        outcome.report = Some(serde_json::Value::Array(report));
    }
    outcome
}

/// What [`kill_group`] found for a recorded process group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StaleGroup {
    /// No live process is in the group: nothing to kill.
    Empty,
    /// Every live member runs inside the attempt's sandbox: it is still the
    /// catalog op's group, and it was killed.
    Owned { members: usize },
    /// A member runs outside the sandbox (or could not be read): the number
    /// was reused by an unrelated group, which is left alone (#1227).
    Foreign { pid: i32, cwd: Option<PathBuf> },
    /// No sandbox root was recorded, or the platform has no `/proc` to prove
    /// the group's identity with: nothing is signalled.
    Unproven,
}

/// Kill a group left behind by an earlier daemon incarnation (plan §2.4),
/// only once it is proven to still be that op's group (#1227).
///
/// The id is a number persisted before a daemon restart. By now the op may
/// have exited and the kernel may have handed the number to any new process
/// group, such as another agent's `systemd-run` test unit, whose main process
/// leads its own group. So the group is signalled only when every live member
/// is still running inside the attempt's sandbox (catalog ops run there).
pub(crate) fn kill_group(pgid: i32, sandbox: Option<&Path>) -> StaleGroup {
    if pgid <= 1 {
        return StaleGroup::Empty;
    }
    let found = match sandbox {
        Some(sandbox) => stale_group_identity(Path::new("/proc"), pgid, sandbox),
        None => StaleGroup::Unproven,
    };
    match &found {
        StaleGroup::Owned { .. } => crate::process_control::terminate_process_group_because(
            nix::unistd::Pid::from_raw(pgid),
            "topology catalog op group left by an earlier daemon incarnation",
        ),
        StaleGroup::Foreign { pid, cwd } => tracing::warn!(
            target: "rsid::signal",
            pgid,
            pid,
            cwd = ?cwd,
            sandbox = ?sandbox,
            unit = crate::process_control::SignalTarget::describe(*pid).unit.as_deref().unwrap_or("-"),
            "stale catalog op group id now belongs to a process outside the sandbox; not signalled"
        ),
        StaleGroup::Unproven => tracing::warn!(
            target: "rsid::signal",
            pgid,
            sandbox = ?sandbox,
            "stale catalog op group cannot be proven to be the op's own; not signalled"
        ),
        StaleGroup::Empty => {}
    }
    found
}

/// Classify the live members of process group `pgid` under `proc_root`
/// against `sandbox` (Linux `/proc` layout).
#[cfg(target_os = "linux")]
pub(crate) fn stale_group_identity(proc_root: &Path, pgid: i32, sandbox: &Path) -> StaleGroup {
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return StaleGroup::Unproven;
    };
    let sandbox = std::fs::canonicalize(sandbox).unwrap_or_else(|_| sandbox.to_path_buf());
    let mut members = 0;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue; // exited during the scan
        };
        if proc_stat_process_group(&stat) != Some(pgid) {
            continue;
        }
        let cwd = std::fs::read_link(entry.path().join("cwd")).ok();
        match &cwd {
            Some(cwd) if cwd.starts_with(&sandbox) => members += 1,
            _ => return StaleGroup::Foreign { pid, cwd },
        }
    }
    if members == 0 {
        StaleGroup::Empty
    } else {
        StaleGroup::Owned { members }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn stale_group_identity(_proc_root: &Path, _pgid: i32, _sandbox: &Path) -> StaleGroup {
    StaleGroup::Unproven
}

/// The process group id (field 5) of a `/proc/<pid>/stat` line. The command
/// name may hold spaces and parentheses, so fields are read after its last `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn proc_stat_process_group(stat: &str) -> Option<i32> {
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(2)?.parse().ok()
}

/// Plan §3.2 postcondition: HEAD is `pre_head` and the tree is clean,
/// ignoring the daemon-owned scratch directories.
pub(crate) fn sandbox_unchanged(sandbox: &Path, pre_head: &str) -> Result<bool> {
    let observed = crate::topology::custody::observe_sandbox_excluding(sandbox, SCRATCH_DIRS)?;
    Ok(!observed.dirty && observed.head == pre_head)
}

/// A fresh command sandbox at the fork commit. A sandbox already at the
/// attempt's path (a crash between allocation and its durable record) is
/// adopted only when it is exactly the fork commit and clean.
#[cfg(test)]
pub(crate) fn allocate_or_adopt(
    allocator: &crate::sandbox::SandboxAllocator,
    session_id: Uuid,
    fork: &crate::topology::custody::TopologyForkSource,
) -> Result<PathBuf> {
    let existing = allocator.base_dir().join(session_id.to_string());
    if existing.exists() {
        let observed =
            crate::topology::custody::observe_sandbox_excluding(&existing, SCRATCH_DIRS)?;
        if observed.dirty || observed.head != fork.commit() {
            return Err(DaemonError::InvalidParam(
                "existing command sandbox diverged from its fork commit".into(),
            ));
        }
        return existing.canonicalize().map_err(DaemonError::Io);
    }
    Ok(allocator
        .allocate(
            session_id,
            fork.origin(),
            rsi_common::types::SandboxKind::GitWorktree,
            fork.commit(),
            None,
        )?
        .root)
}

pub(crate) fn allocate_or_adopt_with_permit(
    allocator: &crate::sandbox::SandboxAllocator,
    permit: crate::sandbox::AllocationPermit,
    session_id: Uuid,
    fork: &crate::topology::custody::TopologyForkSource,
) -> Result<PathBuf> {
    let existing = allocator.base_dir().join(session_id.to_string());
    if existing.exists() {
        let observed =
            crate::topology::custody::observe_sandbox_excluding(&existing, SCRATCH_DIRS)?;
        if observed.dirty || observed.head != fork.commit() {
            return Err(DaemonError::InvalidParam(
                "existing command sandbox diverged from its fork commit".into(),
            ));
        }
        return existing.canonicalize().map_err(DaemonError::Io);
    }
    Ok(allocator
        .allocate_with_permit(
            permit,
            session_id,
            fork.origin(),
            rsi_common::types::SandboxKind::GitWorktree,
            fork.commit(),
            None,
        )?
        .root)
}

/// Delete the op's build cache (`<sandbox>/target`); the worktree stays.
pub(crate) fn reclaim_cache(sandbox: &Path) {
    let target = sandbox.join("target");
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_dir() => {
            if let Err(error) = std::fs::remove_dir_all(&target) {
                tracing::warn!(target = %target.display(), %error, "catalog op cache reclaim failed");
            }
        }
        _ => {}
    }
}

/// Live state of one op run in this daemon incarnation.
#[derive(Default)]
struct RunSlot {
    pgid: AtomicI32,
    cancel: CancellationToken,
    outcome: std::sync::Mutex<Option<CommandOutcome>>,
}

/// What the executor observes about a command attempt.
#[derive(Clone, Debug)]
pub(crate) enum CommandPoll {
    Running { pgid: Option<i32> },
    Exited(CommandOutcome),
}

/// Production runner: one tokio task per op run, keyed by attempt id.
pub(crate) struct CommandRunner {
    program: OsString,
    runs: std::sync::Mutex<HashMap<Uuid, Arc<RunSlot>>>,
    /// #1641 S4b: attempt id → its governor build-slot ticket and whether the
    /// slot was granted.
    slots: std::sync::Mutex<HashMap<Uuid, (Uuid, bool)>>,
}

impl Default for CommandRunner {
    fn default() -> Self {
        Self::new(OsString::from("cargo"))
    }
}

impl CommandRunner {
    pub(crate) fn new(program: OsString) -> Self {
        Self {
            program,
            runs: std::sync::Mutex::new(HashMap::new()),
            slots: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// #1641 S4b: ask the resource governor for a build slot for this attempt
    /// (the same gates a `cargo-slot` client faces: slots, load, memory and
    /// the `min_free_disk_gb` disk floor). `None`: the slot is held. `Some(kind)`:
    /// still waiting, `disk_floor` when the floor is the blocking gate and
    /// `build_slot` otherwise. The ticket is kept so each tick polls the same
    /// queue position; a platform where the governor cannot see this process
    /// admits (there is nothing to gate on).
    pub(crate) fn build_slot(
        &self,
        governor: &crate::governor::Governor,
        policy: &crate::governor::GovernorPolicy,
        attempt_id: Uuid,
    ) -> Option<&'static str> {
        use crate::governor::{AcquireOutcome, AcquireParams, AdmissionClass, BlockReason};
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ticket = match slots.get(&attempt_id) {
            Some((_, true)) => return None,
            Some((ticket, false)) => Some(*ticket),
            None => None,
        };
        let params = AcquireParams {
            class: AdmissionClass::Build,
            pid: std::process::id(),
            label: Some(format!("topology command {attempt_id}")),
            ticket_id: ticket,
        };
        match governor.acquire(policy, &params) {
            Ok(AcquireOutcome::Granted { lease_id, .. }) => {
                slots.insert(attempt_id, (lease_id, true));
                None
            }
            Ok(AcquireOutcome::Queued {
                ticket_id, reason, ..
            }) => {
                slots.insert(attempt_id, (ticket_id, false));
                Some(if matches!(reason, BlockReason::Disk { .. }) {
                    "disk_floor"
                } else {
                    "build_slot"
                })
            }
            Err(error) => {
                if ticket.is_some() {
                    // The queued ticket expired: ask again from the back.
                    slots.remove(&attempt_id);
                    Some("build_slot")
                } else {
                    tracing::warn!(%error, "topology command build slot not gated");
                    None
                }
            }
        }
    }

    /// Give the attempt's build slot (or queued ticket) back; idempotent.
    pub(crate) fn release_build_slot(
        &self,
        governor: &crate::governor::Governor,
        policy: &crate::governor::GovernorPolicy,
        attempt_id: Uuid,
    ) {
        let ticket = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&attempt_id);
        if let Some((ticket, _)) = ticket {
            governor.release(policy, ticket);
        }
    }

    fn slot(&self, attempt_id: Uuid) -> Option<Arc<RunSlot>> {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&attempt_id)
            .cloned()
    }

    /// Start the op and wait (bounded) for its first process group id.
    pub(crate) async fn start(
        &self,
        attempt_id: Uuid,
        sandbox: PathBuf,
        op: CatalogOp,
    ) -> Result<Option<i32>> {
        let slot = {
            let mut runs = self
                .runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if runs.contains_key(&attempt_id) {
                return Err(DaemonError::InvalidParam(
                    "catalog op is already running for this attempt".into(),
                ));
            }
            let slot = Arc::new(RunSlot::default());
            runs.insert(attempt_id, Arc::clone(&slot));
            slot
        };
        let program = self.program.clone();
        let task_slot = Arc::clone(&slot);
        tokio::spawn(async move {
            let outcome = run(&program, &sandbox, &op, &task_slot.pgid, &task_slot.cancel).await;
            *task_slot
                .outcome
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outcome);
        });
        for _ in 0..200 {
            let pgid = slot.pgid.load(Ordering::SeqCst);
            let done = slot
                .outcome
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
            if pgid > 0 || done {
                return Ok((pgid > 0).then_some(pgid));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(None)
    }

    pub(crate) fn poll(&self, attempt_id: Uuid) -> Option<CommandPoll> {
        let slot = self.slot(attempt_id)?;
        let outcome = slot
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Some(outcome.map_or_else(
            || {
                let pgid = slot.pgid.load(Ordering::SeqCst);
                CommandPoll::Running {
                    pgid: (pgid > 0).then_some(pgid),
                }
            },
            CommandPoll::Exited,
        ))
    }

    /// Kill the run's process group; the task records the outcome.
    pub(crate) fn cancel(&self, attempt_id: Uuid) {
        if let Some(slot) = self.slot(attempt_id) {
            slot.cancel.cancel();
        }
    }

    pub(crate) fn forget(&self, attempt_id: Uuid) {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&attempt_id);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// #1641 S4b: a cargo command takes a governor build slot; the disk floor
    /// holds it (`disk_floor`), the same ticket is granted once space returns,
    /// and releasing gives the slot back.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn build_slot_is_held_by_the_disk_floor_then_granted_and_released() {
        use crate::governor::{Governor, GovernorPolicy, Holder, ResourceSample};
        use std::sync::Mutex;
        let free = Arc::new(Mutex::new(1.0_f64));
        let sample = Arc::clone(&free);
        let governor = Governor::new(
            Box::new(move || ResourceSample {
                load1: 0.0,
                cores: 8,
                disk_free_gb: *sample.lock().unwrap(),
                mem_available_gb: 1000.0,
                workers_slice_anon_gb: None,
            }),
            Box::new(|pid| {
                Some(Holder {
                    pid,
                    start_ticks: 1,
                })
            }),
        );
        let policy = GovernorPolicy {
            build_slots: 1,
            min_free_disk_gb: 30,
            ..GovernorPolicy::default()
        };
        let runner = CommandRunner::default();
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());

        assert_eq!(
            runner.build_slot(&governor, &policy, first),
            Some("disk_floor")
        );
        assert_eq!(
            runner.build_slot(&governor, &policy, first),
            Some("disk_floor")
        );
        *free.lock().unwrap() = 500.0;
        assert_eq!(runner.build_slot(&governor, &policy, first), None);
        assert_eq!(runner.build_slot(&governor, &policy, first), None, "kept");
        // The single slot is taken: another command waits for a slot.
        assert_eq!(
            runner.build_slot(&governor, &policy, second),
            Some("build_slot")
        );
        runner.release_build_slot(&governor, &policy, first);
        runner.release_build_slot(&governor, &policy, first);
        assert_eq!(runner.build_slot(&governor, &policy, second), None);
        runner.release_build_slot(&governor, &policy, second);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn argv_and_env_are_daemon_built() {
        let op = CatalogOp::CargoTestFocused {
            krate: "rsid".into(),
            filter: Some("topology::tests".into()),
            lib_only: true,
        };
        assert_eq!(
            invocations(&op)[0].args,
            [
                "test",
                "-p",
                "rsid",
                "--lib",
                "--offline",
                "topology::tests",
                "--",
                "--test-threads=8"
            ]
        );
        assert_eq!(effect_class(&op), EFFECT_CLASS_CHECK);
        let env = command_env(Path::new("/sandbox"));
        assert!(env.contains(&("CARGO_BUILD_JOBS".into(), "6".into())));
        assert!(env.contains(&("CARGO_PROFILE_DEV_DEBUG".into(), "line-tables-only".into())));
        assert!(env.contains(&("CARGO_TARGET_DIR".into(), "/sandbox/target".into())));
        let baseline = invocations(&CatalogOp::RollingBaseline {
            crates: Some(vec!["rsi-common".into()]),
        });
        let labels: Vec<&str> = baseline.iter().map(|step| step.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "cargo_check_crate:rsi-common",
                "cargo_clippy_crate:rsi-common",
                "cargo_test_focused:rsi-common"
            ]
        );
    }

    /// The runner spawns its own process group, keeps bounded tails and
    /// reports the exit code (a stand-in program replaces `cargo`).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn runner_reports_exit_code_tails_and_group() {
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("fake-cargo");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"out $1 $CARGO_BUILD_JOBS $CARGO_TARGET_DIR\"\necho err >&2\nexit 3\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let runner = CommandRunner::new(script.into_os_string());
        let attempt = Uuid::new_v4();
        let op = CatalogOp::CargoCheckCrate {
            krate: "rsid".into(),
        };
        let pgid = runner
            .start(attempt, dir.path().to_path_buf(), op.clone())
            .await
            .unwrap();
        assert!(pgid.is_some_and(|pgid| pgid > 1));
        let outcome = loop {
            if let Some(CommandPoll::Exited(outcome)) = runner.poll(attempt) {
                break outcome;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(outcome.exit_code, 3);
        let target = dir.path().join("target");
        assert!(
            outcome
                .stdout_tail
                .contains(&format!("out check 6 {}", target.display()))
        );
        assert!(outcome.stderr_tail.contains("err"));
        assert_eq!(outcome.output(&op)["op"], "cargo_check_crate");
        runner.forget(attempt);
        assert!(runner.poll(attempt).is_none());
    }

    /// A `sleep` in its own process group with `cwd` as its working directory.
    #[cfg(target_os = "linux")]
    fn group_leader_in(cwd: &Path) -> (std::process::Child, i32) {
        use std::os::unix::process::CommandExt;
        let child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(cwd)
            .process_group(0)
            .spawn()
            .expect("spawn sleep");
        let pgid = i32::try_from(child.id()).expect("pid fits");
        // Until it execs, the forked child may still sit in the parent's cwd.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while crate::process_control::SignalTarget::describe(pgid)
            .comm
            .as_deref()
            != Some("sleep")
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        (child, pgid)
    }

    /// #1227: a stale group id still held by the op (running in its sandbox)
    /// is killed.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn stale_group_inside_the_sandbox_is_killed() {
        let sandbox = tempfile::TempDir::new().unwrap();
        let (mut child, pgid) = group_leader_in(sandbox.path());
        assert_eq!(
            kill_group(pgid, Some(sandbox.path())),
            StaleGroup::Owned { members: 1 }
        );
        let status = child.wait().expect("reap");
        assert!(!status.success(), "the op group was killed: {status:?}");
    }

    /// #1227: a stale group id the kernel has handed to an unrelated group
    /// (here: a process running outside the sandbox, like another agent's
    /// test unit) is left alone.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn stale_group_id_reused_outside_the_sandbox_is_not_signalled() {
        let sandbox = tempfile::TempDir::new().unwrap();
        let elsewhere = tempfile::TempDir::new().unwrap();
        let (mut child, pgid) = group_leader_in(elsewhere.path());
        let found = kill_group(pgid, Some(sandbox.path()));
        assert!(
            matches!(found, StaleGroup::Foreign { pid, .. } if pid == pgid),
            "{found:?}"
        );
        assert!(
            child.try_wait().expect("poll").is_none(),
            "the unrelated group is still running"
        );
        let unproven = kill_group(pgid, None);
        assert_eq!(unproven, StaleGroup::Unproven);
        assert!(child.try_wait().expect("poll").is_none());
        child.kill().expect("kill stand-in");
        child.wait().expect("reap");
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn stale_group_identity_reads_group_and_cwd_from_proc() {
        let proc_root = tempfile::TempDir::new().unwrap();
        let sandbox = tempfile::TempDir::new().unwrap();
        let elsewhere = tempfile::TempDir::new().unwrap();
        let process = |pid: i32, pgid: i32, cwd: &Path| {
            let dir = proc_root.path().join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            // A command name with spaces and a `)` must not shift the fields.
            std::fs::write(
                dir.join("stat"),
                format!("{pid} (odd) name) S 1 {pgid} {pgid} 0 -1 4194560"),
            )
            .unwrap();
            std::os::unix::fs::symlink(cwd, dir.join("cwd")).unwrap();
        };
        process(4100, 4100, &sandbox.path().join("crates"));
        process(4101, 4100, sandbox.path());
        process(4200, 4200, elsewhere.path());
        std::fs::create_dir_all(proc_root.path().join("self")).unwrap();
        assert_eq!(
            stale_group_identity(proc_root.path(), 4100, sandbox.path()),
            StaleGroup::Owned { members: 2 }
        );
        assert_eq!(
            stale_group_identity(proc_root.path(), 4200, sandbox.path()),
            StaleGroup::Foreign {
                pid: 4200,
                cwd: Some(elsewhere.path().to_path_buf())
            }
        );
        assert_eq!(
            stale_group_identity(proc_root.path(), 4300, sandbox.path()),
            StaleGroup::Empty
        );
    }
}
