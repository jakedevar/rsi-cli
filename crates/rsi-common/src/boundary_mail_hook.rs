//! #1049: the Claude `PostToolUse` hook that delivers mail at a tool boundary.
//!
//! rsid launches Claude with a per-session `--settings` JSON that runs
//! `rsi-rpc boundary-mail-hook` after every tool call. The hook asks the daemon
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

/// The hook-only daemon verb (not part of the agent catalog).
pub const CLAIM_METHOD: &str = "ClaimBoundaryMail";
/// The `rsi-rpc` subcommand the Claude settings invoke.
pub const HOOK_SUBCOMMAND: &str = "boundary-mail-hook";
/// Daemon I/O bound for one claim.
pub const DAEMON_TIMEOUT: Duration = Duration::from_secs(3);
/// Claude-side hook timeout in seconds (must exceed [`DAEMON_TIMEOUT`]).
pub const CLAUDE_HOOK_TIMEOUT_SECS: u64 = 5;

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
            .filter(|role| !role.is_empty())
            .unwrap_or("agent");
        blocks.push(format!(
            "[message from {sender}, delivered at a tool boundary]\n{text}"
        ));
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

/// Run the hook with no event input (mail only). Always returns exit code 0.
pub fn run_hook<F>(stdout: &mut impl Write, dispatch: F) -> u8
where
    F: FnOnce(&Path, &str, Value, Duration) -> std::io::Result<RpcResponse>,
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
    F: FnOnce(&Path, &str, Value, Duration) -> std::io::Result<RpcResponse>,
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
    run_hook_with_input(
        &input,
        &crate::spill::SpillConfig::from_env(),
        &wrapper,
        stdout,
        dispatch,
    )
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
    dispatch: F,
) -> u8
where
    F: FnOnce(&Path, &str, Value, Duration) -> std::io::Result<RpcResponse>,
{
    let event: Option<Value> = serde_json::from_str(input).ok();
    let event_name = event
        .as_ref()
        .and_then(|e| e.get("hook_event_name"))
        .and_then(Value::as_str)
        .unwrap_or("PostToolUse");
    if event_name == "PreToolUse" {
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
    let mail = dispatch(&socket, CLAIM_METHOD, Value::Null, DAEMON_TIMEOUT)
        .ok()
        .and_then(|response| render_hook_output(&response));
    if let Some(output) = combine_post_output(mail, updated) {
        let _ = writeln!(stdout, "{output}");
    }
    0
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
        let code = run_hook(&mut out, |_, method, params, timeout| {
            assert_eq!(method, CLAIM_METHOD);
            assert!(params.is_null());
            assert_eq!(timeout, DAEMON_TIMEOUT);
            Ok(reply(json!({"messages": [
                {"message_id": "m1", "sender_role": "lead", "text": "hello"}
            ]})))
        });
        assert_eq!(code, 0);
        assert!(String::from_utf8(out).unwrap().contains("hello"));
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
