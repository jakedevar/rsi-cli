//! Anthropic Messages API provider.
//!
//! Makes direct HTTP calls to `{base_url}/v1/messages` using reqwest.
//! Handles Anthropic-specific request format: system message as top-level field,
//! tool definitions in Anthropic format, and model-appropriate extended thinking.

// Provider layer is built ahead of resolve_provider() wiring (Phase 6+).
#![allow(dead_code)]

use crate::error::DaemonError;
use crate::model_control::ModelExecutionCapability;
use crate::session::harness::api_key::{ANTHROPIC_ENV_VARS, resolve_api_key};
use crate::session::harness::provider::{ApiProvider, Result};
use crate::session::harness::sse::{AccumulatedToolCall, read_anthropic_sse_stream};
use crate::session::harness::types::*;
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub struct AnthropicProvider {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

pub(crate) fn uses_adaptive_thinking(model: &str) -> bool {
    // Entries are matched as prefixes, so `claude-fable-5` covers the
    // offered `claude-fable-5-1` and any later Fable 5.x alongside the
    // retired bare id — both reject `temperature`/`budget_tokens`.
    [
        "claude-fable-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-sonnet-5",
    ]
    .iter()
    .any(|prefix| {
        model == *prefix
            || model
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('-'))
    })
}

impl AnthropicProvider {
    pub fn new(explicit_key: Option<&str>) -> Result<Self> {
        let api_key = resolve_api_key(explicit_key, ANTHROPIC_ENV_VARS).ok_or_else(|| {
            DaemonError::Process("No Anthropic API key found. Set ANTHROPIC_API_KEY.".into())
        })?;

        let base_url = std::env::var("ANTHROPIC_API_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".into());

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| DaemonError::Process(format!("HTTP client build error: {e}")))?;

        Ok(Self {
            http,
            api_key,
            base_url,
        })
    }

    /// Build Anthropic-format request body.
    fn build_request_body(&self, request: &ChatRequest) -> Value {
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
                        "content": msg.content,
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
                            let input: Value =
                                serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                            content_blocks.push(json!({
                                "type": "tool_use",
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
                    messages.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""),
                            "content": msg.content,
                        }],
                    }));
                }
            }
        }

        // Build tools array in Anthropic format
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                let schema: Value =
                    serde_json::from_str(&t.parameters_json).unwrap_or(json!({"type": "object"}));
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": schema,
                })
            })
            .collect();

        let mut body = json!({
            "model": request.model,
            "messages": messages,
            "max_tokens": request.max_tokens.unwrap_or(8192),
        });

        if let Some(ref sys) = system_content {
            body["system"] = json!(sys);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        let adaptive_thinking = uses_adaptive_thinking(&request.model);
        if !adaptive_thinking {
            if let Some(temp) = request.temperature {
                body["temperature"] = json!(temp);
            }
        }
        if request.stream {
            body["stream"] = json!(true);
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

    fn auth_header(&self) -> (&str, String) {
        // OAuth tokens use Bearer, standard keys use x-api-key
        if self.api_key.starts_with("sk-ant-oat01-") {
            ("Authorization", format!("Bearer {}", self.api_key))
        } else {
            ("x-api-key", self.api_key.clone())
        }
    }

    fn convert_tool_calls(accumulated: Vec<AccumulatedToolCall>) -> Vec<ToolCall> {
        accumulated
            .into_iter()
            .map(|tc| ToolCall {
                id: tc.id,
                name: tc.name,
                arguments: tc.arguments,
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl ApiProvider for AnthropicProvider {
    async fn chat(
        &self,
        request: &ChatRequest,
        execution: ModelExecutionCapability,
    ) -> Result<ChatResponse> {
        let url = format!("{}/v1/messages", self.base_url);
        let mut req_body = self.build_request_body(request);
        // Ensure non-streaming
        req_body.as_object_mut().map(|o| o.remove("stream"));

        let (header_name, header_value) = self.auth_header();
        let request = self
            .http
            .post(&url)
            .header(header_name, header_value)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&req_body);
        let resp = execution
            .bind_http(
                crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessAnthropicHttp,
                request,
            )
            .send("Anthropic")
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(DaemonError::Process(format!(
                "Anthropic API {status}: {body}"
            )));
        }

        let json: Value = resp
            .json()
            .await
            .map_err(|e| DaemonError::Process(format!("Anthropic parse error: {e}")))?;

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
                        });
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
                total_tokens: 0, // Computed below
            }
        } else {
            TokenUsage::default()
        };

        let stop_reason = json
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .map(String::from);

        Ok(ChatResponse {
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
        let url = format!("{}/v1/messages", self.base_url);
        let mut streaming_request = request.clone();
        streaming_request.stream = true;
        let body = self.build_request_body(&streaming_request);

        let (header_name, header_value) = self.auth_header();
        let request = self
            .http
            .post(&url)
            .header(header_name, header_value)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body);
        let resp = execution
            .bind_http(
                crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessAnthropicHttp,
                request,
            )
            .send("Anthropic")
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();

            if status.is_server_error() {
                return Err(DaemonError::StreamFallbackRequired(format!(
                    "Anthropic API {status}: {body_text}"
                )));
            }

            return Err(DaemonError::Process(format!(
                "Anthropic API {status}: {body_text}"
            )));
        }

        let (content, tool_calls, usage, stop_reason) =
            read_anthropic_sse_stream(resp, &chunk_tx, cancel).await?;

        // Send final chunk
        let _ = chunk_tx
            .send(StreamChunk {
                delta_text: String::new(),
                tool_call_deltas: Vec::new(),
                is_final: true,
                usage: Some(usage.clone()),
                stop_reason: stop_reason.clone(),
            })
            .await;

        Ok(ChatResponse {
            content,
            tool_calls: Self::convert_tool_calls(tool_calls),
            usage,
            reasoning_content: None,
            stop_reason,
        })
    }

    fn supports_native_tools(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "anthropic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> AnthropicProvider {
        AnthropicProvider {
            http: reqwest::Client::new(),
            api_key: String::new(),
            base_url: String::new(),
        }
    }

    fn request(model: &str, effort: Option<&str>) -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage::user("test")],
            model: model.to_string(),
            temperature: Some(0.2),
            max_tokens: Some(512),
            tools: Vec::new(),
            stream: false,
            reasoning_effort: effort.map(str::to_string),
        }
    }

    #[test]
    fn current_models_use_adaptive_thinking_without_temperature_or_budget() {
        for model in [
            "claude-fable-5-1",
            "claude-fable-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-5",
        ] {
            let body = provider().build_request_body(&request(model, Some("xhigh")));

            assert_eq!(body["thinking"]["type"], "adaptive", "{model}");
            assert!(body["thinking"].get("budget_tokens").is_none(), "{model}");
            assert_eq!(body["output_config"]["effort"], "xhigh", "{model}");
            assert!(body.get("temperature").is_none(), "{model}");
        }
    }

    #[test]
    fn suffixed_current_models_use_adaptive_thinking() {
        let body = provider().build_request_body(&request("claude-opus-5-20260724", Some("max")));

        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "max");
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn current_models_without_effort_omit_thinking_and_output_config() {
        let body = provider().build_request_body(&request("claude-opus-5", None));

        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn older_models_keep_legacy_thinking_request_shape() {
        let body = provider().build_request_body(&request("claude-opus-4-6", Some("high")));

        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 8192);
        assert!(body.get("output_config").is_none());
        let temperature = body["temperature"]
            .as_f64()
            .expect("temperature is numeric");
        assert!((temperature - 0.2).abs() < 1e-6);
    }
}
