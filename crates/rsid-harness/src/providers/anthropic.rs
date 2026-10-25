//! Anthropic Messages wire transformations: request body, response body, tool-call join.
//!
//! No credential, transport or network admission lives here: the daemon adapter decides
//! the route (direct or Bedrock) and sends the result.

use crate::sse::AccumulatedToolCall;
use crate::types::*;
use serde_json::{Value, json};

/// Beta header that enables server-side context editing.
pub const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";
/// Context editing starts only past this prompt size, so a short session
/// never pays a cache write for it.
const CONTEXT_EDIT_TRIGGER_TOKENS: u64 = 100_000;
/// Recent tool use/result pairs that always stay readable.
const CONTEXT_EDIT_KEEP_TOOL_USES: u64 = 8;
/// An edit that clears less than this is skipped: clearing breaks the prompt
/// cache prefix, so it must free enough to be worth the write (bulk, rarely).
const CONTEXT_EDIT_CLEAR_AT_LEAST_TOKENS: u64 = 40_000;

/// The `context_management` request field (names verified against the
/// context-editing docs). Constant for a session, so the request prefix stays
/// cache-stable until the API actually clears.
pub fn context_management_config() -> Value {
    json!({
        "edits": [{
            "type": "clear_tool_uses_20250919",
            "trigger": {"type": "input_tokens", "value": CONTEXT_EDIT_TRIGGER_TOKENS},
            "keep": {"type": "tool_uses", "value": CONTEXT_EDIT_KEEP_TOOL_USES},
            "clear_at_least": {"type": "input_tokens", "value": CONTEXT_EDIT_CLEAR_AT_LEAST_TOKENS},
        }]
    })
}

/// `anthropic_version` Bedrock requires in the Messages body.
pub const BEDROCK_ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";
/// Whether a request carries server-side context editing: the request opted in, the
/// route is the direct Anthropic API (not Bedrock) and the model is a Claude model.
pub fn context_editing_active(request: &ChatRequest, direct_transport: bool) -> bool {
    request.context_editing && direct_transport && request.model.starts_with("claude")
}

/// Bedrock takes the model from the URL, picks streaming by operation, and requires its
/// own `anthropic_version` in the Messages body.
pub fn bedrock_body(mut body: Value) -> Value {
    if let Some(object) = body.as_object_mut() {
        object.remove("model");
        object.remove("stream");
        object.insert(
            "anthropic_version".into(),
            Value::String(BEDROCK_ANTHROPIC_VERSION.into()),
        );
    }
    body
}

/// Build the Anthropic-format request body.
///
/// `context_editing_active` and `adaptive_thinking` are the daemon's decisions (route
/// transport and model capability tables); this function only shapes the wire body.
pub fn build_request_body(
    request: &ChatRequest,
    context_editing_active: bool,
    adaptive_thinking: bool,
) -> Value {
    // Extract system message (Anthropic wants it as top-level field)
    let mut system_content: Option<String> = None;
    let mut messages = Vec::new();

    for msg in &request.messages {
        match msg.role {
            MessageRole::System => {
                // Anthropic: system is top-level, not in messages array
                system_content = Some(msg.content.clone());
            }
            MessageRole::User => {
                messages.push(json!({
                    "role": "user",
                    "content": [{"type": "text", "text": msg.content}],
                }));
            }
            MessageRole::Assistant => {
                if msg.tool_calls.is_empty() {
                    messages.push(json!({
                        "role": "assistant",
                        "content": msg.content,
                    }));
                } else {
                    // Assistant with tool calls
                    let mut content_blocks: Vec<Value> = Vec::new();
                    if !msg.content.is_empty() {
                        content_blocks.push(json!({
                            "type": "text",
                            "text": msg.content,
                        }));
                    }
                    for tc in &msg.tool_calls {
                        let input: Value = serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                        content_blocks.push(json!({
                            "type": if tc.hosted { "server_tool_use" } else { "tool_use" },
                            "id": tc.id,
                            "name": tc.name,
                            "input": input,
                        }));
                    }
                    messages.push(json!({
                        "role": "assistant",
                        "content": content_blocks,
                    }));
                }
            }
            MessageRole::Tool => {
                if let Some(block) = msg
                    .tool_blocks()
                    .into_iter()
                    .find(|block| matches!(block, ToolContentBlock::ServerToolResult { .. }))
                    && let ToolContentBlock::ServerToolResult { result } = block
                {
                    messages.push(json!({
                        "role": "assistant",
                        "content": [result],
                    }));
                    continue;
                }
                let content = if msg.has_typed_tool_blocks() {
                    json!(
                        msg.tool_blocks()
                            .into_iter()
                            .map(|block| match block {
                                ToolContentBlock::Text { text } => json!({
                                    "type": "text",
                                    "text": text,
                                }),
                                ToolContentBlock::Image {
                                    media_type, data, ..
                                } => json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": media_type,
                                        "data": data,
                                    },
                                }),
                                ToolContentBlock::ServerToolResult { result } => result,
                            })
                            .collect::<Vec<_>>()
                    )
                } else {
                    json!(msg.content)
                };
                let mut tool_result = json!({
                    "type": "tool_result",
                    "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""),
                    "content": content,
                });
                if msg.is_error {
                    tool_result["is_error"] = json!(true);
                }
                messages.push(json!({
                    "role": "user",
                    "content": [tool_result],
                }));
            }
        }
    }

    // Build tools array in Anthropic format
    let mut tools: Vec<Value> = request
        .tools
        .iter()
        .map(|t| match &t.kind {
            HarnessToolSpecKind::Function => {
                let schema: Value =
                    serde_json::from_str(&t.parameters_json).unwrap_or(json!({"type": "object"}));
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": schema,
                })
            }
            HarnessToolSpecKind::AnthropicWebSearch { wire_type }
            | HarnessToolSpecKind::AnthropicWebFetch { wire_type } => json!({
                "type": wire_type,
                "name": t.name,
            }),
            HarnessToolSpecKind::ResponsesWebSearch => Value::Null,
        })
        .filter(|tool| !tool.is_null())
        .collect();

    // Explicit ephemeral checkpoints work on both Messages and Bedrock InvokeModel
    // (not Converse's cachePoint shape). At most three: tools, system, latest
    // user/tool result. Derive them afresh on the wire; never edit the transcript.
    // https://platform.claude.com/docs/en/build-with-claude/prompt-caching
    // https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html
    if let Some(tool) = tools.last_mut() {
        tool["cache_control"] = json!({"type": "ephemeral"});
    }
    if let Some(message) = messages.iter_mut().rev().find(|msg| msg["role"] == "user")
        && let Some(block) = message["content"].as_array_mut().and_then(|b| b.last_mut())
    {
        block["cache_control"] = json!({"type": "ephemeral"});
    }

    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "max_tokens": request.max_tokens.unwrap_or(8192),
    });

    if let Some(ref sys) = system_content {
        body["system"] = json!([{
            "type": "text", "text": sys,
            "cache_control": {"type": "ephemeral"},
        }]);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if !adaptive_thinking {
        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }
    }
    if request.stream {
        body["stream"] = json!(true);
    }
    if context_editing_active {
        body["context_management"] = context_management_config();
    }

    // Current Claude models use adaptive thinking and output_config effort.
    if let Some(ref effort) = request.reasoning_effort {
        if adaptive_thinking {
            body["thinking"] = json!({ "type": "adaptive" });
            body["output_config"] = json!({ "effort": effort });
        } else {
            let budget = match effort.as_str() {
                "low" => 1024,
                "medium" => 4096,
                "high" => 8192,
                _ => 4096,
            };
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget,
            });
        }
    }

    body
}

/// Join streamed tool-call accumulations with their hosted results.
pub fn convert_tool_calls(accumulated: Vec<AccumulatedToolCall>) -> Vec<ToolCall> {
    let (results, calls): (Vec<_>, Vec<_>) = accumulated.into_iter().partition(|tc| tc.is_result);
    let mut calls: Vec<ToolCall> = calls
        .into_iter()
        .map(|tc| ToolCall {
            id: tc.id,
            name: tc.name,
            arguments: tc.arguments,
            hosted: tc.hosted,
            hosted_result: None,
        })
        .collect();
    for result in results {
        if let Some(result_value) = &result.hosted_result {
            if let Some(call) = calls.iter_mut().find(|call| call.id == result.id) {
                call.hosted_result = Some(result_value.clone());
            }
        }
    }
    calls
}

/// Parse a non-streaming Messages response body.
pub fn parse_chat_response(json: &Value) -> ChatResponse {
    // Parse response
    let mut content = String::new();
    let mut tool_calls = Vec::new();

    if let Some(blocks) = json.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        content.push_str(text);
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(ToolCall {
                        id: block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        arguments: block
                            .get("input")
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                        hosted: false,
                        hosted_result: None,
                    });
                }
                Some("server_tool_use") => {
                    tool_calls.push(ToolCall {
                        id: block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        arguments: block
                            .get("input")
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                        hosted: true,
                        hosted_result: None,
                    });
                }
                Some("web_search_tool_result" | "web_fetch_tool_result") => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Some(call) = tool_calls.iter_mut().find(|call| call.id == id) {
                        call.hosted_result = Some(block.clone());
                    }
                }
                _ => {}
            }
        }
    }

    let usage = if let Some(u) = json.get("usage") {
        TokenUsage {
            prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            cache_creation_tokens: u
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            cache_read_tokens: u
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            reasoning_tokens: 0,
            total_tokens: 0, // Computed below
            cost_usd: None,
        }
    } else {
        TokenUsage::default()
    };

    let stop_reason = json
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .map(String::from);

    ChatResponse {
        content,
        tool_calls,
        usage: TokenUsage {
            total_tokens: usage.prompt_tokens
                + usage.completion_tokens
                + usage.cache_creation_tokens
                + usage.cache_read_tokens,
            ..usage
        },
        reasoning_content: None,
        reasoning_details: None,
        stop_reason,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::test_support::test_png_image_block;

    fn request(model: &str, effort: Option<&str>) -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage::user("test")],
            model: model.to_string(),
            temperature: Some(0.2),
            max_tokens: Some(512),
            tools: Vec::new(),
            stream: false,
            reasoning_effort: effort.map(str::to_string),
            context_editing: false,
            tool_choice_none: false,
        }
    }

    fn cache_markers(value: &Value) -> usize {
        match value {
            Value::Object(object) => {
                usize::from(object.contains_key("cache_control"))
                    + object.values().map(cache_markers).sum::<usize>()
            }
            Value::Array(array) => array.iter().map(cache_markers).sum(),
            _ => 0,
        }
    }

    #[test]
    fn stable_prefix_and_latest_user_have_ephemeral_breakpoints_on_both_routes() {
        let mut request = request("claude-sonnet-5", None);
        request
            .messages
            .insert(0, ChatMessage::system("Stable instructions"));
        request.tools = ["shell", "read"]
            .into_iter()
            .map(|name| HarnessToolSpec {
                name: name.into(),
                description: format!("Use {name}"),
                parameters_json: r#"{"type":"object"}"#.into(),
                freeform: None,
                kind: HarnessToolSpecKind::Function,
            })
            .collect();
        // A foreign hosted tool is filtered before selecting the tools checkpoint.
        request.tools.push(HarnessToolSpec {
            name: "web_search".into(),
            description: String::new(),
            parameters_json: "{}".into(),
            freeform: None,
            kind: HarnessToolSpecKind::ResponsesWebSearch,
        });
        let direct = build_request_body(&request, false, true);
        for body in [direct.clone(), bedrock_body(direct)] {
            assert_eq!(
                body["system"],
                json!([{
                    "type": "text", "text": "Stable instructions",
                    "cache_control": {"type": "ephemeral"},
                }])
            );
            assert_eq!(body["tools"].as_array().unwrap().len(), 2);
            assert_eq!(
                body["tools"][1]["cache_control"],
                json!({"type":"ephemeral"})
            );
            assert_eq!(
                body["messages"][0]["content"],
                json!([{
                    "type": "text", "text": "test",
                    "cache_control": {"type": "ephemeral"},
                }])
            );
            assert_eq!(cache_markers(&body), 3);
        }
    }

    #[test]
    fn rolling_tool_result_breakpoint_is_bounded_on_long_transcripts() {
        let mut request = request("claude-sonnet-5", None);
        request
            .messages
            .insert(0, ChatMessage::system("Stable instructions"));
        request.tools.push(HarnessToolSpec {
            name: "shell".into(),
            description: "Run a command".into(),
            parameters_json: "{}".into(),
            freeform: None,
            kind: HarnessToolSpecKind::Function,
        });
        for turn in 0..80 {
            let id = format!("tool-{turn}");
            let mut assistant = ChatMessage::assistant("Running it");
            assistant.tool_calls.push(ToolCall {
                id: id.clone(),
                name: "shell".into(),
                arguments: "{}".into(),
                hosted: false,
                hosted_result: None,
            });
            request
                .messages
                .extend([assistant, ChatMessage::tool_result(&id, "ok")]);
            let body = build_request_body(&request, false, true);
            let messages = body["messages"].as_array().unwrap();
            let result = &messages.last().unwrap()["content"][0];
            assert_eq!(result["type"], "tool_result");
            assert_eq!(result["tool_use_id"], id);
            assert_eq!(result["cache_control"], json!({"type":"ephemeral"}));
            assert_eq!(cache_markers(&body), 3);
            assert_eq!(cache_markers(&body["messages"]), 1);
        }
    }

    #[test]
    fn growing_turns_keep_prefix_bytes_and_transcript_content_identical() {
        let mut request = request("claude-sonnet-5", None);
        request.tools.push(HarnessToolSpec {
            name: "shell".into(),
            description: "Run a command".into(),
            parameters_json: "{}".into(),
            freeform: None,
            kind: HarnessToolSpecKind::Function,
        });
        request
            .messages
            .insert(0, ChatMessage::system("Stable instructions"));
        let first = build_request_body(&request, false, true);
        let transcript = serde_json::to_vec(&request.messages).unwrap();
        for turn in 0..25 {
            request.messages.extend([
                ChatMessage::assistant("answer"),
                ChatMessage::user(&format!("next {turn}")),
            ]);
            let next = build_request_body(&request, false, true);
            assert_eq!(
                serde_json::to_vec(&first["system"]).unwrap(),
                serde_json::to_vec(&next["system"]).unwrap()
            );
            assert_eq!(
                serde_json::to_vec(&first["tools"]).unwrap(),
                serde_json::to_vec(&next["tools"]).unwrap()
            );
            assert_eq!(
                serde_json::to_vec(&request.messages[..2]).unwrap(),
                transcript
            );
            // cache_control is wire metadata, excluded from Anthropic's cache key.
            // Moving that sole marker must preserve all earlier content bytes.
            let mut before = first["messages"][0].clone();
            before["content"][0]
                .as_object_mut()
                .unwrap()
                .remove("cache_control");
            assert_eq!(
                serde_json::to_vec(&before).unwrap(),
                serde_json::to_vec(&next["messages"][0]).unwrap()
            );
            assert_eq!(cache_markers(&next), 3);
        }
    }

    #[test]
    fn tool_result_error_flag_is_sent_only_for_failures() {
        let mut successful = request("claude-opus-5", None);
        successful.messages = vec![ChatMessage::tool_result("call-1", "ok")];
        let success_body = build_request_body(&successful, false, true);
        assert_eq!(
            success_body["messages"][0]["content"][0],
            json!({
                "type": "tool_result",
                "tool_use_id": "call-1",
                "content": "ok",
                "cache_control": {"type": "ephemeral"}
            })
        );

        let mut failed = request("claude-opus-5", None);
        failed.messages = vec![ChatMessage::tool_error_result("call-1", "Error: failed")];
        let failure_body = build_request_body(&failed, false, true);
        assert_eq!(failure_body["messages"][0]["content"][0]["is_error"], true);
        assert_eq!(
            failure_body["messages"][0]["content"][0]["content"],
            "Error: failed"
        );
    }

    #[test]
    fn typed_tool_results_use_anthropic_content_blocks() {
        let mut request = request("claude-opus-5", None);
        let image = test_png_image_block(Some("low"));
        let ToolContentBlock::Image {
            media_type, data, ..
        } = &image
        else {
            panic!("test image block");
        };
        request.messages = vec![
            ChatMessage::tool_result_blocks(
                "text",
                vec![ToolContentBlock::Text { text: "ok".into() }],
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
                    text: "failed".into(),
                }],
                true,
            ),
        ];
        let body = build_request_body(&request, false, true);
        assert_eq!(
            body["messages"][0]["content"][0]["content"],
            json!([{"type": "text", "text": "ok"}])
        );
        assert_eq!(
            body["messages"][1]["content"][0]["content"],
            json!([
                {"type": "text", "text": "preview"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": media_type, "data": data
                }}
            ])
        );
        assert_eq!(
            body["messages"][2]["content"][0]["content"],
            json!([{"type": "text", "text": "failed"}])
        );
        assert_eq!(body["messages"][2]["content"][0]["is_error"], true);
    }

    #[test]
    fn adaptive_models_send_effort_without_temperature_or_budget() {
        let body = build_request_body(&request("claude-opus-5", Some("xhigh")), false, true);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert_eq!(body["output_config"]["effort"], "xhigh");
        assert!(body.get("temperature").is_none());

        let body = build_request_body(&request("claude-opus-5", None), false, true);
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn older_models_keep_legacy_thinking_request_shape() {
        let body = build_request_body(&request("claude-opus-4-6", Some("high")), false, false);

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 8192);
        assert!(body.get("output_config").is_none());
        let temperature = body["temperature"]
            .as_f64()
            .expect("temperature is numeric");
        assert!((temperature - 0.2).abs() < 1e-6);
    }

    #[test]
    fn context_editing_is_a_constant_field_for_the_direct_claude_route_only() {
        let mut on = request("claude-opus-5-5", None);
        on.context_editing = true;
        assert!(context_editing_active(&on, true));
        assert!(!context_editing_active(&on, false));
        let mut other = on.clone();
        other.model = "gpt-5".into();
        assert!(!context_editing_active(&other, true));
        assert!(!context_editing_active(
            &request("claude-opus-5-5", None),
            true
        ));

        let body = build_request_body(&on, true, true);
        assert_eq!(body["context_management"], context_management_config());
        let edit = &body["context_management"]["edits"][0];
        assert_eq!(edit["type"], "clear_tool_uses_20250919");
        assert_eq!(edit["keep"]["type"], "tool_uses");
        assert!(
            build_request_body(&on, false, true)
                .get("context_management")
                .is_none()
        );
    }

    #[test]
    fn bedrock_body_drops_model_and_stream_and_pins_the_bedrock_version() {
        let mut streaming = request("us.anthropic.claude-opus-5-5-v1:0", Some("high"));
        streaming.stream = true;
        let body = bedrock_body(build_request_body(&streaming, false, true));
        assert_eq!(body["anthropic_version"], BEDROCK_ANTHROPIC_VERSION);
        assert!(body.get("model").is_none());
        assert!(body.get("stream").is_none());
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn chat_response_joins_hosted_results_and_totals_usage() {
        let response = parse_chat_response(&json!({
            "content": [
                {"type": "text", "text": "a"},
                {"type": "text", "text": "b"},
                {"type": "tool_use", "id": "t1", "name": "shell", "input": {"cmd": "ls"}},
                {"type": "server_tool_use", "id": "s1", "name": "web_search", "input": {"query": "q"}},
                {"type": "web_search_tool_result", "tool_use_id": "s1", "content": []}
            ],
            "usage": {
                "input_tokens": 10, "output_tokens": 5,
                "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3
            },
            "stop_reason": "tool_use"
        }));
        assert_eq!(response.content, "ab");
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].arguments, r#"{"cmd":"ls"}"#);
        assert!(!response.tool_calls[0].hosted);
        assert!(response.tool_calls[1].hosted);
        assert_eq!(
            response.tool_calls[1].hosted_result.as_ref().unwrap()["tool_use_id"],
            "s1"
        );
        assert_eq!(response.usage.total_tokens, 20);
        assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn streamed_tool_calls_attach_hosted_results_by_id() {
        let calls = convert_tool_calls(vec![
            AccumulatedToolCall {
                id: "s1".into(),
                name: "web_search".into(),
                arguments: "{}".into(),
                hosted: true,
                ..Default::default()
            },
            AccumulatedToolCall {
                id: "s1".into(),
                is_result: true,
                hosted_result: Some(json!({"type": "web_search_tool_result"})),
                ..Default::default()
            },
        ]);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].hosted_result,
            Some(json!({"type": "web_search_tool_result"}))
        );
    }
}
