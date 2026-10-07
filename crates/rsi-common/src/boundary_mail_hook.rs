//! #1049: the Claude `PostToolUse` hook that delivers mail at a tool boundary.
//!
//! rsid launches Claude with a per-session `--settings` JSON that runs
//! `rsi-rpc boundary-mail-hook` after every tool call. #1183: Codex CLI
//! launches (Codex, Pioneer, and the Codex routes of OpenRouter and Bedrock)
//! install the same command as a session-flag `PostToolUse` hook, with
//! [`MAIL_ONLY_FLAG`]: Codex accepts the same `additionalContext` answer. The hook asks the daemon
//! to claim this session's pending messages (`ClaimBoundaryMail`, authenticated
//! by the session's own `RSI_SESSION_TOKEN` from the environment, never a
//! file or argument) and prints Claude's `PostToolUse` output so the model sees
//! them as `additionalContext` after that tool result.
//!
//! Failure is silent by design: any error, timeout, empty answer or malformed
//! reply prints nothing and exits 0, so a tool call is never failed or delayed
//! by the hook.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::rpc::RpcResponse;

mod test_guard;

/// The hook-only daemon verb (not part of the agent catalog).
pub const CLAIM_METHOD: &str = "ClaimBoundaryMail";
/// The `rsi-rpc` subcommand the Claude settings invoke.
pub const HOOK_SUBCOMMAND: &str = "boundary-mail-hook";
/// #1183: deliver mail only (no #1097 spill or Bash rewrite). Codex hooks use
/// it: their tool-result shape differs from Claude's and they cannot take an
/// `updatedToolOutput` replacement.
pub const MAIL_ONLY_FLAG: &str = "--mail-only";
/// Daemon I/O bound for one claim.
pub const DAEMON_TIMEOUT: Duration = Duration::from_secs(3);
/// #1183: the hook-only verb the hook calls AFTER writing its output, so the
/// daemon records a delivery only once the text has really been printed.
pub const CONFIRM_METHOD: &str = "ConfirmBoundaryMail";
/// Daemon I/O bound for the confirmation (claim plus confirm stay inside the
/// provider's hook timeout).
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(1);

/// `ConfirmBoundaryMail` params: the ids the hook printed, and where the
/// provider records what it accepted from the hook (the `session_id` and
/// `transcript_path` of the provider's own hook input). Printing is not
/// delivery: the daemon records the mail delivered only once that transcript
/// shows the provider took the hook's context (#1183).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmBoundaryMailParams {
    pub message_ids: Vec<uuid::Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}
/// Provider-side hook timeout in seconds, Claude and Codex alike. Kept well
/// above the hook's own daemon I/O ([`DAEMON_TIMEOUT`] + [`CONFIRM_TIMEOUT`])
/// so the provider does not kill a hook that already printed (#1183).
pub const CLAUDE_HOOK_TIMEOUT_SECS: u64 = 10;

/// One delivered message as the model sees it: the labelled block the hook
/// prints, and the exact text the daemon looks for in the provider transcript.
#[must_use]
pub fn render_mail_block(sender_role: &str, text: &str) -> String {
    let sender = if sender_role.is_empty() {
        "agent"
    } else {
        sender_role
    };
    format!("[message from {sender}, delivered at a tool boundary]\n{text}")
}

/// Whether one provider transcript line records that the provider accepted
/// hook `additionalContext` containing `block` (#1183): Codex writes a
/// developer `response_item` tagged `hooks.additional_context`; Claude writes
/// a `hook_additional_context` attachment for a `PostToolUse` hook. A hook the
/// provider timed out or rejected leaves no such line.
#[must_use]
pub fn transcript_line_accepts_block(line: &str, block: &str) -> bool {
    if !line.contains("additional_context") {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    let texts: Vec<&str> = match value.get("type").and_then(Value::as_str) {
        Some("response_item") => {
            let payload = &value["payload"];
            let hook_context =
                payload["internal_chat_message_metadata_passthrough"]["content_item_kinds"]
                    .as_array()
                    .is_some_and(|kinds| {
                        kinds
                            .iter()
                            .any(|kind| kind.as_str() == Some("hooks.additional_context"))
                    });
            if payload["type"] != "message" || payload["role"] != "developer" || !hook_context {
                return false;
            }
            payload["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect()
        }
        Some("attachment") => {
            let attachment = &value["attachment"];
            if attachment["type"] != "hook_additional_context"
                || attachment["hookEvent"] != "PostToolUse"
            {
                return false;
            }
            attachment["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect()
        }
        _ => return false,
    };
    texts.iter().any(|text| text.contains(block))
}

/// The `--settings` JSON that installs the hook for one Claude launch.
/// `command` is the shell command to run (the rsi-rpc invocation).
#[must_use]
pub fn claude_settings_json(command: &str) -> String {
    json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": CLAUDE_HOOK_TIMEOUT_SECS,
                }]
            }],
            "PostToolUse": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": CLAUDE_HOOK_TIMEOUT_SECS,
                }]
            }]
        }
    })
    .to_string()
}

/// Render the daemon's claim reply as Claude's `PostToolUse` hook output, or
/// `None` when there is nothing to deliver or the reply is unusable.
#[must_use]
pub fn render_hook_output(response: &RpcResponse) -> Option<String> {
    if response.error.is_some() {
        return None;
    }
    let messages = response.result.as_ref()?.get("messages")?.as_array()?;
    let mut blocks = Vec::new();
    for message in messages {
        let text = message.get("text").and_then(Value::as_str)?;
        if text.trim().is_empty() {
            continue;
        }
        let sender = message
            .get("sender_role")
            .and_then(Value::as_str)
            .unwrap_or("");
        blocks.push(render_mail_block(sender, text));
    }
    if blocks.is_empty() {
        return None;
    }
    Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": blocks.join("\n\n"),
            }
        })
        .to_string(),
    )
}

/// Merge the spill replacement (#1097) into the mail output as one
/// `PostToolUse` answer.
#[must_use]
fn combine_post_output(mail: Option<String>, updated: Option<Value>) -> Option<String> {
    let Some(updated) = updated else {
        return mail;
    };
    let mut output = mail
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_else(|| json!({"hookSpecificOutput": {"hookEventName": "PostToolUse"}}));
    output["hookSpecificOutput"]["updatedToolOutput"] = updated;
    Some(output.to_string())
}

/// The `PreToolUse` answer routing a heavy Bash command through the spill
/// wrapper (#1097), or `None` to leave the call alone.
fn render_pre_tool_use(event: &Value, wrapper: &str) -> Option<String> {
    if std::env::var(crate::spill::ENV_PRE).is_ok_and(|v| v.trim() == "0") {
        return None;
    }
    if event.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    let updated = crate::spill::rewrite_bash_input(event, wrapper)?;
    // No permissionDecision: the rewrite must not bypass the permission flow.
    Some(
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "updatedInput": updated}})
            .to_string(),
    )
}

/// Deny broad process signals (#1227) and unscoped library tests (#1339).
fn render_shell_denial(event: &Value, full_suite: bool) -> Option<String> {
    if event.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    let command = event
        .get("tool_input")
        .and_then(|input| input.get("command"))
        .and_then(Value::as_str)?;
    let reason = crate::kill_guard::broad_kill_refusal(command)
        .or_else(|| test_guard::refusal(command, full_suite))?;
    Some(
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }})
        .to_string(),
    )
}

/// Run the hook with no event input (mail only). Always returns exit code 0.
pub fn run_hook<F>(stdout: &mut impl Write, dispatch: F) -> u8
where
    F: FnMut(&Path, &str, Value, Duration) -> std::io::Result<RpcResponse>,
{
    run_hook_with_input(
        "",
        &crate::spill::SpillConfig::from_env(),
        "rsi-rpc",
        stdout,
        dispatch,
    )
}

/// Run the hook for the event Claude pipes on stdin. Largest accepted input.
const MAX_HOOK_INPUT: u64 = 256 * 1024 * 1024;

/// [`run_hook_with_input`] reading the event from this process's stdin.
pub fn run_hook_stdin<F>(stdout: &mut impl Write, dispatch: F) -> u8
where
    F: FnMut(&Path, &str, Value, Duration) -> std::io::Result<RpcResponse>,
{
    run_hook_stdin_with_args(&[], stdout, dispatch)
}

/// [`run_hook_stdin`] honouring the subcommand's trailing `args`
/// ([`MAIL_ONLY_FLAG`] turns the #1097 spill off for this event).
pub fn run_hook_stdin_with_args<F>(args: &[String], stdout: &mut impl Write, dispatch: F) -> u8
where
    F: FnMut(&Path, &str, Value, Duration) -> std::io::Result<RpcResponse>,
{
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin()
        .lock()
        .take(MAX_HOOK_INPUT)
        .read_to_string(&mut input);
    let wrapper = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.to_str().map(str::to_string))
        .unwrap_or_else(|| "rsi-rpc".to_string());
    let cfg = spill_config_for_args(args, crate::spill::SpillConfig::from_env());
    run_hook_with_input(&input, &cfg, &wrapper, stdout, dispatch)
}

/// The spill configuration for one hook run: [`MAIL_ONLY_FLAG`] disables it.
fn spill_config_for_args(
    args: &[String],
    mut cfg: crate::spill::SpillConfig,
) -> crate::spill::SpillConfig {
    if args.iter().any(|arg| arg == MAIL_ONLY_FLAG) {
        cfg.disabled = true;
    }
    cfg
}

/// Run the hook for one event. `PreToolUse` routes heavy Bash commands through
/// `<wrapper> spill`; `PostToolUse` spills a large result (#1097) and delivers
/// mail (#1049) in one answer. `dispatch` performs the daemon call (injected
/// for tests). Always returns exit code 0; every failure prints nothing.
pub fn run_hook_with_input<F>(
    input: &str,
    cfg: &crate::spill::SpillConfig,
    wrapper: &str,
    stdout: &mut impl Write,
    mut dispatch: F,
) -> u8
where
    F: FnMut(&Path, &str, Value, Duration) -> std::io::Result<RpcResponse>,
{
    let event: Option<Value> = serde_json::from_str(input).ok();
    let event_name = event
        .as_ref()
        .and_then(|e| e.get("hook_event_name"))
        .and_then(Value::as_str)
        .unwrap_or("PostToolUse");
    if event_name == "PreToolUse" {
        // Guards apply regardless of the spill switches. Only the test guard
        // has a QA opt-out; it never bypasses the process signal guard.
        let full_suite = std::env::var("RSI_FULL_SUITE").as_deref() == Ok("1");
        if let Some(output) = event
            .as_ref()
            .and_then(|event| render_shell_denial(event, full_suite))
        {
            let _ = writeln!(stdout, "{output}");
            return 0;
        }
        if cfg.disabled {
            return 0;
        }
        if let Some(output) = event.as_ref().and_then(|e| render_pre_tool_use(e, wrapper)) {
            let _ = writeln!(stdout, "{output}");
        }
        return 0;
    }
    let updated = event
        .as_ref()
        .and_then(|e| crate::spill::spill_tool_response(e, cfg));
    let socket = crate::agent_rpc_client::resolve_socket_path();
    // The daemon target is the token-resolved caller; params carry nothing.
    let claimed = dispatch(&socket, CLAIM_METHOD, Value::Null, DAEMON_TIMEOUT).ok();
    let mail = claimed.as_ref().and_then(render_hook_output);
    let delivered = if mail.is_some() {
        claimed
            .as_ref()
            .map(rendered_message_ids)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let Some(output) = combine_post_output(mail, updated) else {
        return 0;
    };
    let written = writeln!(stdout, "{output}").and_then(|()| stdout.flush());
    // #1183: tell the daemon the mail is out only once it is. A hook that dies
    // before this leaves the delivery unconfirmed, which the daemon settles
    // `uncertain`, never acknowledged.
    if written.is_ok() && !delivered.is_empty() {
        let field = |name: &str| {
            event
                .as_ref()
                .and_then(|e| e.get(name))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let mut params = json!({ "message_ids": delivered });
        if let Some(session_id) = field("session_id") {
            params["provider_session_id"] = Value::String(session_id);
        }
        if let Some(path) = field("transcript_path") {
            params["transcript_path"] = Value::String(path);
        }
        let _ = dispatch(&socket, CONFIRM_METHOD, params, CONFIRM_TIMEOUT);
    }
    0
}

/// The ids of the messages [`render_hook_output`] printed.
fn rendered_message_ids(response: &RpcResponse) -> Vec<String> {
    response
        .result
        .as_ref()
        .and_then(|result| result.get("messages"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| {
            message
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
        })
        .filter_map(|message| message.get("message_id")?.as_str().map(str::to_string))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(result: Value) -> RpcResponse {
        RpcResponse::success(Some(json!(1)), result)
    }

    #[test]
    fn output_carries_sender_labelled_additional_context() {
        let out = render_hook_output(&reply(json!({"messages": [
            {"message_id": "m1", "sender_role": "manager", "text": "watch your context"},
            {"message_id": "m2", "sender_role": "operator", "text": "stop after tests"},
        ]})))
        .expect("output");
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        let context = value["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(context.contains("[message from manager, delivered at a tool boundary]"));
        assert!(context.contains("watch your context"));
        assert!(context.contains("[message from operator, delivered at a tool boundary]"));
    }

    #[test]
    fn nothing_to_deliver_prints_nothing() {
        for result in [json!({"messages": []}), json!({}), json!({"messages": 3})] {
            assert_eq!(render_hook_output(&reply(result)), None);
        }
        let error = RpcResponse::error(
            Some(json!(1)),
            crate::rpc::RpcError {
                code: -32602,
                message: "denied".into(),
                data: None,
            },
        );
        assert_eq!(render_hook_output(&error), None);
    }

    #[test]
    fn transport_failure_prints_nothing_and_exits_zero() {
        let mut out = Vec::new();
        let code = run_hook(&mut out, |_, _, _, _| {
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "slow"))
        });
        assert_eq!(code, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn a_claimed_message_is_printed_and_the_daemon_is_asked_with_a_bound() {
        let mut out = Vec::new();
        let mut calls = Vec::new();
        let code = run_hook(&mut out, |_, method, params, timeout| {
            calls.push((method.to_string(), params.clone(), timeout));
            Ok(reply(json!({"messages": [
                {"message_id": "m1", "sender_role": "lead", "text": "hello"}
            ]})))
        });
        assert_eq!(code, 0);
        assert!(String::from_utf8(out).unwrap().contains("hello"));
        assert_eq!(
            calls[0],
            (CLAIM_METHOD.to_string(), Value::Null, DAEMON_TIMEOUT)
        );
        // #1183: confirmed only after the output was written.
        assert_eq!(
            calls[1],
            (
                CONFIRM_METHOD.to_string(),
                json!({"message_ids": ["m1"]}),
                CONFIRM_TIMEOUT
            )
        );
        assert_eq!(calls.len(), 2);
    }

    /// A writer that fails, standing in for a hook whose stdout is gone.
    struct BrokenStdout;
    impl Write for BrokenStdout {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
        }
    }

    /// #1183 review: mail the hook could not print is never confirmed, so the
    /// daemon settles it `uncertain` instead of letting a later assistant event
    /// acknowledge it.
    #[test]
    fn mail_that_could_not_be_printed_is_never_confirmed() {
        let mut methods = Vec::new();
        let code = run_hook(&mut BrokenStdout, |_, method, _, _| {
            methods.push(method.to_string());
            Ok(reply(json!({"messages": [
                {"message_id": "m1", "sender_role": "manager", "text": "hello"}
            ]})))
        });
        assert_eq!(code, 0);
        assert_eq!(methods, vec![CLAIM_METHOD.to_string()]);
    }

    /// #1183: the shapes Codex (0.159.1 rollout) and Claude Code (2.1.x
    /// transcript) write when they ACCEPT a hook's `additionalContext`.
    #[test]
    fn transcript_evidence_is_the_providers_accepted_hook_context_only() {
        let block = render_mail_block("manager", "MAILMARKER-1183");
        let codex = json!({"timestamp": "2026-10-05T23:12:00.775Z", "type": "response_item",
            "payload": {"type": "message", "role": "developer",
                "content": [{"type": "input_text", "text": block}],
                "internal_chat_message_metadata_passthrough":
                    {"turn_id": "t", "content_item_kinds": ["hooks.additional_context"]}}})
        .to_string();
        assert!(transcript_line_accepts_block(&codex, &block));
        let claude = json!({"type": "attachment", "attachment": {
            "type": "hook_additional_context", "hookName": "PostToolUse:Bash",
            "hookEvent": "PostToolUse",
            "content": [format!("{}\n\n{}", render_mail_block("operator", "x"), block)]}})
        .to_string();
        assert!(transcript_line_accepts_block(&claude, &block));

        // Not hook context: the model quoting the block, a user message, or
        // context for another message.
        let assistant = json!({"type": "response_item", "payload": {"type": "message",
            "role": "assistant", "content": [{"type": "output_text", "text": block}],
            "internal_chat_message_metadata_passthrough":
                {"content_item_kinds": ["hooks.additional_context"]}}})
        .to_string();
        assert!(!transcript_line_accepts_block(&assistant, &block));
        let untagged = json!({"type": "response_item", "payload": {"type": "message",
            "role": "developer", "content": [{"type": "input_text", "text": block}]}})
        .to_string();
        assert!(!transcript_line_accepts_block(&untagged, &block));
        assert!(!transcript_line_accepts_block(
            &codex,
            &render_mail_block("manager", "another message")
        ));
        assert!(!transcript_line_accepts_block(
            "not json additional_context",
            &block
        ));
    }

    #[test]
    fn confirm_params_reject_unknown_fields() {
        let ok: ConfirmBoundaryMailParams =
            serde_json::from_value(json!({"message_ids": [uuid::Uuid::nil()]})).unwrap();
        assert_eq!(ok.message_ids, vec![uuid::Uuid::nil()]);
        assert!(
            serde_json::from_value::<ConfirmBoundaryMailParams>(
                json!({"message_ids": [], "session_id": "x"})
            )
            .is_err()
        );
    }

    #[test]
    fn settings_json_installs_a_bounded_post_tool_use_command_hook() {
        let value: Value =
            serde_json::from_str(&claude_settings_json("rsi-rpc boundary-mail-hook")).unwrap();
        let hook = &value["hooks"]["PostToolUse"][0]["hooks"][0];
        assert_eq!(hook["type"], "command");
        assert_eq!(hook["command"], "rsi-rpc boundary-mail-hook");
        assert!(hook["timeout"].as_u64().unwrap() >= DAEMON_TIMEOUT.as_secs());
    }

    fn spill_cfg(root: &Path) -> crate::spill::SpillConfig {
        crate::spill::SpillConfig {
            root: root.to_path_buf(),
            max_bytes: crate::spill::DEFAULT_MAX_BYTES,
            max_lines: crate::spill::DEFAULT_MAX_LINES,
            disabled: false,
        }
    }

    #[test]
    fn settings_json_also_routes_heavy_bash_through_pre_tool_use() {
        let value: Value = serde_json::from_str(&claude_settings_json("rsi-rpc x")).unwrap();
        let pre = &value["hooks"]["PreToolUse"][0];
        assert_eq!(pre["matcher"], "Bash");
        assert_eq!(pre["hooks"][0]["command"], "rsi-rpc x");
    }

    #[test]
    fn pre_tool_use_rewrites_a_heavy_bash_command_without_a_permission_decision() {
        let event = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": {"command": "cargo test -p rsid", "description": "t"}});
        let mut out = Vec::new();
        let tmp = tempfile::tempdir().unwrap();
        let code = run_hook_with_input(
            &event.to_string(),
            &spill_cfg(tmp.path()),
            "/x/rsi-rpc",
            &mut out,
            |_, _, _, _| panic!("PreToolUse must not call the daemon"),
        );
        assert_eq!(code, 0);
        let value: Value = serde_json::from_slice(&out).unwrap();
        let specific = &value["hookSpecificOutput"];
        assert_eq!(specific["hookEventName"], "PreToolUse");
        assert!(specific.get("permissionDecision").is_none());
        assert_eq!(
            specific["updatedInput"]["command"],
            "'/x/rsi-rpc' spill --shell 'cargo test -p rsid'"
        );
        assert_eq!(specific["updatedInput"]["description"], "t");
    }

    /// #1227: a pattern kill is denied before it runs, even with the spill
    /// rewrite switched off.
    #[test]
    fn pre_tool_use_denies_a_pattern_kill_even_with_spill_disabled() {
        let event = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": {"command": "pkill -f \"run-rsid-test-shards.sh shard\"; sleep 2"}});
        let mut out = Vec::new();
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = spill_cfg(tmp.path());
        cfg.disabled = true;
        let code = run_hook_with_input(
            &event.to_string(),
            &cfg,
            "rsi-rpc",
            &mut out,
            |_, _, _, _| panic!("PreToolUse must not call the daemon"),
        );
        assert_eq!(code, 0);
        let value: Value = serde_json::from_slice(&out).unwrap();
        let specific = &value["hookSpecificOutput"];
        assert_eq!(specific["hookEventName"], "PreToolUse");
        assert_eq!(specific["permissionDecision"], "deny");
        assert_eq!(
            specific["permissionDecisionReason"],
            crate::kill_guard::BROAD_KILL_REFUSAL
        );
    }

    #[test]
    fn pre_tool_use_leaves_light_commands_alone() {
        let event = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": {"command": "git status"}});
        let mut out = Vec::new();
        let tmp = tempfile::tempdir().unwrap();
        run_hook_with_input(
            &event.to_string(),
            &spill_cfg(tmp.path()),
            "rsi-rpc",
            &mut out,
            |_, _, _, _| panic!(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn post_tool_use_spills_a_large_result_and_still_delivers_mail() {
        let tmp = tempfile::tempdir().unwrap();
        let big = "test a::b ... ok\n".repeat(4000);
        let event = json!({"hook_event_name": "PostToolUse", "session_id": "cafe0001-aaaa",
            "tool_name": "Bash", "tool_input": {"command": "ls"},
            "tool_response": {"stdout": big, "stderr": "", "interrupted": false}});
        let mut out = Vec::new();
        let code = run_hook_with_input(
            &event.to_string(),
            &spill_cfg(tmp.path()),
            "rsi-rpc",
            &mut out,
            |_, _, _, _| {
                Ok(reply(
                    json!({"messages": [{"message_id": "m1", "sender_role": "manager", "text": "hi"}]}),
                ))
            },
        );
        assert_eq!(code, 0);
        let value: Value = serde_json::from_slice(&out).unwrap();
        let specific = &value["hookSpecificOutput"];
        assert_eq!(specific["hookEventName"], "PostToolUse");
        assert!(
            specific["additionalContext"]
                .as_str()
                .unwrap()
                .contains("hi")
        );
        let stdout = specific["updatedToolOutput"]["stdout"].as_str().unwrap();
        assert!(stdout.starts_with("[rsi-spill "), "{stdout}");
        assert!(stdout.len() < 2000);
        assert_eq!(specific["updatedToolOutput"]["interrupted"], false);
    }

    #[test]
    fn post_tool_use_spill_works_when_the_daemon_is_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let event = json!({"hook_event_name": "PostToolUse", "session_id": "cafe0002",
            "tool_name": "Bash", "tool_input": {"command": "ls"},
            "tool_response": {"stdout": "x\n".repeat(900), "stderr": ""}});
        let mut out = Vec::new();
        run_hook_with_input(
            &event.to_string(),
            &spill_cfg(tmp.path()),
            "rsi-rpc",
            &mut out,
            |_, _, _, _| Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "slow")),
        );
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert!(
            value["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
                .as_str()
                .unwrap()
                .starts_with("[rsi-spill ")
        );
        assert!(
            value["hookSpecificOutput"]
                .get("additionalContext")
                .is_none()
        );
    }

    /// #1183: the Codex hook (`--mail-only`) gets mail as `additionalContext`
    /// for Codex's PostToolUse event shape (a string `tool_response`) and never
    /// an output replacement, however large the result.
    #[test]
    fn mail_only_codex_post_tool_use_delivers_mail_without_a_spill() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = spill_config_for_args(&[MAIL_ONLY_FLAG.to_string()], spill_cfg(tmp.path()));
        assert!(cfg.disabled);
        assert!(!spill_config_for_args(&[], spill_cfg(tmp.path())).disabled);
        let event = json!({"hook_event_name": "PostToolUse", "session_id": "codex-thread",
            "transcript_path": "/home/u/.codex/sessions/rollout-x-codex-thread.jsonl",
            "turn_id": "t1", "tool_name": "Bash", "tool_input": {"command": "cargo test"},
            "tool_response": "test a::b ... ok\n".repeat(4000), "tool_use_id": "call_1"});
        let mut out = Vec::new();
        let code = run_hook_with_input(
            &event.to_string(),
            &cfg,
            "rsi-rpc",
            &mut out,
            |_, method, params, _| {
                if method == CLAIM_METHOD {
                    assert!(params.is_null());
                } else {
                    assert_eq!(method, CONFIRM_METHOD);
                    // #1183: the provider's own transcript locator rides along.
                    assert_eq!(
                        params,
                        json!({"message_ids": ["m"], "provider_session_id": "codex-thread",
                            "transcript_path": "/home/u/.codex/sessions/rollout-x-codex-thread.jsonl"})
                    );
                }
                Ok(reply(
                    json!({"messages": [{"message_id": "m", "sender_role": "manager", "text": "rebase first"}]}),
                ))
            },
        );
        assert_eq!(code, 0);
        let value: Value = serde_json::from_slice(&out).unwrap();
        let specific = &value["hookSpecificOutput"];
        assert_eq!(specific["hookEventName"], "PostToolUse");
        assert!(specific.get("updatedToolOutput").is_none());
        assert_eq!(
            specific["additionalContext"],
            "[message from manager, delivered at a tool boundary]\nrebase first"
        );
    }

    #[test]
    fn kill_switch_stops_both_the_rewrite_and_the_spill() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = spill_cfg(tmp.path());
        cfg.disabled = true;
        let pre = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": {"command": "cargo test"}});
        let mut out = Vec::new();
        run_hook_with_input(
            &pre.to_string(),
            &cfg,
            "rsi-rpc",
            &mut out,
            |_, _, _, _| panic!(),
        );
        assert!(out.is_empty());
        let post = json!({"hook_event_name": "PostToolUse", "session_id": "k",
            "tool_name": "Bash", "tool_input": {"command": "ls"},
            "tool_response": {"stdout": "x\n".repeat(5000), "stderr": ""}});
        let mut out = Vec::new();
        run_hook_with_input(
            &post.to_string(),
            &cfg,
            "rsi-rpc",
            &mut out,
            |_, _, _, _| {
                Ok(reply(
                    json!({"messages": [{"message_id": "m", "sender_role": "lead", "text": "mail"}]}),
                ))
            },
        );
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert!(
            value["hookSpecificOutput"]
                .get("updatedToolOutput")
                .is_none()
        );
        assert!(
            value["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .contains("mail")
        );
    }
}
