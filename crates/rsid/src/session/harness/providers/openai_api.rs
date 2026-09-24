//! OpenAI Chat Completions API provider.
//!
//! Extracted from openai.rs::run_agentic_loop with generalized types.
//! Covers OpenAI proper and serves as the base for CompatibleProvider.

// Provider layer is built ahead of resolve_provider() wiring (Phase 6+).
#![allow(dead_code)]

use crate::error::DaemonError;
use crate::model_control::ModelExecutionCapability;
use crate::session::harness::api_key::{OPENAI_ENV_VARS, resolve_api_key};
use crate::session::harness::provider::{ApiProvider, Result};
use crate::session::harness::sse::read_openai_sse_stream;
use crate::session::harness::types::*;
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub struct OpenAiApiProvider {
    http: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
    quirks: ProviderQuirks,
}

impl OpenAiApiProvider {
    pub fn new(explicit_key: Option<&str>) -> Result<Self> {
        let api_key = resolve_api_key(explicit_key, OPENAI_ENV_VARS);
        let base_url = std::env::var("OPENAI_API_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| DaemonError::Process(format!("HTTP client build error: {e}")))?;

        Ok(Self {
            http,
            api_key,
            base_url,
            quirks: ProviderQuirks {
                native_tools: true,
                ..Default::default()
            },
        })
    }

    /// Create with explicit base URL and key (for compatible providers).
    pub fn with_config(
        base_url: String,
        api_key: Option<String>,
        quirks: ProviderQuirks,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| DaemonError::Process(format!("HTTP client build error: {e}")))?;

        Ok(Self {
            http,
            api_key,
            base_url,
            quirks,
        })
    }

    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let mut messages: Vec<Value> = Vec::new();

        for msg in &request.messages {
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
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": msg.tool_call_id.as_deref().unwrap_or(""),
                        "content": msg.content,
                    }));
                }
            }
        }

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
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = self.build_request_body(request);

        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .json(&body);

        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = execution
            .bind_http(
                crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
                req,
            )
            .send("OpenAI")
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(DaemonError::Process(format!("OpenAI API {status}: {body}")));
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

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut streaming_request = request.clone();
        streaming_request.stream = true;
        let body = self.build_request_body(&streaming_request);

        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .json(&body);

        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = execution
            .bind_http(
                crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
                req,
            )
            .send("OpenAI")
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(DaemonError::Process(format!(
                "OpenAI API {status}: {body_text}"
            )));
        }

        let (content, acc_tool_calls, usage, stop_reason) =
            read_openai_sse_stream(resp, &chunk_tx, cancel).await?;

        let tool_calls: Vec<ToolCall> = acc_tool_calls
            .into_iter()
            .map(|tc| ToolCall {
                id: tc.id,
                name: tc.name,
                arguments: tc.arguments,
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

    fn name(&self) -> &str {
        "openai"
    }

    fn supports_streaming(&self) -> bool {
        !self.quirks.disable_streaming
    }
}
