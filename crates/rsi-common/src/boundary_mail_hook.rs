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
/// The Claude tool whose `PreToolUse` call the hook answers with the typed
/// undeliverable notice (hub #1161).
pub const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";
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
    transcript_line_accepts_hook_block(line, block, "PostToolUse")
}

/// Accepted context for a specific hook event. Codex tags hook context without
/// an event name; Claude attachments must name the requested event.
#[must_use]
pub fn transcript_line_accepts_hook_block(line: &str, block: &str, hook_event: &str) -> bool {
    if block.is_empty() || !line.contains("additional_context") {
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
                || attachment["hookEvent"] != hook_event
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
            }, {
                // Hub #1161: a headless session's AskUserQuestion gets the
                // typed undeliverable notice as its tool result, in-turn.
                "matcher": ASK_USER_QUESTION_TOOL,
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

/// Hub #1161: deny an `AskUserQuestion` with the typed
/// `[rsi:ask_user_question_undeliverable]` notice, which the CLI hands to the
/// model as that call's tool result in the same turn. Every session this hook is
/// installed for is an rsid-launched headless `-p` run with no human on the
/// other end of the question, so the denial is unconditional; it needs no
/// daemon call, so there is nothing to fail or delay.
fn render_ask_user_question_denial(event: &Value) -> Option<String> {
    if event.get("tool_name").and_then(Value::as_str) != Some(ASK_USER_QUESTION_TOOL) {
        return None;
    }
    Some(
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": crate::ask_user_question_notice::undeliverable_notice(),
        }})
        .to_string(),
    )
}

/// Deny broad process signals (#1227), Python (#654) and unscoped
/// library tests (#1339).
fn render_shell_denial(event: &Value, full_suite: bool) -> Option<String> {
    if event.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    let command = event
        .get("tool_input")
        .and_then(|input| input.get("command"))
        .and_then(Value::as_str)?;
    let reason = crate::kill_guard::broad_kill_refusal(command)
        .map(str::to_owned)
        .or_else(|| crate::python_guard::python_refusal(command).map(str::to_owned))
        .or_else(|| test_guard::refusal(command, full_suite).map(str::to_owned))
        .or_else(|| {
            let cwd = event_cwd(event)?;
            custody_refusal(command, &cwd, &sandbox_base())
        })?;
    Some(
        json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }})
        .to_string(),
    )
}

/// The working directory the provider reports for the tool call, else the
/// hook's own (the provider launches the hook in the session's directory).
fn event_cwd(event: &Value) -> Option<std::path::PathBuf> {
    event
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
}

/// Where RSI sandboxes live: `RSI_SANDBOX_BASE`, else `~/.rsi/sandboxes`.
pub fn sandbox_base() -> std::path::PathBuf {
    std::env::var_os("RSI_SANDBOX_BASE")
        .filter(|base| !base.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".rsi").join("sandboxes")))
        .unwrap_or_default()
}

/// #2320: AGENTS.md hard rule 2 (never change the sandbox branch), rules 1, 8
/// and 9 for a CLI provider shell (Claude `Bash`, Codex `Bash`), the same scan
/// the Harness `shell` tool applies ([`crate::custody_scan`]). A command is
/// checked when it starts inside an RSI sandbox or when it names a path inside
/// the sandbox base (`git -C <sandbox>`, `cd <sandbox>`, `--git-dir=`,
/// `--work-tree=`; #2540): the shell's starting directory says nothing about the
/// repository a command targets. A command that never names a sandbox, run from
/// a repository elsewhere, is not ours to guard. `Some(reason)` denies the call
/// before it runs.
fn custody_refusal(command: &str, cwd: &Path, base: &Path) -> Option<String> {
    if !command_in_custody_scope(command, cwd, base) {
        return None;
    }
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    crate::custody_scan::check_command_sync_in(command, &cwd, &base)
}

/// Whether `command`, run from `cwd`, is inside RSI's git custody (#3165): it
/// starts inside the sandbox `base` or names a path under it. This is the one
/// location predicate of the sandbox branch custody; the CLI hook and the
/// Harness `shell`, `exec_command` and `git` tools share it, so a scratch
/// repository elsewhere is not guarded by either. An empty `base` guards nothing.
pub fn command_in_custody_scope(command: &str, cwd: &Path, base: &Path) -> bool {
    if base.as_os_str().is_empty() {
        return false;
    }
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    cwd.starts_with(&base) || command_names_path_under(command, &cwd, &base)
}

/// Whether any word of `command` names a path under `base`, resolved the way
/// the shell would from `cwd` (`~`, `$HOME`, `$RSI_SANDBOX_BASE`, relative
/// words, `.`/`..`). Lexical and conservative: a word is a candidate wherever
/// quoting or an operator puts it. A `cd` chain is not tracked, so a target
/// assembled across several `cd` words is not recognised; the turn-end branch
/// repair covers that residual.
fn command_names_path_under(command: &str, cwd: &Path, base: &Path) -> bool {
    let home = dirs::home_dir();
    let base_text = base.to_string_lossy();
    if command.contains(base_text.as_ref()) {
        return true;
    }
    let words = command.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                ';' | '&' | '|' | '(' | ')' | '<' | '>' | '"' | '\'' | '`' | '='
            )
    });
    words.filter(|word| !word.is_empty()).any(|word| {
        let expanded = if let Some(rest) = ["$RSI_SANDBOX_BASE", "${RSI_SANDBOX_BASE}"]
            .iter()
            .find_map(|var| word.strip_prefix(var))
        {
            base.join(rest.trim_start_matches('/'))
        } else if let Some(rest) = ["~", "$HOME", "${HOME}"]
            .iter()
            .find_map(|prefix| word.strip_prefix(prefix))
            .filter(|rest| rest.is_empty() || rest.starts_with('/'))
        {
            let Some(home) = home.as_ref() else {
                return false;
            };
            home.join(rest.trim_start_matches('/'))
        } else {
            cwd.join(word)
        };
        lexically_normalized(&expanded).starts_with(base)
    })
}

/// `path` with `.` and `..` folded away, without touching the filesystem.
fn lexically_normalized(path: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
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
    run_hook_with_input_and_args(args, &input, &cfg, &wrapper, stdout, dispatch)
}

/// Route the installed handler's part number through the hook-only lifecycle
/// request. It comes from RSI's argv, never from the provider hook input.
fn run_hook_with_input_and_args<F>(
    args: &[String],
    input: &str,
    cfg: &crate::spill::SpillConfig,
    wrapper: &str,
    stdout: &mut impl Write,
    mut dispatch: F,
) -> u8
where
    F: FnMut(&Path, &str, Value, Duration) -> std::io::Result<RpcResponse>,
{
    let dialect = crate::provider_lifecycle_hooks::HookOutputDialect::from_hook_args(args);
    let part = args.iter().find_map(|arg| {
        arg.strip_prefix("--session-start-part=")?
            .parse::<usize>()
            .ok()
    });
    if args.iter().any(|arg| arg == "--session-start-fallback") {
        // This handler has a single purpose. Never forward the user's prompt
        // or fall through to the mail/spill path.
        let Ok(mut event) = serde_json::from_str::<Value>(input) else {
            return 0;
        };
        if event["hook_event_name"] != "UserPromptSubmit" {
            return 0;
        }
        event["hook_event_name"] = serde_json::json!("SessionStart");
        let Some(mut params) =
            crate::provider_lifecycle_hooks::RecordProviderLifecycleEventParams::from_hook_input(
                &event,
            )
        else {
            return 0;
        };
        params.session_start_fallback = true;
        params.session_start_part = part;
        let socket = crate::agent_rpc_client::resolve_socket_path();
        if let Ok(reply) = dispatch(
            &socket,
            crate::provider_lifecycle_hooks::RECORD_METHOD,
            serde_json::to_value(params).expect("serializable hook params"),
            crate::provider_lifecycle_hooks::RECORD_TIMEOUT,
        ) && let Some(output) =
            crate::provider_lifecycle_hooks::render_start_context_output(&reply, "UserPromptSubmit")
        {
            let _ = writeln!(stdout, "{output}").and_then(|()| stdout.flush());
        }
        return 0;
    }
    run_hook_with_input_for(
        input,
        cfg,
        wrapper,
        dialect,
        stdout,
        |socket, method, mut params, timeout| {
            if method == crate::provider_lifecycle_hooks::RECORD_METHOD
                && params.get("event").and_then(Value::as_str) == Some("SessionStart")
                && let Some(part) = part
            {
                params["session_start_part"] = serde_json::json!(part);
            }
            dispatch(socket, method, params, timeout)
        },
    )
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
    dispatch: F,
) -> u8
where
    F: FnMut(&Path, &str, Value, Duration) -> std::io::Result<RpcResponse>,
{
    run_hook_with_input_for(
        input,
        cfg,
        wrapper,
        crate::provider_lifecycle_hooks::HookOutputDialect::Claude,
        stdout,
        dispatch,
    )
}

/// [`run_hook_with_input`] for the provider whose hook output shape is
/// `dialect` (#2898: only a `PreCompact` handoff instruction differs).
pub fn run_hook_with_input_for<F>(
    input: &str,
    cfg: &crate::spill::SpillConfig,
    wrapper: &str,
    dialect: crate::provider_lifecycle_hooks::HookOutputDialect,
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
    // #2766: a lifecycle event (`StopFailure`, `PreCompact`, `SessionStart`)
    // is reported and never claims mail; only a `PreCompact` handoff
    // instruction (#2898) and a `SessionStart` preamble (hub #2919) are
    // printed.
    if crate::provider_lifecycle_hooks::run_lifecycle_hook(
        event.as_ref(),
        dialect,
        stdout,
        &mut dispatch,
    ) {
        return 0;
    }
    if event_name == "PreToolUse" {
        if let Some(output) = event.as_ref().and_then(render_ask_user_question_denial) {
            let _ = writeln!(stdout, "{output}");
            return 0;
        }
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
    fn prompt_submit_fallback_transports_only_locator_and_renders_capped_context() {
        for mail_only in [false, true] {
            let mut args = vec![
                "--session-start-fallback".into(),
                "--session-start-part=2".into(),
            ];
            if mail_only {
                args.push(MAIL_ONLY_FLAG.into());
            }
            let mut output = Vec::new();
            let code = run_hook_with_input_and_args(
                &args,
                r#"{"hook_event_name":"UserPromptSubmit","session_id":"thread","transcript_path":"/thread.jsonl","prompt":"private query"}"#,
                &crate::spill::SpillConfig::from_env(),
                "rsi-rpc",
                &mut output,
                |_, method, params, _| {
                    assert_eq!(method, crate::provider_lifecycle_hooks::RECORD_METHOD);
                    assert_eq!(
                        params,
                        json!({"event":"SessionStart", "source":"other",
                        "session_start_fallback":true,"session_start_part":2,
                        "provider_session_id":"thread","transcript_path":"/thread.jsonl"})
                    );
                    Ok(RpcResponse::success(
                        Some(json!(1)),
                        json!({"outcome":"served", "additional_context":"PART TWO"}),
                    ))
                },
            );
            assert_eq!(code, 0);
            let output: Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(
                output,
                json!({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"PART TWO"}})
            );
        }
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
    fn session_start_evidence_requires_exact_accepted_context() {
        let block = "[RSI preamble part 1/2]\n\nPREAMBLE";
        let claude = json!({"type": "attachment", "attachment": {
            "type": "hook_additional_context", "hookEvent": "SessionStart", "content": [block]
        }});
        let codex = json!({"type": "response_item", "payload": {
            "type": "message", "role": "developer", "content": [{"type": "input_text", "text": block}],
            "internal_chat_message_metadata_passthrough": {"content_item_kinds": ["hooks.additional_context"]}
        }});
        for value in [&claude, &codex] {
            assert!(transcript_line_accepts_hook_block(
                &value.to_string(),
                block,
                "SessionStart"
            ));
            assert!(!transcript_line_accepts_hook_block(
                &value.to_string(),
                "MISSING PART",
                "SessionStart"
            ));
        }
        assert!(!transcript_line_accepts_block(&claude.to_string(), block));
        let mut wrong_hook = claude.clone();
        wrong_hook["attachment"]["hookEvent"] = json!("PostToolUse");
        assert!(!transcript_line_accepts_hook_block(
            &wrong_hook.to_string(),
            block,
            "SessionStart"
        ));
        let mut truncated = claude;
        truncated["attachment"]["content"] = json!(["[RSI preamble part 1/2]"]);
        assert!(!transcript_line_accepts_hook_block(
            &truncated.to_string(),
            block,
            "SessionStart"
        ));
        let mut quoted = codex;
        quoted["payload"]["role"] = json!("assistant");
        assert!(!transcript_line_accepts_hook_block(
            &quoted.to_string(),
            block,
            "SessionStart"
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
    fn a_numbered_handler_selects_its_session_start_part_through_the_rpc() {
        let mut calls = Vec::new();
        let mut out = Vec::new();
        let code = run_hook_with_input_and_args(
            &["--session-start-part=2".into()],
            r#"{"hook_event_name":"SessionStart","source":"startup","session_start_part":99}"#,
            &crate::spill::SpillConfig::from_lookup(&|_| None),
            "rsi-rpc",
            &mut out,
            |_, method, params, _| {
                calls.push((method.to_string(), params));
                Ok(RpcResponse::success(
                    Some(json!(1)),
                    json!({"outcome":"partial", "additional_context":"[RSI preamble part 2/3]\n\nBODY"}),
                ))
            },
        );
        assert_eq!(code, 0);
        assert_eq!(
            calls,
            vec![(
                crate::provider_lifecycle_hooks::RECORD_METHOD.to_string(),
                json!({"event":"SessionStart", "source":"startup", "session_start_part":2})
            )]
        );
        let output: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            output["hookSpecificOutput"]["additionalContext"],
            "[RSI preamble part 2/3]\n\nBODY"
        );
    }

    /// #2766: a lifecycle event reports itself once, claims no mail, prints
    /// nothing and exits 0 even when the daemon call fails.
    #[test]
    fn a_lifecycle_event_is_reported_without_claiming_mail_and_never_fails() {
        for name in ["StopFailure", "PreCompact"] {
            let event = json!({"hook_event_name": name, "error": "rate_limit", "trigger": "auto"});
            let mut out = Vec::new();
            let tmp = tempfile::tempdir().unwrap();
            let mut methods = Vec::new();
            let code = run_hook_with_input(
                &event.to_string(),
                &spill_cfg(tmp.path()),
                "/x/rsi-rpc",
                &mut out,
                |_, method, _, _| {
                    methods.push(method.to_string());
                    Err(std::io::Error::other("daemon unreachable"))
                },
            );
            assert_eq!(code, 0, "{name}");
            assert!(out.is_empty(), "{name} prints nothing");
            assert_eq!(
                methods,
                [crate::provider_lifecycle_hooks::RECORD_METHOD],
                "{name}"
            );
        }
    }

    /// #2898: a `PreCompact` whose daemon reply carries a handoff prints the
    /// instruction in the provider's shape, claims no mail and exits 0.
    #[test]
    fn a_pre_compact_handoff_is_printed_per_provider_and_claims_no_mail() {
        use crate::provider_lifecycle_hooks::HookOutputDialect;
        let event = json!({"hook_event_name": "PreCompact", "trigger": "manual"});
        for dialect in [HookOutputDialect::Claude, HookOutputDialect::Codex] {
            let mut out = Vec::new();
            let tmp = tempfile::tempdir().unwrap();
            let mut methods = Vec::new();
            let code = run_hook_with_input_for(
                &event.to_string(),
                &spill_cfg(tmp.path()),
                "/x/rsi-rpc",
                dialect,
                &mut out,
                |_, method, _, _| {
                    methods.push(method.to_string());
                    Ok(reply(json!({"outcome": "queued", "handoff": {
                        "action": "worker_baton", "requested": true,
                        "instruction": "Pass the baton: PIPELINE HANDOFF — BATON <sha>",
                    }})))
                },
            );
            assert_eq!(code, 0);
            assert_eq!(methods, [crate::provider_lifecycle_hooks::RECORD_METHOD]);
            let out = String::from_utf8(out).unwrap();
            assert!(out.contains("PIPELINE HANDOFF — BATON <sha>"), "{out}");
            let json = serde_json::from_str::<Value>(out.trim());
            assert_eq!(json.is_ok(), dialect == HookOutputDialect::Codex, "{out}");
        }
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

    #[test]
    fn settings_json_installs_the_ask_user_question_matcher() {
        let value: Value = serde_json::from_str(&claude_settings_json("rsi-rpc x")).unwrap();
        let groups = value["hooks"]["PreToolUse"].as_array().unwrap();
        let ask = groups
            .iter()
            .find(|group| group["matcher"] == ASK_USER_QUESTION_TOOL)
            .expect("AskUserQuestion matcher");
        assert_eq!(ask["hooks"][0]["type"], "command");
        assert_eq!(ask["hooks"][0]["command"], "rsi-rpc x");
        assert_eq!(
            ask["hooks"][0]["timeout"].as_u64(),
            Some(CLAUDE_HOOK_TIMEOUT_SECS)
        );
        // The Bash guard group stays first and untouched.
        assert_eq!(groups[0]["matcher"], "Bash");
    }

    #[test]
    fn pre_tool_use_answers_ask_user_question_with_the_typed_notice_without_the_daemon() {
        let event = json!({"hook_event_name": "PreToolUse", "tool_name": "AskUserQuestion",
            "tool_input": {"questions": []}});
        let tmp = tempfile::tempdir().unwrap();
        // Even with the spill switch off: this is a denial, not a rewrite.
        let mut cfg = spill_cfg(tmp.path());
        cfg.disabled = true;
        let mut out = Vec::new();
        let code = run_hook_with_input(
            &event.to_string(),
            &cfg,
            "rsi-rpc",
            &mut out,
            |_, _, _, _| panic!("the AskUserQuestion denial must not call the daemon"),
        );
        assert_eq!(code, 0);
        let value: Value = serde_json::from_slice(&out).unwrap();
        let specific = &value["hookSpecificOutput"];
        assert_eq!(specific["hookEventName"], "PreToolUse");
        assert_eq!(specific["permissionDecision"], "deny");
        let reason = specific["permissionDecisionReason"].as_str().unwrap();
        assert_eq!(
            reason,
            crate::ask_user_question_notice::undeliverable_notice()
        );
        assert!(reason.starts_with("[rsi:ask_user_question_undeliverable] "));
        assert!(reason.contains("NOT declined by a human"));
    }

    #[test]
    fn pre_tool_use_leaves_other_tools_alone_for_the_ask_user_question_hook() {
        let event = json!({"hook_event_name": "PreToolUse", "tool_name": "Read",
            "tool_input": {"file_path": "/x"}});
        let tmp = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let code = run_hook_with_input(
            &event.to_string(),
            &spill_cfg(tmp.path()),
            "rsi-rpc",
            &mut out,
            |_, _, _, _| panic!("PreToolUse must not call the daemon"),
        );
        assert_eq!(code, 0);
        assert!(out.is_empty());
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
            "tool_input": {"command": "cargo build -p rsid", "description": "t"}});
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
            "'/x/rsi-rpc' spill --shell 'cargo build -p rsid'"
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

    /// #654: Claude and Codex deny every interpreter invocation at PreToolUse,
    /// including repository scripts. Mentioning Python as data stays allowed.
    #[test]
    fn pre_tool_use_denies_python_for_claude_and_codex_and_allows_mentions() {
        let run = |command: &str, args: &[String]| {
            let event = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
                "tool_input": {"command": command}});
            let mut out = Vec::new();
            let tmp = tempfile::tempdir().unwrap();
            let cfg = spill_config_for_args(args, spill_cfg(tmp.path()));
            let code = run_hook_with_input_and_args(
                args,
                &event.to_string(),
                &cfg,
                "rsi-rpc",
                &mut out,
                |_, _, _, _| panic!("PreToolUse must not call the daemon"),
            );
            assert_eq!(code, 0);
            out
        };
        // Codex installs the same Bash matcher with --mail-only; its disabled
        // spill configuration must still enforce the shell guards.
        for args in [vec![], vec![MAIL_ONLY_FLAG.to_string()]] {
            for command in [
                "python",
                "python3",
                "python3 -",
                "python3 --version",
                "cd crates && python3 --version; ls",
                "python3 -c 'print(1)'",
                "python3 -m unittest -v scripts/tests/test_rsi_stash.py",
                "python3 -I scripts/rsid-sample-stacks --pid 1",
                "pip install x",
            ] {
                let value: Value = serde_json::from_slice(&run(command, &args)).unwrap();
                let specific = &value["hookSpecificOutput"];
                assert_eq!(specific["hookEventName"], "PreToolUse", "{command}");
                assert_eq!(specific["permissionDecision"], "deny", "{command}");
                assert_eq!(
                    specific["permissionDecisionReason"],
                    crate::python_guard::PYTHON_REFUSAL,
                    "{command}"
                );
            }
            for command in ["ls ~/python-notes", "echo python", "git log --grep python"] {
                assert_eq!(run(command, &args), Vec::<u8>::new(), "{command}");
            }
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    /// A repository under a fake sandbox base, with one committed file.
    fn sandbox_repo() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("sandboxes");
        let repo = base.join("session");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "work"]);
        std::fs::write(repo.join("a.txt"), "one").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        (tmp, base, repo)
    }

    /// #2320: a CLI shell may not change the sandbox branch. The incident was
    /// a backticked example inside a double-quoted rsi-rpc argument that the
    /// shell evaluated as `git checkout main`.
    #[test]
    fn custody_refuses_a_branch_change_in_a_sandbox() {
        let (_tmp, base, repo) = sandbox_repo();
        for command in [
            "git checkout main",
            "git switch main",
            "git switch -c other",
            "git checkout -b other",
            "git reset --hard origin/rolling",
            "git stash",
            "git push --force origin work",
            "cd /tmp && git checkout main",
            "bash -c 'git checkout main'",
            "rsi-rpc AgentSendMessage --params \"run `git checkout main` now\"",
            "git -c alias.co=checkout co main",
        ] {
            let reason = custody_refusal(command, &repo, &base)
                .unwrap_or_else(|| panic!("`{command}` must be refused"));
            assert!(
                reason.contains("rsi refused this command"),
                "{command}: {reason}"
            );
        }
    }

    #[test]
    fn custody_allows_ordinary_git_in_a_sandbox() {
        let (_tmp, base, repo) = sandbox_repo();
        for command in [
            "git status",
            "git log --oneline -3",
            "git diff origin/rolling",
            "git commit -m 'a checkout of main'",
            "git add a.txt && git commit -m x",
            "git fetch origin && git merge origin/rolling",
            "git show main:a.txt",
            "git branch --show-current",
            "echo git checkout main",
        ] {
            assert_eq!(custody_refusal(command, &repo, &base), None, "{command}");
        }
    }

    /// #2571: the CLI hook refuses the failed-`cd` chain that let fixture git
    /// commit and reset in a worker's sandbox, before any command runs. The
    /// scratch directory here does not exist, so an unguarded chain would have
    /// run `git commit` in the sandbox.
    #[test]
    fn custody_refuses_a_scratch_cd_chain_before_git_can_touch_the_sandbox() {
        let (tmp, base, repo) = sandbox_repo();
        let missing = tmp.path().join("scratch-never-created");
        let missing = missing.display();
        let head_before = std::fs::read_to_string(repo.join(".git/HEAD")).unwrap();
        for command in [
            format!("cd {missing}; git commit --allow-empty -m junk"),
            format!("cd {missing} && git init -q && git commit --allow-empty -m junk"),
            format!("mkdir -p {missing}; cd {missing}; git reset --mixed HEAD~1"),
            "cd \"$(mktemp -d)\"; git commit --allow-empty -m junk".to_owned(),
            "cd /tmp/fixture; git update-ref refs/heads/stray HEAD".to_owned(),
            "cd /tmp/fixture; git branch stray HEAD".to_owned(),
            "bash -c 'cd /tmp/fixture; git commit -m junk'".to_owned(),
            // An empty or unknown pin is no pin: `git -C ""` acts on the cwd.
            "d=; cd /tmp/missing; git -C \"$d\" commit -m junk".to_owned(),
            "cd /tmp/missing; git -C \"$(mktemp -d)\" commit -m junk".to_owned(),
            "cd /tmp/missing; GIT_DIR=\"$d\" GIT_WORK_TREE=\"$d\" git commit -m junk".to_owned(),
            // A pin on an earlier line or command does not cover a later one.
            "export GIT_DIR=/tmp/f/.git GIT_WORK_TREE=/tmp/f; cd /tmp/missing; git commit -m junk"
                .to_owned(),
            "cd /tmp/missing; cd /somewhere/else; git commit -m junk".to_owned(),
        ] {
            let reason = custody_refusal(&command, &repo, &base)
                .unwrap_or_else(|| panic!("`{command}` must be refused"));
            assert!(
                reason.contains("scratch or temporary"),
                "{command}: {reason}"
            );
            assert!(reason.contains("git -C <absolute scratch dir>"), "{reason}");
        }
        // Nothing ran: the refusal is a verdict on the text, so the assigned
        // branch and the missing directory are exactly as they were.
        assert_eq!(
            std::fs::read_to_string(repo.join(".git/HEAD")).unwrap(),
            head_before
        );
        assert!(!std::path::Path::new(&missing.to_string()).exists());
    }

    /// #2571: the same words in a pinned or ordinary command are not refused,
    /// and a scratch chain from outside any sandbox is not ours to guard.
    #[test]
    fn custody_allows_pinned_scratch_git_and_ordinary_sandbox_work() {
        let (tmp, base, repo) = sandbox_repo();
        let scratch = tmp.path().join("scratch-repo");
        std::fs::create_dir(&scratch).unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&scratch)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        let scratch = scratch.display();
        for command in [
            format!("git -C {scratch} commit --allow-empty -m fixture"),
            format!("git -C {scratch} reset --mixed HEAD~1"),
            format!("cd {scratch} && git -C {scratch} commit -m fixture"),
            format!("GIT_DIR={scratch}/.git GIT_WORK_TREE={scratch} git commit -m fixture"),
            format!("cd {scratch} && git status"),
            format!(
                "cd {scratch} && env GIT_DIR={scratch}/.git GIT_WORK_TREE={scratch} git commit -m fixture"
            ),
            format!("cd {scratch} && git --git-dir {scratch}/.git --work-tree {scratch} add -A"),
            "git -C \"$dir\" commit -m 'no scratch cd here'".to_owned(),
            "git commit -m 'cd /tmp/scratch; git reset --mixed HEAD~2'".to_owned(),
            "git add a.txt && git commit -m 'mktemp /tmp fixture'".to_owned(),
            "git fetch origin && git merge origin/rolling && git push origin HEAD:rolling"
                .to_owned(),
            "git log --oneline -3 && git diff origin/rolling".to_owned(),
        ] {
            assert_eq!(custody_refusal(&command, &repo, &base), None, "{command}");
        }
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert_eq!(
            custody_refusal("cd /tmp/fixture; git commit -m x", &elsewhere, &base),
            None
        );
    }

    /// A repository outside every sandbox is not ours to guard.
    #[test]
    fn custody_ignores_a_directory_outside_the_sandbox_base() {
        let (tmp, base, _repo) = sandbox_repo();
        let elsewhere = tmp.path().join("scratch");
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert_eq!(
            custody_refusal("git checkout main", &elsewhere, &base),
            None
        );
        assert_eq!(
            custody_refusal("git checkout main", &elsewhere, Path::new("")),
            None
        );
    }

    /// #2540: the shell's starting directory is not the repository a command
    /// targets. From outside the base, a literal `-C`/`cd`/`--git-dir`/
    /// `--work-tree` target inside it is still refused; an unrelated repository
    /// stays usable.
    #[test]
    fn custody_refuses_a_sandbox_target_from_a_directory_outside_the_base() {
        let (tmp, base, repo) = sandbox_repo();
        let elsewhere = tmp.path().join("scratch");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let other = tmp.path().join("other-repo");
        let repo = repo.display();
        for command in [
            format!("git -C {repo} switch main"),
            format!("git -C '{repo}' checkout main"),
            format!("cd {repo} && git checkout main"),
            format!("(cd \"{repo}\"; git switch -c other)"),
            format!("pushd {repo} && git reset --hard origin/rolling"),
            format!("git --git-dir={repo}/.git checkout main"),
            format!("git --work-tree={repo} checkout main"),
            format!(
                "git -C {}/../sandboxes/session switch main",
                elsewhere.display()
            ),
            format!("env -C {repo} git checkout main"),
            format!("bash -c 'cd {repo} && git switch main'"),
        ] {
            let reason = custody_refusal(&command, &elsewhere, &base)
                .unwrap_or_else(|| panic!("`{command}` must be refused"));
            assert!(
                reason.contains("rsi refused this command"),
                "{command}: {reason}"
            );
        }
        // A relative target resolved from a directory above the base.
        assert!(
            custody_refusal("git -C sandboxes/session switch main", tmp.path(), &base).is_some()
        );
        // Positive controls: an unrelated repository stays usable, including
        // the same branch-changing commands, and sandbox reads stay allowed.
        let other = other.display();
        for command in [
            "git checkout main".to_owned(),
            format!("git -C {other} switch main"),
            format!("cd {other} && git checkout -b topic"),
            format!("git --git-dir={other}/.git checkout main"),
            format!("git -C {repo} status"),
            format!("cd {repo} && git log --oneline -3"),
        ] {
            assert_eq!(
                custody_refusal(&command, &elsewhere, &base),
                None,
                "{command}"
            );
        }
    }

    /// A discard is allowed while nothing would be lost and refused when the
    /// path has uncommitted changes (hard rules 8 and 9).
    #[test]
    fn custody_refuses_a_discard_only_when_it_would_lose_work() {
        let (_tmp, base, repo) = sandbox_repo();
        assert_eq!(
            custody_refusal("git restore a.txt", &repo, &base),
            None,
            "clean path"
        );
        assert_eq!(
            custody_refusal("git checkout -- a.txt", &repo, &base),
            None,
            "clean path"
        );
        std::fs::write(repo.join("a.txt"), "two").unwrap();
        let reason = custody_refusal("git restore a.txt", &repo, &base).expect("dirty path");
        assert!(reason.contains("a.txt"), "{reason}");
        assert!(custody_refusal("git checkout -- a.txt", &repo, &base).is_some());
        std::fs::write(repo.join("scratch.txt"), "x").unwrap();
        assert!(custody_refusal("git clean -f", &repo, &base).is_some());
    }

    /// The PreToolUse answer for a denied custody command has the same shape as
    /// the #1227 denial, and a non-Bash tool is never inspected.
    #[test]
    fn pre_tool_use_denial_covers_custody_and_leaves_other_tools_alone() {
        let (_tmp, base, repo) = sandbox_repo();
        // render_shell_denial reads RSI_SANDBOX_BASE / the home default, so
        // the shape is checked through the same reason string it would carry.
        let reason = custody_refusal("git switch main", &repo, &base).unwrap();
        assert!(reason.contains("AGENTS.md hard rule 2"), "{reason}");
        let event = json!({"hook_event_name": "PreToolUse", "tool_name": "Read",
            "tool_input": {"command": "git checkout main"}});
        assert_eq!(render_shell_denial(&event, false), None);
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
