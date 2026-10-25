//! Shell execution tool with timeout, output cap, and env scrubbing.
//!
//! - 120-second default timeout, `timeout_secs` up to 600
//! - 1 MB output cap
//! - Environment scrubbed: only safe vars carried through

use super::{HarnessTool, ToolContext, custody_guard, truncation::truncate_text};
use crate::process_control::{
    CaptureError, CaptureLimits, EscapeSweep, ProcessContainment, SESSION_TOOL_MAX_STREAM_BYTES,
    SESSION_TOOL_TIMEOUT, capture_bounded,
};
use crate::sandbox::execution_scratch::SandboxExecutionScratch;
use crate::session::harness::types::ToolResult;
use rsi_common::egress_policy::EgressMode;
use rsi_common::exec_target::ExecTarget;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// The longest foreground `timeout_secs` a model may request (#3168). Longer
/// runs belong to `exec_command`.
const MAX_TIMEOUT_SECS: u64 = 600;
const MAX_OUTPUT_BYTES: usize = SESSION_TOOL_MAX_STREAM_BYTES;

/// #3176: a container target's `shell` timeout is its per-call ceiling; a
/// `timeout_secs` argument may only lower it.
fn container_command_timeout(args: &serde_json::Value, ceiling_secs: u64) -> Duration {
    let secs = args
        .get("timeout_secs")
        .and_then(|value| value.as_u64())
        .map_or(ceiling_secs, |requested| requested.clamp(1, ceiling_secs));
    Duration::from_secs(secs)
}

fn command_timeout(args: &serde_json::Value, default: Duration) -> Duration {
    args.get("timeout_secs")
        .and_then(|value| value.as_u64())
        .map(|seconds| Duration::from_secs(seconds.clamp(1, MAX_TIMEOUT_SECS)))
        .unwrap_or(default.min(SESSION_TOOL_TIMEOUT))
}

/// Environment variables safe to pass to shell commands. Shared with the
/// Local provider's bash tool (`openai.rs`).
pub const SAFE_ENV_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "TERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "EDITOR",
    "VISUAL",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "SSH_AUTH_SOCK",
    // Development tools
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GOPATH",
    "GOROOT",
    "NVM_DIR",
    "NODE_PATH",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
];

/// Non-interactive defaults every `shell` and `exec_command` launch gets when
/// the variable is otherwise unset (#3169): no pager to block on, no ANSI
/// colour in the transcript, a UTF-8 locale every image ships.
pub(crate) const NON_INTERACTIVE_ENV_DEFAULTS: [(&str, &str); 6] = [
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("NO_COLOR", "1"),
    ("LANG", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
];
/// `TERM` of a command on plain pipes. A PTY launch sets an xterm-like `TERM`.
pub(crate) const PIPE_TERM: &str = "dumb";

/// The environment of a host command: the fixed [`SAFE_ENV_VARS`] allowlist,
/// the policy's extra names (never a credential-looking one), then the
/// non-interactive defaults for what is still unset. `lookup` reads the
/// daemon's environment.
pub(crate) fn host_command_env(
    extra: &[String],
    tty: bool,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    fn put(env: &mut Vec<(String, String)>, name: &str, value: String) {
        match env.iter_mut().find(|(existing, _)| existing == name) {
            Some(slot) => slot.1 = value,
            None => env.push((name.to_string(), value)),
        }
    }
    let mut env: Vec<(String, String)> = Vec::new();
    for name in SAFE_ENV_VARS {
        if let Some(value) = lookup(name) {
            put(&mut env, name, value);
        }
    }
    for name in extra {
        if rsi_common::harness_tool_policy::env_name_is_withheld(name) {
            continue;
        }
        if let Some(value) = lookup(name) {
            put(&mut env, name, value);
        }
    }
    for (name, value) in NON_INTERACTIVE_ENV_DEFAULTS {
        if !env.iter().any(|(existing, _)| existing == name) {
            put(&mut env, name, value.to_string());
        }
    }
    if !tty {
        put(&mut env, "TERM", PIPE_TERM.to_string());
    }
    env
}

#[derive(Clone)]
pub(crate) struct ShellLaunchIdentity {
    pub(crate) session_id: Option<uuid::Uuid>,
    pub(crate) invocation_id: Option<uuid::Uuid>,
    pub(crate) execution_scratch: Option<SandboxExecutionScratch>,
    /// Session egress mode (#774). `offline` runs the command in an empty
    /// network namespace; `deny_private` leaves the shell's network untouched
    /// (systemd IP filtering is not enforced for user scopes, and builds and
    /// git need the network), so the shell is NOT covered by the private-range
    /// guard that protects network tools.
    pub(crate) egress: EgressMode,
    /// #3176: where the command runs. `Host` (the default) is the scoped local
    /// `bash -c`; `Container` is `docker -H <host> exec` into the validated
    /// container, with an explicit environment and no host `unshare` wrapper.
    pub(crate) exec_target: ExecTarget,
    /// #3176: stamps an `exec_command` command inside its container so a
    /// cancel can kill its whole tree there. Container targets only.
    pub(crate) exec_marker: Option<uuid::Uuid>,
    /// #3176: wrap the container command in `timeout -k 5 <secs>` so the limit
    /// is enforced inside the container too (`shell`; `None` for `exec_command`).
    pub(crate) container_timeout_secs: Option<u64>,
    /// #3162: the command wants a terminal. `exec_command` with `tty: true`
    /// launches it on a PTY; a container target adds `-t` to `docker exec`.
    pub(crate) tty: bool,
    /// #3169: extra environment names the session policy passes through to a
    /// host command, on top of [`SAFE_ENV_VARS`].
    pub(crate) env_passthrough: std::sync::Arc<[String]>,
}

impl ShellLaunchIdentity {
    /// An identity that runs on the daemon host.
    pub(crate) fn host(
        session_id: Option<uuid::Uuid>,
        invocation_id: Option<uuid::Uuid>,
        execution_scratch: Option<SandboxExecutionScratch>,
        egress: EgressMode,
    ) -> Self {
        Self {
            session_id,
            invocation_id,
            execution_scratch,
            egress,
            exec_target: ExecTarget::Host,
            exec_marker: None,
            container_timeout_secs: None,
            tty: false,
            env_passthrough: std::sync::Arc::from([]),
        }
    }
}

/// argv that runs `command` with no network: a fresh user + network namespace
/// holds only a down loopback interface. Fails closed: when `unshare` or
/// unprivileged user namespaces are unavailable the command does not run.
fn offline_argv(command: &str) -> Vec<String> {
    [
        "--user",
        "--map-current-user",
        "--net",
        "--",
        "bash",
        "-c",
        command,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The program that creates the empty network namespace.
const OFFLINE_NAMESPACE_PROGRAM: &str = "unshare";

#[cfg(test)]
thread_local! {
    /// Test seam: stand in for `unshare` so the fail-closed path can be driven
    /// on a host that does have user namespaces.
    static OFFLINE_PROGRAM_OVERRIDE: std::cell::RefCell<Option<&'static str>> =
        const { std::cell::RefCell::new(None) };
}

fn offline_namespace_program() -> &'static str {
    #[cfg(test)]
    if let Some(program) = OFFLINE_PROGRAM_OVERRIDE.with(|cell| *cell.borrow()) {
        return program;
    }
    OFFLINE_NAMESPACE_PROGRAM
}

pub(crate) fn scoped_shell_command(
    command: &str,
    working_dir: &Path,
    identity: &ShellLaunchIdentity,
) -> Result<tokio::process::Command, String> {
    if let ExecTarget::Container(target) = &identity.exec_target {
        // The container workdir is the caller's `working_dir`, read as a path
        // inside the container; the host path never reaches the docker client.
        let workdir = super::exec_target::container_workdir(working_dir)?;
        return super::exec_target::scoped_container_command(
            target,
            &workdir,
            command,
            identity,
            identity.container_timeout_secs,
        );
    }
    let env = host_command_env(&identity.env_passthrough, identity.tty, &|name| {
        std::env::var(name).ok()
    });

    let limits = crate::process_scope::WorkerScopeLimits::from_launcher_snapshot()
        .map_err(|error| error.to_string())?;
    let offline = identity.egress == EgressMode::Offline;
    let scoped = crate::process_scope::ScopedWorkerCommand::new(
        std::ffi::OsStr::new(if offline {
            offline_namespace_program()
        } else {
            "bash"
        }),
        identity.invocation_id.unwrap_or_else(uuid::Uuid::new_v4),
        limits,
    )
    .map_err(|error| error.to_string())?;
    let mut cmd = scoped.into_command();
    if offline {
        cmd.args(offline_argv(command));
    } else {
        cmd.args(["-c", command]);
    }
    cmd.current_dir(working_dir).env_clear();
    for (key, value) in &env {
        cmd.env(key, value);
    }
    cmd.env(
        rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
        rsi_common::identity::process_ownership_namespace(),
    );
    if let Some(session_id) = identity.session_id {
        cmd.env(rsi_common::identity::ENV_SESSION_ID, session_id.to_string());
    }
    if let Some(invocation_id) = identity.invocation_id {
        cmd.env(
            rsi_common::identity::ENV_MODEL_INVOCATION_ID,
            invocation_id.to_string(),
        );
    }
    if let Some(scratch) = &identity.execution_scratch {
        scratch.revalidate().map_err(|error| error.to_string())?;
        cmd.env(rsi_common::identity::ENV_CARGO_TARGET_DIR, scratch.target());
        cmd.env(rsi_common::identity::ENV_TMPDIR, scratch.temp());
        scratch.stamp_build_env(&mut cmd);
    }
    Ok(cmd)
}

pub struct ShellTool {
    timeout: Duration,
    session_id: Option<uuid::Uuid>,
    invocation_id: Option<uuid::Uuid>,
    execution_scratch: Option<SandboxExecutionScratch>,
    /// #3176: `Host` unless the session is bound to a container.
    exec_target: ExecTarget,
    /// #3165: the custody scan guards only commands inside this base.
    sandbox_base: std::path::PathBuf,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            session_id: None,
            invocation_id: None,
            execution_scratch: None,
            exec_target: ExecTarget::Host,
            sandbox_base: rsi_common::boundary_mail_hook::sandbox_base(),
        }
    }
}

impl ShellTool {
    pub(super) fn new(
        session_id: Option<uuid::Uuid>,
        invocation_id: Option<uuid::Uuid>,
        execution_scratch: Option<SandboxExecutionScratch>,
    ) -> Self {
        Self {
            session_id,
            invocation_id,
            execution_scratch,
            ..Self::default()
        }
    }

    /// #3176: run this tool's commands in the container instead of on the host.
    pub(super) fn with_exec_target(mut self, exec_target: ExecTarget) -> Self {
        self.exec_target = exec_target;
        self
    }

    /// Test seam: the directory the custody scan treats as the sandbox base.
    #[cfg(test)]
    pub(super) fn with_sandbox_base(mut self, base: &Path) -> Self {
        self.sandbox_base = base.to_path_buf();
        self
    }

    async fn execute_with_cancel(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
        max_output_bytes: usize,
        egress: EgressMode,
        env_passthrough: std::sync::Arc<[String]>,
    ) -> ToolResult {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let container = self.exec_target.container();
        let timeout = match container {
            Some(target) => container_command_timeout(&args, target.per_call_ceiling_secs),
            None => command_timeout(&args, self.timeout),
        };

        let command = command
            .strip_prefix("```bash\n")
            .or_else(|| command.strip_prefix("```sh\n"))
            .and_then(|c| c.strip_suffix("\n```"))
            .unwrap_or(command);
        // #3176: the kill guard and the custody guard protect an RSI worktree.
        // Inside a task container they are wrong (TB2 has git tasks and kills
        // processes it started), so they are skipped for a container target
        // only; the host path keeps the kill guard (a host `pkill` can reach
        // other sessions' processes). #3165: the custody scan applies only to a
        // command inside the RSI sandbox base, like the CLI provider hook.
        if container.is_none() {
            if let Some(refusal) = rsi_common::kill_guard::broad_kill_refusal(command) {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some(refusal.to_string()),
                };
            }
            // S6 (#2256): refuse git commands that break AGENTS.md hard rules 1, 2
            // and 9 before anything in the command runs.
            let probe = custody_guard::Probe {
                session_id: self.session_id,
                invocation_id: self.invocation_id,
                working_dir,
                cancel,
                sandbox_base: &self.sandbox_base,
            };
            if let Some(refusal) = custody_guard::check_shell(command, &probe).await {
                return custody_guard::refused_result(refusal);
            }
        }

        let identity = ShellLaunchIdentity {
            session_id: self.session_id,
            invocation_id: self.invocation_id,
            execution_scratch: self.execution_scratch.clone(),
            egress,
            exec_target: self.exec_target.clone(),
            exec_marker: None,
            container_timeout_secs: container.map(|_| timeout.as_secs()),
            tty: false,
            env_passthrough,
        };
        // A container target runs in its own workdir, never the host path.
        let container_dir = container.map(|target| std::path::PathBuf::from(&target.workdir));
        let run_dir = container_dir.as_deref().unwrap_or(working_dir);
        let started = std::time::Instant::now();
        let mut cmd = match scoped_shell_command(command, run_dir, &identity) {
            Ok(command) => command,
            Err(error) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error_msg: Some(error),
                };
            }
        };

        let mut limits = CaptureLimits::session_tool();
        // The in-container `timeout` fires first; the client waits a grace longer.
        limits.execution_timeout = if container.is_some() {
            timeout + super::exec_target::CLIENT_TIMEOUT_GRACE
        } else {
            timeout
        };
        // #3168: a timeout still kills the group but keeps what it printed.
        limits.partial_on_timeout = true;
        let max_output_bytes = max_output_bytes.min(MAX_OUTPUT_BYTES);
        limits.max_stdout_bytes = max_output_bytes;
        limits.max_stderr_bytes = max_output_bytes;
        // #4029: a container target runs the docker client, which starts its `ssh://`
        // helper with `setsid`; the no-escape seccomp filter denies that and every call
        // failed as "permission denied ... docker API". Contain the client by group
        // only (as the bench runner does, #3890) and sweep any helper that left the
        // group once the capture ends (#3900). The host path keeps the filter.
        let sweep = container.map(|_| {
            limits.containment = ProcessContainment::Group;
            EscapeSweep::arm(cmd.as_std_mut())
        });
        let captured = capture_bounded(cmd, limits, cancel).await;
        if let Some(sweep) = sweep {
            let _ = tokio::task::spawn_blocking(move || sweep.sweep(None)).await;
        }
        match captured {
            Ok(output) => {
                let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
                if !output.stderr.is_empty() {
                    if !output.stdout.is_empty() {
                        combined.push_str("\n--- stderr ---\n");
                    }
                    combined.push_str(&String::from_utf8_lossy(&output.stderr));
                }
                let combined = truncate_text(
                    &combined,
                    max_output_bytes,
                    output.stdout_truncated || output.stderr_truncated,
                )
                .content;
                // `timeout` exits 124 (TERM) or 137 (it had to KILL) at the limit.
                let container_timed_out = container.is_some()
                    && matches!(output.status.code(), Some(124 | 137))
                    && started.elapsed() + Duration::from_secs(1) >= timeout;
                let timed_out = output.timed_out || container_timed_out;
                ToolResult {
                    success: output.status.success(),
                    output: combined,
                    error_msg: if output.status.success() {
                        None
                    } else if timed_out {
                        Some(format!("Command timed out after {}s", timeout.as_secs()))
                    } else {
                        Some(format!("Exit code: {}", output.status.code().unwrap_or(-1)))
                    },
                }
            }
            // Not reached with `partial_on_timeout` unless the leader could not
            // be reaped; there is then no output to keep.
            Err(CaptureError::ExecutionTimedOut) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Command timed out after {}s", timeout.as_secs())),
            },
            Err(CaptureError::Cancelled) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("Command cancelled".to_string()),
            },
            Err(error) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Failed to execute command: {error}")),
            },
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        if self.exec_target.is_container() {
            return "Execute a shell command inside the task container, in its working \
                    directory. Returns stdout and stderr. Commands are run with bash -c. \
                    Output capped at 1 MB. Default timeout is the container's per-call limit.";
        }
        "Execute a shell command in the working directory. Returns stdout and stderr. \
         Commands are run with bash -c. Default timeout 120 seconds; pass timeout_secs for \
         a longer run, up to 600 seconds, and use exec_command for anything longer. On a \
         timeout the process is killed and the output printed so far is returned. \
         Output capped at 1 MB."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["command"],"properties":{"command":{"type":"string","description":"Shell command to execute"},"timeout_secs":{"type":"integer","description":"Timeout in seconds (default 120, maximum 600; use exec_command for longer runs)"}}}"#
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        let cancel = CancellationToken::new();
        self.execute_with_cancel(
            args,
            working_dir,
            &cancel,
            MAX_OUTPUT_BYTES,
            EgressMode::default(),
            std::sync::Arc::from([]),
        )
        .await
    }

    async fn execute_cancellable(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
    ) -> ToolResult {
        self.execute_with_cancel(
            args,
            working_dir,
            cancel,
            MAX_OUTPUT_BYTES,
            EgressMode::default(),
            std::sync::Arc::from([]),
        )
        .await
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> ToolResult {
        self.execute_with_cancel(
            args,
            &context.working_dir,
            &context.cancel,
            context.policy.max_output_bytes,
            context.policy.egress.mode,
            context.policy.env_passthrough.clone(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn test_shell_echo() {
        let tool = ShellTool::default();
        let result = tool
            .execute(
                serde_json::json!({"command": "echo hello"}),
                Path::new("/tmp"),
            )
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert_eq!(result.output.trim(), "hello");
    }

    /// #1227: a pattern kill is refused before any process starts.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn shell_refuses_a_pattern_kill() {
        let tool = ShellTool::default();
        let result = tool
            .execute(
                serde_json::json!({"command": "echo start; killall rsi-1227-no-such-process"}),
                Path::new("/tmp"),
            )
            .await;
        assert!(!result.success);
        assert_eq!(
            result.error_msg.as_deref(),
            Some(rsi_common::kill_guard::BROAD_KILL_REFUSAL)
        );
        assert_eq!(result.output, "");
    }

    /// S6 (#2256): custody refusals name the hard rule, run nothing and leave
    /// the repository exactly as it was.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn shell_refuses_custody_git_commands_and_runs_nothing() {
        use super::custody_guard::test_support::{git, repo};
        let repo = repo();
        let dir = repo.path();
        std::fs::write(dir.join("a.txt"), "dirty\n").unwrap();
        std::fs::write(dir.join("sentinel.txt"), "keep\n").unwrap();
        let head = git(dir, &["rev-parse", "HEAD"]);
        let tool = ShellTool::default().with_sandbox_base(dir);
        for (command, rule) in [
            ("echo ran > ran.txt && git stash", "hard rule 9"),
            ("git stash pop", "hard rule 9"),
            ("git stash push -u", "hard rule 9"),
            ("bash -c 'git stash'", "hard rule 9"),
            ("\"git\" 'stash'; touch ran.txt", "hard rule 9"),
            ("touch ran.txt; git checkout -b other", "hard rule 2"),
            ("git checkout main", "hard rule 2"),
            ("echo ran > ran.txt && git switch main", "hard rule 2"),
            ("git reset --hard HEAD; touch ran.txt", "hard rule 2"),
            ("git push --force origin main", "hard rule 1"),
            // a.txt is dirty: the probe refuses the discard itself.
            (
                "echo ran > ran.txt; git checkout -- a.txt",
                "hard rules 8 and 9",
            ),
            ("git clean -fd; touch ran.txt", "hard rules 8 and 9"),
            // #2290: b.txt is clean now, but the probe runs before the shell, so
            // a discard that shares the line with anything is refused.
            (
                "echo ran > ran.txt; git checkout -- b.txt",
                "hard rules 8 and 9",
            ),
            ("sleep 0.2; git restore b.txt", "hard rules 8 and 9"),
            // #2289: `env -C` moves the child, not the probe.
            ("env -C . git restore b.txt", "hard rules 8 and 9"),
            // #2287: aliases are refused, whatever they stand for.
            ("git -c 'alias.x=!git stash' x", "hard rules 1, 2 and 9"),
            (
                "touch ran.txt; git -c alias.co=checkout co main",
                "hard rules 1, 2 and 9",
            ),
            ("git config alias.x '!git stash'", "hard rules 1, 2 and 9"),
            // #2288: substitutions inside double quotes run too.
            ("printf '%s' \"$(git stash)\"", "hard rule 9"),
            ("echo \"`git checkout main`\"; touch ran.txt", "hard rule 2"),
        ] {
            let result = tool
                .execute(serde_json::json!({"command": command}), dir)
                .await;
            assert!(!result.success, "{command}");
            assert_eq!(result.output, "", "{command}");
            let message = result.error_msg.unwrap_or_default();
            assert!(
                message.starts_with("custody_git_refused: "),
                "{command}: {message}"
            );
            assert!(message.contains(rule), "{command}: {message}");
        }
        assert!(!dir.join("ran.txt").exists());
        assert_eq!(git(dir, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(dir, &["branch", "--show-current"]).trim(), "main");
        assert_eq!(git(dir, &["stash", "list"]), "");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "dirty\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sentinel.txt")).unwrap(),
            "keep\n"
        );
    }

    /// #3165: the custody scan guards RSI worktrees only. A repository outside
    /// the sandbox base (a benchmark task's own repository, a scratch repo) may
    /// stash and reset; the same commands in a sandbox are refused.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn shell_custody_scan_applies_only_inside_the_sandbox_base() {
        use super::custody_guard::test_support::{git, repo};
        let sandbox_base = tempfile::tempdir().unwrap();
        let sandbox = sandbox_base.path().join("session");
        std::fs::create_dir(&sandbox).unwrap();
        let scratch = repo();
        let dir = scratch.path();
        let tool = ShellTool::default().with_sandbox_base(sandbox_base.path());

        std::fs::write(dir.join("a.txt"), "dirty\n").unwrap();
        let result = tool
            .execute(serde_json::json!({"command": "git stash"}), dir)
            .await;
        assert!(result.success, "{:?} {}", result.error_msg, result.output);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert!(git(dir, &["stash", "list"]).contains("WIP"));
        let result = tool
            .execute(serde_json::json!({"command": "git stash pop"}), dir)
            .await;
        assert!(result.success, "{:?} {}", result.error_msg, result.output);
        let result = tool
            .execute(
                serde_json::json!({"command": "git checkout -b recovery && git reset --hard HEAD"}),
                dir,
            )
            .await;
        assert!(result.success, "{:?} {}", result.error_msg, result.output);
        assert_eq!(git(dir, &["branch", "--show-current"]).trim(), "recovery");

        // The same commands from a directory inside the base are refused, and so
        // is a command that names the base from outside it.
        for (command, cwd) in [
            ("git stash", sandbox.as_path()),
            ("git reset --hard", sandbox.as_path()),
            (&format!("git -C {} stash", sandbox.display()) as &str, dir),
        ] {
            let result = tool
                .execute(serde_json::json!({"command": command}), cwd)
                .await;
            assert!(!result.success, "{command}");
            let message = result.error_msg.unwrap_or_default();
            assert!(
                message.starts_with("custody_git_refused: "),
                "{command}: {message}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn shell_still_runs_allowed_git_commands() {
        use super::custody_guard::test_support::{git, repo};
        let repo = repo();
        let dir = repo.path();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let tool = ShellTool::default();
        let result = tool
            .execute(
                serde_json::json!({"command":
                    "git add new.txt && git -c commit.gpgsign=false commit -q -m added \
                     && git log --oneline | head -1 \
                     && echo 'git stash is only text' && git branch --show-current"}),
                dir,
            )
            .await;
        assert!(result.success, "{:?} {}", result.error_msg, result.output);
        assert!(result.output.contains("added"), "{}", result.output);
        assert!(result.output.contains("git stash is only text"));
        assert!(
            result.output.lines().any(|line| line == "main"),
            "{}",
            result.output
        );
        assert!(git(dir, &["log", "--oneline"]).contains("added"));

        // A discard is allowed on a clean path as the only command in the line
        // (#2290), including through `env` without `-C` and a quoted substitution.
        for command in [
            "git checkout -- a.txt",
            "env GIT_PAGER=cat git restore b.txt",
            "git -c core.pager=cat restore .",
            "echo \"$(git rev-parse --abbrev-ref HEAD)\"",
        ] {
            let result = tool
                .execute(serde_json::json!({"command": command}), dir)
                .await;
            assert!(
                result.success,
                "{command}: {:?} {}",
                result.error_msg, result.output
            );
        }
    }

    fn launch_args(egress: EgressMode) -> Vec<String> {
        let identity = ShellLaunchIdentity::host(None, None, None, egress);
        scoped_shell_command("echo hi", Path::new("/tmp"), &identity)
            .unwrap()
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn offline_egress_launches_the_shell_in_an_empty_network_namespace() {
        let offline = launch_args(EgressMode::Offline);
        let tail: Vec<&str> = offline
            .iter()
            .rev()
            .take(7)
            .rev()
            .map(String::as_str)
            .collect();
        assert_eq!(
            tail,
            [
                "--user",
                "--map-current-user",
                "--net",
                "--",
                "bash",
                "-c",
                "echo hi"
            ]
        );
        let unscoped = launch_args(EgressMode::DenyPrivate);
        assert_eq!(&unscoped[unscoped.len() - 2..], ["-c", "echo hi"]);
        assert!(!unscoped.iter().any(|arg| arg == "--net"));
    }

    fn offline_context(working_dir: &Path) -> ToolContext {
        ToolContext {
            session_id: None,
            working_dir: working_dir.to_path_buf(),
            cancel: CancellationToken::new(),
            event_sink: None,
            policy: super::super::ToolPolicy {
                egress: rsi_common::egress_policy::EgressPolicy::for_mode(EgressMode::Offline),
                ..super::super::ToolPolicy::default()
            },
        }
    }

    fn user_namespaces_available() -> bool {
        std::process::Command::new("unshare")
            .args(["--user", "--map-current-user", "--net", "true"])
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Positive on a host with unprivileged user namespaces (the shell sees
    /// only `lo`); on a host without them the same offline run must fail closed
    /// and never execute the command. Both branches assert, so the test never
    /// passes by returning early.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn offline_shell_sees_only_the_loopback_interface_or_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let context = offline_context(dir.path());
        if user_namespaces_available() {
            let result = ShellTool::default()
                .execute_with_context(
                    serde_json::json!({"command": "tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '"}),
                    &context,
                )
                .await;
            assert!(result.success, "{:?}", result.error_msg);
            assert_eq!(result.output.trim(), "lo");
        } else {
            let result = ShellTool::default()
                .execute_with_context(
                    serde_json::json!({"command": "touch ran-without-isolation"}),
                    &context,
                )
                .await;
            assert!(!result.success, "{result:?}");
            assert!(!dir.path().join("ran-without-isolation").exists());
        }
    }

    /// An unusable `unshare` (missing, or one that refuses to create the
    /// namespaces) must never fall back to running the command un-isolated.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn offline_shell_fails_closed_when_the_namespace_cannot_be_created() {
        for program in ["/nonexistent/rsi-unshare", "false"] {
            OFFLINE_PROGRAM_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(program));
            let dir = tempfile::tempdir().unwrap();
            let result = ShellTool::default()
                .execute_with_context(
                    serde_json::json!({"command": "touch ran-without-isolation"}),
                    &offline_context(dir.path()),
                )
                .await;
            OFFLINE_PROGRAM_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
            assert!(!result.success, "{program}: {result:?}");
            let error = result.error_msg.unwrap_or_default();
            assert!(
                error.contains("Failed to execute command") || error.contains("Exit code"),
                "{program}: {error}"
            );
            assert!(
                !dir.path().join("ran-without-isolation").exists(),
                "{program}: the command ran outside the namespace"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn shell_context_policy_bounds_captured_output() {
        let context = ToolContext {
            session_id: None,
            working_dir: Path::new("/tmp").to_path_buf(),
            cancel: CancellationToken::new(),
            event_sink: None,
            policy: super::super::ToolPolicy {
                max_output_bytes: 80,
                ..super::super::ToolPolicy::default()
            },
        };
        let result = ShellTool::default()
            .execute_with_context(
                serde_json::json!({"command": "printf 'first line\\n'; printf '%0200d\\n' 0"}),
                &context,
            )
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert!(
            result
                .output
                .starts_with("first line\n[truncated: limit 80 bytes;")
        );
        assert!(result.output.len() <= 80);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn test_shell_exit_nonzero() {
        let tool = ShellTool::default();
        let result = tool
            .execute(serde_json::json!({"command": "exit 1"}), Path::new("/tmp"))
            .await;
        assert!(!result.success);
        assert!(result.error_msg.is_some());
        assert!(result.error_msg.unwrap().contains("Exit code: 1"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn test_shell_timeout() {
        let tool = ShellTool {
            timeout: Duration::from_millis(100),
            session_id: None,
            invocation_id: None,
            execution_scratch: None,
            exec_target: ExecTarget::Host,
            sandbox_base: rsi_common::boundary_mail_hook::sandbox_base(),
        };
        let result = tool
            .execute(
                serde_json::json!({"command": "sleep 10"}),
                Path::new("/tmp"),
            )
            .await;
        assert!(!result.success);
        let msg = result.error_msg.unwrap();
        assert!(msg.contains("timed out"), "unexpected msg: {msg}");
    }

    /// #3168: a model-requested `timeout_secs` above the old 120 s clamp is
    /// kept up to 600; the default stays 120 and the schema says so.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn command_timeout_keeps_values_up_to_the_documented_maximum() {
        let default = Duration::from_secs(120);
        let secs = |value: serde_json::Value| command_timeout(&value, default).as_secs();
        assert_eq!(secs(serde_json::json!({"timeout_secs": 300})), 300);
        assert_eq!(secs(serde_json::json!({"timeout_secs": 600})), 600);
        assert_eq!(secs(serde_json::json!({"timeout_secs": 601})), 600);
        assert_eq!(secs(serde_json::json!({"timeout_secs": 0})), 1);
        assert_eq!(secs(serde_json::json!({})), 120);
        let tool = ShellTool::default();
        assert!(tool.parameters_json().contains("maximum 600"));
        assert!(tool.parameters_json().contains("exec_command"));
        assert!(tool.description().contains("600"));
        assert!(tool.description().contains("exec_command"));
    }

    /// #3168: a command that prints and then outlives its timeout is killed and
    /// the model still sees what it printed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn shell_timeout_returns_the_output_printed_before_it() {
        let started = std::time::Instant::now();
        let result = ShellTool::default()
            .execute(
                serde_json::json!({
                    "command": "echo before-hang; echo err-before-hang >&2; sleep 30",
                    "timeout_secs": 1,
                }),
                Path::new("/tmp"),
            )
            .await;
        assert!(!result.success);
        assert_eq!(
            result.error_msg.as_deref(),
            Some("Command timed out after 1s")
        );
        assert!(result.output.contains("before-hang"), "{}", result.output);
        assert!(
            result.output.contains("err-before-hang"),
            "{}",
            result.output
        );
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn test_shell_strips_markdown_fence() {
        let tool = ShellTool::default();
        let result = tool
            .execute(
                serde_json::json!({"command": "```bash\necho stripped\n```"}),
                Path::new("/tmp"),
            )
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert_eq!(result.output.trim(), "stripped");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn test_shell_env_scrubbing() {
        // The daemon's variable must not be visible to the subprocess.
        // SAFETY: the name is unique to this test (#190), so no concurrent
        // test reads or writes it.
        unsafe { std::env::set_var("RSI_SHELL_SCRUB_TEST_SECRET", "supersecret") };
        let tool = ShellTool::default();
        let result = tool
            .execute(
                serde_json::json!({"command": "echo ${RSI_SHELL_SCRUB_TEST_SECRET:-EMPTY}"}),
                Path::new("/tmp"),
            )
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        // Since env is scrubbed, the var should be unset → output is EMPTY
        assert_eq!(result.output.trim(), "EMPTY");
        // SAFETY: test cleanup; the name is unique to this test.
        unsafe { std::env::remove_var("RSI_SHELL_SCRUB_TEST_SECRET") };
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn bound_shell_preserves_its_exact_ownership_stamps() {
        let session_id = uuid::Uuid::new_v4();
        let invocation_id = uuid::Uuid::new_v4();
        let tool = ShellTool::new(Some(session_id), Some(invocation_id), None);
        // #187: a directory this test owns. The process-global
        // `CARGO_TARGET_DIR` (or `<crate>/target` when it is unset) is not
        // guaranteed to exist, and another test of this process may change it.
        let cwd = tempfile::tempdir().unwrap();
        let result = tool
            .execute(
                serde_json::json!({
                    "command": format!(
                        "printf '%s\\n%s\\n%s' \"${{{}:-MISSING}}\" \"${{{}:-MISSING}}\" \"${{{}:-MISSING}}\"",
                        rsi_common::identity::ENV_SESSION_ID,
                        rsi_common::identity::ENV_MODEL_INVOCATION_ID,
                        rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
                    )
                }),
                cwd.path(),
            )
            .await;

        assert!(result.success, "{:?}", result.error_msg);
        assert_eq!(
            result.output,
            format!(
                "{session_id}\n{invocation_id}\n{}",
                rsi_common::identity::process_ownership_namespace()
            )
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn bound_shell_stamps_descriptor_target_and_tmpdir_after_env_clear() {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"))
            .join("slice8-harness-shell-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("scratch-")
            .tempdir_in(base)
            .unwrap();
        let scratch = SandboxExecutionScratch::prepare_for_test(root.path()).unwrap();
        let expected = format!(
            "{}|{}",
            scratch.target().display(),
            scratch.temp().display()
        );
        let tool = ShellTool::new(None, None, Some(scratch));
        let result = tool
            .execute(
                serde_json::json!({"command": "printf '%s|%s' \"$CARGO_TARGET_DIR\" \"$TMPDIR\""}),
                root.path(),
            )
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        assert_eq!(result.output, expected);
    }
    fn env_of(
        pairs: &[(&str, &str)],
        extra: &[&str],
        tty: bool,
    ) -> std::collections::BTreeMap<String, String> {
        let extra: Vec<String> = extra.iter().map(|name| (*name).to_string()).collect();
        host_command_env(&extra, tty, &|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        })
        .into_iter()
        .collect()
    }

    /// #3169: every launch gets the non-interactive defaults, and `TERM` is
    /// `dumb` on pipes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn host_env_overlays_non_interactive_defaults() {
        let env = env_of(&[("PATH", "/usr/bin"), ("TERM", "xterm-kitty")], &[], false);
        for (name, value) in [
            ("PAGER", "cat"),
            ("GIT_PAGER", "cat"),
            ("GH_PAGER", "cat"),
            ("NO_COLOR", "1"),
            ("LANG", "C.UTF-8"),
            ("LC_ALL", "C.UTF-8"),
            ("TERM", "dumb"),
        ] {
            assert_eq!(env.get(name).map(String::as_str), Some(value), "{name}");
        }
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
        // On a PTY the daemon's TERM is not forced to dumb.
        let tty = env_of(&[("TERM", "xterm-256color")], &[], true);
        assert_eq!(tty.get("TERM").map(String::as_str), Some("xterm-256color"));
    }

    /// An operator's own value wins over a default, and a name outside the
    /// allowlist is dropped.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn host_env_keeps_set_values_and_drops_unlisted_names() {
        let env = env_of(
            &[
                ("LANG", "en_US.UTF-8"),
                ("JAVA_HOME", "/jdk"),
                ("CC", "gcc"),
            ],
            &[],
            false,
        );
        assert_eq!(env.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
        assert!(!env.contains_key("JAVA_HOME"));
        assert!(!env.contains_key("CC"));
    }

    /// An extra allowlisted name passes; a credential-looking one never does,
    /// nor does a daemon variable.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn host_env_extra_names_pass_but_credentials_never() {
        let pairs = [
            ("JAVA_HOME", "/jdk"),
            ("HTTPS_PROXY", "http://proxy:3128"),
            ("GITHUB_TOKEN", "t0ken"),
            ("OPENROUTER_API_KEY", "k3y"),
            ("DB_PASSWORD", "pw"),
            ("CLIENT_SECRET", "s3cret"),
            ("RSI_SESSION_TOKEN", "rsi"),
        ];
        let names = [
            "JAVA_HOME",
            "HTTPS_PROXY",
            "GITHUB_TOKEN",
            "OPENROUTER_API_KEY",
            "DB_PASSWORD",
            "CLIENT_SECRET",
            "RSI_SESSION_TOKEN",
        ];
        let env = env_of(&pairs, &names, false);
        assert_eq!(env.get("JAVA_HOME").map(String::as_str), Some("/jdk"));
        assert_eq!(
            env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://proxy:3128")
        );
        for withheld in &names[2..] {
            assert!(!env.contains_key(*withheld), "{withheld} leaked");
        }
    }

    /// End to end through the real tool: the overlay reaches the command, and a
    /// process variable outside the allowlist is dropped until the policy lists
    /// it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn shell_command_sees_the_overlay_and_the_policy_allowlist() {
        // Scoped runners also launch compiled test binaries directly, without
        // Cargo's runtime environment. Own the fixture instead (#593).
        const LISTED: &str = "HARNESS_SHELL_POLICY_ALLOWLIST_TEST";
        const EXPECTED: &str = "policy-listed-value";
        temp_env::async_with_vars([(LISTED, Some(EXPECTED))], async {
            assert!(!SAFE_ENV_VARS.contains(&LISTED));
            let command = format!(
                "printf '%s|%s|%s|%s|%s|%s' \"$PAGER\" \"$GIT_PAGER\" \"$GH_PAGER\" \
             \"$NO_COLOR\" \"$TERM\" \"${{{LISTED}:-unset}}\""
            );
            let run = |env_passthrough: Vec<String>| {
                let context = ToolContext {
                    session_id: None,
                    working_dir: Path::new("/tmp").to_path_buf(),
                    cancel: CancellationToken::new(),
                    event_sink: None,
                    policy: super::super::ToolPolicy {
                        env_passthrough: env_passthrough.into(),
                        ..super::super::ToolPolicy::default()
                    },
                };
                let args = serde_json::json!({ "command": command });
                async move {
                    ShellTool::default()
                        .execute_with_context(args, &context)
                        .await
                }
            };
            let plain = run(Vec::new()).await;
            assert!(plain.success, "{:?}", plain.error_msg);
            assert_eq!(plain.output, "cat|cat|cat|1|dumb|unset");
            let listed = run(vec![LISTED.into()]).await;
            assert!(listed.success, "{:?}", listed.error_msg);
            assert_eq!(listed.output, format!("cat|cat|cat|1|dumb|{EXPECTED}"));
        })
        .await;
    }
}
