//! `OpenAI` Responses wire transformations for Bedrock's bearer-authenticated endpoint:
//! request body, response objects and the SSE stream reader.
//!
//! The daemon adapter owns the credential, the URL and the egress admission; the reader
//! here takes any byte stream.

use crate::errors::{ProviderError, from_http};
use crate::types::{
    ChatRequest, ChatResponse, HarnessToolSpecKind, MessageRole, StreamChunk, TokenUsage, ToolCall,
    ToolContentBlock,
};
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A Responses stream failure: a classified provider error or a plain process message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponsesError {
    Provider(ProviderError),
    Process(String),
}

impl From<ProviderError> for ResponsesError {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

/// Build the Responses request body.
pub fn request_body(request: &ChatRequest) -> Value {
    let mut input = Vec::new();
    let custom_call_ids: std::collections::HashSet<String> = request
        .messages
        .iter()
        .flat_map(|message| message.tool_calls.iter())
        .filter(|call| call.name == "apply_patch")
        .map(|call| call.id.clone())
        .collect();
    let hosted_call_ids: std::collections::HashSet<String> = request
        .messages
        .iter()
        .flat_map(|message| message.tool_calls.iter())
        .filter(|call| call.hosted)
        .map(|call| call.id.clone())
        .collect();
    for message in &request.messages {
        match message.role {
            MessageRole::System | MessageRole::User => input.push(json!({
                "role": if message.role == MessageRole::System { "system" } else { "user" },
                "content": message.content,
            })),
            MessageRole::Assistant => {
                if !message.content.is_empty() {
                    input.push(json!({"role":"assistant", "content":message.content}));
                }
                for call in &message.tool_calls {
                    if call.hosted {
                        let item = call
                            .hosted_result
                            .clone()
                            .unwrap_or_else(|| json!({"type":"web_search_call", "id":call.id}));
                        input.push(item);
                        continue;
                    }
                    if custom_call_ids.contains(&call.id) {
                        let patch = serde_json::from_str::<Value>(&call.arguments)
                            .ok()
                            .and_then(|value| value.as_str().map(str::to_owned))
                            .unwrap_or_else(|| call.arguments.clone());
                        input.push(json!({
                            "type":"custom_tool_call", "call_id":call.id,
                            "name":call.name, "input":patch,
                        }));
                    } else {
                        input.push(json!({
                            "type":"function_call", "call_id":call.id,
                            "name":call.name, "arguments":call.arguments,
                        }));
                    }
                }
            }
            MessageRole::Tool => {
                let call_id = message.tool_call_id.as_deref().unwrap_or("");
                if hosted_call_ids.contains(call_id) {
                    continue;
                }
                let blocks = message.tool_blocks();
                let text = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ToolContentBlock::Text { text } => Some(text.as_str()),
                        ToolContentBlock::ServerToolResult { .. } => None,
                        ToolContentBlock::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if custom_call_ids.contains(call_id) {
                    input.push(json!({
                        "type":"custom_tool_call_output",
                        "call_id":call_id,
                        "output":text,
                    }));
                } else {
                    let output = if message.is_error {
                        json!({"error":{"type":"tool_execution","message":text}})
                            .to_string()
                            .into()
                    } else if blocks
                        .iter()
                        .any(|block| matches!(block, ToolContentBlock::Image { .. }))
                    {
                        Value::Array(
                            blocks
                                .iter()
                                .map(|block| match block {
                                    ToolContentBlock::Text { text } => {
                                        json!({"type":"input_text", "text":text})
                                    }
                                    ToolContentBlock::ServerToolResult { .. } => Value::Null,
                                    ToolContentBlock::Image {
                                        media_type,
                                        data,
                                        detail,
                                        ..
                                    } => {
                                        let mut image = json!({
                                            "type":"input_image",
                                            "image_url":format!("data:{media_type};base64,{data}"),
                                        });
                                        if let Some(detail) = detail {
                                            image["detail"] = json!(detail);
                                        }
                                        image
                                    }
                                })
                                .collect(),
                        )
                    } else {
                        Value::String(text)
                    };
                    input.push(json!({
                        "type":"function_call_output",
                        "call_id":call_id,
                        "output":output,
                    }));
                }
            }
        }
    }
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            if matches!(tool.kind, HarnessToolSpecKind::ResponsesWebSearch) {
                json!({"type":"web_search"})
            } else if let Some(format) = &tool.freeform {
                json!({
                    "type":"custom", "name":tool.name, "description":tool.description,
                    "format":format,
                })
            } else {
                json!({
                    "type":"function", "name":tool.name, "description":tool.description,
                    "parameters":serde_json::from_str::<Value>(&tool.parameters_json)
                        .unwrap_or_else(|_| json!({"type":"object"})),
                })
            }
        })
        .filter(|tool| !tool.is_null())
        .collect();
    let mut body = json!({
        "model":request.model, "input":input, "stream":request.stream,
        "max_output_tokens":request.max_tokens.unwrap_or(8192),
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if let Some(effort) = &request.reasoning_effort {
        body["reasoning"] = json!({"effort":effort});
    }
    body
}

pub fn usage(value: &Value) -> TokenUsage {
    let input = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    TokenUsage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: value
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(input + output),
        cache_read_tokens: value
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_creation_tokens: value
            .get("cache_write_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: value
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cost_usd: None,
    }
}

/// Classify a Responses `error`/`response.failed` payload.
pub fn response_error(value: &Value) -> ProviderError {
    let error = value
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| value.get("error"))
        .unwrap_or(value);
    let status = error
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .unwrap_or_else(|| match error.get("code").and_then(Value::as_str) {
            Some("rate_limit_exceeded" | "rate_limit_error") => 429,
            Some("server_error" | "internal_error") => 500,
            Some("authentication_error" | "invalid_api_key") => 401,
            _ => 400,
        });
    from_http(status, &error.to_string(), None)
}

pub fn output_items(response: &Value) -> (String, Vec<ToolCall>) {
    let mut content = String::new();
    let mut calls = Vec::new();
    if let Some(items) = response.get("output").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                content.push_str(text);
                            }
                        }
                    }
                }
                Some("function_call") => {
                    if let (Some(id), Some(name)) = (
                        item.get("call_id").and_then(Value::as_str),
                        item.get("name").and_then(Value::as_str),
                    ) {
                        calls.push(ToolCall {
                            id: id.to_owned(),
                            name: name.to_owned(),
                            arguments: item
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("{}")
                                .to_owned(),
                            hosted: false,
                            hosted_result: None,
                        });
                    }
                }
                Some("custom_tool_call") => {
                    if let (Some(id), Some(name)) = (
                        item.get("call_id").and_then(Value::as_str),
                        item.get("name").and_then(Value::as_str),
                    ) {
                        let input = item
                            .get("input")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        calls.push(ToolCall {
                            id: id.to_owned(),
                            name: name.to_owned(),
                            arguments: serde_json::to_string(input).unwrap_or_default(),
                            hosted: false,
                            hosted_result: None,
                        });
                    }
                }
                Some("web_search_call") => {
                    if let Some(id) = item.get("id").and_then(Value::as_str) {
                        calls.push(ToolCall {
                            id: id.to_owned(),
                            name: "web_search".to_owned(),
                            arguments: item
                                .get("action")
                                .map(|action| action.to_string())
                                .unwrap_or_else(|| "{}".to_owned()),
                            hosted: true,
                            hosted_result: Some(item.clone()),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    (content, calls)
}

/// Convert a completed (or failed) Responses object.
pub fn completed_response(response: &Value) -> Result<ChatResponse, ProviderError> {
    if matches!(
        response.get("status").and_then(Value::as_str),
        Some("failed" | "incomplete")
    ) {
        return Err(response_error(&json!({"response":response})));
    }
    let (content, tool_calls) = output_items(response);
    Ok(ChatResponse {
        content,
        tool_calls,
        usage: response.get("usage").map(usage).unwrap_or_default(),
        reasoning_content: None,
        stop_reason: response
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Read a Responses SSE byte stream into the final response, forwarding text deltas.
pub async fn read_response_stream<S, B, E>(
    byte_stream: S,
    chunks: &mpsc::Sender<StreamChunk>,
    cancel: &CancellationToken,
) -> Result<ChatResponse, ResponsesError>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    let mut stream = std::pin::pin!(byte_stream);
    let mut pending = Vec::<u8>::new();
    let mut event_data = String::new();
    let mut content = String::new();
    let mut calls = Vec::<ToolCall>::new();
    loop {
        let next = tokio::select! {
            () = cancel.cancelled() => return Err(ResponsesError::Process("Bedrock request cancelled".into())),
            next = stream.next() => next,
        };
        let Some(bytes) = next else { break };
        let bytes = bytes.map_err(|_| ProviderError::transient("stream_disconnected"))?;
        pending.extend_from_slice(bytes.as_ref());
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line = pending.drain(..=end).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(data) = line.strip_prefix("data:") {
                event_data.push_str(data.trim_start());
            } else if line.is_empty() && !event_data.is_empty() {
                let value: Value = serde_json::from_str(&event_data).map_err(|_| {
                    ResponsesError::Process("invalid Bedrock Responses event".into())
                })?;
                event_data.clear();
                match value.get("type").and_then(Value::as_str) {
                    Some("response.output_text.delta") => {
                        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                            content.push_str(delta);
                            let _ = chunks
                                .send(StreamChunk {
                                    delta_text: delta.to_owned(),
                                    tool_call_deltas: Vec::new(),
                                    is_final: false,
                                    usage: None,
                                    stop_reason: None,
                                    warning: None,
                                })
                                .await;
                        }
                    }
                    Some("response.output_item.done") => {
                        if let Some(item) = value.get("item") {
                            let (_, new_calls) = output_items(&json!({"output":[item]}));
                            for call in new_calls {
                                if !calls.iter().any(|old| old.id == call.id) {
                                    calls.push(call);
                                }
                            }
                        }
                    }
                    Some("response.completed") => {
                        let mut result =
                            completed_response(value.get("response").unwrap_or(&value))?;
                        if result.content.is_empty() {
                            result.content = content;
                        }
                        if result.tool_calls.is_empty() {
                            result.tool_calls = calls;
                        }
                        return Ok(result);
                    }
                    Some("response.failed" | "error" | "response.incomplete") => {
                        return Err(response_error(&value).into());
                    }
                    _ => {}
                }
            }
        }
    }
    Err(ProviderError::transient("stream_disconnected").into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tools::HarnessTool;
    use crate::types::{ChatMessage, HarnessToolSpec};

    fn request(messages: Vec<ChatMessage>, stream: bool) -> ChatRequest {
        ChatRequest {
            messages,
            model: "global.openai.gpt-5.6-sol".into(),
            temperature: None,
            max_tokens: Some(128),
            stream,
            reasoning_effort: Some("high".into()),
            tools: vec![HarnessToolSpec {
                name: "shell".into(),
                description: "Run a command".into(),
                parameters_json: r#"{"type":"object","properties":{"command":{"type":"string"}}}"#
                    .into(),
                freeform: None,
                kind: HarnessToolSpecKind::Function,
            }],
            context_editing: false,
            tool_choice_none: false,
        }
    }

    #[test]
    fn request_body_uses_custom_wire_for_apply_patch() {
        let mut assistant = ChatMessage::assistant("patching");
        let raw_patch = "*** Begin Patch\n*** End Patch\n";
        assistant.tool_calls.push(ToolCall {
            id: "patch-call".into(),
            name: "apply_patch".into(),
            arguments: serde_json::to_string(raw_patch).unwrap(),
            hosted: false,
            hosted_result: None,
        });
        let request = ChatRequest {
            messages: vec![
                assistant,
                ChatMessage::tool_result_blocks(
                    "patch-call",
                    vec![ToolContentBlock::Text {
                        text: "Applied patch".into(),
                    }],
                    false,
                ),
            ],
            model: "global.openai.gpt-5.6-sol".into(),
            temperature: None,
            max_tokens: Some(128),
            stream: false,
            reasoning_effort: Some("high".into()),
            tools: vec![crate::tools::apply_patch::ApplyPatchTool.to_spec()],
            context_editing: false,
            tool_choice_none: false,
        };
        let body = request_body(&request);

        assert_eq!(body["tools"][0]["type"], "custom");
        assert_eq!(body["tools"][0]["format"]["syntax"], "lark");
        assert!(
            body["tools"][0]["format"]["definition"]
                .as_str()
                .unwrap()
                .contains("*** Begin Patch")
        );
        assert_eq!(body["input"][0]["role"], "assistant");
        assert_eq!(body["input"][0]["content"], "patching");
        assert_eq!(body["input"][1]["type"], "custom_tool_call");
        assert_eq!(body["input"][1]["input"], raw_patch);
        assert_eq!(body["input"][2]["type"], "custom_tool_call_output");
        assert_eq!(body["input"][2]["output"], "Applied patch");

        let calls = output_items(&json!({
            "output": [{
                "type": "custom_tool_call",
                "call_id": "patch-call",
                "name": "apply_patch",
                "input": raw_patch
            }]
        }))
        .1;
        assert_eq!(calls[0].name, "apply_patch");
        assert_eq!(
            calls[0].arguments,
            serde_json::to_string(raw_patch).unwrap()
        );
    }

    #[test]
    fn responses_output_items_parse_hosted_web_search_calls() {
        let item = json!({
            "type":"web_search_call",
            "id":"websearch-hosted",
            "status":"completed",
            "action":{"type":"search", "query":"harness hosted tools"}
        });
        let (content, calls) = output_items(&json!({"output":[item]}));

        assert_eq!(content, "");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "websearch-hosted");
        assert_eq!(calls[0].name, "web_search");
        assert!(calls[0].hosted);
        assert_eq!(calls[0].hosted_result.as_ref().unwrap(), &item);
    }

    #[test]
    fn request_body_serializes_text_image_and_error_tool_outputs() {
        let image = crate::test_support::test_png_image_block(Some("low"));
        let ToolContentBlock::Image {
            media_type, data, ..
        } = &image
        else {
            panic!("test image block");
        };
        let body = request_body(&request(
            vec![
                ChatMessage::tool_result("ordinary", "plain text"),
                ChatMessage::tool_result_blocks(
                    "typed-text",
                    vec![ToolContentBlock::Text {
                        text: "typed text".into(),
                    }],
                    false,
                ),
                ChatMessage::tool_result_blocks(
                    "image",
                    vec![
                        ToolContentBlock::Text {
                            text: "preview".into(),
                        },
                        image.clone(),
                    ],
                    false,
                ),
                ChatMessage::tool_result_blocks(
                    "error",
                    vec![ToolContentBlock::Text {
                        text: "permission denied".into(),
                    }],
                    true,
                ),
            ],
            false,
        ));

        assert_eq!(
            body["input"],
            json!([
                {"type":"function_call_output", "call_id":"ordinary", "output":"plain text"},
                {"type":"function_call_output", "call_id":"typed-text", "output":"typed text"},
                {"type":"function_call_output", "call_id":"image", "output":[
                    {"type":"input_text", "text":"preview"},
                    {"type":"input_image", "image_url":format!("data:{media_type};base64,{data}"), "detail":"low"},
                ]},
                {"type":"function_call_output", "call_id":"error", "output":
                    r#"{"error":{"message":"permission denied","type":"tool_execution"}}"#},
            ])
        );
    }

    async fn read(stream_text: &str) -> Result<ChatResponse, ResponsesError> {
        let (tx, mut rx) = mpsc::channel(16);
        let chunks = futures::stream::iter(
            stream_text
                .as_bytes()
                .chunks(7)
                .map(|bytes| Ok::<_, ()>(bytes.to_vec()))
                .collect::<Vec<_>>(),
        );
        let result = read_response_stream(chunks, &tx, &CancellationToken::new()).await;
        drop(tx);
        while rx.try_recv().is_ok() {}
        result
    }

    #[tokio::test]
    async fn stream_reader_assembles_text_tools_and_usage_across_split_chunks() {
        let completed = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 3, "output_tokens": 4}
            }
        });
        let text = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\n",
            json!({"type": "response.output_text.delta", "delta": "hel"}),
            json!({"type": "response.output_item.done", "item": {
                "type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{}"
            }}),
            completed
        );
        let response = read(&text).await.unwrap();
        assert_eq!(response.content, "hel");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "shell");
        assert_eq!(response.usage.total_tokens, 7);
        assert_eq!(response.stop_reason.as_deref(), Some("completed"));
    }

    #[tokio::test]
    async fn stream_reader_classifies_embedded_failures_and_truncated_streams() {
        let rate_limited = format!(
            "data: {}\n\n",
            json!({"type": "response.failed", "response": {"error": {"code": "rate_limit_exceeded"}}})
        );
        let Err(ResponsesError::Provider(error)) = read(&rate_limited).await else {
            panic!("expected a provider error");
        };
        assert_eq!(error.class, crate::errors::ProviderErrorClass::RateLimited);

        let Err(ResponsesError::Provider(error)) = read("data: {\"type\":\"x\"}\n\n").await else {
            panic!("a stream that never completes is a disconnect");
        };
        assert_eq!(error.detail_code, "stream_disconnected");

        assert_eq!(
            read("data: not json\n\n").await.unwrap_err(),
            ResponsesError::Process("invalid Bedrock Responses event".into())
        );
    }
}
