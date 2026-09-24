//! Guard runner: executes a caller-supplied verification spec on a candidate.
//!
//! The spec is data. Which commands constitute "green" is policy owned by a
//! later slice; this runner only guarantees ordered, bounded, fail-closed
//! execution with the whole process group reaped on timeout.

use crate::process_control::{
    CaptureError, CaptureLimits, OverflowBehavior, ProcessContainment, capture_bounded,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Bytes retained per stream before the report tail is cut from them.
const CAPTURE_BYTES: usize = 8 * 1024 * 1024;
/// Daemon authority and inherited repository overrides never reach a guard.
const SCRUBBED_ENV_PREFIX: &str = "RSI_";
const SCRUBBED_ENV: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardCommand {
    pub program: String,
    /// Passed verbatim as argv; never interpreted by a shell.
    pub args: Vec<String>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardSpec {
    /// Executed in order; the first failure stops the run.
    pub commands: Vec<GuardCommand>,
    pub env: BTreeMap<String, String>,
    /// Bytes of each stream's tail kept in the report.
    pub output_tail_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardStatus {
    Passed,
    /// Non-zero exit. `code` is `None` when a signal ended the process.
    Failed {
        code: Option<i32>,
    },
    TimedOut,
    /// The command could not be started or supervised.
    Unavailable {
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardCommandReport {
    pub program: String,
    pub args: Vec<String>,
    pub status: GuardStatus,
    pub stdout_tail: String,
    pub stderr_tail: String,
    /// True when either stream exceeded the capture bound before its tail.
    pub output_truncated: bool,
    pub duration: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardReport {
    /// True only when the spec is non-empty and every command passed.
    pub passed: bool,
    /// Reports for the commands that actually ran, in order.
    pub commands: Vec<GuardCommandReport>,
}

/// Run `spec` inside `worktree`. An empty spec fails: a guard that verifies
/// nothing must never read as green.
pub async fn run_guard(worktree: &Path, spec: &GuardSpec) -> GuardReport {
    let mut commands = Vec::with_capacity(spec.commands.len());
    for command in &spec.commands {
        let report = run_command(worktree, spec, command).await;
        let passed = report.status == GuardStatus::Passed;
        commands.push(report);
        if !passed {
            break;
        }
    }
    let passed = !spec.commands.is_empty()
        && commands.len() == spec.commands.len()
        && commands
            .iter()
            .all(|report| report.status == GuardStatus::Passed);
    GuardReport { passed, commands }
}

async fn run_command(
    worktree: &Path,
    spec: &GuardSpec,
    guard: &GuardCommand,
) -> GuardCommandReport {
    let mut command = Command::new(&guard.program);
    command.args(&guard.args).current_dir(worktree);
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(scrubbed) {
            command.env_remove(&key);
        }
    }
    command.envs(&spec.env);

    let limits = CaptureLimits::new(
        CAPTURE_BYTES,
        CAPTURE_BYTES,
        guard.timeout,
        OverflowBehavior::RetainTail,
        // The guarded suite manages process groups itself, so descendants must
        // stay free to call `setpgid`; the group kill still reaps the tree.
        ProcessContainment::Group,
    );
    let started = Instant::now();
    let captured = capture_bounded(command, limits, &CancellationToken::new()).await;
    let duration = started.elapsed();

    let (status, stdout, stderr, output_truncated) = match captured {
        Ok(output) => {
            let status = if output.timed_out {
                GuardStatus::TimedOut
            } else if output.status.success() {
                GuardStatus::Passed
            } else {
                GuardStatus::Failed {
                    code: output.status.code(),
                }
            };
            let truncated = output.stdout_truncated || output.stderr_truncated;
            (status, output.stdout, output.stderr, truncated)
        }
        // Unreachable for RetainTail (timeout returns Ok), but retained for
        // safety if the overflow mode is ever changed.
        Err(CaptureError::ExecutionTimedOut) => {
            (GuardStatus::TimedOut, Vec::new(), Vec::new(), false)
        }
        Err(error) => (
            GuardStatus::Unavailable {
                detail: error.to_string(),
            },
            Vec::new(),
            Vec::new(),
            false,
        ),
    };
    GuardCommandReport {
        program: guard.program.clone(),
        args: guard.args.clone(),
        status,
        stdout_tail: tail(&stdout, spec.output_tail_bytes),
        stderr_tail: tail(&stderr, spec.output_tail_bytes),
        output_truncated,
        duration,
    }
}

/// Whether an inherited environment variable is withheld from guard commands.
pub(super) fn scrubbed(key: &str) -> bool {
    key.starts_with(SCRUBBED_ENV_PREFIX) || SCRUBBED_ENV.contains(&key)
}

fn tail(bytes: &[u8], limit: usize) -> String {
    let start = bytes.len().saturating_sub(limit);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}
