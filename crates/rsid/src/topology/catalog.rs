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
    terminate_process_group,
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

/// Kill a group left behind by an earlier daemon incarnation (plan §2.4).
pub(crate) fn kill_group(pgid: i32) {
    if pgid > 1 {
        terminate_process_group(nix::unistd::Pid::from_raw(pgid));
    }
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
}
