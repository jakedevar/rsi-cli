//! Extracted helpers for session monitoring.
//!
//! These functions decompose the monolithic `monitor_session()` loop in `session.rs`
//! into focused, testable units. Each helper takes only the data it needs rather
//! than the entire `SessionManager`.

use crate::bus::{DaemonEvent, EventBus};
use crate::claude::StreamEvent;
use rsi_common::types::{ContextUsageConfidence, TurnMetric};
use std::sync::Arc;
use tiktoken_rs::CoreBPE;
use uuid::Uuid;

/// Daemon-side token counter using the cl100k_base BPE encoding.
///
/// Initialized once at daemon startup and shared across all sessions via `Arc`.
/// `cl100k_base` is a close approximation for Claude 3+ and exact for OpenAI models.
pub(crate) struct TokenCounter {
    bpe: CoreBPE,
}

impl TokenCounter {
    /// Initialize with cl100k_base encoding.
    pub fn new() -> Self {
        Self {
            bpe: tiktoken_rs::cl100k_base().expect("cl100k_base BPE must load"),
        }
    }

    /// Count tokens in a plain text string.
    pub fn count(&self, text: &str) -> u64 {
        self.bpe.encode_ordinary(text).len() as u64
    }

    /// Count all output tokens from an `assistant` stream event's message content.
    /// Counts `text`, `thinking`, and `tool_use` blocks. Returns 0 if no content found.
    pub fn count_assistant_event(&self, data: &serde_json::Value) -> u64 {
        let mut total = 0u64;
        if let Some(arr) = data
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_array())
        {
            for block in arr {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    total += self.count(text);
                }
                if let Some(thinking) = block.get("thinking").and_then(|v| v.as_str()) {
                    total += self.count(thinking);
                }
                // Tool calls: count the serialized input JSON. Tool call parameters
                // (file contents, code edits, etc.) are significant context consumers
                // that should be tracked between API-reported token updates.
                if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    if let Some(input) = block.get("input") {
                        if let Ok(serialized) = serde_json::to_string(input) {
                            total += self.count(&serialized);
                        }
                    }
                    // Also count the tool name itself (minor but accurate)
                    if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                        total += self.count(name);
                    }
                }
            }
        }
        total
    }

    /// Count input tokens from a `user` stream event's tool result echo-backs.
    pub fn count_user_event(&self, data: &serde_json::Value) -> u64 {
        let mut total = 0u64;
        if let Some(arr) = data
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_array())
        {
            for block in arr {
                // tool_result blocks contain a nested "content" array of text blocks
                if let Some(content_arr) = block.get("content").and_then(|v| v.as_array()) {
                    for inner in content_arr {
                        if let Some(text) = inner.get("text").and_then(|v| v.as_str()) {
                            total += self.count(text);
                        }
                    }
                }
                // Some blocks carry text directly
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    total += self.count(text);
                }
            }
        }
        total
    }
}

/// Parsed token usage from an assistant stream event.
pub(crate) struct TokenUsage {
    pub input: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub output: u64,
    pub total_input: u64,
    pub confidence: ContextUsageConfidence,
    pub stop_reason: Option<String>,
    // === V99 richer usage capture (P1-C) ===
    /// Raw full-prompt tokens from the provider, before cache subtraction.
    /// Metric-only; does not affect context fill or confidence.
    pub prompt_tokens_total: u64,
    /// The full prompt total covers the provider thread, including earlier
    /// process launches (Codex CLI), rather than just this call or launch.
    pub prompt_tokens_cumulative: bool,
    /// `usage.output_tokens_details.thinking_tokens` for this turn.
    pub thinking: u64,
    /// `usage.cache_creation.ephemeral_1h_input_tokens` for this turn.
    pub cache_creation_1h: u64,
    /// `usage.cache_creation.ephemeral_5m_input_tokens` for this turn.
    pub cache_creation_5m: u64,
    /// `usage.service_tier` for this turn, when the provider reported one.
    pub service_tier: Option<String>,
}

/// Current-window context usage from Codex CLI `event_msg/token_count`.
pub(crate) struct CodexContextUsage {
    pub context_tokens: u64,
    pub output_tokens: u64,
    pub context_window: Option<u64>,
    /// Cache-read tokens from Codex's most recent model call.
    /// This is accounting data only; it must not enter the live context
    /// numerator, which uses `last_token_usage.total_tokens`.
    pub cache_read_tokens: Option<u64>,
}

/// Extract current prompt/context usage from Codex CLI token-count events.
///
/// Codex `turn.completed.usage` is cumulative across internal model calls and
/// can legitimately exceed the model context window. `last_token_usage.total_tokens`
/// is the active context size Codex uses for its own context indicator, measured
/// against `model_context_window`, so it is the correct numerator for live
/// context fill and rotation threshold checks.
pub(crate) fn extract_codex_context_usage(stream_event: &StreamEvent) -> Option<CodexContextUsage> {
    if stream_event.event_type != "codex_token_count" {
        return None;
    }

    let info = stream_event.data.get("info")?;
    let last = info.get("last_token_usage")?;
    let context_tokens = last.get("total_tokens").and_then(|v| v.as_u64())?;
    let output_tokens = last
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let context_window = info
        .get("model_context_window")
        .and_then(|v| v.as_u64())
        .filter(|window| *window > 0);
    let cache_read_tokens = last.get("cached_input_tokens").and_then(|v| v.as_u64());

    Some(CodexContextUsage {
        context_tokens,
        output_tokens,
        context_window,
        cache_read_tokens,
    })
}

/// Extract token usage data from an assistant stream event.
/// Returns `None` if the event is not an assistant event.
pub(crate) fn extract_token_usage(stream_event: &StreamEvent) -> Option<TokenUsage> {
    enum UsageSource<'a> {
        Assistant {
            usage: Option<&'a serde_json::Value>,
            stop_reason: Option<&'a serde_json::Value>,
        },
        TurnCompleted {
            usage: Option<&'a serde_json::Value>,
            stop_reason: Option<&'a serde_json::Value>,
        },
    }

    let source = match stream_event.event_type.as_str() {
        "assistant" => {
            let message = stream_event.data.get("message");
            UsageSource::Assistant {
                usage: message.and_then(|m| m.get("usage")),
                stop_reason: message.and_then(|m| m.get("stop_reason")),
            }
        }
        "result" => {
            let subtype = stream_event.data.get("subtype").and_then(|v| v.as_str());
            if subtype != Some("turn_completed") {
                return None;
            }
            UsageSource::TurnCompleted {
                usage: stream_event.data.get("usage"),
                stop_reason: stream_event.data.get("stop_reason"),
            }
        }
        _ => return None,
    };

    let (usage, stop_reason_value) = match source {
        UsageSource::Assistant { usage, stop_reason } => (usage, stop_reason),
        UsageSource::TurnCompleted { usage, stop_reason } => (usage, stop_reason),
    };

    let (input, cache_creation, cache_read, output, confidence) = if let Some(usage) = usage {
        let inp = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cc = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64());
        let cr = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64());
        let out = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let conf = if cc.is_some() || cr.is_some() {
            ContextUsageConfidence::Full
        } else {
            ContextUsageConfidence::Partial
        };
        (inp, cc.unwrap_or(0), cr.unwrap_or(0), out, conf)
    } else {
        (0, 0, 0, 0, ContextUsageConfidence::Missing)
    };

    let stop_reason = stop_reason_value.and_then(|v| v.as_str()).map(String::from);

    // V99/P1-C. These are read straight off the same `usage` object the
    // counters above come from, so they need no extra event handling. The
    // 1h/5m split is the point: Claude Code drops from the 1-hour prompt-cache
    // TTL to the 5-minute one once an account draws on usage credits, and
    // capturing both is what makes that transition visible per turn.
    let thinking = usage
        .and_then(|u| u.get("output_tokens_details"))
        .and_then(|d| d.get("thinking_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation_detail = usage.and_then(|u| u.get("cache_creation"));
    let cache_creation_1h = cache_creation_detail
        .and_then(|c| c.get("ephemeral_1h_input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation_5m = cache_creation_detail
        .and_then(|c| c.get("ephemeral_5m_input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let service_tier = usage
        .and_then(|u| u.get("service_tier"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let prompt_tokens_total = usage
        .and_then(|u| u.get("prompt_tokens_total"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    Some(TokenUsage {
        input,
        cache_creation,
        cache_read,
        output,
        total_input: input + cache_creation + cache_read,
        prompt_tokens_total,
        prompt_tokens_cumulative: usage
            .and_then(|u| u.get("prompt_tokens_cumulative"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        confidence,
        stop_reason,
        thinking,
        cache_creation_1h,
        cache_creation_5m,
        service_tier,
    })
}

/// Build a `TurnMetric` from extracted token usage and accumulated tool names.
pub(crate) fn build_turn_metric(
    session_id: Uuid,
    turn_number: i32,
    usage: &TokenUsage,
    current_turn_tools: &[String],
    model: Option<String>,
) -> TurnMetric {
    TurnMetric {
        id: 0, // Placeholder - DB assigns real ID
        session_id,
        turn_number,
        input_tokens: usage.input,
        cache_creation_tokens: usage.cache_creation,
        cache_read_tokens: usage.cache_read,
        output_tokens: usage.output,
        stop_reason: usage.stop_reason.clone(),
        tools_used: if current_turn_tools.is_empty() {
            None
        } else {
            Some(current_turn_tools.to_vec())
        },
        tool_count: current_turn_tools.len() as u32,
        created_at: chrono::Utc::now(),
        model,
        thinking_tokens: usage.thinking,
        cache_creation_1h_tokens: usage.cache_creation_1h,
        cache_creation_5m_tokens: usage.cache_creation_5m,
        service_tier: usage.service_tier.clone(),
    }
}

/// Publish a real-time context usage event to the event bus.
pub(crate) fn publish_context_usage(
    event_bus: &Arc<EventBus>,
    session_id: Uuid,
    pct: f64,
    input_tokens: u64,
    output_tokens: u64,
    daemon_total: u64,
    confidence: ContextUsageConfidence,
    resolved_context_budget: rsi_common::ResolvedContextBudget,
) {
    let context_window = resolved_context_budget.active_tokens;
    event_bus.publish(DaemonEvent::ContextUsageUpdated {
        session_id,
        pct,
        input_tokens,
        output_tokens,
        daemon_total,
        confidence,
        context_window,
        resolved_context_budget: Some(resolved_context_budget),
    });
}

/// Metadata extracted from a "result" stream event.
pub(crate) struct ResultMetadata {
    pub duration_ms: Option<u64>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u32>,
    pub context_window: Option<u64>,
    pub final_input_tokens: Option<u64>,
    pub final_output_tokens: Option<u64>,
    pub stop_reason: Option<String>,
    // === V99 richer usage capture (P1-C) ===
    //
    // Every field is `Option`: `None` means the provider did not report the
    // counter at all, which is distinct from a reported zero. Callers must not
    // collapse the two.
    pub thinking_tokens: Option<u64>,
    pub service_tier: Option<String>,
    pub cache_creation_1h_tokens: Option<u64>,
    pub cache_creation_5m_tokens: Option<u64>,
    /// Length of the `permission_denials` array. A count, not the elements:
    /// only the empty case has ever been observed, so the element shape would
    /// be invented rather than known (deferred to P2-DENIALS).
    pub permission_denial_count: Option<u64>,
    /// `subagent_stats` verbatim as JSON. Opaque by design — the shape has six
    /// nested sub-objects and a `by_type` map with unknown keys.
    pub subagent_stats_json: Option<String>,
    pub queued_turn_count: Option<u64>,
    pub terminal_reason: Option<String>,
}

/// Pick the `modelUsage` entry that describes the session's own model.
///
/// `modelUsage` is a map KEYED BY MODEL, and any turn that ran a subagent or
/// tripped `--fallback-model` has more than one entry. The map key can carry a
/// variant suffix (observed: `"claude-opus-5[1m]"`) while the entry's
/// `canonicalModel` field holds the clean ID, so the canonical field is the
/// match to trust.
///
/// G-005/F-151: this used to be `obj.iter().next()`. This workspace builds
/// `serde_json` WITHOUT `preserve_order`, so `Map` is a `BTreeMap` and
/// `iter().next()` returns the **alphabetically first** key, deterministically —
/// not the first one the CLI serialized. `claude-haiku-4-5-…` sorts before
/// `claude-opus-5[1m]`, so an Opus (1M) session that ran a Haiku (200k) subagent
/// bound `context_window = 200_000` **every time**, overstating context fill ~5x.
/// Verified empirically against this workspace's compiled `serde_json`: a map
/// parsed from `{"zebra","apple","middle"}` iterates `["apple","middle","zebra"]`.
///
/// Falls back to the first entry only when nothing matches, which preserves
/// the previous behavior for single-entry payloads and for sessions whose
/// model RSI does not know yet.
fn select_model_usage<'a>(
    usage_by_model: &'a serde_json::Map<String, serde_json::Value>,
    session_model: Option<&str>,
) -> Option<&'a serde_json::Value> {
    if let Some(model) = session_model {
        if let Some((_, usage)) = usage_by_model
            .iter()
            .find(|(_, usage)| usage.get("canonicalModel").and_then(|v| v.as_str()) == Some(model))
        {
            return Some(usage);
        }
        if let Some(usage) = usage_by_model.get(model) {
            return Some(usage);
        }
        // No `canonicalModel` (older CLI): match the `<model>[variant]` key shape.
        if let Some((_, usage)) = usage_by_model
            .iter()
            .find(|(key, _)| key.starts_with(model) && key[model.len()..].starts_with('['))
        {
            return Some(usage);
        }
    }
    usage_by_model.values().next()
}

/// A `--fallback-model` that ran (#2762): the model a session was launched on
/// and the chain member that served part of the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelFallback {
    pub from: String,
    pub to: String,
}

/// The fallback a `result` reports, if one ran: its `modelUsage` has an entry
/// other than the session's own model that is a member of the operator's chain
/// (`chain` is already the chain this session launched with, see
/// `claude_fallback_chain_for`). An entry that is not a chain member is a
/// subagent's model, not a fallback. The map key can carry a variant suffix
/// and the entry a `canonicalModel`; both are compared without it. `to` is the
/// chain member's name as the operator wrote it, so the event names the
/// configured model.
pub(crate) fn model_fallback_used(
    stream_event: &StreamEvent,
    session_model: Option<&str>,
    chain: &[String],
) -> Option<ModelFallback> {
    let usage_by_model = stream_event.data.get("modelUsage")?.as_object()?;
    if usage_by_model.len() < 2 || chain.is_empty() {
        return None;
    }
    let names_of = |key: &str, usage: &serde_json::Value| -> Vec<String> {
        let mut names = vec![key.to_string()];
        if let Some(canonical) = usage.get("canonicalModel").and_then(|v| v.as_str()) {
            names.push(canonical.to_string());
        }
        names
    };
    let chain_member = |names: &[String]| {
        chain.iter().find(|member| {
            names
                .iter()
                .any(|name| crate::claude::same_claude_model(name, member))
        })
    };
    let is_session_model = |names: &[String]| {
        session_model.is_some_and(|own| {
            names
                .iter()
                .any(|name| crate::claude::same_claude_model(name, own))
        })
    };
    let mut fallback = None;
    let mut unmatched_primary = None;
    for (key, usage) in usage_by_model {
        let names = names_of(key, usage);
        if is_session_model(&names) {
            continue;
        }
        match chain_member(&names) {
            Some(member) if fallback.is_none() => fallback = Some(member.clone()),
            Some(_) => {}
            None => unmatched_primary = unmatched_primary.or(Some(key.clone())),
        }
    }
    let to = fallback?;
    let from = session_model
        .map(rsi_common::claude_catalog::strip_context_variant_suffix)
        .map(str::to_string)
        .or_else(|| {
            unmatched_primary.map(|key| {
                rsi_common::claude_catalog::strip_context_variant_suffix(&key).to_string()
            })
        })?;
    Some(ModelFallback { from, to })
}

/// Stable `stop_reason`/`terminal_reason` for a Claude Code exit that rejected
/// the selected model (#1484).
pub(crate) const PROVIDER_MODEL_UNRECOGNIZED_REASON: &str = "provider_model_unrecognized";

/// Detect Claude Code's rejection of the selected model in stderr or result
/// text and return the model name it names (empty when the text names none).
pub(crate) fn unrecognized_model_in_text(text: &str) -> Option<String> {
    if let Some(at) = text.find("[claude-code:unrecognized_model]") {
        let model = text[at..]
            .split("\"model\"")
            .nth(1)
            .and_then(|tail| tail.split('"').nth(1))
            .unwrap_or_default();
        return Some(model.to_string());
    }
    let marker = "issue with the selected model";
    let at = text.to_ascii_lowercase().find(marker)?;
    let model = text[at + marker.len()..]
        .trim_start()
        .strip_prefix('(')
        .and_then(|r| r.split(')').next())
        .unwrap_or_default();
    Some(model.trim().to_string())
}

/// Stable `terminal_reason` for a Claude turn that ended in an error result
/// right after the provider echoed a tool call as rejected (#31). The
/// provider's own `stop_reason` stays as reported; this names the shape so a
/// lead is told the child stopped at a rejected tool call (work possibly
/// left in its sandbox) instead of reading a bare provider error.
pub(crate) const PROVIDER_TOOL_REJECTED_REASON: &str = "provider_tool_rejected";

/// The text Claude Code puts in a tool result for a rejected tool call.
const TOOL_REJECTION_MARKER: &str = "doesn't want to proceed with this tool use";

/// Does a tool result body carry the provider's tool-rejection text.
pub(crate) fn is_tool_rejection_text(text: &str) -> bool {
    text.contains(TOOL_REJECTION_MARKER)
}

/// How one stream event moves the "the last thing the provider said was a
/// rejected tool call" flag: `Some(true)` for a `user` event whose tool result
/// is the rejection text, `Some(false)` for output that proves the turn went on
/// (an assistant or tool-use event), `None` for events that say nothing.
pub(crate) fn tool_rejection_transition(stream_event: &StreamEvent) -> Option<bool> {
    match stream_event.event_type.as_str() {
        "assistant" | "tool_use" => Some(false),
        "user" => {
            let blocks = stream_event
                .data
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(|content| content.as_array())?;
            let rejected = blocks
                .iter()
                .filter(|block| block.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
                .filter_map(|block| block.get("content"))
                .any(|content| match content {
                    serde_json::Value::String(text) => is_tool_rejection_text(text),
                    serde_json::Value::Array(parts) => parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
                        .any(is_tool_rejection_text),
                    _ => false,
                });
            rejected.then_some(true)
        }
        _ => None,
    }
}

/// The typed terminal reason for an error result that follows a rejected tool
/// call, or `None` when the turn did not end that way or the provider already
/// typed its own `terminal_reason` (kept as reported).
pub(crate) fn tool_rejection_terminal_reason(
    rejection_is_last_output: bool,
    result_is_error: bool,
    provider_terminal_reason: Option<&str>,
) -> Option<&'static str> {
    (rejection_is_last_output && result_is_error && provider_terminal_reason.is_none())
        .then_some(PROVIDER_TOOL_REJECTED_REASON)
}

/// The auth or billing class a Claude retry notice names (#2753), or `None`
/// for any other event. Claude Code writes the notice as a `system` event with
/// `subtype: "api_retry"` (`{"type":"system","subtype":"api_retry",
/// "error":"billing_error",...}`); the reader forwards the wire `type` as
/// `event_type` unchanged, so the subtype is what identifies it (#2811). A
/// top-level `type: "api_retry"` is accepted too.
pub(crate) fn api_retry_auth_class(
    stream_event: &StreamEvent,
) -> Option<crate::store_support::provider_defaults::ProviderAuthClass> {
    let data = &stream_event.data;
    let is_api_retry = match stream_event.event_type.as_str() {
        "system" => data.get("subtype").and_then(serde_json::Value::as_str) == Some("api_retry"),
        "api_retry" => true,
        _ => false,
    };
    if !is_api_retry {
        return None;
    }
    data.get("error")
        .and_then(serde_json::Value::as_str)
        .and_then(crate::store_support::provider_defaults::classify_api_retry_error)
}

/// The stop reason of an error result that follows a Claude `system/api_retry`
/// naming an auth or billing class (`authentication_failed`, `account_on_hold`,
/// `billing_error`, #2753): that class, unless the result's own text already
/// typed it (`provider_*`), e.g. as a rejected model or a missing login.
pub(crate) fn stop_reason_after_api_retry(
    stop_reason: Option<String>,
    class: Option<crate::store_support::provider_defaults::ProviderAuthClass>,
) -> Option<String> {
    match (stop_reason, class) {
        (Some(reason), _) if reason.starts_with("provider_") => Some(reason),
        (_, Some(class)) => Some(class.stop_reason().to_string()),
        (reason, None) => reason,
    }
}

/// What the monitor knows about the turn around a `result` event, beyond the
/// event itself, that types the stop and terminal reasons (#2824).
#[derive(Clone, Copy, Default)]
pub(crate) struct ResultContext {
    /// The `result` is a provider error (not a clean completion).
    pub provider_error: bool,
    /// The auth or billing class a preceding `system/api_retry` named (#2753).
    pub api_retry_class: Option<crate::store_support::provider_defaults::ProviderAuthClass>,
    /// The last provider output was a rejected tool call (#31).
    pub tool_rejection_last: bool,
    /// The provider's stderr or notices rejected the selected model (#1484).
    pub rejected_model: bool,
}

/// The result metadata the session keeps, with every stop-reason adjustment
/// applied exactly once: the `api_retry` class (#2753), the rejected-tool
/// terminal reason (#31) and the rejected-model reason (#1484). The monitor
/// computes this once per `result` and uses the one value for the login-probe
/// note and for the tracked-session write, so a new adjustment cannot reach
/// one consumer and miss the other (#2824).
pub(crate) fn extract_adjusted_result_metadata(
    stream_event: &StreamEvent,
    session_model: Option<&str>,
    context: ResultContext,
) -> ResultMetadata {
    let mut meta = extract_result_metadata(stream_event, session_model);
    if context.provider_error {
        meta.stop_reason =
            stop_reason_after_api_retry(meta.stop_reason.take(), context.api_retry_class);
    }
    if let Some(reason) = tool_rejection_terminal_reason(
        context.tool_rejection_last,
        context.provider_error,
        meta.terminal_reason.as_deref(),
    ) {
        meta.terminal_reason = Some(reason.to_string());
    }
    if context.rejected_model {
        meta.stop_reason = Some(PROVIDER_MODEL_UNRECOGNIZED_REASON.to_string());
        meta.terminal_reason = Some(PROVIDER_MODEL_UNRECOGNIZED_REASON.to_string());
    }
    meta
}

/// Extract session metadata from a "result" stream event.
///
/// `session_model` is the session's configured model, used to pick the right
/// `modelUsage` entry (see [`select_model_usage`]).
pub(crate) fn extract_result_metadata(
    stream_event: &StreamEvent,
    session_model: Option<&str>,
) -> ResultMetadata {
    let duration_ms = stream_event
        .data
        .get("duration_ms")
        .and_then(|v| v.as_u64());
    let cost_usd = stream_event
        .data
        .get("total_cost_usd")
        .and_then(|v| v.as_f64());
    let num_turns = stream_event
        .data
        .get("num_turns")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let context_window = stream_event
        .data
        .get("modelUsage")
        .and_then(|v| v.as_object())
        .and_then(|usage_by_model| select_model_usage(usage_by_model, session_model))
        .and_then(|usage| usage.get("contextWindow"))
        .and_then(|v| v.as_u64());

    let (final_input_tokens, final_output_tokens) =
        if let Some(usage) = stream_event.data.get("usage") {
            let input = usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let cache_creation = usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let cache_read = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let output = usage
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            (Some(input + cache_creation + cache_read), Some(output))
        } else {
            (None, None)
        };

    let rejected_model = stream_event
        .data
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        .then(|| stream_event.data.get("result").and_then(|v| v.as_str()))
        .flatten()
        .and_then(unrecognized_model_in_text);
    // #2753: an error result whose text says the CLI holds no login (`Not
    // logged in · Please run /login`, `Login expired`) or that the provider
    // rejected the credential is that typed class, not the bare subtype.
    let auth_class = stream_event
        .data
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        .then(|| stream_event.data.get("result").and_then(|v| v.as_str()))
        .flatten()
        .and_then(crate::store_support::provider_defaults::classify_provider_auth_text);
    let budget_exceeded = stream_event.data.get("subtype").and_then(|v| v.as_str())
        == Some("error_max_budget_usd")
        || stream_event
            .data
            .get("terminal_reason")
            .and_then(|v| v.as_str())
            == Some("budget_exhausted");
    let stop_reason = if rejected_model.is_some() {
        Some(PROVIDER_MODEL_UNRECOGNIZED_REASON.to_string())
    } else if budget_exceeded {
        Some(
            crate::store_support::provider_defaults::CLAUDE_BUDGET_EXCEEDED_STOP_REASON.to_string(),
        )
    } else if let Some(class) = auth_class {
        Some(class.stop_reason().to_string())
    } else {
        stream_event
            .data
            .get("subtype")
            .and_then(|v| v.as_str())
            .filter(|subtype| *subtype != "turn_completed")
            .map(String::from)
    };

    // V99/P1-C. `usage` is absent on some result subtypes, so every extraction
    // below tolerates its absence and yields `None` rather than a synthetic 0.
    let usage = stream_event.data.get("usage");
    let thinking_tokens = usage
        .and_then(|u| u.get("output_tokens_details"))
        .and_then(|d| d.get("thinking_tokens"))
        .and_then(|v| v.as_u64());
    let service_tier = usage
        .and_then(|u| u.get("service_tier"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let cache_creation_detail = usage.and_then(|u| u.get("cache_creation"));
    let cache_creation_1h_tokens = cache_creation_detail
        .and_then(|c| c.get("ephemeral_1h_input_tokens"))
        .and_then(|v| v.as_u64());
    let cache_creation_5m_tokens = cache_creation_detail
        .and_then(|c| c.get("ephemeral_5m_input_tokens"))
        .and_then(|v| v.as_u64());
    let permission_denial_count = stream_event
        .data
        .get("permission_denials")
        .and_then(|v| v.as_array())
        .map(|denials| denials.len() as u64);
    let subagent_stats_json = stream_event
        .data
        .get("subagent_stats")
        .filter(|v| !v.is_null())
        .map(|v| v.to_string());
    let queued_turn_count = stream_event
        .data
        .get("queued_turn_count")
        .and_then(|v| v.as_u64());
    let terminal_reason = if rejected_model.is_some() {
        Some(PROVIDER_MODEL_UNRECOGNIZED_REASON.to_string())
    } else {
        stream_event
            .data
            .get("terminal_reason")
            .and_then(|v| v.as_str())
            .map(String::from)
    };

    ResultMetadata {
        duration_ms,
        cost_usd,
        num_turns,
        context_window,
        final_input_tokens,
        final_output_tokens,
        stop_reason,
        thinking_tokens,
        service_tier,
        cache_creation_1h_tokens,
        cache_creation_5m_tokens,
        permission_denial_count,
        subagent_stats_json,
        queued_turn_count,
        terminal_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::SessionProvider;

    fn context_window_for_model(model: &str) -> u64 {
        let provider = if model.trim().to_ascii_lowercase().starts_with("claude-") {
            SessionProvider::Claude
        } else {
            SessionProvider::Local
        };
        crate::provider_capabilities::resolve_fresh_context_budget(provider, model, None)
            .active_tokens
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_extract_token_usage_assistant_with_full_usage() {
        let event = StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({
                "message": {
                    "usage": {
                        "input_tokens": 1000,
                        "cache_creation_input_tokens": 200,
                        "cache_read_input_tokens": 300,
                        "output_tokens": 500
                    }
                }
            }),
        };

        let usage = extract_token_usage(&event).unwrap();
        assert_eq!(usage.input, 1000);
        assert_eq!(usage.cache_creation, 200);
        assert_eq!(usage.cache_read, 300);
        assert_eq!(usage.output, 500);
        assert_eq!(usage.total_input, 1500);
        assert_eq!(usage.confidence, ContextUsageConfidence::Full);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_extract_token_usage_assistant_partial() {
        let event = StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({
                "message": {
                    "usage": {
                        "input_tokens": 1000,
                        "output_tokens": 500
                    }
                }
            }),
        };

        let usage = extract_token_usage(&event).unwrap();
        assert_eq!(usage.confidence, ContextUsageConfidence::Partial);
        assert_eq!(usage.cache_creation, 0);
        assert_eq!(usage.cache_read, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_extract_token_usage_non_assistant_returns_none() {
        let event = StreamEvent {
            event_type: "tool_use".to_string(),
            data: serde_json::json!({}),
        };

        assert!(extract_token_usage(&event).is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_extract_token_usage_no_usage_block() {
        let event = StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({
                "message": {
                    "role": "assistant"
                }
            }),
        };

        let usage = extract_token_usage(&event).unwrap();
        assert_eq!(usage.confidence, ContextUsageConfidence::Missing);
        assert_eq!(usage.total_input, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_extract_token_usage_with_stop_reason() {
        let event = StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({
                "message": {
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 50
                    }
                }
            }),
        };

        let usage = extract_token_usage(&event).unwrap();
        assert_eq!(usage.stop_reason, Some("end_turn".to_string()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_extract_token_usage_turn_completed_event() {
        let event = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({
                "subtype": "turn_completed",
                "stop_reason": "max_context",
                "usage": {
                    "input_tokens": 4000,
                    "cache_creation_input_tokens": 500,
                    "cache_read_input_tokens": 250,
                    "output_tokens": 800
                }
            }),
        };

        let usage = extract_token_usage(&event).unwrap();
        assert_eq!(usage.total_input, 4750);
        assert_eq!(usage.output, 800);
        assert_eq!(usage.confidence, ContextUsageConfidence::Full);
        assert_eq!(usage.stop_reason, Some("max_context".to_string()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_extract_token_usage_turn_completed_without_usage() {
        let event = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({
                "subtype": "turn_completed"
            }),
        };

        let usage = extract_token_usage(&event).unwrap();
        assert_eq!(usage.total_input, 0);
        assert_eq!(usage.confidence, ContextUsageConfidence::Missing);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_extract_codex_context_usage_uses_last_total_tokens() {
        let event = StreamEvent {
            event_type: "codex_token_count".to_string(),
            data: serde_json::json!({
                "info": {
                    "last_token_usage": {
                        "input_tokens": 80_000,
                        "cached_input_tokens": 20_000,
                        "output_tokens": 3_000,
                        "reasoning_output_tokens": 1_000,
                        "total_tokens": 92_000
                    },
                    "model_context_window": 258_400
                }
            }),
        };

        let usage = extract_codex_context_usage(&event).unwrap();
        assert_eq!(usage.context_tokens, 92_000);
        assert_eq!(usage.output_tokens, 3_000);
        assert_eq!(usage.context_window, Some(258_400));
        assert_eq!(usage.cache_read_tokens, Some(20_000));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn extract_codex_context_distinguishes_true_zero_from_missing() {
        let mut event = StreamEvent {
            event_type: "codex_token_count".into(),
            data: serde_json::json!({"info":{"last_token_usage":{"total_tokens":0}}}),
        };
        let zero = extract_codex_context_usage(&event).unwrap();
        assert_eq!(zero.context_tokens, 0);
        assert_eq!(zero.context_window, None);
        event.data["info"]["last_token_usage"] = serde_json::json!({});
        assert!(extract_codex_context_usage(&event).is_none());
        event.data["info"] = serde_json::Value::Null;
        assert!(extract_codex_context_usage(&event).is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_build_turn_metric() {
        let session_id = Uuid::new_v4();
        let usage = TokenUsage {
            input: 1000,
            cache_creation: 200,
            cache_read: 300,
            output: 500,
            total_input: 1500,
            prompt_tokens_total: 0,
            prompt_tokens_cumulative: false,
            confidence: ContextUsageConfidence::Full,
            stop_reason: Some("end_turn".to_string()),
            thinking: 120,
            cache_creation_1h: 7411,
            cache_creation_5m: 0,
            service_tier: Some("standard".to_string()),
        };
        let tools = vec!["Read".to_string(), "Edit".to_string()];

        let metric = build_turn_metric(
            session_id,
            3,
            &usage,
            &tools,
            Some("claude-sonnet-5".to_string()),
        );
        assert_eq!(metric.session_id, session_id);
        assert_eq!(metric.turn_number, 3);
        assert_eq!(metric.input_tokens, 1000);
        assert_eq!(metric.cache_creation_tokens, 200);
        assert_eq!(metric.cache_read_tokens, 300);
        assert_eq!(metric.output_tokens, 500);
        assert_eq!(metric.stop_reason, Some("end_turn".to_string()));
        assert_eq!(
            metric.tools_used,
            Some(vec!["Read".to_string(), "Edit".to_string()])
        );
        assert_eq!(metric.tool_count, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_build_turn_metric_no_tools() {
        let usage = TokenUsage {
            input: 100,
            cache_creation: 0,
            cache_read: 0,
            output: 50,
            total_input: 100,
            prompt_tokens_total: 0,
            prompt_tokens_cumulative: false,
            confidence: ContextUsageConfidence::Partial,
            stop_reason: None,
            thinking: 0,
            cache_creation_1h: 0,
            cache_creation_5m: 0,
            service_tier: None,
        };

        let metric = build_turn_metric(Uuid::new_v4(), 1, &usage, &[], None);
        assert!(metric.tools_used.is_none());
        assert_eq!(metric.tool_count, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_extract_result_metadata() {
        let event = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({
                "duration_ms": 5000,
                "total_cost_usd": 0.0123,
                "num_turns": 3,
                "subtype": "success",
                "modelUsage": {
                    "claude-sonnet-5": {
                        "contextWindow": 200000
                    }
                },
                "usage": {
                    "input_tokens": 10000,
                    "cache_creation_input_tokens": 2000,
                    "cache_read_input_tokens": 3000,
                    "output_tokens": 1500
                }
            }),
        };

        let meta = extract_result_metadata(&event, Some("claude-sonnet-5"));
        assert_eq!(meta.duration_ms, Some(5000));
        assert_eq!(meta.cost_usd, Some(0.0123));
        assert_eq!(meta.num_turns, Some(3));
        assert_eq!(meta.context_window, Some(200000));
        assert_eq!(meta.final_input_tokens, Some(15000));
        assert_eq!(meta.final_output_tokens, Some(1500));
        assert_eq!(meta.stop_reason, Some("success".to_string()));
    }

    /// #31: the incident shape (child 53b1437b): a Bash tool call, the
    /// provider's rejection echoed as a tool result, then an
    /// `error_during_execution` result and no further output.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_rejected_tool_call_then_an_error_result_is_typed_provider_tool_rejected() {
        let rejection = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";
        let user = |content: serde_json::Value| StreamEvent {
            event_type: "user".to_string(),
            data: serde_json::json!({"message": {"content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": content}
            ]}}),
        };
        // Both content shapes the CLI emits (bare string, text-block array).
        assert_eq!(
            tool_rejection_transition(&user(serde_json::json!(rejection))),
            Some(true)
        );
        assert_eq!(
            tool_rejection_transition(&user(
                serde_json::json!([{"type": "text", "text": rejection}])
            )),
            Some(true)
        );
        // An ordinary tool result says nothing; later assistant output clears it.
        assert_eq!(
            tool_rejection_transition(&user(serde_json::json!("ok"))),
            None
        );
        let assistant = StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({"message": {"content": []}}),
        };
        assert_eq!(tool_rejection_transition(&assistant), Some(false));

        let result = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({"subtype": "error_during_execution", "is_error": true}),
        };
        let meta = extract_result_metadata(&result, None);
        assert_eq!(meta.stop_reason.as_deref(), Some("error_during_execution"));
        assert_eq!(
            tool_rejection_terminal_reason(true, true, meta.terminal_reason.as_deref()),
            Some("provider_tool_rejected")
        );
        // Not at a rejection, a clean result, or a provider-typed reason: no relabel.
        assert_eq!(tool_rejection_terminal_reason(false, true, None), None);
        assert_eq!(tool_rejection_terminal_reason(true, false, None), None);
        assert_eq!(
            tool_rejection_terminal_reason(true, true, Some("aborted_tools")),
            None
        );
    }

    /// #2753: the observed and documented Claude texts type the error result
    /// as a missing login or a rejected credential; the api_retry class types
    /// one that carries neither.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn claude_auth_results_and_api_retry_classes_map_to_typed_stop_reasons() {
        use crate::store_support::provider_defaults::{
            PROVIDER_AUTH_INVALID_STOP_REASON, PROVIDER_BILLING_ERROR_STOP_REASON,
            PROVIDER_LOGIN_REQUIRED_STOP_REASON, ProviderAuthClass, classify_api_retry_error,
        };
        let result = |text: &str| StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({
                "subtype": "success",
                "is_error": true,
                "terminal_reason": "api_error",
                "result": text,
            }),
        };
        for text in [
            // Observed on a logged-out `claude -p`.
            "Not logged in \u{b7} Please run /login",
            // Documented on expiry (CC-AUTH).
            "Login expired \u{b7} Please run /login",
        ] {
            let meta = extract_result_metadata(&result(text), None);
            assert_eq!(
                meta.stop_reason.as_deref(),
                Some(PROVIDER_LOGIN_REQUIRED_STOP_REASON),
                "{text}"
            );
            // The provider's own terminal reason is kept as reported.
            assert_eq!(meta.terminal_reason.as_deref(), Some("api_error"));
        }
        let meta = extract_result_metadata(
            &result("API Error: 401 Unauthorized \u{b7} invalidated oauth token"),
            None,
        );
        assert_eq!(
            meta.stop_reason.as_deref(),
            Some(PROVIDER_AUTH_INVALID_STOP_REASON)
        );
        // A successful result that merely mentions a login is not a failure.
        let ok = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({
                "subtype": "success",
                "is_error": false,
                "result": "You are not logged in to the app; I fixed the redirect.",
            }),
        };
        assert_eq!(
            extract_result_metadata(&ok, None).stop_reason.as_deref(),
            Some("success")
        );

        // `system/api_retry` `error` classes.
        for (error, class, stop) in [
            (
                "authentication_failed",
                ProviderAuthClass::LoginRequired,
                PROVIDER_LOGIN_REQUIRED_STOP_REASON,
            ),
            (
                "oauth_org_not_allowed",
                ProviderAuthClass::AuthInvalid,
                PROVIDER_AUTH_INVALID_STOP_REASON,
            ),
            (
                "account_on_hold",
                ProviderAuthClass::AuthInvalid,
                PROVIDER_AUTH_INVALID_STOP_REASON,
            ),
            (
                "billing_error",
                ProviderAuthClass::Billing,
                PROVIDER_BILLING_ERROR_STOP_REASON,
            ),
        ] {
            assert_eq!(classify_api_retry_error(error), Some(class), "{error}");
            assert_eq!(
                stop_reason_after_api_retry(Some("error_during_execution".into()), Some(class))
                    .as_deref(),
                Some(stop),
                "{error}"
            );
        }
        assert_eq!(classify_api_retry_error("rate_limit"), None);
        // The result's own typed reason wins; no class leaves the reason alone.
        assert_eq!(
            stop_reason_after_api_retry(
                Some(PROVIDER_MODEL_UNRECOGNIZED_REASON.into()),
                Some(ProviderAuthClass::Billing)
            )
            .as_deref(),
            Some(PROVIDER_MODEL_UNRECOGNIZED_REASON)
        );
        assert_eq!(
            stop_reason_after_api_retry(Some("success".into()), None).as_deref(),
            Some("success")
        );
    }

    /// #2811: the retry notice is recognised in its wire shape, a `system`
    /// event with `subtype: "api_retry"`, parsed exactly as the Claude reader
    /// parses a stdout line.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn api_retry_auth_class_reads_the_system_api_retry_wire_shape() {
        use crate::store_support::provider_defaults::ProviderAuthClass;
        let wire = |line: &str| serde_json::from_str::<StreamEvent>(line).unwrap();
        assert_eq!(
            api_retry_auth_class(&wire(
                r#"{"type":"system","subtype":"api_retry","attempt":2,"error":"billing_error"}"#
            )),
            Some(ProviderAuthClass::Billing)
        );
        assert_eq!(
            api_retry_auth_class(&wire(
                r#"{"type":"system","subtype":"api_retry","error":"authentication_failed"}"#
            )),
            Some(ProviderAuthClass::LoginRequired)
        );
        assert_eq!(
            api_retry_auth_class(&wire(r#"{"type":"api_retry","error":"account_on_hold"}"#)),
            Some(ProviderAuthClass::AuthInvalid)
        );
        // A retryable class, another system subtype and another event type
        // carry no auth class.
        assert_eq!(
            api_retry_auth_class(&wire(
                r#"{"type":"system","subtype":"api_retry","error":"rate_limit"}"#
            )),
            None
        );
        assert_eq!(
            api_retry_auth_class(&wire(
                r#"{"type":"system","subtype":"init","error":"billing_error"}"#
            )),
            None
        );
        assert_eq!(
            api_retry_auth_class(&wire(r#"{"type":"assistant","error":"billing_error"}"#)),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn unrecognized_model_maps_to_provider_model_unrecognized() {
        assert_eq!(
            unrecognized_model_in_text(
                "[claude-code:unrecognized_model] {\"model\":\"claude-haiku-5-5\"}"
            )
            .as_deref(),
            Some("claude-haiku-5-5")
        );
        assert_eq!(
            unrecognized_model_in_text(
                "There's an issue with the selected model (claude-haiku-5-5). It may not exist"
            )
            .as_deref(),
            Some("claude-haiku-5-5")
        );
        assert_eq!(unrecognized_model_in_text("all good"), None);
        let event = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({
                "subtype": "success",
                "is_error": true,
                "result": "There's an issue with the selected model (claude-haiku-5-5). It may not exist or you may not have access to it."
            }),
        };
        let meta = extract_result_metadata(&event, None);
        assert_eq!(
            meta.stop_reason.as_deref(),
            Some("provider_model_unrecognized")
        );
        assert_eq!(
            meta.terminal_reason.as_deref(),
            Some("provider_model_unrecognized")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_extract_result_metadata_ignores_turn_completed_subtype() {
        let event = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({
                "subtype": "turn_completed",
                "usage": {
                    "input_tokens": 1000,
                    "output_tokens": 250
                }
            }),
        };

        let meta = extract_result_metadata(&event, Some("claude-sonnet-5"));
        assert_eq!(meta.stop_reason, None);
        assert_eq!(meta.final_input_tokens, Some(1000));
        assert_eq!(meta.final_output_tokens, Some(250));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_extract_result_metadata_minimal() {
        let event = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({}),
        };

        let meta = extract_result_metadata(&event, Some("claude-sonnet-5"));
        assert!(meta.duration_ms.is_none());
        assert!(meta.cost_usd.is_none());
        assert!(meta.num_turns.is_none());
        assert!(meta.context_window.is_none());
        assert!(meta.final_input_tokens.is_none());
        assert!(meta.stop_reason.is_none());
    }

    /// #2824: one helper types the stop and terminal reasons, so the
    /// `api_retry` class, the rejected tool and the rejected model all reach
    /// the one value the monitor writes to the session.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn adjusted_result_metadata_applies_every_stop_reason_adjustment_once() {
        use crate::store_support::provider_defaults::ProviderAuthClass;
        let error_result = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({"subtype": "error_during_execution", "is_error": true}),
        };
        let error = ResultContext {
            provider_error: true,
            ..ResultContext::default()
        };

        let retried = extract_adjusted_result_metadata(
            &error_result,
            None,
            ResultContext {
                api_retry_class: Some(ProviderAuthClass::Billing),
                ..error
            },
        );
        assert_eq!(
            retried.stop_reason.as_deref(),
            Some(ProviderAuthClass::Billing.stop_reason())
        );

        let tool = extract_adjusted_result_metadata(
            &error_result,
            None,
            ResultContext {
                tool_rejection_last: true,
                ..error
            },
        );
        assert_eq!(
            tool.terminal_reason.as_deref(),
            Some(PROVIDER_TOOL_REJECTED_REASON)
        );
        assert_eq!(tool.stop_reason.as_deref(), Some("error_during_execution"));

        let model = extract_adjusted_result_metadata(
            &error_result,
            None,
            ResultContext {
                api_retry_class: Some(ProviderAuthClass::Billing),
                tool_rejection_last: true,
                rejected_model: true,
                ..error
            },
        );
        assert_eq!(
            model.stop_reason.as_deref(),
            Some(PROVIDER_MODEL_UNRECOGNIZED_REASON)
        );
        assert_eq!(
            model.terminal_reason.as_deref(),
            Some(PROVIDER_MODEL_UNRECOGNIZED_REASON)
        );

        let clean = extract_adjusted_result_metadata(
            &error_result,
            None,
            ResultContext {
                api_retry_class: Some(ProviderAuthClass::Billing),
                tool_rejection_last: true,
                ..ResultContext::default()
            },
        );
        assert_eq!(clean.stop_reason.as_deref(), Some("error_during_execution"));
        assert_eq!(clean.terminal_reason, None);
    }

    /// G-005/F-151: a multi-model `modelUsage` map must resolve to the
    /// SESSION's window, not to whichever entry the CLI serialized first.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn extract_result_metadata_picks_the_sessions_model_usage_entry() {
        // Observed shape: the map key carries a variant suffix while
        // `canonicalModel` holds the clean ID. The Haiku subagent entry is
        // deliberately first, which is exactly what the old
        // `iter().next()` selection would have taken.
        let event = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({
                "subtype": "success",
                "modelUsage": {
                    "claude-haiku-4-5-20251001": {
                        "contextWindow": 200_000,
                        "maxOutputTokens": 32_000,
                        "canonicalModel": "claude-haiku-4-5-20251001"
                    },
                    "claude-opus-5[1m]": {
                        "contextWindow": 1_000_000,
                        "maxOutputTokens": 64_000,
                        "canonicalModel": "claude-opus-5"
                    }
                }
            }),
        };

        assert_eq!(
            extract_result_metadata(&event, Some("claude-opus-5")).context_window,
            Some(1_000_000),
            "an Opus session that ran a Haiku subagent must keep its 1M window"
        );
        assert_eq!(
            extract_result_metadata(&event, Some("claude-haiku-4-5-20251001")).context_window,
            Some(200_000),
            "a Haiku session must resolve to the Haiku entry"
        );
    }

    /// #2762: a result with two `modelUsage` entries where the second is a
    /// chain member is a fallback; a subagent's model that is not in the chain
    /// is not; a result with the session's model alone is not.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn model_fallback_used_names_the_chain_member_that_ran() {
        let event = |usage: serde_json::Value| StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({ "subtype": "success", "modelUsage": usage }),
        };
        let chain = vec![
            "claude-sonnet-5-5".to_string(),
            "claude-haiku-4-5".to_string(),
        ];

        let fell_back = event(serde_json::json!({
            "claude-opus-5[1m]": { "canonicalModel": "claude-opus-5" },
            "claude-sonnet-5-5": { "canonicalModel": "claude-sonnet-5-5" }
        }));
        assert_eq!(
            model_fallback_used(&fell_back, Some("claude-opus-5"), &chain),
            Some(ModelFallback {
                from: "claude-opus-5".to_string(),
                to: "claude-sonnet-5-5".to_string(),
            })
        );

        // The primary never served a token: it is still the `from`.
        let only_fallback = event(serde_json::json!({
            "claude-opus-5-shadow": {},
            "claude-haiku-4-5": {}
        }));
        assert_eq!(
            model_fallback_used(&only_fallback, Some("claude-opus-5"), &chain)
                .map(|fallback| (fallback.from, fallback.to)),
            Some(("claude-opus-5".to_string(), "claude-haiku-4-5".to_string()))
        );

        let subagent = event(serde_json::json!({
            "claude-opus-5[1m]": { "canonicalModel": "claude-opus-5" },
            "claude-haiku-4-5-20251001": { "canonicalModel": "claude-haiku-4-5-20251001" }
        }));
        assert_eq!(
            model_fallback_used(&subagent, Some("claude-opus-5"), &chain),
            None
        );

        let single = event(serde_json::json!({
            "claude-opus-5[1m]": { "canonicalModel": "claude-opus-5" }
        }));
        assert_eq!(
            model_fallback_used(&single, Some("claude-opus-5"), &chain),
            None
        );
        assert_eq!(
            model_fallback_used(&fell_back, Some("claude-opus-5"), &[]),
            None
        );
    }

    /// #2762: the budget-exceeded result (`subtype: error_max_budget_usd`,
    /// `terminal_reason: budget_exhausted`, read from Claude Code's result
    /// schema) maps to the typed stop reason, by either field alone.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn a_budget_exceeded_result_is_typed_provider_budget_exceeded() {
        for data in [
            serde_json::json!({
                "subtype": "error_max_budget_usd",
                "is_error": true,
                "terminal_reason": "budget_exhausted",
                "total_cost_usd": 5.01,
            }),
            serde_json::json!({ "subtype": "error_max_budget_usd", "is_error": true }),
            serde_json::json!({ "subtype": "error_during_execution", "terminal_reason": "budget_exhausted" }),
        ] {
            let meta = extract_result_metadata(
                &StreamEvent {
                    event_type: "result".to_string(),
                    data,
                },
                None,
            );
            assert_eq!(
                meta.stop_reason.as_deref(),
                Some("provider_budget_exceeded")
            );
        }
        let other = extract_result_metadata(
            &StreamEvent {
                event_type: "result".to_string(),
                data: serde_json::json!({ "subtype": "error_max_turns", "is_error": true }),
            },
            None,
        );
        assert_eq!(other.stop_reason.as_deref(), Some("error_max_turns"));
    }

    /// Older payloads carry no `canonicalModel`; the `<model>[variant]` key
    /// shape still resolves to the session's own entry.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn extract_result_metadata_matches_variant_suffixed_keys_without_canonical_model() {
        let event = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({
                "modelUsage": {
                    "claude-haiku-4-5-20251001": {"contextWindow": 200_000},
                    "claude-opus-5[1m]": {"contextWindow": 1_000_000}
                }
            }),
        };

        assert_eq!(
            extract_result_metadata(&event, Some("claude-opus-5")).context_window,
            Some(1_000_000)
        );
    }

    /// No match (unknown session model, or a model absent from the payload)
    /// keeps the previous first-entry behavior rather than dropping the
    /// authoritative window entirely.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn extract_result_metadata_falls_back_to_the_first_entry_when_nothing_matches() {
        let event = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({
                "modelUsage": {
                    "claude-opus-5[1m]": {
                        "contextWindow": 1_000_000,
                        "canonicalModel": "claude-opus-5"
                    }
                }
            }),
        };

        assert_eq!(
            extract_result_metadata(&event, Some("claude-sonnet-5")).context_window,
            Some(1_000_000)
        );
        assert_eq!(
            extract_result_metadata(&event, None).context_window,
            Some(1_000_000)
        );
    }

    // ── --resume fixture: high-watermark behavior (Open Q1 verification) ──
    //
    // The Anthropic CLI has been observed to report `input_tokens` cumulatively
    // per-request after `--resume`, but the published schema does not contract
    // that — a future CLI release could stream deltas. Our code path at
    // `session/monitor.rs::monitor_session` applies
    //     tracked.live_input_tokens = tracked.live_input_tokens.max(usage.total_input)
    // which is correct in both interpretations: if cumulative, the `max()` is a
    // no-op and the new value wins; if delta, the `max()` preserves the larger
    // prior value and the displayed pct never ratchets backward.
    //
    // These two tests feed the fixture through `extract_token_usage` and then
    // emulate the one-line `.max()` assignment that the monitor loop performs.
    // The test name is scoped to `resume_fixture` to make the provenance
    // obvious in CI output.

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_resume_fixture_cumulative_watermark_rises() {
        // Simulate: prior assistant chunk reported total_input=30_000 →
        // live_input_tokens = 30_000. Now a post-`--resume` chunk reports
        // cumulative total_input=48_000. The high-watermark must rise.
        let event = StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({
                "message": {
                    "usage": {
                        "input_tokens": 48_000,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0,
                        "output_tokens": 0
                    }
                }
            }),
        };
        let usage = extract_token_usage(&event).expect("usage parses");
        let mut live_input_tokens: u64 = 30_000;
        live_input_tokens = live_input_tokens.max(usage.total_input);
        assert_eq!(
            live_input_tokens, 48_000,
            "High-watermark must rise to the cumulative post-resume report"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_resume_fixture_delta_watermark_holds() {
        // Simulate: live_input_tokens = 48_000 from a prior chunk. A subsequent
        // chunk reports a smaller total_input=24_000 (as would happen on a
        // `Partial` confidence turn where cache fields are absent, or on the
        // hypothetical delta-style `--resume` semantics). The watermark holds
        // at 48_000 — displayed pct must never slide backward.
        let event = StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({
                "message": {
                    "usage": {
                        "input_tokens": 24_000,
                        "output_tokens": 0
                    }
                }
            }),
        };
        let usage = extract_token_usage(&event).expect("usage parses");
        let mut live_input_tokens: u64 = 48_000;
        live_input_tokens = live_input_tokens.max(usage.total_input);
        assert_eq!(
            live_input_tokens, 48_000,
            "High-watermark must hold against a smaller subsequent report \
             (proves --resume delta interpretation does not regress pct)"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn context_usage_bus_payload_carries_the_exact_resolved_budget() {
        let event_bus = Arc::new(EventBus::new(1));
        let mut events = event_bus.subscribe();
        let session_id = Uuid::new_v4();
        let budget = rsi_common::ResolvedContextBudget::new(
            258_400,
            rsi_common::ContextCapacity {
                provider_default_tokens: Some(272_000),
                provider_max_tokens: Some(872_000),
                effective_percent: Some(95),
                runtime_effective_tokens: Some(258_400),
                ..rsi_common::ContextCapacity::default()
            },
            rsi_common::CapabilityEvidence {
                source: rsi_common::CapabilitySource::RuntimeTelemetry,
                source_version: Some("codex-cli 0.155.1".to_string()),
                source_digest: Some(format!("sha256:{}", "a".repeat(64))),
                observed_at: Some(chrono::Utc::now()),
                confidence: rsi_common::CapabilityConfidence::Authoritative,
            },
        )
        .expect("positive runtime budget");

        publish_context_usage(
            &event_bus,
            session_id,
            9.0,
            34_000,
            500,
            34_500,
            ContextUsageConfidence::Full,
            budget.clone(),
        );

        let event = events.try_recv().expect("context event published");
        assert!(matches!(
            event.as_ref(),
            DaemonEvent::ContextUsageUpdated {
                session_id: actual_session_id,
                pct,
                input_tokens: 34_000,
                output_tokens: 500,
                daemon_total: 34_500,
                confidence: ContextUsageConfidence::Full,
                context_window: 258_400,
                resolved_context_budget: Some(actual_budget),
            } if *actual_session_id == session_id && *pct == 9.0 && actual_budget == &budget
        ));
        event_bus.unsubscribe();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_context_window_for_model() {
        // Opus 5's complete model ID is a single version component.
        assert_eq!(context_window_for_model("claude-opus-5"), 1_000_000);
        // Opus 4.6 has a 1M context window.
        assert_eq!(context_window_for_model("claude-opus-4-6"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-4.6"), 1_000_000);
        // The standard (200K) Opus 4.6 variant.
        assert_eq!(context_window_for_model("claude-opus-4-6-200k"), 200_000);
        assert_eq!(context_window_for_model("claude-opus-4.6-200k"), 200_000);
        assert_eq!(context_window_for_model("claude-sonnet-5"), 1_000_000);
        assert_eq!(context_window_for_model("claude-haiku-4"), 200_000);
        // F-130/V-001: the offered Fable model is answered by the canonical
        // catalog's exact-id lookup, which supplies its real 1M window
        // instead of the 128k default.
        assert_eq!(context_window_for_model("claude-fable-5-1"), 1_000_000);
        // The retired id is no longer catalogued, so it is answered by the
        // Claude-scoped retired-id map — still 1M, never the 128k default.
        assert_eq!(context_window_for_model("claude-fable-5"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-4-8"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-4-5"), 1_000_000);
        assert_eq!(
            context_window_for_model("claude-haiku-4-5-20251001"),
            200_000
        );
        // Every model the picker offers has a real window — none may fall
        // through to the unknown-model default.
        for spec in rsi_common::claude_catalog::CLAUDE_MODEL_CATALOG {
            assert_eq!(
                context_window_for_model(spec.id),
                spec.context_window,
                "{} must resolve to its catalogued window",
                spec.id
            );
        }
        assert_eq!(context_window_for_model("gpt-5.4"), 1_050_000);
        assert_eq!(context_window_for_model("gpt-5.4-mini"), 400_000);
        assert_eq!(context_window_for_model("gpt-5.3-codex"), 400_000);
        assert_eq!(context_window_for_model("gpt-5-codex"), 400_000);
        assert_eq!(context_window_for_model("gpt-4.1"), 1_047_576);
        assert_eq!(context_window_for_model("o4-mini"), 200_000);
        assert_eq!(context_window_for_model("gemini-2.5-pro"), 1_048_576);
        assert_eq!(
            context_window_for_model("gemini-3.1-pro-preview"),
            1_048_576
        );
        assert_eq!(context_window_for_model("unknown-model"), 128_000);
        // Local models. Qwen 3.6 must NOT fall through to the bare "qwen3"
        // 32k entry — that would fire /create_handoff at ~12% of the real
        // window. Ordering-sensitive: keep "qwen3.6" ahead of "qwen3".
        assert_eq!(context_window_for_model("qwen3.6:27b"), 262_144);
        assert_eq!(context_window_for_model("qwen3.6:35b-a3b"), 262_144);
        assert_eq!(context_window_for_model("qwen3:14b"), 32_768);
        assert_eq!(context_window_for_model("gemma4:12b"), 262_144);
        assert_eq!(context_window_for_model("gemma4:e4b"), 262_144);
        assert_eq!(context_window_for_model("gemma3:27b"), 128_000);
        // Opus 4.7 — explicit patterns
        assert_eq!(context_window_for_model("claude-opus-4-7"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-4.7"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-4-7-200k"), 200_000);
        assert_eq!(context_window_for_model("claude-opus-4.7-200k"), 200_000);
        // Opus 4 family fallback — future versions default to 1M
        assert_eq!(context_window_for_model("claude-opus-4-9"), 1_000_000);
        assert_eq!(
            context_window_for_model("claude-opus-4-10-experimental"),
            1_000_000
        );
        // Negative: legacy non-Opus-4 names still fall through to default
        assert_eq!(context_window_for_model("claude-3-opus-20240229"), 128_000);
    }

    /// V-020 regression gate. `system/init` announces `claude-opus-5[1m]` for a
    /// session launched with the bare catalog id, and `authoritative_model_update`
    /// accepts init's value unconditionally — so this string, not `claude-opus-5`,
    /// is what reaches the window lookup on the default path. Before the strip it
    /// matched neither the catalog (exact) nor any of the 61 substring patterns
    /// (there is no `opus-5` row), and resolved to 128_000 for a 1_000_000 window:
    /// a 7.8x overstatement of context fill, persisted from the first event.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_context_window_ignores_variant_suffix() {
        // The suffixed id and the bare id must agree.
        assert_eq!(context_window_for_model("claude-opus-5[1m]"), 1_000_000);
        assert_eq!(context_window_for_model("claude-opus-5"), 1_000_000);

        // Resolved via the catalog's exact-id path.
        assert_eq!(context_window_for_model("claude-fable-5-1[1m]"), 1_000_000);
        assert_eq!(
            context_window_for_model("claude-haiku-4-5-20251001[1m]"),
            200_000
        );

        // Resolved via the ordered substring table, which proves the strip runs
        // ahead of the `contains` loop and not just ahead of the catalog lookup.
        assert_eq!(
            context_window_for_model("claude-opus-4-7-200k[1m]"),
            200_000
        );
        assert_eq!(context_window_for_model("claude-opus-4-9[1m]"), 1_000_000);

        // Every catalogued model answers identically with and without the tag.
        for spec in rsi_common::claude_catalog::CLAUDE_MODEL_CATALOG {
            assert_eq!(
                context_window_for_model(&format!("{}[1m]", spec.id)),
                spec.context_window,
                "variant-suffixed {} disagreed with its catalog window",
                spec.id
            );
        }
    }

    /// Verbatim `result` payload observed from `claude 2.1.259` (V-022).
    /// Every value here was seen on the wire; nothing is invented.
    fn observed_result_event() -> StreamEvent {
        StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({
                "type": "result",
                "subtype": "success",
                "duration_ms": 4210,
                "total_cost_usd": 0.0421,
                "num_turns": 1,
                "stop_reason": "end_turn",
                "terminal_reason": "completed",
                "queued_turn_count": 0,
                "permission_denials": [],
                "api_error_status": serde_json::Value::Null,
                "subagent_stats": {
                    "spawned": 0,
                    "requested": {"background": 0, "foreground": 0, "unset": 0},
                    "started_in_background": 0,
                    "max_depth": 0,
                    "spawned_by_subagents": 0,
                    "completed": 0,
                    "failed": 0,
                    "killed": {"parent": 0, "user": 0, "system": 0},
                    "refused": {"depth_limit": 0, "concurrency_limit": 0, "budget": 0},
                    "by_type": {}
                },
                "usage": {
                    "input_tokens": 12,
                    "output_tokens": 34,
                    "cache_creation_input_tokens": 7411,
                    "cache_read_input_tokens": 0,
                    "service_tier": "standard",
                    "inference_geo": "not_available",
                    "speed": "standard",
                    "output_tokens_details": {"thinking_tokens": 0},
                    "cache_creation": {
                        "ephemeral_1h_input_tokens": 7411,
                        "ephemeral_5m_input_tokens": 0
                    }
                },
                "modelUsage": {
                    "claude-opus-5[1m]": {
                        "contextWindow": 1_000_000,
                        "canonicalModel": "claude-opus-5"
                    }
                }
            }),
        }
    }

    /// V99/P1-C. The counters below are the ones RSI previously discarded:
    /// the CLI reported ~12 usage fields and RSI persisted four of them.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn extract_result_metadata_captures_v99_usage_counters() {
        let meta = extract_result_metadata(&observed_result_event(), Some("claude-opus-5"));

        assert_eq!(meta.thinking_tokens, Some(0));
        assert_eq!(meta.service_tier.as_deref(), Some("standard"));
        assert_eq!(meta.cache_creation_1h_tokens, Some(7411));
        assert_eq!(meta.cache_creation_5m_tokens, Some(0));
        assert_eq!(meta.permission_denial_count, Some(0));
        assert_eq!(meta.queued_turn_count, Some(0));
        assert_eq!(meta.terminal_reason.as_deref(), Some("completed"));

        // subagent_stats is stored verbatim, so its nested content survives
        // round-tripping without a schema for it.
        let stats = meta.subagent_stats_json.expect("subagent_stats captured");
        let parsed: serde_json::Value =
            serde_json::from_str(&stats).expect("stored blob is valid JSON");
        assert_eq!(parsed["spawned"], 0);
        assert_eq!(parsed["killed"]["user"], 0);
        assert_eq!(parsed["refused"]["budget"], 0);
    }

    /// Pre-existing counters must be unchanged by the V99 additions: this
    /// phase is additive telemetry, not a reinterpretation of existing fields.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn extract_result_metadata_preserves_existing_counter_semantics() {
        let meta = extract_result_metadata(&observed_result_event(), Some("claude-opus-5"));

        assert_eq!(meta.duration_ms, Some(4210));
        assert_eq!(meta.cost_usd, Some(0.0421));
        assert_eq!(meta.num_turns, Some(1));
        assert_eq!(meta.context_window, Some(1_000_000));
        // input = input + cache_creation + cache_read
        assert_eq!(meta.final_input_tokens, Some(12 + 7411));
        assert_eq!(meta.final_output_tokens, Some(34));
    }

    /// A `result` with no `usage` key must leave every new field `None` rather
    /// than writing a synthetic zero — "unmeasured" and "measured as 0" are
    /// different facts and downstream analytics rely on the distinction.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn extract_result_metadata_without_usage_yields_none_not_zero() {
        let event = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({"subtype": "success", "num_turns": 1}),
        };
        let meta = extract_result_metadata(&event, Some("claude-opus-5"));

        assert_eq!(meta.thinking_tokens, None);
        assert_eq!(meta.service_tier, None);
        assert_eq!(meta.cache_creation_1h_tokens, None);
        assert_eq!(meta.cache_creation_5m_tokens, None);
        assert_eq!(meta.permission_denial_count, None);
        assert_eq!(meta.subagent_stats_json, None);
        assert_eq!(meta.queued_turn_count, None);
        assert_eq!(meta.terminal_reason, None);
    }

    /// The denial count is the array LENGTH; the element shape is unobserved
    /// and deliberately not modelled (P2-DENIALS).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn permission_denial_count_is_the_array_length() {
        let mut event = observed_result_event();
        event.data["permission_denials"] = serde_json::json!([
            {"tool_name": "Bash"},
            {"tool_name": "Write"},
            {"tool_name": "Edit"},
        ]);
        let meta = extract_result_metadata(&event, Some("claude-opus-5"));
        assert_eq!(meta.permission_denial_count, Some(3));
    }

    /// The per-turn path reads the same counters off the same `usage` object.
    /// The 1h/5m split is the point of the item: it is what makes an account's
    /// prompt-cache TTL transition observable rather than a mystery cost jump.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn turn_usage_captures_the_cache_ttl_split() {
        let event = StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({
                "message": {
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 50,
                        "cache_creation_input_tokens": 7411,
                        "cache_read_input_tokens": 20,
                        "service_tier": "standard",
                        "output_tokens_details": {"thinking_tokens": 640},
                        "cache_creation": {
                            "ephemeral_1h_input_tokens": 7411,
                            "ephemeral_5m_input_tokens": 0
                        }
                    }
                }
            }),
        };
        let usage = extract_token_usage(&event).expect("assistant usage parses");

        assert_eq!(usage.thinking, 640);
        assert_eq!(usage.cache_creation_1h, 7411);
        assert_eq!(usage.cache_creation_5m, 0);
        assert_eq!(usage.service_tier.as_deref(), Some("standard"));
        // Pre-existing fields unchanged.
        assert_eq!(usage.input, 100);
        assert_eq!(usage.output, 50);
        assert_eq!(usage.total_input, 100 + 7411 + 20);

        // And they reach the persisted turn row.
        let metric = build_turn_metric(Uuid::new_v4(), 1, &usage, &[], None);
        assert_eq!(metric.thinking_tokens, 640);
        assert_eq!(metric.cache_creation_1h_tokens, 7411);
        assert_eq!(metric.cache_creation_5m_tokens, 0);
        assert_eq!(metric.service_tier.as_deref(), Some("standard"));
    }

    /// A usage object with no cache/thinking detail must yield zeros, not a
    /// panic — older CLIs and other providers omit these sub-objects.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn turn_usage_without_detail_objects_is_zero_not_a_panic() {
        let event = StreamEvent {
            event_type: "assistant".to_string(),
            data: serde_json::json!({
                "message": {"usage": {"input_tokens": 10, "output_tokens": 5}}
            }),
        };
        let usage = extract_token_usage(&event).expect("assistant usage parses");
        assert_eq!(usage.thinking, 0);
        assert_eq!(usage.cache_creation_1h, 0);
        assert_eq!(usage.cache_creation_5m, 0);
        assert_eq!(usage.service_tier, None);
    }

    /// The strip must not turn a miss into a hit: an unrecognized model keeps
    /// the 128k fallback whether or not it carries a variant tag.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn test_variant_suffix_strip_preserves_fallback_behaviour() {
        assert_eq!(
            context_window_for_model("totally-unknown-model[1m]"),
            128_000
        );
        assert_eq!(context_window_for_model("totally-unknown-model"), 128_000);
        // A bare tag is not a model id and must not resolve to anything.
        assert_eq!(context_window_for_model("[1m]"), 128_000);
        // Bracketed but not trailing: left alone, still a miss.
        assert_eq!(context_window_for_model("weird[1m]model"), 128_000);
    }
}
