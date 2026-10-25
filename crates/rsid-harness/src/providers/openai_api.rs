//! OpenAI Chat Completions wire transformations: request body, response body, tool-call join.
//!
//! The daemon adapter resolves the route, quirks and credential; this module only shapes
//! JSON and never sees a key, a URL or an admission.

use crate::sse::AccumulatedToolCall;
use crate::types::*;
use serde_json::{Value, json};

/// Chat Completions wire options the daemon resolves from its provider table and route.
///
/// The leaf shapes the body only; which route, key or egress applies stays in the daemon.
pub struct ChatCompletionsWire<'a> {
    pub merge_system_into_user: bool,
    pub native_tools: bool,
    /// Request field that carries the output cap (`max_tokens` or `max_completion_tokens`).
    pub max_tokens_field: &'static str,
    pub tool_choice_none: bool,
    pub disable_streaming: bool,
    /// OpenRouter normalises reasoning control under `reasoning.effort`.
    pub openrouter_reasoning: bool,
    /// Send an assistant message's `reasoning` / `reasoning_details` back on the
    /// tool-call chain of the current turn (#3170). OpenRouter accepts both on
    /// assistant messages; a route that does not is never asked to.
    pub replay_reasoning: bool,
    /// The route's own top-level `reasoning_effort` vocabulary, if it has one.
    pub reasoning_effort_wire: &'a dyn Fn(&str) -> Option<&'static str>,
}

/// Build the Chat Completions request body.
pub fn build_request_body(request: &ChatRequest, wire: &ChatCompletionsWire<'_>) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    let mut pending_image_messages: Vec<Value> = Vec::new();

    // The current turn's tool-call chain starts after the last user message;
    // earlier turns' reasoning is not replayed.
    let chain_start = request
        .messages
        .iter()
        .rposition(|msg| msg.role == MessageRole::User)
        .map_or(0, |index| index + 1);

    for (index, msg) in request.messages.iter().enumerate() {
        // Chat Completions requires every tool reply for one assistant turn
        // before the next user message. Defer image parts until the batch ends.
        if msg.role != MessageRole::Tool {
            messages.append(&mut pending_image_messages);
        }
        match msg.role {
            MessageRole::System => {
                if !wire.merge_system_into_user {
                    messages.push(json!({
                        "role": "system",
                        "content": msg.content,
                    }));
                }
            }
            MessageRole::User => {
                messages.push(json!({
                    "role": "user",
                    "content": msg.content,
                }));
            }
            MessageRole::Assistant => {
                let mut m = json!({ "role": "assistant" });
                if !msg.content.is_empty() {
                    m["content"] = json!(msg.content);
                }
                if !msg.tool_calls.is_empty() {
                    m["tool_calls"] = json!(
                        msg.tool_calls
                            .iter()
                            .map(|tc| {
                                json!({
                                    "id": tc.id,
                                    "type": "function",
                                    "function": {
                                        "name": tc.name,
                                        "arguments": tc.arguments,
                                    },
                                })
                            })
                            .collect::<Vec<_>>()
                    );
                }
                if wire.replay_reasoning && index >= chain_start {
                    // Structured blocks go back verbatim and win over the flat
                    // text, which they already contain; send one form, not both.
                    if let Some(details) = msg.reasoning_details.as_ref().filter(|d| !d.is_empty())
                    {
                        m["reasoning_details"] = json!(details);
                    } else if let Some(text) =
                        msg.reasoning.as_deref().filter(|t| !t.trim().is_empty())
                    {
                        m["reasoning"] = json!(text);
                    }
                }
                messages.push(m);
            }
            MessageRole::Tool => {
                let blocks = msg.tool_blocks();
                let mut text = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ToolContentBlock::Text { text } => Some(text.as_str()),
                        ToolContentBlock::ServerToolResult { .. } => None,
                        ToolContentBlock::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let image_parts = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ToolContentBlock::Image {
                            media_type,
                            data,
                            detail,
                            ..
                        } => {
                            let mut image_url = json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{media_type};base64,{data}"),
                            },
                            });
                            if let Some(detail) = detail {
                                image_url["image_url"]["detail"] = json!(detail);
                            }
                            Some(image_url)
                        }
                        ToolContentBlock::Text { .. } => None,
                        ToolContentBlock::ServerToolResult { .. } => None,
                    })
                    .collect::<Vec<_>>();
                if !image_parts.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str("[Image attached in the following user message]");
                }
                let content = if msg.is_error {
                    json!({
                        "error": {
                            "type": "tool_execution",
                            "message": text,
                        }
                    })
                    .to_string()
                } else {
                    text
                };
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": msg.tool_call_id.as_deref().unwrap_or(""),
                    "content": content,
                }));
                if !image_parts.is_empty() {
                    let mut user_parts = vec![json!({
                        "type": "text",
                        "text": format!(
                            "Image returned by tool call {}:",
                            msg.tool_call_id.as_deref().unwrap_or("")
                        ),
                    })];
                    user_parts.extend(image_parts);
                    pending_image_messages.push(json!({
                        "role": "user",
                        "content": user_parts,
                    }));
                }
            }
        }
    }
    messages.append(&mut pending_image_messages);

    // Handle merge_system_into_user quirk
    if wire.merge_system_into_user {
        let mut system_text = String::new();
        messages.retain(|m| {
            if m.get("role").and_then(|r| r.as_str()) == Some("system") {
                if let Some(content) = m.get("content").and_then(|c| c.as_str()) {
                    system_text = format!("[System: {}]\n\n", content);
                }
                false
            } else {
                true
            }
        });
        if !system_text.is_empty()
            && let Some(first_user) = messages
                .iter_mut()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        {
            let existing = first_user
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            first_user["content"] = json!(format!("{system_text}{existing}"));
        }
    }

    let tools: Vec<Value> = request
        .tools
        .iter()
        .filter(|t| matches!(t.kind, HarnessToolSpecKind::Function))
        .map(|t| {
            let schema: Value =
                serde_json::from_str(&t.parameters_json).unwrap_or(json!({"type": "object"}));
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": schema,
                },
            })
        })
        .collect();

    let mut body = json!({
        "model": request.model,
        "messages": messages,
    });

    if !tools.is_empty() && wire.native_tools {
        body["tools"] = json!(tools);
    }
    if let Some(temp) = request.temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(max) = request.max_tokens {
        // The route's own field: Inception deprecates `max_tokens` for
        // `max_completion_tokens`, which counts reasoning tokens (#2253).
        body[wire.max_tokens_field] = json!(max);
    }
    // OpenRouter normalises reasoning control under `reasoning.effort`.
    if wire.openrouter_reasoning
        && let Some(effort) = request
            .reasoning_effort
            .as_deref()
            .filter(|effort| matches!(*effort, "low" | "medium" | "high"))
    {
        body["reasoning"] = json!({ "effort": effort });
    }
    // A route with its own effort vocabulary takes a top-level
    // `reasoning_effort` (#2253). An unknown level is not sent.
    if let Some(effort) = request
        .reasoning_effort
        .as_deref()
        .and_then(|effort| (wire.reasoning_effort_wire)(effort))
    {
        body["reasoning_effort"] = json!(effort);
    }
    // Keep `tools` and forbid calling them on a route that allows it
    // (#2253). Never without tools, and never on a route that does not.
    if request.tool_choice_none && wire.tool_choice_none && body.get("tools").is_some() {
        body["tool_choice"] = json!("none");
    }
    if request.stream && !wire.disable_streaming {
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage": true});
    }

    body
}

/// Parse a non-streaming Chat Completions response. The error is the daemon-facing
/// description of the missing part.
pub fn parse_chat_response(json: &Value) -> Result<ChatResponse, &'static str> {
    let choice = json
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .ok_or("No choices in OpenAI response")?;

    let message = choice.get("message").ok_or("No message in choice")?;

    let content = message
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    // Ollama and DeepSeek-style routes return the thinking beside the
    // answer under `reasoning_content` or `reasoning`.
    let reasoning_content = ["reasoning_content", "reasoning"]
        .iter()
        .filter_map(|key| message.get(*key).and_then(|value| value.as_str()))
        .find(|text| !text.trim().is_empty())
        .map(String::from);
    let reasoning_details = message
        .get("reasoning_details")
        .and_then(|value| value.as_array())
        .filter(|blocks| !blocks.is_empty())
        .cloned();

    let tool_calls: Vec<ToolCall> = message
        .get("tool_calls")
        .and_then(|tcs| tcs.as_array())
        .map(|tcs| {
            tcs.iter()
                .filter_map(|tc| {
                    Some(ToolCall {
                        id: tc.get("id")?.as_str()?.to_string(),
                        name: tc.get("function")?.get("name")?.as_str()?.to_string(),
                        arguments: tc.get("function")?.get("arguments")?.as_str()?.to_string(),
                        hosted: false,
                        hosted_result: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    if let Some(warning) = crate::sse::chat_warning(json) {
        tracing::warn!(%warning, "chat completions provider warning");
    }
    let usage = json
        .get("usage")
        .filter(|u| !u.is_null())
        .map(|u| {
            let mut usage = TokenUsage::default();
            crate::sse::apply_chat_usage(&mut usage, u);
            // Keep the provider's own total when it reports one.
            if let Some(total) = u.get("total_tokens").and_then(|v| v.as_u64()) {
                usage.total_tokens = total;
            }
            usage
        })
        .unwrap_or_default();

    let stop_reason = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(ChatResponse {
        content,
        tool_calls,
        usage,
        reasoning_content,
        reasoning_details,
        stop_reason,
    })
}

/// Convert streamed accumulations into the harness tool calls.
pub fn convert_tool_calls(accumulated: Vec<AccumulatedToolCall>) -> Vec<ToolCall> {
    accumulated
        .into_iter()
        .map(|tc| ToolCall {
            id: tc.id,
            name: tc.name,
            arguments: tc.arguments,
            hosted: false,
            hosted_result: None,
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::test_support::test_png_image_block;

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            messages,
            model: "test-model".into(),
            temperature: None,
            max_tokens: None,
            tools: Vec::new(),
            stream: false,
            reasoning_effort: None,
            context_editing: false,
            tool_choice_none: false,
        }
    }

    fn no_effort(_: &str) -> Option<&'static str> {
        None
    }

    fn wire() -> ChatCompletionsWire<'static> {
        ChatCompletionsWire {
            merge_system_into_user: false,
            native_tools: false,
            max_tokens_field: "max_tokens",
            tool_choice_none: false,
            disable_streaming: false,
            openrouter_reasoning: false,
            replay_reasoning: false,
            reasoning_effort_wire: &no_effort,
        }
    }

    #[test]
    fn context_editing_never_reaches_an_openai_style_body() {
        let mut request = request(vec![ChatMessage::user("hi")]);
        request.context_editing = true;
        let body = build_request_body(&request, &wire());
        assert!(body.get("context_management").is_none());
        assert!(!body.to_string().contains("clear_tool_uses"));
    }

    #[test]
    fn tool_results_use_text_tool_messages_and_user_image_parts() {
        let image = test_png_image_block(Some("low"));
        let ToolContentBlock::Image {
            media_type, data, ..
        } = &image
        else {
            panic!("test image block");
        };
        let body = build_request_body(
            &request(vec![
                ChatMessage::tool_result("legacy", "plain text"),
                ChatMessage::tool_result_blocks(
                    "text",
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
                        text: "failed".into(),
                    }],
                    true,
                ),
            ]),
            &wire(),
        );
        assert_eq!(
            body["messages"][0],
            json!({"role": "tool", "tool_call_id": "legacy", "content": "plain text"})
        );
        assert_eq!(
            body["messages"][1],
            json!({"role": "tool", "tool_call_id": "text", "content": "typed text"})
        );
        assert_eq!(
            body["messages"][2],
            json!({
                "role": "tool", "tool_call_id": "image",
                "content": "preview\n[Image attached in the following user message]"
            })
        );
        assert_eq!(body["messages"][3]["role"], "tool");
        let error: Value =
            serde_json::from_str(body["messages"][3]["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            error,
            json!({"error": {"type": "tool_execution", "message": "failed"}})
        );
        assert_eq!(
            body["messages"][4],
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "Image returned by tool call image:"},
                    {"type": "image_url", "image_url": {"url": format!("data:{media_type};base64,{data}"), "detail": "low"}}
                ]
            })
        );
    }

    #[test]
    fn wire_options_gate_system_merge_tools_effort_and_streaming() {
        let mut req = request(vec![
            ChatMessage::system("be brief"),
            ChatMessage::user("hi"),
        ]);
        req.max_tokens = Some(64);
        req.stream = true;
        req.reasoning_effort = Some("high".into());
        req.tool_choice_none = true;
        req.tools = vec![HarnessToolSpec {
            name: "probe".into(),
            description: "Probe.".into(),
            parameters_json: r#"{"type":"object"}"#.into(),
            kind: HarnessToolSpecKind::Function,
            freeform: None,
        }];

        let plain = build_request_body(&req, &wire());
        assert_eq!(plain["messages"][0]["role"], "system");
        assert!(plain.get("tools").is_none(), "no native tools");
        assert_eq!(plain["max_tokens"], 64);
        assert_eq!(plain["stream"], true);
        assert!(plain.get("reasoning").is_none());
        assert!(plain.get("reasoning_effort").is_none());

        let effort = |effort: &str| (effort == "high").then_some("medium");
        let configured = build_request_body(
            &req,
            &ChatCompletionsWire {
                merge_system_into_user: true,
                native_tools: true,
                max_tokens_field: "max_completion_tokens",
                tool_choice_none: true,
                disable_streaming: true,
                openrouter_reasoning: true,
                replay_reasoning: false,
                reasoning_effort_wire: &effort,
            },
        );
        assert_eq!(configured["messages"].as_array().unwrap().len(), 1);
        assert_eq!(configured["messages"][0]["role"], "user");
        assert_eq!(configured["max_completion_tokens"], 64);
        assert!(configured.get("max_tokens").is_none());
        assert_eq!(configured["tools"][0]["function"]["name"], "probe");
        assert_eq!(configured["tool_choice"], "none");
        assert_eq!(configured["reasoning"], json!({"effort": "high"}));
        assert_eq!(configured["reasoning_effort"], "medium");
        assert!(configured.get("stream").is_none());
    }

    #[test]
    fn chat_response_reads_message_reasoning_usage_and_tool_calls() {
        let response = parse_chat_response(&json!({
            "choices": [{
                "message": {
                    "content": "done",
                    "reasoning": "thinking",
                    "tool_calls": [
                        {"id": "c1", "function": {"name": "shell", "arguments": "{}"}},
                        {"id": "bad"}
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 4, "completion_tokens": 6, "total_tokens": 11}
        }))
        .unwrap();
        assert_eq!(response.content, "done");
        assert_eq!(response.reasoning_content.as_deref(), Some("thinking"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "shell");
        assert_eq!(response.usage.prompt_tokens, 4);
        assert_eq!(response.usage.total_tokens, 11);
        assert_eq!(response.stop_reason.as_deref(), Some("tool_calls"));

        assert_eq!(
            parse_chat_response(&json!({"choices": []})).unwrap_err(),
            "No choices in OpenAI response"
        );
        assert_eq!(
            parse_chat_response(&json!({"choices": [{}]})).unwrap_err(),
            "No message in choice"
        );
    }

    fn thinking_assistant(text: &str, details: Option<Vec<Value>>) -> ChatMessage {
        let mut message = ChatMessage::assistant("");
        message.tool_calls = vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: "{}".into(),
            hosted: false,
            hosted_result: None,
        }];
        message.reasoning = Some(text.into());
        message.reasoning_details = details;
        message
    }

    #[test]
    fn replay_sends_reasoning_on_the_current_chain_only_when_the_route_asks() {
        let details = vec![json!({"type": "reasoning.text", "text": "step one", "index": 0})];
        let req = request(vec![
            ChatMessage::user("first task"),
            thinking_assistant("old turn thoughts", None),
            ChatMessage::tool_result("c1", "ok"),
            ChatMessage::user("second task"),
            thinking_assistant("flat thoughts", None),
            ChatMessage::tool_result("c1", "ok"),
            thinking_assistant("structured thoughts", Some(details.clone())),
        ]);
        let replay = ChatCompletionsWire {
            replay_reasoning: true,
            ..wire()
        };

        let body = build_request_body(&req, &replay);
        let messages = body["messages"].as_array().unwrap();
        // The earlier turn's reasoning is not replayed.
        assert!(messages[1].get("reasoning").is_none());
        assert!(messages[1].get("reasoning_details").is_none());
        // The current chain replays the flat text, or the verbatim blocks when
        // the route returned them (one form, never both).
        assert_eq!(messages[4]["reasoning"], "flat thoughts");
        assert!(messages[4].get("reasoning_details").is_none());
        assert_eq!(messages[6]["reasoning_details"], json!(details));
        assert!(messages[6].get("reasoning").is_none());
        assert_eq!(messages[6]["tool_calls"][0]["id"], "c1");

        // Any other route is unchanged: no reasoning on the wire.
        let plain = build_request_body(&req, &wire());
        let rendered = plain.to_string();
        assert!(!rendered.contains("thoughts"), "{rendered}");
        assert!(!rendered.contains("reasoning_details"), "{rendered}");
    }

    #[test]
    fn chat_response_keeps_reasoning_details_verbatim() {
        let blocks = json!([{"type": "reasoning.text", "text": "t", "index": 0}]);
        let response = parse_chat_response(&json!({
            "choices": [{
                "message": {"content": "x", "reasoning": "t", "reasoning_details": blocks},
                "finish_reason": "stop"
            }]
        }))
        .unwrap();
        assert_eq!(response.reasoning_details, blocks.as_array().cloned());
        let none = parse_chat_response(&json!({
            "choices": [{"message": {"content": "x", "reasoning_details": []}}]
        }))
        .unwrap();
        assert_eq!(none.reasoning_details, None);
    }
}
