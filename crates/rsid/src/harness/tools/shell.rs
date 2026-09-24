//! Shell execution tool with timeout, output cap, and env scrubbing.
//!
//! Hardening:
//! - 120-second default timeout
//! - 1 MB output cap
//! - Environment scrubbed: only safe vars carried through

use super::HarnessTool;
use crate::harness::types::ToolResult;
use crate::process_control::{
    CaptureError, CaptureLimits, SESSION_TOOL_MAX_STREAM_BYTES, SESSION_TOOL_TIMEOUT,
    bounded_lossy_concat, capture_bounded,
};
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_BYTES: usize = SESSION_TOOL_MAX_STREAM_BYTES;
const OUTPUT_TRUNCATED_MARKER: &str = "\n\n[output truncated at 1 MB]";

fn command_timeout(args: &serde_json::Value, default: Duration) -> Duration {
    args.get("timeout_secs")
        .and_then(|value| value.as_u64())
        .map(|seconds| Duration::from_secs(seconds.clamp(1, DEFAULT_TIMEOUT_SECS)))
        .unwrap_or(default.min(SESSION_TOOL_TIMEOUT))
}

/// Environment variables safe to pass to shell commands.
const SAFE_ENV_VARS: &[&str] = &[
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

pub struct ShellTool {
    timeout: Duration,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
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
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let timeout = command_timeout(&args, self.timeout);

        // Strip markdown fences if present
        let command = command
            .strip_prefix("```bash\n")
            .or_else(|| command.strip_prefix("```sh\n"))
            .and_then(|c| c.strip_suffix("\n```"))
            .unwrap_or(command);

        // Build scrubbed environment
        let mut env: Vec<(String, String)> = Vec::new();
        for var in SAFE_ENV_VARS {
            if let Ok(val) = std::env::var(var) {
                env.push((var.to_string(), val));
            }
        }

        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-c", command])
            .current_dir(working_dir)
            .env_clear();
        for (key, value) in &env {
            cmd.env(key, value);
        }
        cmd.env(
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::process_ownership_namespace(),
        );

        let mut limits = CaptureLimits::session_tool();
        limits.execution_timeout = timeout;
        let cancel = CancellationToken::new();
        match capture_bounded(cmd, limits, &cancel).await {
            Ok(output) => {
                let mut parts: Vec<&[u8]> = vec![&output.stdout];
                if !output.stderr.is_empty() {
                    if !output.stdout.is_empty() {
                        parts.push(b"\n--- stderr ---\n");
                    }
                    parts.push(&output.stderr);
                }
                ToolResult {
                    success: output.status.success(),
                    output: bounded_lossy_concat(
                        &parts,
                        MAX_OUTPUT_BYTES,
                        output.stdout_truncated || output.stderr_truncated,
                        OUTPUT_TRUNCATED_MARKER,
                    ),
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
            Err(error) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Failed to execute command: {error}")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn test_shell_timeout() {
        let tool = ShellTool {
            timeout: Duration::from_millis(100),
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
}
