//! Git operations tool -- status, diff, log, branch operations.

use super::HarnessTool;
use crate::process_control::{
    CaptureError, CaptureLimits, SESSION_TOOL_MAX_STREAM_BYTES, SESSION_TOOL_TIMEOUT,
    bounded_lossy_concat, capture_bounded,
};
use crate::session::harness::types::ToolResult;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEFAULT_GIT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_BYTES: usize = SESSION_TOOL_MAX_STREAM_BYTES;
const OUTPUT_TRUNCATED_MARKER: &str = "\n\n[output truncated at 1 MB]";

fn command_timeout(args: &serde_json::Value, default: Duration) -> Duration {
    args.get("timeout_secs")
        .and_then(|value| value.as_u64())
        .map(|seconds| Duration::from_secs(seconds.clamp(1, DEFAULT_GIT_TIMEOUT_SECS)))
        .unwrap_or(default.min(SESSION_TOOL_TIMEOUT))
}

pub struct GitTool {
    timeout: Duration,
    session_id: Option<uuid::Uuid>,
    invocation_id: Option<uuid::Uuid>,
}

impl Default for GitTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_GIT_TIMEOUT_SECS),
            session_id: None,
            invocation_id: None,
        }
    }
}

impl GitTool {
    pub(super) fn new(session_id: Option<uuid::Uuid>, invocation_id: Option<uuid::Uuid>) -> Self {
        Self {
            session_id,
            invocation_id,
            ..Self::default()
        }
    }

    async fn execute_with_cancel(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let git_args = args
            .get("args")
            .and_then(|v| v.as_str())
            .unwrap_or("status");
        let timeout = command_timeout(&args, self.timeout);
        let parts: Vec<&str> = git_args.split_whitespace().collect();

        let mut command = tokio::process::Command::new("git");
        command.args(&parts).current_dir(working_dir);
        command.env(
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::process_ownership_namespace(),
        );
        if let Some(session_id) = self.session_id {
            command.env(rsi_common::identity::ENV_SESSION_ID, session_id.to_string());
        }
        if let Some(invocation_id) = self.invocation_id {
            command.env(
                rsi_common::identity::ENV_MODEL_INVOCATION_ID,
                invocation_id.to_string(),
            );
        }

        let mut limits = CaptureLimits::session_tool();
        limits.execution_timeout = timeout;
        match capture_bounded(command, limits, cancel).await {
            Ok(output) => {
                let mut parts: Vec<&[u8]> = vec![&output.stdout];
                if !output.stderr.is_empty() {
                    if !output.stdout.is_empty() {
                        parts.push(b"\n");
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
                        Some(format!(
                            "git exited with code {}",
                            output.status.code().unwrap_or(-1)
                        ))
                    },
                }
            }
            Err(CaptureError::ExecutionTimedOut) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!(
                    "Git command timed out after {}ms",
                    timeout.as_millis()
                )),
            },
            Err(CaptureError::Cancelled) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("Git command cancelled".to_string()),
            },
            Err(error) => ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some(format!("Failed to run git: {error}")),
            },
        }
    }
}

#[async_trait::async_trait]
impl HarnessTool for GitTool {
    fn name(&self) -> &str {
        "git"
    }

    fn description(&self) -> &str {
        "Run git commands in the working directory. Supports: status, diff, log, \
         branch, add, commit, stash, and other read/write git operations. \
         Commands time out after 120 seconds."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["args"],"properties":{"args":{"type":"string","description":"Git arguments (e.g., 'status', 'diff --cached', 'log --oneline -10')"},"timeout_secs":{"type":"integer","minimum":1,"maximum":120,"description":"Timeout in seconds (default and maximum: 120)"}}}"#
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        let cancel = CancellationToken::new();
        self.execute_with_cancel(args, working_dir, &cancel).await
    }

    async fn execute_cancellable(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
    ) -> ToolResult {
        self.execute_with_cancel(args, working_dir, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_git_status_in_repo() {
        // Run git status in the workspace root (which is a git repo)
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();

        let tool = GitTool::default();
        let result = tool
            .execute(serde_json::json!({"args": "status"}), workspace)
            .await;
        assert!(result.success, "{:?}", result.error_msg);
        // git status output should mention "On branch" or "HEAD"
        assert!(
            result.output.contains("branch") || result.output.contains("HEAD"),
            "Unexpected output: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn test_git_invalid_command() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();

        let tool = GitTool::default();
        let result = tool
            .execute(
                serde_json::json!({"args": "definitely-not-a-real-git-subcommand"}),
                workspace,
            )
            .await;
        assert!(!result.success);
        assert!(result.error_msg.is_some());
    }

    #[tokio::test]
    async fn test_git_defaults_to_status() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();

        let tool = GitTool::default();
        // No "args" key — should default to "status"
        let result = tool.execute(serde_json::json!({}), workspace).await;
        assert!(result.success, "{:?}", result.error_msg);
    }

    #[tokio::test]
    async fn bound_git_preserves_its_exact_ownership_stamps() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let session_id = uuid::Uuid::new_v4();
        let invocation_id = uuid::Uuid::new_v4();
        let tool = GitTool::new(Some(session_id), Some(invocation_id));

        let result = tool
            .execute(
                serde_json::json!({"args": "-c alias.session=!env session"}),
                workspace,
            )
            .await;

        assert!(result.success, "{:?}", result.error_msg);
        assert!(
            result.output.lines().any(|line| {
                line == format!("{}={session_id}", rsi_common::identity::ENV_SESSION_ID)
            }),
            "bound session stamp missing from git environment"
        );
        assert!(
            result.output.lines().any(|line| {
                line == format!(
                    "{}={invocation_id}",
                    rsi_common::identity::ENV_MODEL_INVOCATION_ID
                )
            }),
            "bound invocation stamp missing from git environment"
        );
    }

    #[tokio::test]
    async fn git_command_has_fixed_timeout() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let tool = GitTool {
            timeout: Duration::from_millis(50),
            session_id: None,
            invocation_id: None,
        };

        let result = tool
            .execute(
                serde_json::json!({"args": "-c alias.hang=!sleep hang 0.2"}),
                workspace,
            )
            .await;

        assert!(!result.success);
        assert_eq!(
            result.error_msg.as_deref(),
            Some("Git command timed out after 50ms")
        );
    }
}
