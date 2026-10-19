//! #2766 (#2748 G15): provider lifecycle hooks beyond `PreToolUse`/`PostToolUse`.
//!
//! The operator setting `provider_lifecycle_hooks` is a set of event names
//! ([`LifecycleHookSet`], default empty). Each enabled event is installed through
//! the same per-session path as the mail and custody hooks (Claude `--settings`,
//! Codex `-c hooks.*` plus its `hooks.state` trust) and runs the same
//! `rsi-rpc boundary-mail-hook` command. For a lifecycle event that command only
//! reports the event to the daemon through the hook-only verb [`RECORD_METHOD`]
//! and prints nothing; every failure is silent and the hook always exits 0, so a
//! lifecycle hook never blocks, delays or ends a session.
//!
//! - `StopFailure` (Claude only): an API error ended the turn. Recorded as a
//!   typed provider-error row ([`KIND_PROVIDER_STOP_FAILURE`]).
//! - `PreCompact` (Claude and Codex): the provider is about to compact the
//!   context. Recorded as a compaction-imminent row ([`KIND_COMPACTION_IMMINENT`])
//!   and (#2898) the daemon drives the session's existing handoff: the worker
//!   baton or the coordinating seat's context-cap succession. The reply's
//!   handoff instruction is the one thing a lifecycle hook prints
//!   ([`render_pre_compact_output`]): Claude appends a successful `PreCompact`
//!   hook's stdout to the compaction's custom instructions, so the summary the
//!   model continues from carries it; Codex takes a `systemMessage`. Never a
//!   block: compaction always proceeds.
//! - `SessionStart` (Claude and Codex, hub #2919): the provider starts, resumes,
//!   clears or compacts a session. When the launch deferred the session's RSI
//!   preamble to this hook (it fits [`MAX_SESSION_START_CONTEXT_CHARS`]), the
//!   daemon's reply carries it and the hook prints it as
//!   `hookSpecificOutput.additionalContext` ([`render_session_start_output`]),
//!   the one output shape both providers accept. Anything else prints nothing,
//!   and the next launch puts an undelivered preamble back in the prompt.
//!
//! An empty set installs exactly the hooks installed before this module existed.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::rpc::RpcResponse;

/// The persisted daemon runtime-config field.
pub const SETTING_FIELD: &str = "provider_lifecycle_hooks";
/// The hook-only daemon verb a lifecycle hook calls (not in the agent catalog).
pub const RECORD_METHOD: &str = "RecordProviderLifecycleEvent";
/// Daemon I/O bound for one record call; well inside the provider hook timeout.
pub const RECORD_TIMEOUT: Duration = Duration::from_secs(3);
/// The `provider_health.kind` of a recorded `StopFailure`.
pub const KIND_PROVIDER_STOP_FAILURE: &str = "provider_stop_failure";
/// The `provider_health.kind` of a recorded `PreCompact`.
pub const KIND_COMPACTION_IMMINENT: &str = "compaction_imminent";
/// The canonical text of the empty set.
pub const NONE: &str = "none";
/// Longest `error_details` text kept from a `StopFailure` input.
pub const MAX_ERROR_DETAILS_CHARS: usize = 300;
/// #2898: the `handoff.action` of a `PreCompact` that passed the worker baton.
pub const COMPACTION_HANDOFF_WORKER_BATON: &str = "worker_baton";
/// #2898: the `handoff.action` of a `PreCompact` on a coordinating seat.
pub const COMPACTION_HANDOFF_SEAT_SUCCESSION: &str = "seat_succession";
/// #2898: the longest handoff instruction a `PreCompact` hook prints.
pub const MAX_HANDOFF_INSTRUCTION_CHARS: usize = 4000;
/// hub #2919: the `provider_health.kind` of a preamble a `SessionStart` hook
/// delivered.
pub const KIND_SESSION_START_CONTEXT: &str = "session_start_context";
/// hub #2919: the longest preamble a `SessionStart` hook carries. Claude Code
/// caps each `additionalContext` string at 10,000 characters and replaces a
/// longer one with a file path and a 2,000-character preview [source] Claude
/// Code hooks reference, so a longer preamble stays in the prompt.
pub const MAX_SESSION_START_CONTEXT_CHARS: usize = 10_000;
/// hub #2919: the `SessionStart` `source` values RSI records; any other value
/// is `other`. Claude sends all five, Codex `startup`, `resume` and `clear`.
pub const SESSION_START_SOURCES: &[&str] = &["startup", "resume", "clear", "compact", "fork"];

/// Which provider's hook output shape a hook run prints (#2898).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookOutputDialect {
    /// Claude Code: a `PreCompact` hook's plain stdout becomes compaction
    /// instructions.
    Claude,
    /// Codex CLI (the hook runs with `--mail-only`): `pre-compact.command.output`
    /// accepts only `continue`, `stopReason`, `suppressOutput`, `systemMessage`.
    Codex,
}

impl HookOutputDialect {
    /// The dialect of one `boundary-mail-hook` run from its trailing args:
    /// the Codex hook command carries [`crate::boundary_mail_hook::MAIL_ONLY_FLAG`].
    #[must_use]
    pub fn from_hook_args(args: &[String]) -> Self {
        if args
            .iter()
            .any(|arg| arg == crate::boundary_mail_hook::MAIL_ONLY_FLAG)
        {
            return Self::Codex;
        }
        Self::Claude
    }
}

/// The `error` values Claude Code 2.1.301 sends with `StopFailure`; any other
/// value is recorded as `unknown`.
pub const STOP_FAILURE_ERRORS: &[&str] = &[
    "authentication_failed",
    "oauth_org_not_allowed",
    "account_on_hold",
    "verification_required",
    "billing_error",
    "rate_limit",
    "overloaded",
    "invalid_request",
    "model_not_found",
    "server_error",
    "unknown",
    "max_output_tokens",
    "cloud_credential_error",
];

/// One lifecycle event RSI can install. The serde names are the provider's
/// hook event names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum LifecycleHookEvent {
    StopFailure,
    PreCompact,
    SessionStart,
}

impl LifecycleHookEvent {
    /// Every supported event, in canonical order.
    pub const ALL: [Self; 3] = [Self::StopFailure, Self::PreCompact, Self::SessionStart];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::StopFailure => "StopFailure",
            Self::PreCompact => "PreCompact",
            Self::SessionStart => "SessionStart",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.name() == name)
    }

    /// Codex documents no `StopFailure`; it has `PreCompact` and
    /// `SessionStart`.
    #[must_use]
    pub const fn codex_supported(self) -> bool {
        matches!(self, Self::PreCompact | Self::SessionStart)
    }

    const fn bit(self) -> u8 {
        match self {
            Self::StopFailure => 1,
            Self::PreCompact => 2,
            Self::SessionStart => 4,
        }
    }
}

/// The operator's set of enabled lifecycle events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LifecycleHookSet(u8);

impl LifecycleHookSet {
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub fn with(self, event: LifecycleHookEvent) -> Self {
        Self(self.0 | event.bit())
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, event: LifecycleHookEvent) -> bool {
        self.0 & event.bit() != 0
    }

    /// The enabled events, in canonical order.
    pub fn events(self) -> impl Iterator<Item = LifecycleHookEvent> {
        LifecycleHookEvent::ALL
            .into_iter()
            .filter(move |event| self.contains(*event))
    }

    /// The enabled events a Codex launch can install.
    #[must_use]
    pub fn codex_events(self) -> Vec<LifecycleHookEvent> {
        self.events()
            .filter(|event| event.codex_supported())
            .collect()
    }

    /// `none`, or the event names joined by `,` in canonical order.
    #[must_use]
    pub fn canonical(self) -> String {
        if self.is_empty() {
            return NONE.to_string();
        }
        self.events()
            .map(LifecycleHookEvent::name)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Parse the setting from its RPC value: a string of names separated by
    /// commas or whitespace (`none` or empty for the empty set), or an array of
    /// names. An unknown or unsupported name is refused with a message naming
    /// it and the supported names.
    pub fn parse_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::String(text) => Self::parse_names(
                text.split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|name| !name.is_empty() && *name != NONE),
            ),
            Value::Array(items) => {
                let names = items
                    .iter()
                    .map(|item| item.as_str().ok_or("expected an array of event names"))
                    .collect::<Result<Vec<_>, _>>()?;
                Self::parse_names(names.into_iter())
            }
            _ => Err("expected a string or an array of event names".to_string()),
        }
    }

    fn parse_names<'a>(names: impl Iterator<Item = &'a str>) -> Result<Self, String> {
        let mut set = Self::empty();
        for name in names {
            let Some(event) = LifecycleHookEvent::from_name(name) else {
                return Err(format!(
                    "unsupported lifecycle hook event {name:?}; supported: {}",
                    supported_names()
                ));
            };
            set = set.with(event);
        }
        Ok(set)
    }
}

fn supported_names() -> String {
    LifecycleHookEvent::ALL
        .into_iter()
        .map(LifecycleHookEvent::name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every value the TUI row cycles through, `none` first.
#[must_use]
pub fn tui_options() -> Vec<String> {
    let all = LifecycleHookEvent::ALL;
    let mut options = vec![NONE.to_string()];
    for mask in 1..(1u8 << all.len()) {
        let set = all
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .fold(LifecycleHookSet::empty(), |set, (_, event)| {
                set.with(*event)
            });
        options.push(set.canonical());
    }
    options
}

/// The `--settings` JSON for one Claude launch: exactly
/// [`crate::boundary_mail_hook::claude_settings_json`] when `set` is empty,
/// else the same document plus one command group per enabled event running the
/// same `command`. No `onFailure`: a hook that fails to run is skipped.
#[must_use]
pub fn claude_settings_json(command: &str, set: LifecycleHookSet) -> String {
    let base = crate::boundary_mail_hook::claude_settings_json(command);
    if set.is_empty() {
        return base;
    }
    let Ok(mut value) = serde_json::from_str::<Value>(&base) else {
        return base;
    };
    let Some(hooks) = value.get_mut("hooks").and_then(Value::as_object_mut) else {
        return base;
    };
    for event in set.events() {
        hooks.insert(
            event.name().to_string(),
            json!([{
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": crate::boundary_mail_hook::CLAUDE_HOOK_TIMEOUT_SECS,
                }]
            }]),
        );
    }
    value.to_string()
}

/// `RecordProviderLifecycleEvent` params: the event and a bounded, normalized
/// summary of the provider's hook input. Never the transcript or the prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordProviderLifecycleEventParams {
    pub event: LifecycleHookEvent,
    /// `StopFailure`: one of [`STOP_FAILURE_ERRORS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `StopFailure`: the provider's error detail, at most
    /// [`MAX_ERROR_DETAILS_CHARS`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_details: Option<String>,
    /// `PreCompact`: `manual`, `auto` or `other`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    /// `SessionStart`: one of [`SESSION_START_SOURCES`] or `other`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl RecordProviderLifecycleEventParams {
    /// The params for one provider hook input, or `None` when the input is
    /// not a lifecycle event.
    #[must_use]
    pub fn from_hook_input(input: &Value) -> Option<Self> {
        let event = input
            .get("hook_event_name")
            .and_then(Value::as_str)
            .and_then(LifecycleHookEvent::from_name)?;
        let text = |field: &str| input.get(field).and_then(Value::as_str).map(str::to_string);
        let params = match event {
            LifecycleHookEvent::StopFailure => Self {
                event,
                error: Some(text("error").unwrap_or_default()),
                error_details: text("error_details"),
                trigger: None,
                source: None,
            },
            LifecycleHookEvent::PreCompact => Self {
                event,
                error: None,
                error_details: None,
                trigger: Some(text("trigger").unwrap_or_default()),
                source: None,
            },
            LifecycleHookEvent::SessionStart => Self {
                event,
                error: None,
                error_details: None,
                trigger: None,
                source: Some(text("source").unwrap_or_default()),
            },
        };
        Some(params.normalized())
    }

    /// The same params with every field bounded to its closed set or length;
    /// the daemon applies it again to what it receives.
    #[must_use]
    pub fn normalized(self) -> Self {
        match self.event {
            LifecycleHookEvent::StopFailure => Self {
                event: self.event,
                error: Some(normalized_error(self.error.as_deref())),
                error_details: self.error_details.as_deref().and_then(bounded_details),
                trigger: None,
                source: None,
            },
            LifecycleHookEvent::PreCompact => Self {
                event: self.event,
                error: None,
                error_details: None,
                trigger: Some(normalized_trigger(self.trigger.as_deref())),
                source: None,
            },
            LifecycleHookEvent::SessionStart => Self {
                event: self.event,
                error: None,
                error_details: None,
                trigger: None,
                source: Some(normalized_source(self.source.as_deref())),
            },
        }
    }

    /// The visible transcript line for this event.
    #[must_use]
    pub fn summary(&self) -> String {
        match self.event {
            LifecycleHookEvent::StopFailure => format!(
                "Provider turn ended by an API error: {} (StopFailure hook)",
                self.error.as_deref().unwrap_or("unknown")
            ),
            LifecycleHookEvent::PreCompact => format!(
                "Provider context compaction imminent ({}, PreCompact hook)",
                self.trigger.as_deref().unwrap_or("other")
            ),
            LifecycleHookEvent::SessionStart => format!(
                "RSI preamble delivered as SessionStart hook context ({})",
                self.source.as_deref().unwrap_or("other")
            ),
        }
    }

    /// The `provider_health` metadata object for this event.
    #[must_use]
    pub fn health_metadata(&self) -> Value {
        match self.event {
            LifecycleHookEvent::StopFailure => json!({
                "kind": KIND_PROVIDER_STOP_FAILURE,
                "source": "hook",
                "hook_event": self.event.name(),
                "error": self.error,
                "error_details": self.error_details,
            }),
            LifecycleHookEvent::PreCompact => json!({
                "kind": KIND_COMPACTION_IMMINENT,
                "source": "hook",
                "hook_event": self.event.name(),
                "trigger": self.trigger,
            }),
            LifecycleHookEvent::SessionStart => json!({
                "kind": KIND_SESSION_START_CONTEXT,
                "source": "hook",
                "hook_event": self.event.name(),
                "session_source": self.source,
            }),
        }
    }
}

fn normalized_error(error: Option<&str>) -> String {
    error
        .filter(|error| STOP_FAILURE_ERRORS.contains(error))
        .unwrap_or("unknown")
        .to_string()
}

fn normalized_trigger(trigger: Option<&str>) -> String {
    match trigger {
        Some("manual") => "manual",
        Some("auto") => "auto",
        _ => "other",
    }
    .to_string()
}

fn normalized_source(source: Option<&str>) -> String {
    source
        .filter(|source| SESSION_START_SOURCES.contains(source))
        .unwrap_or("other")
        .to_string()
}

fn bounded_details(details: &str) -> Option<String> {
    let cleaned: String = details
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_ERROR_DETAILS_CHARS)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// Hook side: report a lifecycle event to the daemon. Only a `PreCompact`
/// whose reply carries a handoff instruction (#2898) and a `SessionStart`
/// whose reply carries the session's preamble (hub #2919) print anything, in
/// the provider's `dialect`; nothing printed ever blocks or stops the provider.
/// Returns `false` when `input` is not a lifecycle event (the caller handles
/// it). Any daemon or write failure is ignored: the hook still exits 0.
pub fn run_lifecycle_hook<F>(
    input: Option<&Value>,
    dialect: HookOutputDialect,
    stdout: &mut impl std::io::Write,
    mut dispatch: F,
) -> bool
where
    F: FnMut(&Path, &str, Value, Duration) -> std::io::Result<RpcResponse>,
{
    let Some(params) = input.and_then(RecordProviderLifecycleEventParams::from_hook_input) else {
        return false;
    };
    let event = params.event;
    let Ok(params) = serde_json::to_value(params) else {
        return true;
    };
    let socket = crate::agent_rpc_client::resolve_socket_path();
    let Ok(reply) = dispatch(&socket, RECORD_METHOD, params, RECORD_TIMEOUT) else {
        return true;
    };
    let output = match event {
        LifecycleHookEvent::PreCompact => render_pre_compact_output(&reply, dialect),
        LifecycleHookEvent::SessionStart => render_session_start_output(&reply),
        LifecycleHookEvent::StopFailure => None,
    };
    let Some(output) = output else {
        return true;
    };
    let _ = writeln!(stdout, "{output}").and_then(|()| stdout.flush());
    true
}

/// #2898: the `PreCompact` hook output for the daemon's `reply`, or `None`
/// when it carries no handoff instruction. Claude: the instruction as plain
/// text (never JSON, so it is compaction instructions and never a decision).
/// Codex: `{"systemMessage": ...}`, its only non-control output field.
#[must_use]
pub fn render_pre_compact_output(
    reply: &RpcResponse,
    dialect: HookOutputDialect,
) -> Option<String> {
    let instruction = reply
        .result
        .as_ref()?
        .get("handoff")?
        .get("instruction")?
        .as_str()?;
    let instruction: String = instruction
        .trim()
        .chars()
        .take(MAX_HANDOFF_INSTRUCTION_CHARS)
        .collect();
    // Claude reads stdout that starts with `{` as JSON hook output.
    let instruction = instruction.trim_start_matches('{').trim();
    if instruction.is_empty() {
        return None;
    }
    match dialect {
        HookOutputDialect::Claude => Some(instruction.to_string()),
        HookOutputDialect::Codex => Some(json!({ "systemMessage": instruction }).to_string()),
    }
}

/// hub #2919: the `SessionStart` hook output for the daemon's `reply`, or
/// `None` when it carries no preamble (`additional_context`) or one longer than
/// [`MAX_SESSION_START_CONTEXT_CHARS`] (the daemon never sends one). Claude and
/// Codex both read `hookSpecificOutput.additionalContext` for this event.
#[must_use]
pub fn render_session_start_output(reply: &RpcResponse) -> Option<String> {
    let context = reply.result.as_ref()?.get("additional_context")?.as_str()?;
    if context.trim().is_empty() || context.chars().count() > MAX_SESSION_START_CONTEXT_CHARS {
        return None;
    }
    Some(
        json!({"hookSpecificOutput": {
            "hookEventName": LifecycleHookEvent::SessionStart.name(),
            "additionalContext": context,
        }})
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_set_parses_strings_and_arrays_and_refuses_unknown_names() {
        let both = LifecycleHookSet::empty()
            .with(LifecycleHookEvent::StopFailure)
            .with(LifecycleHookEvent::PreCompact);
        for value in [
            json!("StopFailure,PreCompact"),
            json!("PreCompact StopFailure"),
            json!(["PreCompact", "StopFailure", "PreCompact"]),
        ] {
            assert_eq!(LifecycleHookSet::parse_value(&value), Ok(both), "{value}");
        }
        for value in [json!(""), json!("none"), json!([])] {
            assert_eq!(
                LifecycleHookSet::parse_value(&value),
                Ok(LifecycleHookSet::empty()),
                "{value}"
            );
        }
        for value in [
            json!("SessionEnd"),
            json!("stopfailure"),
            json!(["PreCompact", 3]),
            json!(true),
            Value::Null,
        ] {
            assert!(LifecycleHookSet::parse_value(&value).is_err(), "{value}");
        }
        assert_eq!(both.canonical(), "StopFailure,PreCompact");
        assert_eq!(LifecycleHookSet::empty().canonical(), NONE);
    }

    #[test]
    fn every_tui_option_round_trips_through_the_parser() {
        let options = tui_options();
        assert_eq!(
            options,
            [
                "none",
                "StopFailure",
                "PreCompact",
                "StopFailure,PreCompact",
                "SessionStart",
                "StopFailure,SessionStart",
                "PreCompact,SessionStart",
                "StopFailure,PreCompact,SessionStart",
            ]
        );
        for option in options {
            let set = LifecycleHookSet::parse_value(&json!(option)).unwrap();
            assert_eq!(set.canonical(), option);
        }
    }

    #[test]
    fn an_empty_set_leaves_the_claude_settings_byte_identical() {
        let command = "'/x/rsi-rpc' boundary-mail-hook";
        assert_eq!(
            claude_settings_json(command, LifecycleHookSet::empty()),
            crate::boundary_mail_hook::claude_settings_json(command)
        );
    }

    #[test]
    fn each_enabled_event_adds_one_unmatched_command_group_and_keeps_the_rest() {
        let command = "'/x/rsi-rpc' boundary-mail-hook";
        for event in LifecycleHookEvent::ALL {
            let set = LifecycleHookSet::empty().with(event);
            let value: Value = serde_json::from_str(&claude_settings_json(command, set)).unwrap();
            let hooks = value["hooks"].as_object().unwrap();
            assert_eq!(hooks.len(), 3, "{event:?}");
            assert_eq!(value["hooks"]["PreToolUse"][0]["matcher"], "Bash");
            assert_eq!(value["hooks"]["PostToolUse"][0]["matcher"], "*");
            let group = &value["hooks"][event.name()][0];
            assert!(group.get("matcher").is_none(), "{event:?}");
            let handler = &group["hooks"][0];
            assert_eq!(handler["type"], "command");
            assert_eq!(handler["command"], command);
            assert_eq!(
                handler["timeout"],
                crate::boundary_mail_hook::CLAUDE_HOOK_TIMEOUT_SECS
            );
            assert!(handler.get("onFailure").is_none());
        }
    }

    #[test]
    fn stop_failure_input_becomes_bounded_typed_params() {
        let details = "x".repeat(MAX_ERROR_DETAILS_CHARS + 50);
        let input = json!({
            "hook_event_name": "StopFailure",
            "session_id": "s",
            "transcript_path": "/t.jsonl",
            "error": "rate_limit",
            "error_details": format!("429\n{details}"),
            "last_assistant_message": "secret work",
        });
        let params = RecordProviderLifecycleEventParams::from_hook_input(&input).unwrap();
        assert_eq!(params.event, LifecycleHookEvent::StopFailure);
        assert_eq!(params.error.as_deref(), Some("rate_limit"));
        let kept = params.error_details.as_deref().unwrap();
        assert!(kept.starts_with("429 x"));
        assert_eq!(kept.chars().count(), MAX_ERROR_DETAILS_CHARS);
        let wire = serde_json::to_value(&params).unwrap();
        assert_eq!(
            wire.as_object().unwrap().len(),
            3,
            "event, error, error_details only: {wire}"
        );
        assert_eq!(params.health_metadata()["kind"], KIND_PROVIDER_STOP_FAILURE);
        assert!(params.summary().contains("rate_limit"));

        let odd = json!({"hook_event_name": "StopFailure", "error": "brand_new"});
        let params = RecordProviderLifecycleEventParams::from_hook_input(&odd).unwrap();
        assert_eq!(params.error.as_deref(), Some("unknown"));
        assert_eq!(params.error_details, None);
    }

    #[test]
    fn pre_compact_input_from_claude_or_codex_becomes_a_compaction_record() {
        for (trigger, expected) in [
            (json!("auto"), "auto"),
            (json!("manual"), "manual"),
            (json!("weird"), "other"),
            (Value::Null, "other"),
        ] {
            let input = json!({"hook_event_name": "PreCompact", "trigger": trigger,
                "custom_instructions": "keep the plan"});
            let params = RecordProviderLifecycleEventParams::from_hook_input(&input).unwrap();
            assert_eq!(params.event, LifecycleHookEvent::PreCompact);
            assert_eq!(params.trigger.as_deref(), Some(expected));
            assert_eq!(params.error, None);
            assert_eq!(params.health_metadata()["kind"], KIND_COMPACTION_IMMINENT);
        }
    }

    #[test]
    fn params_reject_unknown_fields_and_unknown_events() {
        assert!(
            serde_json::from_value::<RecordProviderLifecycleEventParams>(
                json!({"event": "PreCompact", "transcript": "x"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<RecordProviderLifecycleEventParams>(
                json!({"event": "SessionEnd"})
            )
            .is_err()
        );
    }

    #[test]
    fn the_hook_reports_once_and_ignores_a_failing_daemon() {
        let input = json!({"hook_event_name": "PreCompact", "trigger": "auto"});
        let mut calls = Vec::new();
        let mut out = Vec::new();
        let handled = run_lifecycle_hook(
            Some(&input),
            HookOutputDialect::Claude,
            &mut out,
            |_, method, params, _| {
                calls.push((method.to_string(), params));
                Err(std::io::Error::other("daemon down"))
            },
        );
        assert!(handled);
        assert!(out.is_empty(), "a failing daemon prints nothing");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, RECORD_METHOD);
        assert_eq!(
            calls[0].1,
            json!({"event": "PreCompact", "trigger": "auto"})
        );

        let tool = json!({"hook_event_name": "PostToolUse"});
        assert!(!run_lifecycle_hook(
            Some(&tool),
            HookOutputDialect::Claude,
            &mut out,
            |_, _, _, _| panic!("not a lifecycle event")
        ));
        assert!(!run_lifecycle_hook(
            None,
            HookOutputDialect::Codex,
            &mut out,
            |_, _, _, _| panic!("no input")
        ));
        assert!(out.is_empty());
    }

    fn handoff_reply(instruction: &str) -> RpcResponse {
        RpcResponse::success(
            Some(json!(1)),
            json!({"outcome": "queued", "handoff": {
                "action": COMPACTION_HANDOFF_WORKER_BATON,
                "requested": true,
                "instruction": instruction,
            }}),
        )
    }

    /// #2898: the daemon's handoff instruction reaches the provider before it
    /// compacts: Claude as plain stdout (its compaction instructions, never a
    /// JSON decision), Codex as its only non-control field, `systemMessage`.
    #[test]
    fn a_pre_compact_prints_the_handoff_instruction_in_each_provider_shape() {
        let input = json!({"hook_event_name": "PreCompact", "trigger": "auto"});
        let instruction = "[rsi daemon, #2898 baton] Pass the baton.\n3. End your turn with `PIPELINE HANDOFF — BATON <sha>`";
        let mut claude = Vec::new();
        assert!(run_lifecycle_hook(
            Some(&input),
            HookOutputDialect::Claude,
            &mut claude,
            |_, _, _, _| Ok(handoff_reply(instruction)),
        ));
        let claude = String::from_utf8(claude).unwrap();
        assert_eq!(claude.trim_end(), instruction);
        assert!(serde_json::from_str::<Value>(claude.trim()).is_err());

        let mut codex = Vec::new();
        assert!(run_lifecycle_hook(
            Some(&input),
            HookOutputDialect::Codex,
            &mut codex,
            |_, _, _, _| Ok(handoff_reply(instruction)),
        ));
        let codex: Value = serde_json::from_slice(&codex).unwrap();
        assert_eq!(codex, json!({"systemMessage": instruction}));

        // A reply without a handoff, and a StopFailure, print nothing.
        let mut out = Vec::new();
        assert!(run_lifecycle_hook(
            Some(&input),
            HookOutputDialect::Claude,
            &mut out,
            |_, _, _, _| Ok(RpcResponse::success(
                Some(json!(1)),
                json!({"outcome": "queued"})
            )),
        ));
        let stop = json!({"hook_event_name": "StopFailure", "error": "overloaded"});
        assert!(run_lifecycle_hook(
            Some(&stop),
            HookOutputDialect::Claude,
            &mut out,
            |_, _, _, _| Ok(handoff_reply(instruction)),
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn session_start_input_from_claude_or_codex_records_a_closed_source() {
        for (source, expected) in [
            (json!("startup"), "startup"),
            (json!("resume"), "resume"),
            (json!("clear"), "clear"),
            (json!("compact"), "compact"),
            (json!("fork"), "fork"),
            (json!("brand_new"), "other"),
            (Value::Null, "other"),
        ] {
            let input = json!({"hook_event_name": "SessionStart", "source": source,
                "model": "m", "transcript_path": "/t.jsonl"});
            let params = RecordProviderLifecycleEventParams::from_hook_input(&input).unwrap();
            assert_eq!(params.event, LifecycleHookEvent::SessionStart);
            assert_eq!(params.source.as_deref(), Some(expected));
            let wire = serde_json::to_value(&params).unwrap();
            assert_eq!(wire, json!({"event": "SessionStart", "source": expected}));
            assert_eq!(params.health_metadata()["kind"], KIND_SESSION_START_CONTEXT);
            assert_eq!(params.health_metadata()["session_source"], expected);
        }
        assert!(LifecycleHookEvent::SessionStart.codex_supported());
    }

    fn preamble_reply(context: &str) -> RpcResponse {
        RpcResponse::success(
            Some(json!(1)),
            json!({"outcome": "written", "additional_context": context}),
        )
    }

    /// hub #2919: the daemon's preamble reaches the provider as
    /// `hookSpecificOutput.additionalContext`, the same shape for Claude and
    /// Codex (`session-start.command.output` in codex-cli 0.163.0).
    #[test]
    fn a_session_start_prints_the_preamble_as_additional_context_for_both_providers() {
        let input = json!({"hook_event_name": "SessionStart", "source": "startup"});
        let preamble =
            "# RPI Worker Preamble\n\n## Agent control (rsi)\nCall AgentGetAuthorityCatalog.";
        for dialect in [HookOutputDialect::Claude, HookOutputDialect::Codex] {
            let mut calls = Vec::new();
            let mut out = Vec::new();
            assert!(run_lifecycle_hook(
                Some(&input),
                dialect,
                &mut out,
                |_, method, params, _| {
                    calls.push((method.to_string(), params));
                    Ok(preamble_reply(preamble))
                },
            ));
            assert_eq!(
                calls,
                [(
                    RECORD_METHOD.to_string(),
                    json!({"event": "SessionStart", "source": "startup"})
                )]
            );
            let printed: Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(
                printed,
                json!({"hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": preamble,
                }}),
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn a_session_start_with_no_preamble_or_a_failing_daemon_prints_nothing() {
        let input = json!({"hook_event_name": "SessionStart", "source": "resume"});
        let over_cap = "z".repeat(MAX_SESSION_START_CONTEXT_CHARS + 1);
        let replies = [
            Ok(RpcResponse::success(
                Some(json!(1)),
                json!({"outcome": "skipped"}),
            )),
            Ok(preamble_reply("   ")),
            Ok(preamble_reply(&over_cap)),
            Err(std::io::Error::other("daemon down")),
        ];
        for reply in replies {
            let mut out = Vec::new();
            let mut reply = Some(reply);
            assert!(run_lifecycle_hook(
                Some(&input),
                HookOutputDialect::Claude,
                &mut out,
                |_, _, _, _| reply.take().unwrap(),
            ));
            assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
        }
        let at_cap = "z".repeat(MAX_SESSION_START_CONTEXT_CHARS);
        assert!(render_session_start_output(&preamble_reply(&at_cap)).is_some());
    }

    #[test]
    fn the_handoff_instruction_is_bounded_and_never_read_as_claude_json() {
        let long = format!("{{\"decision\": \"block\"}} {}", "y".repeat(10_000));
        let rendered =
            render_pre_compact_output(&handoff_reply(&long), HookOutputDialect::Claude).unwrap();
        assert!(!rendered.starts_with('{'), "{rendered}");
        assert!(rendered.chars().count() <= MAX_HANDOFF_INSTRUCTION_CHARS);
        assert_eq!(
            render_pre_compact_output(&handoff_reply("  "), HookOutputDialect::Codex),
            None
        );
        assert_eq!(
            HookOutputDialect::from_hook_args(&["--mail-only".to_string()]),
            HookOutputDialect::Codex
        );
        assert_eq!(
            HookOutputDialect::from_hook_args(&[]),
            HookOutputDialect::Claude
        );
    }
}
