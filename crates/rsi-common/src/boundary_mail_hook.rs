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

/// Run the hook. `dispatch` performs the daemon call (injected for tests).
/// Always returns exit code 0.
pub fn run_hook<F>(stdout: &mut impl Write, dispatch: F) -> u8
where
    F: FnOnce(&Path, &str, Value, Duration) -> std::io::Result<RpcResponse>,
{
    let socket = crate::agent_rpc_client::resolve_socket_path();
    // The daemon target is the token-resolved caller; params carry nothing.
    let Ok(response) = dispatch(&socket, CLAIM_METHOD, Value::Null, DAEMON_TIMEOUT) else {
        return 0;
    };
    if let Some(output) = render_hook_output(&response) {
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
}
