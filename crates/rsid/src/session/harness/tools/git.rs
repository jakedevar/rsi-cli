//! Git operations tool -- status, diff, log, branch operations.

use super::{HarnessTool, ToolContext, custody_guard, truncation::truncate_text};
use crate::process_control::{
    CaptureError, CaptureLimits, SESSION_TOOL_MAX_STREAM_BYTES, SESSION_TOOL_TIMEOUT,
    capture_bounded,
};
use crate::session::harness::types::ToolResult;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEFAULT_GIT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_BYTES: usize = SESSION_TOOL_MAX_STREAM_BYTES;

fn command_timeout(args: &serde_json::Value, default: Duration) -> Duration {
    args.get("timeout_secs")
        .and_then(|value| value.as_u64())
        .map(|seconds| Duration::from_secs(seconds.clamp(1, DEFAULT_GIT_TIMEOUT_SECS)))
        .unwrap_or(default.min(SESSION_TOOL_TIMEOUT))
}

/// A scoped, credential-scrubbed `git` command with this tool's ownership
/// stamps. Shared by the tool itself and the custody guard's read-only probes,
/// so a probe is reaped and attributed exactly like the command it checks.
pub(super) fn scoped_git_command<S: AsRef<std::ffi::OsStr>>(
    args: &[S],
    working_dir: &Path,
    session_id: Option<uuid::Uuid>,
    invocation_id: Option<uuid::Uuid>,
) -> Result<tokio::process::Command, String> {
    let limits = crate::process_scope::WorkerScopeLimits::from_launcher_snapshot()
        .map_err(|error| error.to_string())?;
    let scoped = crate::process_scope::ScopedWorkerCommand::new(
        std::ffi::OsStr::new("git"),
        invocation_id.unwrap_or_else(uuid::Uuid::new_v4),
        limits,
    )
    .map_err(|error| error.to_string())?;
    let mut command = scoped.into_command();
    command.args(args).current_dir(working_dir);
    command.env(
        rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
        rsi_common::identity::process_ownership_namespace(),
    );
    if let Some(session_id) = session_id {
        command.env(rsi_common::identity::ENV_SESSION_ID, session_id.to_string());
    }
    if let Some(invocation_id) = invocation_id {
        command.env(
            rsi_common::identity::ENV_MODEL_INVOCATION_ID,
            invocation_id.to_string(),
        );
    }

    // #694 K1: git can run agent-supplied aliases/hooks; no provider key.
    crate::vault::scrub_credential_env(&mut command);
    Ok(command)
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
        max_output_bytes: usize,
    ) -> ToolResult {
        let git_args = args
            .get("args")
            .and_then(|v| v.as_str())
            .unwrap_or("status");
        let timeout = command_timeout(&args, self.timeout);
        let parts: Vec<&str> = git_args.split_whitespace().collect();

        // S6 (#2256): the same custody rules as the shell tools, applied to
        // the exact argv git would receive. Nothing has run when this refuses.
        let probe = custody_guard::Probe {
            session_id: self.session_id,
            invocation_id: self.invocation_id,
            working_dir,
            cancel,
        };
        if let Some(refusal) = custody_guard::check_git_args(&parts, &probe).await {
            return custody_guard::refused_result(refusal);
        }

        let command =
            match scoped_git_command(&parts, working_dir, self.session_id, self.invocation_id) {
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
        match capture_bounded(command, limits, cancel).await {
            Ok(output) => {
                let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
                if !output.stderr.is_empty() {
                    if !output.stdout.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(&String::from_utf8_lossy(&output.stderr));
                }
                ToolResult {
                    success: output.status.success(),
                    output: truncate_text(
                        &combined,
                        max_output_bytes,
                        output.stdout_truncated || output.stderr_truncated,
                    )
                    .content,
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
         branch, add, commit, and other read/write git operations. Refused by \
         custody rules (error_code custody_git_refused): stash, checkout, switch, \
         reset --hard, force push, forced branch updates, and discarding \
         uncommitted changes (restore, clean -f); to set work aside commit it or \
         use scripts/rsi-stash from the shell tool. \
         Commands time out after 120 seconds."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","required":["args"],"properties":{"args":{"type":"string","description":"Git arguments (e.g., 'status', 'diff --cached', 'log --oneline -10')"},"timeout_secs":{"type":"integer","minimum":1,"maximum":120,"description":"Timeout in seconds (default and maximum: 120)"}}}"#
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        let cancel = CancellationToken::new();
        self.execute_with_cancel(args, working_dir, &cancel, MAX_OUTPUT_BYTES)
            .await
    }

    async fn execute_cancellable(
        &self,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
    ) -> ToolResult {
        self.execute_with_cancel(args, working_dir, cancel, MAX_OUTPUT_BYTES)
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
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn test_git_status_in_repo() {
        // Run git status in the workspace root (which is a git repo)
        let workspace = std::path::Path::new(rsi_common::manifest_dir!())
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn test_git_invalid_command() {
        let workspace = std::path::Path::new(rsi_common::manifest_dir!())
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn test_git_defaults_to_status() {
        let workspace = std::path::Path::new(rsi_common::manifest_dir!())
            .parent()
            .unwrap()
            .parent()
            .unwrap();

        let tool = GitTool::default();
        // No "args" key — should default to "status"
        let result = tool.execute(serde_json::json!({}), workspace).await;
        assert!(result.success, "{:?}", result.error_msg);
    }

    /// A throwaway repository with a `pre-commit` hook running `body`, and a
    /// staged change for `commit` to hook. The hook is the way a test reaches
    /// the environment of the git process (an alias would do it, but aliases are
    /// refused by the custody guard, #2287).
    #[cfg(unix)]
    fn repo_with_pre_commit_hook(body: &str) -> tempfile::TempDir {
        use super::custody_guard::test_support::{git, repo};
        use std::os::unix::fs::PermissionsExt;
        let repo = repo();
        let dir = repo.path();
        // A host-level `core.hooksPath` must not move the hook.
        git(dir, &["config", "core.hooksPath", ".git/hooks"]);
        let hook = dir.join(".git/hooks/pre-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("a.txt"), "staged\n").unwrap();
        git(dir, &["add", "a.txt"]);
        repo
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn bound_git_preserves_its_exact_ownership_stamps() {
        let repo = repo_with_pre_commit_hook("env");
        let session_id = uuid::Uuid::new_v4();
        let invocation_id = uuid::Uuid::new_v4();
        let tool = GitTool::new(Some(session_id), Some(invocation_id));

        let result = tool
            .execute(
                serde_json::json!({"args": "-c commit.gpgsign=false commit -q -m stamped"}),
                repo.path(),
            )
            .await;

        assert!(result.success, "{:?}", result.error_msg);
        assert!(
            result.output.lines().any(|line| {
                line == format!("{}={session_id}", rsi_common::identity::ENV_SESSION_ID)
            }),
            "bound session stamp missing from git environment: {}",
            result.output
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

    /// S6 (#2256): the git tool refuses the same custody commands as the shell
    /// tools, with the hard rule named, before git runs.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn git_tool_refuses_custody_commands_and_runs_nothing() {
        use super::custody_guard::test_support::{git, repo};
        let repo = repo();
        let dir = repo.path();
        std::fs::write(dir.join("a.txt"), "dirty\n").unwrap();
        std::fs::write(dir.join("sentinel.txt"), "keep\n").unwrap();
        let head = git(dir, &["rev-parse", "HEAD"]);
        let tool = GitTool::default();
        for (args, rule) in [
            ("stash", "hard rule 9"),
            ("stash pop", "hard rule 9"),
            ("stash push -u", "hard rule 9"),
            ("-C . stash", "hard rule 9"),
            ("-c user.name=x stash list", "hard rule 9"),
            // #2287: aliases are refused, however they are supplied.
            ("-c alias.x=!git x", "hard rules 1, 2 and 9"),
            ("-c Alias.co=checkout co main", "hard rules 1, 2 and 9"),
            ("--config-env=alias.x=E x", "hard rules 1, 2 and 9"),
            ("config alias.x stash", "hard rules 1, 2 and 9"),
            ("checkout main", "hard rule 2"),
            ("checkout -b other", "hard rule 2"),
            ("switch main", "hard rule 2"),
            ("reset --hard", "hard rule 2"),
            ("reset --hard HEAD~1", "hard rule 2"),
            ("push --force origin main", "hard rule 1"),
            ("push origin +main", "hard rule 1"),
            ("branch -D main", "hard rules 1 and 2"),
            ("checkout -- a.txt", "hard rules 8 and 9"),
            ("restore .", "hard rules 8 and 9"),
            ("clean -fd", "hard rules 8 and 9"),
        ] {
            let result = tool.execute(serde_json::json!({"args": args}), dir).await;
            assert!(!result.success, "{args}");
            assert_eq!(result.output, "", "{args}");
            let message = result.error_msg.unwrap_or_default();
            assert!(
                message.starts_with("custody_git_refused: "),
                "{args}: {message}"
            );
            assert!(message.contains(rule), "{args}: {message}");
        }
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn git_tool_still_runs_allowed_commands() {
        use super::custody_guard::test_support::repo;
        let repo = repo();
        let dir = repo.path();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let tool = GitTool::default();
        let run = |args: &'static str| {
            let tool = &tool;
            async move { tool.execute(serde_json::json!({"args": args}), dir).await }
        };
        let status = run("status --short").await;
        assert!(
            status.success && status.output.contains("new.txt"),
            "{status:?}"
        );
        for args in [
            "add new.txt",
            "-c commit.gpgsign=false commit -q -m added",
            "checkout -- a.txt",
            "restore --staged a.txt",
            "branch --show-current",
            "revert --no-edit HEAD",
            "push --dry-run origin",
        ] {
            let result = run(args).await;
            // The dry-run push has no remote and fails on its own, not by custody.
            if args.starts_with("push") {
                let message = result.error_msg.unwrap_or_default();
                assert!(!message.starts_with("custody_git_refused"), "{message}");
            } else {
                assert!(
                    result.success,
                    "{args}: {:?} {}",
                    result.error_msg, result.output
                );
            }
        }
        // `git reset <rev>` can drop custody ancestry (#3074), so the undo
        // example is `revert`: it adds a commit and keeps the original ancestry.
        let log = run("log --oneline").await.output;
        assert!(log.contains("init"), "{log}");
        assert!(log.contains("Revert"), "{log}");
        assert!(!dir.join("new.txt").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn git_description_points_at_rsi_stash_and_the_refusal_code() {
        let description = GitTool::default().description().to_owned();
        assert!(description.contains("custody_git_refused"), "{description}");
        assert!(description.contains("scripts/rsi-stash"), "{description}");
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn git_command_has_fixed_timeout() {
        // A hook that outlives the tool's timeout (an alias would do it, but
        // aliases are refused by the custody guard, #2287).
        let repo = repo_with_pre_commit_hook("sleep 2");
        let tool = GitTool {
            timeout: Duration::from_millis(50),
            session_id: None,
            invocation_id: None,
        };

        let result = tool
            .execute(
                serde_json::json!({"args": "-c commit.gpgsign=false commit -q -m slow"}),
                repo.path(),
            )
            .await;

        assert!(!result.success);
        assert_eq!(
            result.error_msg.as_deref(),
            Some("Git command timed out after 50ms")
        );
    }
}
