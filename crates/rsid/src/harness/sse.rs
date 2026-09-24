//! Server-Sent Events parser for streaming LLM responses.
//!
//! Handles two wire formats:
//! - OpenAI: `data: {json}` with `data: [DONE]` sentinel
//! - Anthropic: `event: TYPE\ndata: {json}` stateful pairs
//!
//! Extracted and generalized from openai.rs:411-552.

use super::types::*;
use crate::error::DaemonError;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Accumulated state for tool call deltas during streaming.
#[derive(Debug, Clone, Default)]
pub struct AccumulatedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Read an OpenAI-format SSE stream, emitting chunks and accumulating the response.
///
/// Extracted from `openai.rs::read_sse_stream()` with generalized types.
pub async fn read_openai_sse_stream(
    resp: reqwest::Response,
    chunk_tx: &mpsc::Sender<StreamChunk>,
    cancel: &CancellationToken,
) -> Result<(String, Vec<AccumulatedToolCall>, TokenUsage, Option<String>), DaemonError> {
    let mut content = String::new();
    let mut tool_calls: Vec<AccumulatedToolCall> = Vec::new();
    let mut finish_reason: Option<String> = None;
    let mut usage = TokenUsage::default();
    let mut line_buf = String::new();

    let mut byte_stream = resp.bytes_stream();

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                return Ok((content, tool_calls, usage, Some("cancelled".into())));
            }
            chunk = byte_stream.next() => chunk,
        };

        let chunk = match chunk {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => {
                return Err(DaemonError::Process(format!("Stream read error: {e}")));
            }
            None => break,
        };

        line_buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(newline_pos) = line_buf.find('\n') {
            let line = line_buf[..newline_pos].trim().to_string();
            line_buf.drain(..=newline_pos);

            if line.is_empty() || line.starts_with(':') {
                continue;
            }

            let data = match line.strip_prefix("data: ") {
                Some(d) => d.trim(),
                None => continue,
            };

            if data == "[DONE]" {
                break;
            }

            let json: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Extract usage from final chunk
            if let Some(u) = json.get("usage").filter(|u| !u.is_null()) {
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
                            tool_calls[idx].name = name.to_string();
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
                        })
                        .await;
                }
            }
        }
    }

    Ok((content, tool_calls, usage, finish_reason))
}

/// Read an Anthropic-format SSE stream.
///
/// Anthropic uses stateful `event: TYPE` + `data: {json}` pairs:
/// - `message_start`: contains model, usage.input_tokens
/// - `content_block_start`: block type (text, tool_use)
/// - `content_block_delta`: incremental text or tool input JSON
/// - `content_block_stop`: end of block
/// - `message_delta`: stop_reason, usage.output_tokens
/// - `message_stop`: end of message
pub async fn read_anthropic_sse_stream(
    resp: reqwest::Response,
    chunk_tx: &mpsc::Sender<StreamChunk>,
    cancel: &CancellationToken,
) -> Result<(String, Vec<AccumulatedToolCall>, TokenUsage, Option<String>), DaemonError> {
    let mut content = String::new();
    let mut tool_calls: Vec<AccumulatedToolCall> = Vec::new();
    let mut finish_reason: Option<String> = None;
    let mut usage = TokenUsage::default();
    let mut line_buf = String::new();
    let mut current_event_type = String::new();
    // Track which tool call index we're building
    let mut current_tool_idx: Option<usize> = None;

    let mut byte_stream = resp.bytes_stream();

    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                return Ok((content, tool_calls, usage, Some("cancelled".into())));
            }
            chunk = byte_stream.next() => chunk,
        };

        let chunk = match chunk {
            Some(Ok(bytes)) => bytes,
            Some(Err(e)) => {
                return Err(DaemonError::Process(format!("Stream read error: {e}")));
            }
            None => break,
        };

        line_buf.push_str(&String::from_utf8_lossy(&chunk));

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

            match current_event_type.as_str() {
                "message_start" => {
                    // Extract input token usage
                    if let Some(u) = json.get("message").and_then(|m| m.get("usage")) {
                        usage.prompt_tokens =
                            u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        usage.cache_creation_tokens = u
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        usage.cache_read_tokens = u
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }
                }
                "content_block_start" => {
                    let block_type = json
                        .get("content_block")
                        .and_then(|b| b.get("type"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    if block_type == "tool_use" {
                        let id = json
                            .get("content_block")
                            .and_then(|b| b.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = json
                            .get("content_block")
                            .and_then(|b| b.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        tool_calls.push(AccumulatedToolCall {
                            id,
                            name,
                            arguments: String::new(),
                        });
                        current_tool_idx = Some(tool_calls.len() - 1);
                    } else {
                        current_tool_idx = None;
                    }
                }
                "content_block_delta" => {
                    let delta_type = json
                        .get("delta")
                        .and_then(|d| d.get("type"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");

                    match delta_type {
                        "text_delta" => {
                            if let Some(text) = json
                                .get("delta")
                                .and_then(|d| d.get("text"))
                                .and_then(|t| t.as_str())
                            {
                                content.push_str(text);
                                let _ = chunk_tx
                                    .send(StreamChunk {
                                        delta_text: text.to_string(),
                                        tool_call_deltas: Vec::new(),
                                        is_final: false,
                                        usage: None,
                                        stop_reason: None,
                                    })
                                    .await;
                            }
                        }
                        "input_json_delta" => {
                            if let Some(idx) = current_tool_idx
                                && let Some(partial) = json
                                    .get("delta")
                                    .and_then(|d| d.get("partial_json"))
                                    .and_then(|p| p.as_str())
                                && let Some(tc) = tool_calls.get_mut(idx)
                            {
                                tc.arguments.push_str(partial);
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    current_tool_idx = None;
                }
                "message_delta" => {
                    if let Some(sr) = json
                        .get("delta")
                        .and_then(|d| d.get("stop_reason"))
                        .and_then(|s| s.as_str())
                    {
                        finish_reason = Some(sr.to_string());
                    }
                    if let Some(u) = json.get("usage") {
                        usage.completion_tokens =
                            u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    }
                }
                "message_stop" => {
                    // Stream complete
                }
                _ => {}
            }
        }
    }

    usage.total_tokens = usage.prompt_tokens
        + usage.completion_tokens
        + usage.cache_creation_tokens
        + usage.cache_read_tokens;

    Ok((content, tool_calls, usage, finish_reason))
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
mod tests {
    use super::*;

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
}
