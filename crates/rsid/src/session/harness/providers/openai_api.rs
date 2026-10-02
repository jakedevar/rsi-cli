//! OpenAI Chat Completions API provider.
//!
//! Extracted from openai.rs::run_agentic_loop with generalized types.
//! Covers OpenAI proper and serves as the base for CompatibleProvider.

// Provider layer is built ahead of resolve_provider() wiring (Phase 6+).
#![allow(dead_code)]

use crate::error::DaemonError;
use crate::model_control::ModelExecutionCapability;
use crate::session::harness::api_key::ApiCredential;
use crate::session::harness::errors::{self, ProviderError, ProviderErrorClass};
use crate::session::harness::provider::{ApiProvider, Result};
use crate::session::harness::sse::read_openai_sse_stream;
use crate::session::harness::types::*;
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub struct OpenAiApiProvider {
    http: reqwest::Client,
    credential: ApiCredential,
    base_url: String,
    quirks: ProviderQuirks,
    execution_route: crate::model_control::registry::RuntimeExecutionRoute,
}

impl OpenAiApiProvider {
    async fn send_openai_request(
        &self,
        request: &ChatRequest,
        execution: ModelExecutionCapability,
    ) -> Result<(reqwest::Response, Option<String>)> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = self.build_request_body(request);
        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .json(&body);
        let key = self.credential.current();
        let fingerprint = key.as_ref().map(crate::vault::SecretString::fingerprint);
        if let Some(key) = key {
            req = req.bearer_auth(key.expose());
        }
        let response = execution
            .bind_http(
                crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
                req,
            )
            .send("OpenAI")
            .await
            .map_err(errors::transport)?;
        Ok((response, fingerprint))
    }

    async fn send_openrouter_request(
        &self,
        request: &ChatRequest,
        execution: ModelExecutionCapability,
    ) -> Result<(reqwest::Response, Option<String>)> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = self.build_request_body(request);
        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .json(&body);
        let key = self.credential.current().ok_or_else(|| {
            DaemonError::OpenAiApiError("OpenRouter credential unavailable".to_string())
        })?;
        let fingerprint = key.fingerprint();
        req = req.bearer_auth(key.expose());
        let response = execution
            .bind_http(
                crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenRouterHttp,
                req,
            )
            .send("OpenRouter")
            .await
            .map_err(errors::transport)?;
        Ok((response, Some(fingerprint)))
    }
    pub fn new(explicit_key: Option<&str>) -> Result<Self> {
        let credential = ApiCredential::for_slot(explicit_key, Some(crate::vault::Slot::Openai));
        let base_url = std::env::var("OPENAI_API_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| DaemonError::Process(format!("HTTP client build error: {e}")))?;

        Ok(Self {
            http,
            credential,
            base_url,
            execution_route:
                crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
            quirks: ProviderQuirks {
                native_tools: true,
                ..Default::default()
            },
        })
    }

    /// Create with explicit base URL and credential (for compatible
    /// providers). The credential is evaluated on every request.
    pub fn with_config(
        base_url: String,
        credential: ApiCredential,
        quirks: ProviderQuirks,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| DaemonError::Process(format!("HTTP client build error: {e}")))?;

        Ok(Self {
            http,
            credential,
            base_url,
            quirks,
            execution_route:
                crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
        })
    }

    pub fn openrouter() -> Result<Self> {
        let mut provider = Self::with_config(
            crate::openrouter::OPENROUTER_API_BASE_URL.to_string(),
            ApiCredential::for_slot_only(crate::vault::Slot::Openrouter),
            ProviderQuirks {
                native_tools: true,
                ..Default::default()
            },
        )?;
        provider.execution_route =
            crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenRouterHttp;
        Ok(provider)
    }

    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        let mut pending_image_messages: Vec<Value> = Vec::new();

        for msg in &request.messages {
            // Chat Completions requires every tool reply for one assistant turn
            // before the next user message. Defer image parts until the batch ends.
            if msg.role != MessageRole::Tool {
                messages.append(&mut pending_image_messages);
            }
            match msg.role {
                MessageRole::System => {
                    if !self.quirks.merge_system_into_user {
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
        if self.quirks.merge_system_into_user {
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

        if !tools.is_empty() && self.quirks.native_tools {
            body["tools"] = json!(tools);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = json!(max);
        }
        // OpenRouter normalises reasoning control under `reasoning.effort`.
        if self.execution_route
            == crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenRouterHttp
            && let Some(effort) = request
                .reasoning_effort
                .as_deref()
                .filter(|effort| matches!(*effort, "low" | "medium" | "high"))
        {
            body["reasoning"] = json!({ "effort": effort });
        }
        if request.stream && !self.quirks.disable_streaming {
            body["stream"] = json!(true);
            body["stream_options"] = json!({"include_usage": true});
        }

        body
    }
}

#[async_trait::async_trait]
impl ApiProvider for OpenAiApiProvider {
    async fn chat(
        &self,
        request: &ChatRequest,
        execution: ModelExecutionCapability,
    ) -> Result<ChatResponse> {
        let (resp, fingerprint) = if self.execution_route
            == crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenRouterHttp
        {
            self.send_openrouter_request(request, execution).await?
        } else {
            self.send_openai_request(request, execution).await?
        };

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let error = errors::classify_response(resp).await;
            if matches!(
                ProviderError::from_daemon_error(&error).map(|error| error.class),
                Some(ProviderErrorClass::CreditExhausted)
            ) && let Some(fingerprint) = fingerprint
            {
                self.credential.mark_exhausted(&fingerprint, status);
            }
            return Err(error);
        }

        let json: Value = resp
            .json()
            .await
            .map_err(|e| DaemonError::Process(format!("OpenAI parse error: {e}")))?;

        let choice = json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .ok_or_else(|| DaemonError::Process("No choices in OpenAI response".into()))?;

        let message = choice
            .get("message")
            .ok_or_else(|| DaemonError::Process("No message in choice".into()))?;

        let content = message
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

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

        let usage = json
            .get("usage")
            .map(|u| TokenUsage {
                prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                completion_tokens: u
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                cost_usd: crate::session::harness::sse::reported_cost_usd(u),
                ..Default::default()
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
            reasoning_content: None,
            stop_reason,
        })
    }

    async fn stream_chat(
        &self,
        request: &ChatRequest,
        chunk_tx: mpsc::Sender<StreamChunk>,
        cancel: &tokio_util::sync::CancellationToken,
        execution: ModelExecutionCapability,
    ) -> Result<ChatResponse> {
        if self.quirks.disable_streaming {
            let response = self.chat(request, execution).await?;
            let _ = chunk_tx
                .send(StreamChunk {
                    delta_text: response.content.clone(),
                    tool_call_deltas: Vec::new(),
                    is_final: true,
                    usage: Some(response.usage.clone()),
                    stop_reason: response.stop_reason.clone(),
                })
                .await;
            return Ok(response);
        }

        let (resp, fingerprint) = if self.execution_route
            == crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenRouterHttp
        {
            self.send_openrouter_request(request, execution).await?
        } else {
            self.send_openai_request(request, execution).await?
        };

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let error = errors::classify_response(resp).await;
            if matches!(
                ProviderError::from_daemon_error(&error).map(|error| error.class),
                Some(ProviderErrorClass::CreditExhausted)
            ) && let Some(fingerprint) = fingerprint
            {
                self.credential.mark_exhausted(&fingerprint, status);
            }
            return Err(error);
        }

        let (content, acc_tool_calls, usage, stop_reason) =
            read_openai_sse_stream(resp, &chunk_tx, cancel).await?;

        let tool_calls: Vec<ToolCall> = acc_tool_calls
            .into_iter()
            .map(|tc| ToolCall {
                id: tc.id,
                name: tc.name,
                arguments: tc.arguments,
                hosted: false,
                hosted_result: None,
            })
            .collect();

        Ok(ChatResponse {
            content,
            tool_calls,
            usage,
            reasoning_content: None,
            stop_reason,
        })
    }

    fn supports_native_tools(&self) -> bool {
        self.quirks.native_tools
    }

    fn supports_image_input(&self, model: &str) -> bool {
        crate::provider_capabilities::supports_image_input(model)
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn supports_streaming(&self) -> bool {
        !self.quirks.disable_streaming
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            messages,
            model: "test-model".into(),
            temperature: None,
            max_tokens: None,
            tools: Vec::new(),
            stream: false,
            reasoning_effort: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn tool_results_use_text_tool_messages_and_user_image_parts() {
        let provider = OpenAiApiProvider::with_config(
            "http://localhost".into(),
            ApiCredential::None,
            ProviderQuirks::default(),
        )
        .unwrap();
        let image = crate::session::harness::tools::view_image::test_png_image_block(Some("low"));
        let ToolContentBlock::Image {
            media_type, data, ..
        } = &image
        else {
            panic!("test image block");
        };
        let body = provider.build_request_body(&request(vec![
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
        ]));
        assert_eq!(
            body["messages"][0],
            json!({
                "role": "tool", "tool_call_id": "legacy", "content": "plain text"
            })
        );
        assert_eq!(
            body["messages"][1],
            json!({
                "role": "tool", "tool_call_id": "text", "content": "typed text"
            })
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

    /// #1061: OpenRouter requests carry the reasoning effort as
    /// `reasoning.effort`; other chat-completions routes do not.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn openrouter_body_carries_reasoning_effort() {
        let mut req = request(vec![ChatMessage::user("hi")]);
        req.reasoning_effort = Some("low".into());
        let body = OpenAiApiProvider::openrouter()
            .unwrap()
            .build_request_body(&req);
        assert_eq!(body["reasoning"], json!({"effort": "low"}));
        let body = OpenAiApiProvider::new(Some("key"))
            .unwrap()
            .build_request_body(&req);
        assert!(body.get("reasoning").is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn openrouter_harness_uses_openrouter_endpoint_and_vault_slot() {
        let provider = OpenAiApiProvider::openrouter().unwrap();
        assert_eq!(
            provider.base_url,
            crate::openrouter::OPENROUTER_API_BASE_URL
        );
        assert_eq!(
            provider.execution_route,
            crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenRouterHttp
        );
        assert!(matches!(
            provider.credential,
            ApiCredential::VaultSlotOnly {
                slot: crate::vault::Slot::Openrouter,
                ..
            }
        ));
        assert!(provider.supports_native_tools());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn chat_completions_local_unknown_and_openrouter_routes_have_no_hosted_tools() {
        for base_url in [
            "http://localhost:11434/v1",
            "https://unknown-compatible-route.test/v1",
            "https://openrouter.ai/api/v1",
        ] {
            let provider = OpenAiApiProvider::with_config(
                base_url.into(),
                ApiCredential::None,
                ProviderQuirks::default(),
            )
            .unwrap();
            let capabilities = provider.web_capabilities("test-model");
            assert!(capabilities.tools.is_empty(), "{base_url}");

            let mut request = request(vec![ChatMessage::user("search")]);
            request.tools = capabilities.specs();
            let body = provider.build_request_body(&request);
            assert!(body.get("tools").is_none(), "{base_url}");
        }
    }
}
