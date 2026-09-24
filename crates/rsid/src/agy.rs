use crate::claude::{LaunchConfig, StreamEvent};
use crate::error::{DaemonError, Result};
use crate::model_control::CliExecutionCapability;
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::process_control::{
    AGY_MAX_TURN_BYTES, BoundedLineError, BoundedLines, CaptureLimits, PROVIDER_MAX_LINE_BYTES,
    ProcessContainment, capture_bounded, configure_tokio_process_group, terminate_process_group,
};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// Wraps a running Antigravity CLI (`agy`) process.
pub struct AgyProcess {
    child: Child,
}

impl AgyProcess {
    /// Send SIGINT to gracefully interrupt the session.
    pub fn interrupt(&self) -> Result<()> {
        if let Some(pid) = self.child.id() {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGINT,
            )
            .map_err(|e| DaemonError::Process(format!("Failed to send SIGINT: {}", e)))?;
        }
        Ok(())
    }

    /// Force kill the process.
    pub async fn kill(&mut self) -> Result<()> {
        self.child.kill().await?;
        Ok(())
    }

    /// Wait for the process to exit.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        Ok(self.child.wait().await?)
    }

    /// Check if the process is still running.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }
}

/// Client for interacting with the Antigravity CLI.
pub struct AgyClient {
    binary_path: PathBuf,
}

fn format_model_name(id: &str) -> String {
    match id {
        "gemini-3-flash" => "Gemini 3.0 Flash".to_string(),
        "gemini-3-pro-high" => "Gemini 3.0 Pro High".to_string(),
        "gemini-3-pro-low" => "Gemini 3.0 Pro Low".to_string(),
        "gemini-3.5-flash" => "Gemini 3.5 Flash".to_string(),
        "gemini-3.5-flash-low" => "Gemini 3.5 Flash (Low)".to_string(),
        "gemini-3.5-flash-medium" => "Gemini 3.5 Flash (Medium)".to_string(),
        "gemini-3.5-flash-high" => "Gemini 3.5 Flash (High)".to_string(),
        "gemini-3.5-pro-high" => "Gemini 3.5 Pro (High)".to_string(),
        "gemini-3.5-pro-medium" => "Gemini 3.5 Pro (Medium)".to_string(),
        "gemini-3.5-pro-low" => "Gemini 3.5 Pro (Low)".to_string(),
        "gpt-oss-120b-medium" => "GPT OSS 120B Medium".to_string(),
        other => {
            let words: Vec<String> = other
                .split('-')
                .map(|w| {
                    if w == "gpt" {
                        "GPT".to_string()
                    } else if w == "oss" {
                        "OSS".to_string()
                    } else if w == "pro" {
                        "Pro".to_string()
                    } else if w == "flash" {
                        "Flash".to_string()
                    } else if w == "low" {
                        "Low".to_string()
                    } else if w == "high" {
                        "High".to_string()
                    } else if w == "thinking" {
                        "(Thinking)".to_string()
                    } else {
                        let mut chars = w.chars();
                        match chars.next() {
                            None => String::new(),
                            Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                        }
                    }
                })
                .collect();
            words.join(" ")
        }
    }
}

/// Build a single coalesced assistant StreamEvent from agy's full --print
/// stdout. `agy --print` has no machine-readable framing, so the whole turn
/// is one message. Interior newlines/blank lines are preserved (markdown lists
/// and headings stay intact); leading/trailing whitespace is trimmed. Returns
/// None when there is no meaningful output.
fn assistant_event_from_output(output: &str) -> Option<StreamEvent> {
    let mut thinking_lines = Vec::new();
    let mut text_lines = Vec::new();

    for line in output.lines() {
        if line.trim_start().starts_with("I will") {
            thinking_lines.push(line);
        } else {
            text_lines.push(line);
        }
    }

    let thinking = thinking_lines.join("\n").trim().to_string();
    let mut text = text_lines.join("\n").trim().to_string();

    if thinking.is_empty() && text.is_empty() {
        return None;
    }

    // Ensure a newline boundary exists between thinking and text when they are
    // concatenated by the session monitor. Otherwise, block-level directives like
    // `<docregblock>` won't be anchored at line start (`^`).
    if !thinking.is_empty() && !text.is_empty() {
        text = format!("\n{}", text);
    }

    let mut content = Vec::new();
    if !thinking.is_empty() {
        content.push(serde_json::json!({
            "type": "thinking",
            "thinking": thinking,
        }));
    }
    if !text.is_empty() {
        content.push(serde_json::json!({
            "type": "text",
            "text": text,
        }));
    }

    Some(StreamEvent {
        event_type: "assistant".to_string(),
        data: serde_json::json!({
            "message": {
                "role": "assistant",
                "content": content,
            },
        }),
    })
}

impl AgyClient {
    /// Create a new client, finding the Antigravity binary in PATH.
    pub fn new() -> Result<Self> {
        let binary_path = which::which("agy")
            .or_else(|_| which::which("antigravity"))
            .or_else(|_| which::which("antigravity-cli"))
            .map_err(|_| DaemonError::AgyBinaryNotFound)?;
        tracing::info!(path = %binary_path.display(), "Found Antigravity binary");
        Ok(Self { binary_path })
    }

    /// Check if the Antigravity binary is available without creating a client.
    pub fn is_available() -> bool {
        which::which("agy").is_ok()
            || which::which("antigravity").is_ok()
            || which::which("antigravity-cli").is_ok()
    }

    /// Discover available models.
    pub async fn discover_models(&self) -> Vec<(String, String)> {
        let fallback_list = vec![
            ("gemini-3-flash".to_string(), "Gemini 3.0 Flash".to_string()),
            (
                "gemini-3-pro-high".to_string(),
                "Gemini 3.0 Pro High".to_string(),
            ),
            (
                "gemini-3-pro-low".to_string(),
                "Gemini 3.0 Pro Low".to_string(),
            ),
            (
                "gemini-3.5-flash".to_string(),
                "Gemini 3.5 Flash".to_string(),
            ),
            (
                "gemini-3.5-flash-low".to_string(),
                "Gemini 3.5 Flash (Low)".to_string(),
            ),
            (
                "gemini-3.5-flash-medium".to_string(),
                "Gemini 3.5 Flash (Medium)".to_string(),
            ),
            (
                "gemini-3.5-flash-high".to_string(),
                "Gemini 3.5 Flash (High)".to_string(),
            ),
            (
                "gemini-3.5-pro-high".to_string(),
                "Gemini 3.5 Pro (High)".to_string(),
            ),
            (
                "gemini-3.5-pro-medium".to_string(),
                "Gemini 3.5 Pro (Medium)".to_string(),
            ),
            (
                "gemini-3.5-pro-low".to_string(),
                "Gemini 3.5 Pro (Low)".to_string(),
            ),
            (
                "gpt-oss-120b-medium".to_string(),
                "GPT OSS 120B Medium".to_string(),
            ),
        ];

        let mut command = Command::new("openclaw");
        command
            .args([
                "models",
                "list",
                "--all",
                "--provider",
                "google-antigravity",
                "--plain",
            ])
            .env(
                rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
                rsi_common::identity::process_ownership_namespace(),
            );
        let mut models = Vec::new();
        if let Ok(out) = capture_bounded(
            command,
            CaptureLimits::catalog(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                for line in stdout.lines() {
                    if !line.starts_with("google-antigravity/") {
                        continue;
                    }
                    let clean_id = line.strip_prefix("google-antigravity/").unwrap();
                    // Antigravity is Gemini-only for this deployment — drop any
                    // Claude/Anthropic model the live CLI happens to report.
                    if clean_id.starts_with("claude-") {
                        continue;
                    }
                    let display_name = format_model_name(clean_id);
                    models.push((clean_id.to_string(), display_name));
                }
            }
        }

        if models.is_empty() {
            return fallback_list;
        }

        // Merge fallback list into discovered models to ensure static/fallback models
        // like Gemini 3.5 are always visible in the TUI model picker.
        for item in fallback_list {
            if !models.iter().any(|(id, _)| id == &item.0) {
                models.push(item);
            }
        }

        models
    }

    /// Launch a new Antigravity session.
    /// Returns the process handle and the event receiver.
    pub(crate) fn launch(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(AgyProcess, mpsc::Receiver<StreamEvent>)> {
        let invocation_id = execution.invocation_id();
        let mut cmd = Command::new(&self.binary_path);
        cmd.args([
            "-p",
            &config.query,
            "--dangerously-skip-permissions",
            "--print-timeout",
            "60m",
        ]);
        if let Some(resume_id) = &config.resume_session_id {
            cmd.args(["--conversation", resume_id]);
        }

        // Set working directory
        if let Some(dir) = &config.working_dir {
            cmd.current_dir(dir);
        }

        // System prompt injection via temp file + GEMINI_SYSTEM_MD env var.
        // Antigravity has no --system-prompt flag; we write to .gemini/system.md in the
        // working directory and point GEMINI_SYSTEM_MD at it.
        if let Some(prompt) = &config.system_prompt {
            if let Some(dir) = &config.working_dir {
                let system_md_path = dir.join(".gemini").join("system.md");
                if let Some(parent) = system_md_path.parent() {
                    if std::fs::create_dir_all(parent).is_ok()
                        && std::fs::write(&system_md_path, prompt).is_ok()
                    {
                        cmd.env("GEMINI_SYSTEM_MD", &system_md_path);
                    }
                }
            }
        }

        crate::claude::stamp_execution_environment(&mut cmd, config, invocation_id)?;

        // agy never prints its conversation id (GitHub issue
        // google-antigravity/antigravity-cli#7); on exit it writes
        // `{ "<abs cwd>": "<conversation-uuid>" }` to last_conversations.json.
        // Capture it from there on a fresh launch so the session is resumable.
        // Resumes already carry the id (passed via --conversation) and have it
        // persisted, so capture is skipped then.
        let conversation_cache_path = agy_conversation_cache_path();
        let capture_working_dir = config
            .working_dir
            .as_ref()
            .map(|d| std::fs::canonicalize(d).unwrap_or_else(|_| d.clone()));
        let do_file_capture = config.resume_session_id.is_none();
        let pre_snapshot = if do_file_capture {
            capture_working_dir
                .as_ref()
                .and_then(|d| read_last_conversation_for_dir(&conversation_cache_path, d))
        } else {
            None
        };

        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());
        configure_tokio_process_group(&mut cmd, ProcessContainment::Group)?;

        let mut child = execution
            .bind_command(RuntimeExecutionRoute::AntigravityCli, cmd)
            .spawn()?;
        let child_pid = child.id();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DaemonError::Process("Failed to capture stdout".to_string()))?;

        // Create channel for stream events
        let (event_tx, event_rx) = mpsc::channel(100);

        // Spawn task to read stdout line-by-line as plain text and stream to TUI.
        tokio::spawn(async move {
            let mut lines = BoundedLines::new(stdout, PROVIDER_MAX_LINE_BYTES);
            let mut buf = String::new();

            loop {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(error) => {
                        if let Some(pid) = child_pid {
                            terminate_process_group(nix::unistd::Pid::from_raw(pid as i32));
                        }
                        let error_class = if matches!(error, BoundedLineError::Exceeded { .. }) {
                            "provider_output_overflow"
                        } else {
                            "provider_output_read_error"
                        };
                        let _ = event_tx.try_send(StreamEvent {
                            event_type: "process_error".to_string(),
                            data: serde_json::json!({
                                "error": error.to_string(),
                                "source": "stdout",
                                "terminal": true,
                                "error_class": error_class,
                            }),
                        });
                        return;
                    }
                };
                if buf.len().saturating_add(line.len()).saturating_add(1) > AGY_MAX_TURN_BYTES {
                    if let Some(pid) = child_pid {
                        terminate_process_group(nix::unistd::Pid::from_raw(pid as i32));
                    }
                    let _ = event_tx.try_send(StreamEvent {
                        event_type: "process_error".to_string(),
                        data: serde_json::json!({
                            "error": format!(
                                "provider turn output exceeded {}-byte bound",
                                AGY_MAX_TURN_BYTES
                            ),
                            "source": "stdout",
                            "terminal": true,
                            "error_class": "provider_output_overflow",
                        }),
                    });
                    return;
                }
                buf.push_str(&line);
                buf.push('\n');
            }

            // Emit the entire turn as ONE assistant message (no per-line fragmentation).
            if let Some(event) = assistant_event_from_output(&buf) {
                let _ = event_tx.send(event).await;
            }

            // Capture the conversation id agy wrote to disk during this run.
            // Poll briefly to absorb the gap between stdout EOF and the cache
            // flush, and require the cwd entry to differ from the pre-launch
            // snapshot so a stale entry from an earlier run is never mistaken
            // for this one. Emitting it as a `session_id` event lets the
            // monitor persist it as the resume token (claude_session_id).
            if do_file_capture && let Some(dir) = &capture_working_dir {
                let mut captured: Option<String> = None;
                for _ in 0..15 {
                    if let Some(id) = read_last_conversation_for_dir(&conversation_cache_path, dir)
                        && pre_snapshot.as_deref() != Some(id.as_str())
                    {
                        captured = Some(id);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                match captured {
                    Some(id) => {
                        let sid_event = StreamEvent {
                            event_type: "system".to_string(),
                            data: serde_json::json!({ "session_id": id }),
                        };
                        let _ = event_tx.send(sid_event).await;
                    }
                    None => {
                        tracing::warn!(
                            working_dir = %dir.display(),
                            cache = %conversation_cache_path.display(),
                            "agy: conversation id not found in last_conversations.json; \
                             this session will not be resumable"
                        );
                    }
                }
            }

            let success_event = StreamEvent {
                event_type: "result".to_string(),
                data: serde_json::json!({
                    "result": "success",
                    "subtype": "turn_completed",
                    "duration_ms": 0,
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "cache_read_input_tokens": 0,
                    },
                }),
            };
            let _ = event_tx.send(success_event).await;
        });

        Ok((AgyProcess { child }, event_rx))
    }
}

/// Path to agy's per-cwd conversation-id cache:
/// `$GEMINI_DIR` (or `$HOME/.gemini`) + `/antigravity-cli/cache/last_conversations.json`.
///
/// agy shares the Gemini config directory; this file maps an absolute working
/// directory to the most recent conversation UUID for that directory. agy never
/// prints the id (see google-antigravity/antigravity-cli#7), so this on-disk map
/// is the only deterministic source for resuming a specific conversation.
fn agy_conversation_cache_path() -> PathBuf {
    let base = if let Some(dir) = std::env::var_os("GEMINI_DIR") {
        PathBuf::from(dir)
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".gemini")
    } else {
        PathBuf::from(".").join(".gemini")
    };
    base.join("antigravity-cli")
        .join("cache")
        .join("last_conversations.json")
}

/// Read the conversation UUID agy last associated with `working_dir` from its
/// on-disk cache. Returns `None` when the file or key is absent or unreadable.
fn read_last_conversation_for_dir(cache_path: &Path, working_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(cache_path).ok()?;
    let map: std::collections::HashMap<String, String> = serde_json::from_slice(&bytes).ok()?;
    let key = working_dir.to_str()?;
    map.get(key).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn test_launch_config(query: &str) -> LaunchConfig {
        LaunchConfig {
            query: query.to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            working_dir: None,
            provider: None,
            model: None,
            configured_context_window: None,
            max_turns: None,
            system_prompt: None,
            resume_session_id: None,
            session_kind: None,
            project_id: None,
            rsi_session_id: None,
            rsi_socket: None,
            rsi_session_token: None,
            continued_from: None,
            openai_base_url: None,
            openai_api_key: None,
            conversation_history: None,
            workflow_id: None,
            workflow_id_override: None,
            max_retries: None,
            group_id: None,
            parent_id: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: None,
            model_invocation_request_fingerprint: None,
            skip_project_model_default: false,
            model_invocation_purpose:
                rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
            sandbox: None,
            cargo_target_dir: None,
            execution_scratch: None,
            is_eval: false,
            skip_context_pipeline: false,
            capability_class: None,
            tags: vec![],
            topology_node_id: None,
            topology_iteration: 0,
            closure_selector: None,
        }
    }

    #[cfg(unix)]
    fn install_fake_agy(dir: &std::path::Path) -> (PathBuf, PathBuf, PathBuf) {
        let binary_path = dir.join("agy");
        let args_path = dir.join("agy_args.bin");
        let env_path = dir.join("agy_gemini_system_md.txt");
        fs::write(
            &binary_path,
            r#"#!/usr/bin/env bash
set -euo pipefail
args_out="$(dirname "$0")/agy_args.bin"
env_out="$(dirname "$0")/agy_gemini_system_md.txt"
: > "$args_out"
for arg in "$@"; do
  printf '%s\0' "$arg" >> "$args_out"
done
printf '%s' "${GEMINI_SYSTEM_MD:-}" > "$env_out"
printf '%s|%s|%s|%s|%s' "${CARGO_TARGET_DIR:-}" "${TMPDIR:-}" "${RSI_PROCESS_OWNERSHIP_NAMESPACE:-}" "${RSI_SESSION_ID:-}" "${RSI_MODEL_INVOCATION_ID:-}" > "$(dirname "$0")/agy_exec_env.txt"
printf 'agy ok\n'
"#,
        )
        .expect("write fake agy");
        let mut perms = fs::metadata(&binary_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&binary_path, perms).unwrap();
        (binary_path, args_path, env_path)
    }

    fn read_nul_args(path: &std::path::Path) -> Vec<String> {
        let bytes = fs::read(path).expect("read captured args");
        bytes
            .split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).expect("utf8 arg"))
            .collect()
    }

    fn assert_arg_pair(args: &[String], flag: &str, expected_value: &str) {
        let pos = args
            .iter()
            .position(|arg| arg == flag)
            .unwrap_or_else(|| panic!("missing {flag} in args: {args:?}"));
        assert_eq!(
            args.get(pos + 1).map(String::as_str),
            Some(expected_value),
            "wrong value for {flag}; args: {args:?}"
        );
    }

    #[test]
    fn test_agy_client_availability() {
        // Documents behavior without asserting (Antigravity may or may not be installed)
        let available = AgyClient::is_available();
        println!("Antigravity CLI available: {}", available);
    }

    #[tokio::test]
    async fn test_discover_models_returns_entries() {
        // Verify static list shape without needing the binary
        let client = AgyClient {
            binary_path: PathBuf::new(),
        };
        let models = client.discover_models().await;
        assert_eq!(models.len(), 11);
        assert_eq!(models[0].0, "gemini-3-flash");
        assert!(
            models.iter().all(|(id, _)| !id.starts_with("claude-")),
            "Antigravity is Gemini-only — no Claude models should appear in the static list"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launch_sends_query_arg_and_system_prompt_via_gemini_system_md() {
        let block = "<docregblock>\n\
/spawn_child kind=Bug tags=e2e\n\
QUERY:\n\
worker prompt body\n\
</docregblock>";
        let directive = crate::session::spawn_directive::SpawnDirective::parse(block)
            .unwrap()
            .unwrap();

        let tmp = TempDir::new().expect("tempdir");
        let workdir = tmp.path().join("work");
        fs::create_dir_all(&workdir).expect("create workdir");
        let (binary_path, args_path, env_capture_path) = install_fake_agy(tmp.path());
        let client = AgyClient { binary_path };

        let mut config = test_launch_config(&directive.query);
        config.working_dir = Some(workdir.clone());
        config.system_prompt = Some("assembled worker preamble".to_string());

        let (mut process, _rx) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::AntigravityCli),
            )
            .expect("launch fake agy");
        let status = process.wait().await.expect("wait fake agy");
        assert!(status.success(), "fake agy failed: {status}");

        let args = read_nul_args(&args_path);
        assert_arg_pair(&args, "-p", "worker prompt body");
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("<docregblock>") || arg.contains("/spawn_child")),
            "docregblock wrapper leaked into Antigravity argv: {args:?}"
        );

        let system_md = PathBuf::from(fs::read_to_string(&env_capture_path).unwrap());
        let expected_system_md = workdir.join(".gemini").join("system.md");
        assert_eq!(system_md, expected_system_md);
        assert_eq!(
            fs::read_to_string(&expected_system_md).unwrap(),
            "assembled worker preamble"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn antigravity_launch_stamps_authenticated_execution_environment() {
        use crate::sandbox::execution_scratch::SandboxExecutionScratch;

        let fixture_base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"))
            .join("slice8-agy-provider-fixtures");
        fs::create_dir_all(&fixture_base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("scratch-")
            .tempdir_in(&fixture_base)
            .unwrap();
        let scratch = SandboxExecutionScratch::prepare_for_test(root.path()).unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("agy-provider-")
            .tempdir_in(&fixture_base)
            .unwrap();
        let (binary_path, _, _) = install_fake_agy(tmp.path());
        let client = AgyClient { binary_path };
        let session_id = uuid::Uuid::new_v4();
        let mut config = test_launch_config("scratch");
        config.working_dir = Some(root.path().to_path_buf());
        config.rsi_session_id = Some(session_id);
        config.execution_scratch = Some(scratch.clone());
        let (mut process, _) = client
            .launch(
                &config,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::AntigravityCli),
            )
            .unwrap();
        assert!(process.wait().await.unwrap().success());
        let values = fs::read_to_string(tmp.path().join("agy_exec_env.txt")).unwrap();
        let parts: Vec<_> = values.split('|').collect();
        assert_eq!(parts[0], scratch.target().display().to_string());
        assert_eq!(parts[1], scratch.temp().display().to_string());
        assert_eq!(
            parts[2],
            rsi_common::identity::process_ownership_namespace()
        );
        assert_eq!(parts[3], session_id.to_string());
        assert!(!parts[4].is_empty());

        let ordinary_root = tempfile::Builder::new()
            .prefix("ordinary-")
            .tempdir_in(&fixture_base)
            .unwrap();
        let mut ordinary = test_launch_config("ordinary");
        ordinary.working_dir = Some(ordinary_root.path().to_path_buf());
        ordinary.rsi_session_id = Some(session_id);
        let (mut process, _) = client
            .launch(
                &ordinary,
                CliExecutionCapability::for_test(RuntimeExecutionRoute::AntigravityCli),
            )
            .unwrap();
        assert!(process.wait().await.unwrap().success());
        let values = fs::read_to_string(tmp.path().join("agy_exec_env.txt")).unwrap();
        let parts: Vec<_> = values.split('|').collect();
        assert_eq!(
            parts[0],
            std::env::var(rsi_common::identity::ENV_CARGO_TARGET_DIR).unwrap_or_default()
        );
        assert_eq!(
            parts[1],
            std::env::var(rsi_common::identity::ENV_TMPDIR).unwrap_or_default()
        );
        assert_eq!(
            parts[2],
            std::env::var(rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE)
                .unwrap_or_default()
        );
        assert_eq!(parts[3], session_id.to_string());
        assert!(!parts[4].is_empty());
    }

    #[test]
    fn coalesces_multiline_output_into_one_event() {
        let out =
            "I will list files.\nHere are the files:\n\n* a.rs\n* b.rs\n\n### Summary\nDone.\n";
        let ev = assistant_event_from_output(out).expect("event");
        assert_eq!(ev.event_type, "assistant");

        let thinking = ev.data["message"]["content"][0]["thinking"]
            .as_str()
            .unwrap();
        assert_eq!(thinking, "I will list files.");

        let text = ev.data["message"]["content"][1]["text"].as_str().unwrap();
        assert!(text.contains("* a.rs\n* b.rs")); // interior newlines preserved
        assert!(text.contains("### Summary"));
        assert!(!text.ends_with('\n')); // trailing trimmed
    }

    #[test]
    fn coalesces_thinking_only() {
        let out = "I will view the file.\nI will list the dir.\n";
        let ev = assistant_event_from_output(out).expect("event");
        assert_eq!(ev.event_type, "assistant");
        let content = ev.data["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(
            content[0]["thinking"],
            "I will view the file.\nI will list the dir."
        );
    }

    #[test]
    fn coalesces_text_only() {
        let out = "Here is the result.\nDone.\n";
        let ev = assistant_event_from_output(out).expect("event");
        assert_eq!(ev.event_type, "assistant");
        let content = ev.data["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Here is the result.\nDone.");
    }

    #[test]
    fn empty_output_yields_no_event() {
        assert!(assistant_event_from_output("\n\n   \n").is_none());
    }
}
