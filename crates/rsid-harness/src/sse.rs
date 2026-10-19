//! Server-Sent Events parser for streaming LLM responses.
//!
//! Handles two wire formats:
//! - OpenAI: `data: {json}` with `data: [DONE]` sentinel
//! - Anthropic: `event: TYPE\ndata: {json}` stateful pairs
//!
//! Extracted and generalized from openai.rs:411-552.

use crate::errors::ProviderError;
use crate::types::*;
use futures::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Accumulated state for tool call deltas during streaming.
#[derive(Debug, Clone, Default)]
pub struct AccumulatedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub hosted: bool,
    pub hosted_result: Option<serde_json::Value>,
    pub is_result: bool,
}

/// A provider-reported USD cost from a chat-completions `usage` object
/// (OpenRouter returns `usage.cost`). Only a finite, non-negative number
/// counts; anything else is "not reported", never an estimate.
pub fn reported_cost_usd(usage: &serde_json::Value) -> Option<f64> {
    usage
        .get("cost")
        .and_then(serde_json::Value::as_f64)
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

/// Fold a Chat Completions `usage` object into `usage` (#2253): the prompt and
/// completion counts, a provider-reported cost, and the nested token details
/// (`prompt_tokens_details.cached_tokens`,
/// `completion_tokens_details.reasoning_tokens`). `completion_tokens` already
/// counts reasoning tokens, and `prompt_tokens` already counts cached ones.
pub fn apply_chat_usage(usage: &mut TokenUsage, u: &serde_json::Value) {
    usage.prompt_tokens = u
        .get("prompt_tokens")
        .or_else(|| u.get("input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    usage.completion_tokens += u
        .get("completion_tokens")
        .or_else(|| u.get("output_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
    usage.cost_usd = reported_cost_usd(u).or(usage.cost_usd);
    if let Some(cached) = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(serde_json::Value::as_u64)
    {
        usage.cache_read_tokens = cached;
    }
    if let Some(reasoning) = u
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(serde_json::Value::as_u64)
    {
        usage.reasoning_tokens = reasoning;
    }
}

/// A provider's top-level `warning` text (Inception reports a reset
/// parameter this way), if the chunk carries a non-empty one (#2253).
pub fn chat_warning(json: &serde_json::Value) -> Option<String> {
    let text = match json.get("warning")? {
        serde_json::Value::String(text) => text.trim().to_string(),
        serde_json::Value::Null => return None,
        other => other.to_string(),
    };
    (!text.is_empty()).then_some(text)
}

/// Fold one streamed tool-call `name` fragment into the accumulated name
/// (#2253). Providers stream the name in two shapes and the Inception docs do
/// not say which Mercury uses, so both are handled: a name split across chunks
/// (`"get_"`, `"weather"`) is appended, and a name repeated in every chunk
/// (`"get_weather"`, `"get_weather"`) or sent as a growing prefix
/// (`"get_"`, `"get_weather"`) is never doubled.
pub fn merge_tool_name(accumulated: &mut String, fragment: &str) {
    if fragment.is_empty() || accumulated.as_str() == fragment {
        return;
    }
    if fragment.starts_with(accumulated.as_str()) {
        // Empty accumulator, or a cumulative name that grew.
        *accumulated = fragment.to_string();
    } else {
        accumulated.push_str(fragment);
    }
}

/// The payload of one SSE `data` line, with or without the space after the
/// colon (`data: {..}` and `data:{..}` are both valid SSE), or `None` for any
/// other line (#2253).
fn sse_data_payload(line: &str) -> Option<&str> {
    line.strip_prefix("data:").map(str::trim)
}

/// Read an OpenAI-format SSE stream, emitting chunks and accumulating the response.
///
/// Extracted from `openai.rs::read_sse_stream()` with generalized types.
pub async fn read_openai_sse_stream<S, B, E>(
    byte_stream: S,
    chunk_tx: &mpsc::Sender<StreamChunk>,
    cancel: &CancellationToken,
) -> Result<(String, Vec<AccumulatedToolCall>, TokenUsage, Option<String>), ProviderError>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    let mut content = String::new();
    let mut tool_calls: Vec<AccumulatedToolCall> = Vec::new();
    let mut finish_reason: Option<String> = None;
    let mut usage = TokenUsage::default();
    let mut line_buf = String::new();
    let mut complete = false;
    let mut last_warning: Option<String> = None;

    let mut byte_stream = std::pin::pin!(byte_stream);

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                return Ok((content, tool_calls, usage, Some("cancelled".into())));
            }
            chunk = byte_stream.next() => chunk,
        };

        let chunk = match chunk {
            Some(Ok(bytes)) => bytes,
            Some(Err(_)) => {
                return Err(stream_disconnect());
            }
            None => break,
        };

        line_buf.push_str(&String::from_utf8_lossy(chunk.as_ref()));

        while let Some(newline_pos) = line_buf.find('\n') {
            let line = line_buf[..newline_pos].trim().to_string();
            line_buf.drain(..=newline_pos);

            if line.is_empty() || line.starts_with(':') {
                continue;
            }

            let Some(data) = sse_data_payload(&line) else {
                continue;
            };

            if data == "[DONE]" {
                complete = true;
                break;
            }

            let json: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // An upstream failure reported inside the stream (OpenRouter sends
            // one when the routed provider dies mid-response, #3163).
            if let Some(error) = json.get("error").filter(|error| !error.is_null()) {
                return Err(stream_error_event(error));
            }

            // A provider notice, such as a reset parameter (#2253).
            if let Some(warning) = chat_warning(&json)
                && last_warning.as_deref() != Some(warning.as_str())
            {
                tracing::warn!(%warning, "chat completions provider warning");
                last_warning = Some(warning.clone());
                let _ = chunk_tx
                    .send(StreamChunk {
                        delta_text: String::new(),
                        tool_call_deltas: Vec::new(),
                        is_final: false,
                        usage: None,
                        stop_reason: None,
                        warning: Some(warning),
                    })
                    .await;
            }

            // Extract usage from final chunk
            if let Some(u) = json.get("usage").filter(|u| !u.is_null()) {
                apply_chat_usage(&mut usage, u);
            }

            let Some(choice) = json
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|c| c.first())
            else {
                continue;
            };

            if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                finish_reason = Some(fr.to_string());
            }

            let Some(delta) = choice.get("delta") else {
                continue;
            };

            // Text content
            if let Some(text) = delta.get("content").and_then(|v| v.as_str())
                && !text.is_empty()
            {
                content.push_str(text);
                let _ = chunk_tx
                    .send(StreamChunk {
                        delta_text: text.to_string(),
                        tool_call_deltas: Vec::new(),
                        is_final: false,
                        usage: None,
                        stop_reason: None,
                        warning: None,
                    })
                    .await;
            }

            // Tool call deltas
            if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                let mut deltas = Vec::new();
                for tc_delta in tcs {
                    let idx = tc_delta.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    while tool_calls.len() <= idx {
                        tool_calls.push(AccumulatedToolCall::default());
                    }
                    let mut delta = ToolCallDelta {
                        index: idx,
                        id: None,
                        name: None,
                        arguments_delta: String::new(),
                    };
                    if let Some(id) = tc_delta.get("id").and_then(|v| v.as_str()) {
                        tool_calls[idx].id = id.to_string();
                        delta.id = Some(id.to_string());
                    }
                    if let Some(func) = tc_delta.get("function") {
                        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                            merge_tool_name(&mut tool_calls[idx].name, name);
                            delta.name = Some(name.to_string());
                        }
                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                            tool_calls[idx].arguments.push_str(args);
                            delta.arguments_delta = args.to_string();
                        }
                    }
                    deltas.push(delta);
                }
                if !deltas.is_empty() {
                    let _ = chunk_tx
                        .send(StreamChunk {
                            delta_text: String::new(),
                            tool_call_deltas: deltas,
                            is_final: false,
                            usage: None,
                            stop_reason: None,
                            warning: None,
                        })
                        .await;
                }
            }
        }
    }

    if !complete && finish_reason.is_none() {
        return Err(stream_disconnect());
    }
    Ok((content, tool_calls, usage, finish_reason))
}

/// Anthropic Messages streaming state shared by the SSE reader (direct API)
/// and the Bedrock event-stream reader. Events arrive in order:
/// - `message_start`: contains model, usage.input_tokens
/// - `content_block_start`: block type (text, tool_use, hosted server tools)
/// - `content_block_delta`: incremental text or tool input JSON
/// - `content_block_stop`: end of block
/// - `message_delta`: stop_reason, usage.output_tokens
/// - `message_stop`: end of message
#[derive(Default)]
pub struct AnthropicStreamState {
    content: String,
    tool_calls: Vec<AccumulatedToolCall>,
    finish_reason: Option<String>,
    usage: TokenUsage,
    complete: bool,
    // Which tool call index a delta extends.
    current_tool_idx: Option<usize>,
}

pub type AnthropicStreamResult = (String, Vec<AccumulatedToolCall>, TokenUsage, Option<String>);

impl AnthropicStreamState {
    /// Apply one decoded stream event.
    pub async fn apply(
        &mut self,
        event_type: &str,
        json: &serde_json::Value,
        chunk_tx: &mpsc::Sender<StreamChunk>,
    ) {
        match event_type {
            "message_start" => {
                // Extract input token usage
                if let Some(u) = json.get("message").and_then(|m| m.get("usage")) {
                    self.usage.prompt_tokens =
                        u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    self.usage.cache_creation_tokens = u
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    self.usage.cache_read_tokens = u
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }
            }
            "content_block_start" => {
                let block = json.get("content_block");
                let block_type = block
                    .and_then(|b| b.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if matches!(block_type, "tool_use" | "server_tool_use") {
                    let field = |name: &str| {
                        block
                            .and_then(|b| b.get(name))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    };
                    let arguments = block
                        .and_then(|b| b.get("input"))
                        .map(serde_json::Value::to_string)
                        .unwrap_or_default();
                    self.tool_calls.push(AccumulatedToolCall {
                        id: field("id"),
                        name: field("name"),
                        arguments,
                        hosted: block_type == "server_tool_use",
                        hosted_result: None,
                        is_result: false,
                    });
                    self.current_tool_idx = Some(self.tool_calls.len() - 1);
                } else if matches!(
                    block_type,
                    "web_search_tool_result" | "web_fetch_tool_result"
                ) {
                    let block = block.cloned().unwrap_or_default();
                    let id = block
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    self.tool_calls.push(AccumulatedToolCall {
                        id,
                        name: String::new(),
                        arguments: String::new(),
                        hosted: true,
                        hosted_result: Some(block),
                        is_result: true,
                    });
                    self.current_tool_idx = None;
                } else {
                    self.current_tool_idx = None;
                }
            }
            "content_block_delta" => {
                let delta = json.get("delta");
                let delta_type = delta
                    .and_then(|d| d.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                match delta_type {
                    "text_delta" => {
                        if let Some(text) =
                            delta.and_then(|d| d.get("text")).and_then(|t| t.as_str())
                        {
                            self.content.push_str(text);
                            let _ = chunk_tx
                                .send(StreamChunk {
                                    delta_text: text.to_string(),
                                    tool_call_deltas: Vec::new(),
                                    is_final: false,
                                    usage: None,
                                    stop_reason: None,
                                    warning: None,
                                })
                                .await;
                        }
                    }
                    "input_json_delta" => {
                        if let Some(idx) = self.current_tool_idx
                            && let Some(partial) = delta
                                .and_then(|d| d.get("partial_json"))
                                .and_then(|p| p.as_str())
                            && let Some(tc) = self.tool_calls.get_mut(idx)
                        {
                            tc.arguments.push_str(partial);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                self.current_tool_idx = None;
            }
            "message_delta" => {
                if let Some(sr) = json
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    self.finish_reason = Some(sr.to_string());
                }
                if let Some(u) = json.get("usage") {
                    self.usage.completion_tokens =
                        u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                }
            }
            "message_stop" => {
                self.complete = true;
            }
            _ => {}
        }
    }

    /// The partial result returned when the caller cancels mid-stream.
    pub fn cancelled(self) -> AnthropicStreamResult {
        (
            self.content,
            self.tool_calls,
            self.usage,
            Some("cancelled".into()),
        )
    }

    /// Final result once the transport ends; a stream that ended before
    /// `message_stop` or a stop reason is a transient disconnect.
    pub fn finish(mut self) -> Result<AnthropicStreamResult, ProviderError> {
        self.usage.total_tokens = self.usage.prompt_tokens
            + self.usage.completion_tokens
            + self.usage.cache_creation_tokens
            + self.usage.cache_read_tokens;
        if !self.complete && self.finish_reason.is_none() {
            return Err(stream_disconnect());
        }
        Ok((
            self.content,
            self.tool_calls,
            self.usage,
            self.finish_reason,
        ))
    }
}

/// Read an Anthropic-format SSE stream (`event: TYPE` + `data: {json}` pairs).
pub async fn read_anthropic_sse_stream<S, B, E>(
    byte_stream: S,
    chunk_tx: &mpsc::Sender<StreamChunk>,
    cancel: &CancellationToken,
) -> Result<AnthropicStreamResult, ProviderError>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    let mut state = AnthropicStreamState::default();
    let mut line_buf = String::new();
    let mut current_event_type = String::new();
    let mut byte_stream = std::pin::pin!(byte_stream);

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Ok(state.cancelled()),
            chunk = byte_stream.next() => chunk,
        };

        let chunk = match chunk {
            Some(Ok(bytes)) => bytes,
            Some(Err(_)) => {
                return Err(stream_disconnect());
            }
            None => break,
        };

        line_buf.push_str(&String::from_utf8_lossy(chunk.as_ref()));

        while let Some(newline_pos) = line_buf.find('\n') {
            let line = line_buf[..newline_pos].trim().to_string();
            line_buf.drain(..=newline_pos);

            if line.is_empty() {
                continue;
            }

            // Track event type
            if let Some(et) = line.strip_prefix("event: ") {
                current_event_type = et.trim().to_string();
                continue;
            }

            let data = match line.strip_prefix("data: ") {
                Some(d) => d.trim(),
                None => continue,
            };

            let json: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // An `error` event (for example `overloaded_error`) is an
            // upstream failure, not a stream to keep reading (#3163).
            if current_event_type == "error"
                || json.get("type").and_then(|t| t.as_str()) == Some("error")
            {
                return Err(stream_error_event(json.get("error").unwrap_or(&json)));
            }
            state.apply(&current_event_type, &json, chunk_tx).await;
        }
    }

    state.finish()
}

pub fn stream_disconnect() -> ProviderError {
    ProviderError::transient("stream_disconnected")
}

/// Classify an error object a provider sent inside an otherwise healthy stream
/// (`{"error": {"code": 502 | "server_error", "type": .., "message": ..}}`).
/// A numeric code reads as the HTTP status it names; a name reads by its
/// wording. An error that says nothing recognisable is treated as a transient
/// upstream failure: the step is retried a bounded number of times.
pub fn stream_error_event(error: &serde_json::Value) -> ProviderError {
    use crate::errors::ProviderErrorClass as Class;
    let field = |name: &str| {
        error
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    let message: String = field("message").chars().take(4096).collect();
    if let Some(status) = error
        .get("code")
        .and_then(serde_json::Value::as_u64)
        .and_then(|code| u16::try_from(code).ok())
    {
        let mut classified = crate::errors::from_http(status, &message, None);
        if classified.class == Class::BadRequest && matches!(status, 408 | 409 | 425) {
            classified.class = Class::Transient;
            classified.detail_code = "stream_error".into();
        }
        classified.detail_code = format!("stream_{}", classified.detail_code);
        return classified;
    }
    let name = format!("{} {} {message}", field("type"), field("code"));
    let has = |terms: &[&str]| terms.iter().any(|term| name.contains(term));
    let (class, detail) = if has(&["rate_limit", "too_many_requests"]) {
        (Class::RateLimited, "stream_rate_limited")
    } else if has(&["overloaded"]) {
        (Class::Overloaded, "stream_overloaded")
    } else if has(&["context_length", "context window", "context length"]) {
        (Class::ContextTooLong, "stream_context_too_long")
    } else if has(&["authentication", "invalid_api_key", "permission"]) {
        (Class::Auth, "stream_auth_failed")
    } else if has(&["insufficient_quota", "credit", "billing"]) {
        (Class::CreditExhausted, "stream_credit_exhausted")
    } else if has(&["invalid_request"]) {
        (Class::BadRequest, "stream_request_rejected")
    } else {
        (Class::Transient, "stream_error")
    };
    ProviderError {
        class,
        http_status: None,
        retry_after_ms: None,
        detail_code: detail.into(),
    }
}

/// Filter that strips `<think>...</think>` blocks from streaming text.
///
/// Operates char-by-char on delta text, maintaining state across chunks.
/// Used for compatible providers that emit thinking blocks inline.
pub struct ThinkStripFilter {
    inside_think: bool,
    partial_tag: String,
}

impl ThinkStripFilter {
    pub fn new() -> Self {
        Self {
            inside_think: false,
            partial_tag: String::new(),
        }
    }

    /// Process a text delta, returning the filtered text.
    pub fn filter(&mut self, text: &str) -> String {
        let mut output = String::new();
        for ch in text.chars() {
            self.partial_tag.push(ch);

            if self.inside_think {
                if self.partial_tag.ends_with("</think>") {
                    self.inside_think = false;
                    self.partial_tag.clear();
                } else if self.partial_tag.len() > 20 {
                    // If partial_tag gets too long without matching, keep consuming
                    self.partial_tag.drain(..self.partial_tag.len() - 8);
                }
            } else if self.partial_tag.ends_with("<think>") {
                self.inside_think = true;
                self.partial_tag.clear();
            } else if !("<think>".starts_with(&self.partial_tag)) {
                // partial_tag can't become "<think>", flush it
                output.push_str(&self.partial_tag);
                self.partial_tag.clear();
            }
        }
        output
    }
}

impl Default for ThinkStripFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_usage_cost_is_read_verbatim_and_absent_stays_none() {
        let usage =
            serde_json::json!({"prompt_tokens": 10, "completion_tokens": 5, "cost": 0.00042});
        assert_eq!(reported_cost_usd(&usage), Some(0.00042));
        let no_cost = serde_json::json!({"prompt_tokens": 10, "completion_tokens": 5});
        assert_eq!(reported_cost_usd(&no_cost), None);
        let bad = serde_json::json!({"cost": -1.0});
        assert_eq!(reported_cost_usd(&bad), None);
        let null = serde_json::json!({"cost": null});
        assert_eq!(reported_cost_usd(&null), None);
    }

    /// #2253: a tool name streamed as fragments, repeated whole in every
    /// chunk, or grown as a prefix always ends as the one name.
    #[test]
    fn streamed_tool_names_accumulate_in_both_delta_shapes() {
        fn fold(fragments: &[&str]) -> String {
            let mut name = String::new();
            for fragment in fragments {
                merge_tool_name(&mut name, fragment);
            }
            name
        }
        // Shape A: the name is split across chunks.
        assert_eq!(fold(&["get_", "weather"]), "get_weather");
        assert_eq!(fold(&["mcp__docs", "__echo"]), "mcp__docs__echo");
        assert_eq!(fold(&["a", "b", "c"]), "abc");
        // Shape B: the full name rides in every chunk.
        assert_eq!(fold(&["get_weather"]), "get_weather");
        assert_eq!(
            fold(&["get_weather", "get_weather", "get_weather"]),
            "get_weather"
        );
        // A cumulative name that grows is replaced, not doubled.
        assert_eq!(fold(&["get_", "get_weather"]), "get_weather");
        // An empty fragment changes nothing.
        assert_eq!(fold(&["get_weather", ""]), "get_weather");
        assert_eq!(fold(&["", "get_weather"]), "get_weather");
    }

    /// #2253: Chat Completions usage details fill the cached and reasoning
    /// counts; a plain usage object leaves them zero.
    #[test]
    fn chat_usage_details_are_parsed_into_the_settlement_counts() {
        let mut usage = TokenUsage::default();
        apply_chat_usage(
            &mut usage,
            &serde_json::json!({
                "prompt_tokens": 120,
                "completion_tokens": 30,
                "total_tokens": 150,
                "prompt_tokens_details": {"cached_tokens": 64},
                "completion_tokens_details": {"reasoning_tokens": 12}
            }),
        );
        assert_eq!(usage.prompt_tokens, 120);
        assert_eq!(usage.completion_tokens, 30);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.cache_read_tokens, 64);
        assert_eq!(usage.reasoning_tokens, 12);

        let mut plain = TokenUsage::default();
        apply_chat_usage(
            &mut plain,
            &serde_json::json!({"prompt_tokens": 10, "completion_tokens": 5, "cost": 0.5}),
        );
        assert_eq!(plain.cache_read_tokens, 0);
        assert_eq!(plain.reasoning_tokens, 0);
        assert_eq!(plain.cost_usd, Some(0.5));
        // Null details (some servers send them) are ignored.
        let mut nulls = TokenUsage::default();
        apply_chat_usage(
            &mut nulls,
            &serde_json::json!({
                "prompt_tokens": 1, "completion_tokens": 1,
                "prompt_tokens_details": null, "completion_tokens_details": null
            }),
        );
        assert_eq!(nulls.cache_read_tokens, 0);
    }

    #[test]
    fn a_top_level_warning_is_read_and_an_empty_one_is_not() {
        assert_eq!(
            chat_warning(&serde_json::json!({"warning": " temperature reset "})),
            Some("temperature reset".to_string())
        );
        assert_eq!(chat_warning(&serde_json::json!({"warning": ""})), None);
        assert_eq!(chat_warning(&serde_json::json!({"warning": null})), None);
        assert_eq!(chat_warning(&serde_json::json!({"choices": []})), None);
        assert_eq!(
            chat_warning(&serde_json::json!({"warning": {"code": "temperature_reset"}})),
            Some(r#"{"code":"temperature_reset"}"#.to_string())
        );
    }

    /// #2253: `data:{..}` and `data: {..}` are both SSE data lines.
    #[test]
    fn sse_data_lines_parse_with_or_without_the_space() {
        assert_eq!(sse_data_payload("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(sse_data_payload("data:{\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(sse_data_payload("data:   [DONE]"), Some("[DONE]"));
        assert_eq!(sse_data_payload("event: message"), None);
        assert_eq!(sse_data_payload(": keep-alive"), None);
    }

    #[test]
    fn test_think_strip_filter_no_tags() {
        let mut f = ThinkStripFilter::new();
        assert_eq!(f.filter("hello world"), "hello world");
    }

    #[test]
    fn test_think_strip_filter_strips_block() {
        let mut f = ThinkStripFilter::new();
        let result = f.filter("before<think>hidden</think>after");
        assert!(result.contains("before"));
        assert!(result.contains("after"));
        assert!(!result.contains("hidden"));
    }

    #[test]
    fn test_think_strip_filter_across_chunks() {
        let mut f = ThinkStripFilter::new();
        let r1 = f.filter("pre<think>th");
        let r2 = f.filter("ink</think>post");
        let combined = r1 + &r2;
        assert!(combined.contains("pre"));
        assert!(combined.contains("post"));
        assert!(!combined.contains("think"));
    }

    type Pieces = futures::stream::Iter<std::vec::IntoIter<Result<Vec<u8>, std::io::Error>>>;

    fn byte_stream(pieces: &[&str]) -> Pieces {
        futures::stream::iter(
            pieces
                .iter()
                .map(|piece| Ok(piece.as_bytes().to_vec()))
                .collect::<Vec<_>>(),
        )
    }

    #[tokio::test]
    async fn an_openrouter_error_chunk_mid_stream_is_a_retryable_upstream_failure() {
        let (tx, mut rx) = mpsc::channel(8);
        let stream = byte_stream(&[
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par\"}}]}\n\n",
            "data: {\"error\":{\"code\":502,\"message\":\"upstream died\"},\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"error\"}]}\n\n",
        ]);
        let error = read_openai_sse_stream(stream, &tx, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.class, crate::errors::ProviderErrorClass::Transient);
        assert!(error.retryable());
        assert_eq!(error.detail_code, "stream_server_error");
        assert_eq!(rx.recv().await.unwrap().delta_text, "par");
    }

    #[tokio::test]
    async fn an_anthropic_overloaded_event_is_an_overloaded_failure() {
        let (tx, _rx) = mpsc::channel(8);
        let stream = byte_stream(&[
            "event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        ]);
        let error = read_anthropic_sse_stream(stream, &tx, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.class, crate::errors::ProviderErrorClass::Overloaded);
        assert!(error.retryable());
    }

    #[test]
    fn stream_error_events_classify_by_status_or_wording() {
        use crate::errors::ProviderErrorClass as Class;
        let class = |value: serde_json::Value| stream_error_event(&value).class;
        assert_eq!(class(serde_json::json!({"code": 429})), Class::RateLimited);
        assert_eq!(class(serde_json::json!({"code": 503})), Class::Overloaded);
        assert_eq!(class(serde_json::json!({"code": 408})), Class::Transient);
        assert_eq!(class(serde_json::json!({"code": 400})), Class::BadRequest);
        assert_eq!(class(serde_json::json!({"code": 401})), Class::Auth);
        assert_eq!(
            class(serde_json::json!({"type": "rate_limit_error"})),
            Class::RateLimited
        );
        assert_eq!(
            class(serde_json::json!({"type": "invalid_request_error"})),
            Class::BadRequest
        );
        assert_eq!(
            class(serde_json::json!({"message": "something odd"})),
            Class::Transient
        );
    }
}
