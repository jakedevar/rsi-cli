//! Shell execution tool with timeout, output cap, and env scrubbing.
//!
//! - 120-second default timeout
//! - 1 MB output cap
//! - Environment scrubbed: only safe vars carried through

use super::{HarnessTool, ToolContext, truncation::truncate_text};
use crate::process_control::{
    CaptureError, CaptureLimits, SESSION_TOOL_MAX_STREAM_BYTES, SESSION_TOOL_TIMEOUT,
    capture_bounded,
};
use crate::sandbox::execution_scratch::SandboxExecutionScratch;
use crate::session::harness::types::ToolResult;
use rsi_common::egress_policy::EgressMode;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_BYTES: usize = SESSION_TOOL_MAX_STREAM_BYTES;

fn command_timeout(args: &serde_json::Value, default: Duration) -> Duration {
    args.get("timeout_secs")
        .and_then(|value| value.as_u64())
        .map(|seconds| Duration::from_secs(seconds.clamp(1, DEFAULT_TIMEOUT_SECS)))
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
    let mut env: Vec<(String, String)> = Vec::new();
    for var in SAFE_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            env.push((var.to_string(), val));
        }
    }

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
}

impl Default for ShellTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            session_id: None,
            invocation_id: None,
            execution_scratch: None,
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

    async fn execute_with_cancel(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
        max_output_bytes: usize,
        egress: EgressMode,
    ) -> ToolResult {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let timeout = command_timeout(&args, self.timeout);

        let command = command
            .strip_prefix("```bash\n")
            .or_else(|| command.strip_prefix("```sh\n"))
            .and_then(|c| c.strip_suffix("\n```"))
            .unwrap_or(command);

        let identity = ShellLaunchIdentity {
            session_id: self.session_id,
            invocation_id: self.invocation_id,
            execution_scratch: self.execution_scratch.clone(),
            egress,
        };
        let cmd = match scoped_shell_command(command, working_dir, &identity) {
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
        limits.execution_timeout = timeout;
        let max_output_bytes = max_output_bytes.min(MAX_OUTPUT_BYTES);
        limits.max_stdout_bytes = max_output_bytes;
        limits.max_stderr_bytes = max_output_bytes;
        match capture_bounded(cmd, limits, cancel).await {
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
                ToolResult {
                    success: output.status.success(),
                    output: combined,
                    error_msg: if output.status.success() {
                        None
                    } else {
                        Some(format!("Exit code: {}", output.status.code().unwrap_or(-1)))
                    },
                }
            }
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
        "Execute a shell command in the working directory. Returns stdout and stderr. \
         Commands are run with bash -c. Timeout: 120 seconds. Output capped at 1 MB."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["command"],"properties":{"command":{"type":"string","description":"Shell command to execute"},"timeout_secs":{"type":"integer","description":"Timeout in seconds (default 120)"}}}"#
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        let cancel = CancellationToken::new();
        self.execute_with_cancel(
            args,
            working_dir,
            &cancel,
            MAX_OUTPUT_BYTES,
            EgressMode::default(),
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

    fn launch_args(egress: EgressMode) -> Vec<String> {
        let identity = ShellLaunchIdentity {
            session_id: None,
            invocation_id: None,
            execution_scratch: None,
            egress,
        };
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
        // RSI_TEST_SECRET should not be visible to the subprocess
        // SAFETY: Test runs sequentially; no concurrent thread reads this var.
        unsafe { std::env::set_var("RSI_TEST_SECRET", "supersecret") };
        let tool = ShellTool::default();
        let result = tool
            .execute(
                serde_json::json!({"command": "echo ${RSI_TEST_SECRET:-EMPTY}"}),
                Path::new("/tmp"),
            )
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        // Since env is scrubbed, the var should be unset → output is EMPTY
        assert_eq!(result.output.trim(), "EMPTY");
        // SAFETY: Test cleanup; no concurrent thread reads this var.
        unsafe { std::env::remove_var("RSI_TEST_SECRET") };
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[tokio::test]
    async fn bound_shell_preserves_its_exact_ownership_stamps() {
        let session_id = uuid::Uuid::new_v4();
        let invocation_id = uuid::Uuid::new_v4();
        let tool = ShellTool::new(Some(session_id), Some(invocation_id), None);
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
                &std::env::var_os("CARGO_TARGET_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::env::current_dir().unwrap().join("target")),
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
}
