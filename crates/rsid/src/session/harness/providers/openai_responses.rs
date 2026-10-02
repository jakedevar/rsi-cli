//! `OpenAI` Responses wire for Bedrock's bearer-authenticated endpoint.

use crate::error::DaemonError;
use crate::model_control::ModelExecutionCapability;
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::session::harness::api_key::ApiCredential;
use crate::session::harness::errors::{self, ProviderError, ProviderErrorClass};
use crate::session::harness::provider::{ApiProvider, Result, WebCapabilities};
use crate::session::harness::types::{
    ChatRequest, ChatResponse, HarnessToolSpecKind, MessageRole, StreamChunk, TokenUsage, ToolCall,
    ToolContentBlock,
};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct OpenAiResponsesProvider {
    http: reqwest::Client,
    base_url: String,
    credential: ApiCredential,
}

impl OpenAiResponsesProvider {
    pub fn bedrock() -> Result<Self> {
        let region = crate::bedrock::region().map_err(DaemonError::InvalidParam)?;
        Self::with_config(
            format!("https://bedrock-runtime.{region}.amazonaws.com/openai/v1"),
            ApiCredential::for_slot_only(crate::vault::Slot::Bedrock),
        )
    }

    fn with_config(base_url: String, credential: ApiCredential) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|error| DaemonError::Process(format!("HTTP client build error: {error}")))?;
        Ok(Self {
            http,
            base_url,
            credential,
        })
    }

    fn request_body(request: &ChatRequest) -> Value {
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
                                    ToolContentBlock::ServerToolResult { .. } => {
                                        Value::Null
                                    }
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

    async fn send_request(
        &self,
        request: &ChatRequest,
        execution: ModelExecutionCapability,
    ) -> Result<(reqwest::Response, String)> {
        // A vault rotation, env-compat toggle or generator refresh is observed
        // for each request. Never retain a generated bearer across turns.
        let secret = self
            .credential
            .current()
            .ok_or_else(|| DaemonError::OpenAiApiError("Bedrock credential unavailable".into()))?;
        let fingerprint = secret.fingerprint();
        let req = self
            .http
            .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
            .bearer_auth(secret.expose())
            .json(&Self::request_body(request));
        let response = execution
            .bind_http(RuntimeExecutionRoute::SessionHarnessOpenAiHttp, req)
            .send("Bedrock Responses")
            .await
            .map_err(errors::transport)?;
        Ok((response, fingerprint))
    }

    async fn checked_response(
        &self,
        response: reqwest::Response,
        fingerprint: &str,
    ) -> Result<reqwest::Response> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let error = errors::classify_response(response).await;
        if matches!(
            ProviderError::from_daemon_error(&error).map(|error| error.class),
            Some(ProviderErrorClass::CreditExhausted)
        ) {
            self.credential.mark_exhausted(fingerprint, status);
        }
        Err(error)
    }
}

fn usage(value: &Value) -> TokenUsage {
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

fn response_error(value: &Value) -> DaemonError {
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
    errors::from_http(status, &error.to_string(), None).into_daemon_error()
}

fn output_items(response: &Value) -> (String, Vec<ToolCall>) {
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

fn completed_response(response: &Value) -> Result<ChatResponse> {
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

async fn read_response_stream(
    response: reqwest::Response,
    chunks: &mpsc::Sender<StreamChunk>,
    cancel: &CancellationToken,
) -> Result<ChatResponse> {
    let mut stream = response.bytes_stream();
    let mut pending = Vec::<u8>::new();
    let mut event_data = String::new();
    let mut content = String::new();
    let mut calls = Vec::<ToolCall>::new();
    loop {
        let next = tokio::select! {
            () = cancel.cancelled() => return Err(DaemonError::Process("Bedrock request cancelled".into())),
            next = stream.next() => next,
        };
        let Some(bytes) = next else { break };
        let bytes = bytes.map_err(|_| {
            ProviderError {
                class: ProviderErrorClass::Transient,
                http_status: None,
                retry_after_ms: None,
                detail_code: "stream_disconnected".into(),
            }
            .into_daemon_error()
        })?;
        pending.extend_from_slice(&bytes);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line = pending.drain(..=end).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(data) = line.strip_prefix("data:") {
                event_data.push_str(data.trim_start());
            } else if line.is_empty() && !event_data.is_empty() {
                let value: Value = serde_json::from_str(&event_data)
                    .map_err(|_| DaemonError::Process("invalid Bedrock Responses event".into()))?;
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
                        return Err(response_error(&value));
                    }
                    _ => {}
                }
            }
        }
    }
    Err(ProviderError {
        class: ProviderErrorClass::Transient,
        http_status: None,
        retry_after_ms: None,
        detail_code: "stream_disconnected".into(),
    }
    .into_daemon_error())
}

#[async_trait::async_trait]
impl ApiProvider for OpenAiResponsesProvider {
    async fn chat(
        &self,
        request: &ChatRequest,
        execution: ModelExecutionCapability,
    ) -> Result<ChatResponse> {
        let (response, fingerprint) = self.send_request(request, execution).await?;
        let response = self.checked_response(response, &fingerprint).await?;
        let value: Value = response
            .json()
            .await
            .map_err(|_| DaemonError::Process("invalid Bedrock Responses body".into()))?;
        completed_response(&value)
    }

    async fn stream_chat(
        &self,
        request: &ChatRequest,
        chunks: mpsc::Sender<StreamChunk>,
        cancel: &CancellationToken,
        execution: ModelExecutionCapability,
    ) -> Result<ChatResponse> {
        let (response, fingerprint) = self.send_request(request, execution).await?;
        let response = self.checked_response(response, &fingerprint).await?;
        read_response_stream(response, &chunks, cancel).await
    }

    fn supports_native_tools(&self) -> bool {
        true
    }

    fn supports_image_input(&self, model: &str) -> bool {
        crate::provider_capabilities::supports_image_input(model)
    }

    fn web_capabilities(&self, model: &str) -> WebCapabilities {
        WebCapabilities::openai_responses(self.base_url.clone(), model)
    }

    fn name(&self) -> &'static str {
        "bedrock"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::session::harness::tools::HarnessTool;
    use crate::session::harness::types::{ChatMessage, HarnessToolSpec, ToolContentBlock};
    use std::sync::Arc;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

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
        }
    }

    fn execution() -> ModelExecutionCapability {
        ModelExecutionCapability::for_test(RuntimeExecutionRoute::SessionHarnessOpenAiHttp)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
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
            tools: vec![crate::session::harness::tools::apply_patch::ApplyPatchTool.to_spec()],
        };
        let body = OpenAiResponsesProvider::request_body(&request);

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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn responses_request_negotiates_and_replays_hosted_web_search() {
        let provider = provider_for_request_body();
        let mut request = request(vec![ChatMessage::user("search")], false);
        request.tools = provider
            .web_capabilities("global.openai.gpt-5.6-sol")
            .specs();
        let body = OpenAiResponsesProvider::request_body(&request);
        assert_eq!(body["tools"], json!([{"type":"web_search"}]));

        let search_call = json!({
            "type":"web_search_call",
            "id":"websearch-hosted",
            "status":"completed",
            "action":{"type":"search", "query":"harness hosted tools"}
        });
        let mut assistant = ChatMessage::assistant("searching");
        assistant.tool_calls.push(ToolCall {
            id: "websearch-hosted".into(),
            name: "web_search".into(),
            arguments: serde_json::to_string(&search_call["action"]).unwrap(),
            hosted: true,
            hosted_result: Some(search_call.clone()),
        });
        let result = ChatMessage::tool_result_blocks(
            "websearch-hosted",
            vec![ToolContentBlock::ServerToolResult {
                result: search_call.clone(),
            }],
            false,
        );
        request.messages = vec![assistant, result];
        request.tools = Vec::new();
        let replay = OpenAiResponsesProvider::request_body(&request);
        assert_eq!(
            replay["input"][0],
            json!({"role":"assistant", "content":"searching"})
        );
        assert_eq!(replay["input"][1], search_call);
        assert_eq!(replay["input"].as_array().unwrap().len(), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
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

    fn provider_for_request_body() -> OpenAiResponsesProvider {
        OpenAiResponsesProvider::with_config(
            "https://bedrock-runtime.test.amazonaws.com/openai/v1".into(),
            ApiCredential::explicit(Some("test-key")),
        )
        .unwrap()
    }

    fn provider(server: &MockServer) -> OpenAiResponsesProvider {
        OpenAiResponsesProvider::with_config(
            server.uri(),
            ApiCredential::explicit(Some("test-bedrock-key")),
        )
        .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn request_body_serializes_text_image_and_error_tool_outputs() {
        let image = crate::session::harness::tools::view_image::test_png_image_block(Some("low"));
        let ToolContentBlock::Image {
            media_type, data, ..
        } = &image
        else {
            panic!("test image block");
        };
        let body = OpenAiResponsesProvider::request_body(&request(
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn request_body_and_stream_round_trip_text_tools_and_usage() {
        let server = MockServer::start().await;
        let final_response = json!({
            "status":"completed",
            "output":[
                {"type":"message","content":[{"type":"output_text","text":"Hello world"}]},
                {"type":"function_call","call_id":"call-2","name":"shell","arguments":"{\"command\":\"pwd\"}"},
            ],
            "usage":{
                "input_tokens":50,"output_tokens":12,"total_tokens":62,
                "input_tokens_details":{"cached_tokens":20},
                "output_tokens_details":{"reasoning_tokens":4},
                "cache_write_tokens":3,
            },
        });
        let body = format!(
            "event: response.output_text.delta\ndata: {}\n\n\
             event: response.output_item.done\ndata: {}\n\n\
             event: response.completed\ndata: {}\n\n",
            json!({"type":"response.output_text.delta","delta":"Hello world"}),
            json!({"type":"response.output_item.done","item":final_response["output"][1]}),
            json!({"type":"response.completed","response":final_response}),
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let mut assistant = ChatMessage::assistant("calling shell");
        assistant.tool_calls.push(ToolCall {
            id: "call-1".into(),
            name: "shell".into(),
            arguments: r#"{"command":"ls"}"#.into(),
            hosted: false,
            hosted_result: None,
        });
        let request = request(
            vec![
                ChatMessage::system("Be helpful"),
                ChatMessage::user("Inspect"),
                assistant,
                ChatMessage::tool_error_result("call-1", "permission denied"),
            ],
            true,
        );
        let (tx, mut rx) = mpsc::channel(8);
        let result = provider(&server)
            .stream_chat(&request, tx, &CancellationToken::new(), execution())
            .await
            .unwrap();
        assert_eq!(result.content, "Hello world");
        assert_eq!(result.tool_calls[0].id, "call-2");
        assert_eq!(result.tool_calls[0].name, "shell");
        assert_eq!(result.usage.prompt_tokens, 50);
        assert_eq!(result.usage.completion_tokens, 12);
        assert_eq!(result.usage.total_tokens, 62);
        assert_eq!(result.usage.cache_read_tokens, 20);
        assert_eq!(result.usage.cache_creation_tokens, 3);
        assert_eq!(result.usage.reasoning_tokens, 4);
        assert_eq!(rx.recv().await.unwrap().delta_text, "Hello world");

        let recorded = server.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(
            recorded[0].headers.get("authorization").unwrap(),
            "Bearer test-bedrock-key"
        );
        assert_eq!(sent["model"], "global.openai.gpt-5.6-sol");
        assert_eq!(sent["reasoning"]["effort"], "high");
        assert_eq!(sent["max_output_tokens"], 128);
        assert_eq!(sent["tools"][0]["type"], "function");
        assert_eq!(sent["input"][2]["role"], "assistant");
        assert_eq!(sent["input"][3]["type"], "function_call");
        assert_eq!(sent["input"][3]["call_id"], "call-1");
        assert_eq!(sent["input"][4]["type"], "function_call_output");
        assert_eq!(sent["input"][4]["call_id"], "call-1");
        assert_eq!(
            serde_json::from_str::<Value>(sent["input"][4]["output"].as_str().unwrap()).unwrap(),
            json!({"error":{"type":"tool_execution","message":"permission denied"}}),
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn non_streaming_response_and_http_error_classes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"completed",
                "output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}],
                "usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5},
            })))
            .mount(&server)
            .await;
        let response = provider(&server)
            .chat(
                &request(vec![ChatMessage::user("hello")], false),
                execution(),
            )
            .await
            .unwrap();
        assert_eq!(response.content, "done");
        assert_eq!(response.usage.total_tokens, 5);

        for (status, body, class) in [
            (401, "invalid_api_key", ProviderErrorClass::Auth),
            (402, "payment required", ProviderErrorClass::CreditExhausted),
            (429, "rate limit", ProviderErrorClass::RateLimited),
            (503, "overloaded", ProviderErrorClass::Overloaded),
            (
                400,
                "context_length_exceeded",
                ProviderErrorClass::ContextTooLong,
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/responses"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            let error = provider(&server)
                .chat(
                    &request(vec![ChatMessage::user("hello")], false),
                    execution(),
                )
                .await
                .unwrap_err();
            assert_eq!(
                ProviderError::from_daemon_error(&error).unwrap().class,
                class
            );
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"incomplete",
                "incomplete_details":{"reason":"context_length_exceeded"},
            })))
            .mount(&server)
            .await;
        let error = provider(&server)
            .chat(
                &request(vec![ChatMessage::user("hello")], false),
                execution(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            ProviderError::from_daemon_error(&error).unwrap().class,
            ProviderErrorClass::ContextTooLong
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn bedrock_vault_rotation_reaches_the_next_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status":"completed", "output":[{"type":"message","content":[
                    {"type":"output_text","text":"ok"}
                ]}]
            })))
            .mount(&server)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let vault =
            crate::vault::VaultHandleBuilder::new(Arc::new(crate::vault::VaultSettings::default()))
                .dir(directory.path().join("vault"))
                .env(|_| None)
                .open()
                .unwrap();
        vault
            .set(crate::vault::Slot::Bedrock, "bedrock-key-old")
            .unwrap();
        let provider = OpenAiResponsesProvider::with_config(
            server.uri(),
            ApiCredential::VaultSlotOnly {
                vault: vault.clone(),
                slot: crate::vault::Slot::Bedrock,
            },
        )
        .unwrap();
        let request = request(vec![ChatMessage::user("hello")], false);
        assert_eq!(
            provider.chat(&request, execution()).await.unwrap().content,
            "ok"
        );
        vault
            .set(crate::vault::Slot::Bedrock, "bedrock-key-new")
            .unwrap();
        assert_eq!(
            provider.chat(&request, execution()).await.unwrap().content,
            "ok"
        );
        let sent = server.received_requests().await.unwrap();
        assert_eq!(
            sent[0].headers.get("authorization").unwrap(),
            "Bearer bedrock-key-old"
        );
        assert_eq!(
            sent[1].headers.get("authorization").unwrap(),
            "Bearer bedrock-key-new"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn streamed_failure_classifies_embedded_quota_and_rate_limit() {
        for (code, class) in [
            ("insufficient_quota", ProviderErrorClass::CreditExhausted),
            ("rate_limit_exceeded", ProviderErrorClass::RateLimited),
        ] {
            let server = MockServer::start().await;
            let body = format!(
                "data: {}\n\n",
                json!({
                    "type":"response.failed", "response":{"error":{"code":code}},
                })
            );
            Mock::given(method("POST"))
                .and(path("/responses"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(body),
                )
                .mount(&server)
                .await;
            let (tx, _) = mpsc::channel(1);
            let error = provider(&server)
                .stream_chat(
                    &request(vec![ChatMessage::user("hello")], true),
                    tx,
                    &CancellationToken::new(),
                    execution(),
                )
                .await
                .unwrap_err();
            assert_eq!(
                ProviderError::from_daemon_error(&error).unwrap().class,
                class
            );
        }
    }
}
