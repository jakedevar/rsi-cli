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

    Some(TokenUsage {
        input,
        cache_creation,
        cache_read,
        output,
        total_input: input + cache_creation + cache_read,
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

    let stop_reason = stream_event
        .data
        .get("subtype")
        .and_then(|v| v.as_str())
        .filter(|subtype| *subtype != "turn_completed")
        .map(String::from);

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
    let terminal_reason = stream_event
        .data
        .get("terminal_reason")
        .and_then(|v| v.as_str())
        .map(String::from);

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

    #[test]
    fn test_extract_token_usage_non_assistant_returns_none() {
        let event = StreamEvent {
            event_type: "tool_use".to_string(),
            data: serde_json::json!({}),
        };

        assert!(extract_token_usage(&event).is_none());
    }

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

    #[test]
    fn test_build_turn_metric() {
        let session_id = Uuid::new_v4();
        let usage = TokenUsage {
            input: 1000,
            cache_creation: 200,
            cache_read: 300,
            output: 500,
            total_input: 1500,
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

    #[test]
    fn test_build_turn_metric_no_tools() {
        let usage = TokenUsage {
            input: 100,
            cache_creation: 0,
            cache_read: 0,
            output: 50,
            total_input: 100,
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

    /// G-005/F-151: a multi-model `modelUsage` map must resolve to the
    /// SESSION's window, not to whichever entry the CLI serialized first.
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

    /// Older payloads carry no `canonicalModel`; the `<model>[variant]` key
    /// shape still resolves to the session's own entry.
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
