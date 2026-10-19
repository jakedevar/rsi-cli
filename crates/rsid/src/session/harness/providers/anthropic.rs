//! Anthropic Messages API provider.
//!
//! Makes direct HTTP calls to `{base_url}/v1/messages` using reqwest, or to
//! Amazon Bedrock's `InvokeModel` for Bedrock Claude model IDs (same Messages
//! body with `anthropic_version` instead of `model`, bearer API key, and an
//! AWS event-stream response).
//! Handles Anthropic-specific request format: system message as top-level field,
//! tool definitions in Anthropic format, and model-appropriate extended thinking.

// Provider layer is built ahead of resolve_provider() wiring (Phase 6+).
#![allow(dead_code)]

use crate::error::DaemonError;
use crate::model_control::ModelExecutionCapability;
use crate::session::harness::api_key::ApiCredential;
use crate::session::harness::bedrock_stream::read_bedrock_anthropic_stream;
use crate::session::harness::errors::{self, ProviderError, ProviderErrorClass, ProviderErrorExt};
use crate::session::harness::provider::{ApiProvider, Result, WebCapabilities};
use crate::session::harness::sse::{AccumulatedToolCall, read_anthropic_sse_stream};
use crate::session::harness::types::*;
use rsid_harness::providers::anthropic as anthropic_wire;
use serde_json::Value;
use tokio::sync::mpsc;

pub struct AnthropicProvider {
    http: reqwest::Client,
    credential: ApiCredential,
    base_url: String,
    transport: Transport,
}

/// Where Messages requests go.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Transport {
    /// `{base_url}/v1/messages` with an Anthropic key or OAuth token.
    Direct,
    /// `bedrock-runtime` `InvokeModel` in `region` with a Bedrock API key.
    Bedrock { region: String },
}

use rsid_harness::providers::anthropic::CONTEXT_MANAGEMENT_BETA;

pub(crate) use crate::store_support::provider_defaults::uses_adaptive_thinking;

impl AnthropicProvider {
    pub fn new(explicit_key: Option<&str>) -> Result<Self> {
        let base_url = std::env::var("ANTHROPIC_API_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".into());
        Self::with_credential(
            base_url,
            ApiCredential::for_slot(explicit_key, Some(crate::vault::Slot::Anthropic)),
        )
    }

    /// Build against `base_url` with a per-request credential. Launch still
    /// fails fast when no key resolves now.
    pub fn with_credential(base_url: String, credential: ApiCredential) -> Result<Self> {
        if credential.current().is_none() {
            return Err(Self::missing_key());
        }
        let http = crate::session::harness::net::client()?;

        Ok(Self {
            http,
            credential,
            base_url,
            transport: Transport::Direct,
        })
    }

    /// Claude on Amazon Bedrock in `region`, authenticated with a Bedrock API
    /// key. Launch fails fast when no key resolves now.
    pub fn bedrock(region: String, credential: ApiCredential) -> Result<Self> {
        if credential.current().is_none() {
            return Err(DaemonError::Process(
                "Bedrock API key missing; set it in the RSI key vault or export AWS_BEARER_TOKEN_BEDROCK".into(),
            ));
        }
        let mut provider = Self::with_credential(String::new(), credential)?;
        provider.transport = Transport::Bedrock { region };
        Ok(provider)
    }

    /// Request URL for `model`.
    fn messages_url(&self, model: &str, stream: bool) -> String {
        match &self.transport {
            Transport::Direct => format!("{}/v1/messages", self.base_url),
            Transport::Bedrock { region } => {
                crate::bedrock::runtime_invoke_url(region, model, stream)
            }
        }
    }

    /// Transport-specific body: Bedrock takes the model from the URL, picks
    /// streaming by operation, and requires its own `anthropic_version`.
    fn transport_body(&self, request: &ChatRequest) -> Value {
        let body = self.build_request_body(request);
        if matches!(self.transport, Transport::Bedrock { .. }) {
            anthropic_wire::bedrock_body(body)
        } else {
            body
        }
    }

    fn missing_key() -> DaemonError {
        DaemonError::Process(
            "No Anthropic API key found. Set it in the RSI key vault or export ANTHROPIC_API_KEY."
                .into(),
        )
    }

    /// Whether this request carries server-side context editing. The edit is
    /// applied by the API to what it is sent; the client never rewrites an
    /// earlier message. Direct Anthropic API only (not Bedrock).
    fn context_editing_active(&self, request: &ChatRequest) -> bool {
        anthropic_wire::context_editing_active(request, self.transport == Transport::Direct)
    }

    /// Extra headers for this request: the context-management beta when the
    /// request carries `context_management`.
    fn beta_headers(&self, request: &ChatRequest) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        if self.context_editing_active(request) {
            headers.insert(
                "anthropic-beta",
                reqwest::header::HeaderValue::from_static(CONTEXT_MANAGEMENT_BETA),
            );
        }
        headers
    }

    /// Build Anthropic-format request body (the wire shape lives in `rsid-harness`).
    fn build_request_body(&self, request: &ChatRequest) -> Value {
        anthropic_wire::build_request_body(
            request,
            self.context_editing_active(request),
            uses_adaptive_thinking(&request.model),
        )
    }

    /// Resolved on every request, so a vault rotation applies to the next
    /// request of a live session.
    fn auth_header(&self) -> Result<(&'static str, String, String)> {
        let key = self.credential.current().ok_or_else(Self::missing_key)?;
        let fingerprint = key.fingerprint();
        let key = key.expose();
        // Bedrock API keys and OAuth tokens use Bearer, standard keys use x-api-key
        Ok(
            if key.starts_with("sk-ant-oat01-") || self.transport != Transport::Direct {
                ("Authorization", format!("Bearer {key}"), fingerprint)
            } else {
                ("x-api-key", key.to_owned(), fingerprint)
            },
        )
    }

    fn convert_tool_calls(accumulated: Vec<AccumulatedToolCall>) -> Vec<ToolCall> {
        anthropic_wire::convert_tool_calls(accumulated)
    }
}

#[async_trait::async_trait]
impl ApiProvider for AnthropicProvider {
    async fn chat(
        &self,
        request: &ChatRequest,
        execution: ModelExecutionCapability,
    ) -> Result<ChatResponse> {
        let url = self.messages_url(&request.model, false);
        let mut req_body = self.transport_body(request);
        // Ensure non-streaming
        req_body.as_object_mut().map(|o| o.remove("stream"));

        let (header_name, header_value, fingerprint) = self.auth_header()?;
        let request = self
            .http
            .post(&url)
            .header(header_name, header_value)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .headers(self.beta_headers(request))
            .json(&req_body)
            .timeout(crate::session::harness::net::total_timeout(false));
        let resp = execution
            .bind_http(
                crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessAnthropicHttp,
                request,
            )
            .send("Anthropic")
            .await
            .map_err(errors::transport)?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let error = errors::classify_response(resp).await;
            if matches!(
                ProviderError::from_daemon_error(&error).map(|error| error.class),
                Some(ProviderErrorClass::CreditExhausted)
            ) {
                self.credential.mark_exhausted(&fingerprint, status);
            }
            return Err(error);
        }

        let json: Value = resp
            .json()
            .await
            .map_err(|e| DaemonError::Process(format!("Anthropic parse error: {e}")))?;

        Ok(anthropic_wire::parse_chat_response(&json))
    }

    async fn stream_chat(
        &self,
        request: &ChatRequest,
        chunk_tx: mpsc::Sender<StreamChunk>,
        cancel: &tokio_util::sync::CancellationToken,
        execution: ModelExecutionCapability,
    ) -> Result<ChatResponse> {
        let url = self.messages_url(&request.model, true);
        let mut streaming_request = request.clone();
        streaming_request.stream = true;
        let body = self.transport_body(&streaming_request);

        let (header_name, header_value, fingerprint) = self.auth_header()?;
        let request = self
            .http
            .post(&url)
            .header(header_name, header_value)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .headers(self.beta_headers(request))
            .json(&body);
        let resp = execution
            .bind_http(
                crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessAnthropicHttp,
                request,
            )
            .send("Anthropic")
            .await
            .map_err(errors::transport)?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let error = errors::classify_response(resp).await;
            if matches!(
                ProviderError::from_daemon_error(&error).map(|error| error.class),
                Some(ProviderErrorClass::CreditExhausted)
            ) {
                self.credential.mark_exhausted(&fingerprint, status);
            }
            return Err(error);
        }

        let (content, tool_calls, usage, stop_reason) = match self.transport {
            Transport::Direct => read_anthropic_sse_stream(resp, &chunk_tx, cancel).await?,
            Transport::Bedrock { .. } => {
                read_bedrock_anthropic_stream(resp, &chunk_tx, cancel).await?
            }
        };

        // Send final chunk
        let _ = chunk_tx
            .send(StreamChunk {
                delta_text: String::new(),
                tool_call_deltas: Vec::new(),
                is_final: true,
                usage: Some(usage.clone()),
                stop_reason: stop_reason.clone(),
                warning: None,
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

    fn supports_image_input(&self, model: &str) -> bool {
        crate::provider_capabilities::supports_image_input(model)
    }

    fn web_capabilities(&self, model: &str) -> WebCapabilities {
        match self.transport {
            Transport::Direct => WebCapabilities::anthropic_messages(self.base_url.clone(), model),
            // Anthropic's hosted server tools are not offered through Bedrock.
            Transport::Bedrock { .. } => WebCapabilities::none(),
        }
    }

    fn name(&self) -> &str {
        "anthropic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_control::registry::RuntimeExecutionRoute;
    use rsid_harness::providers::anthropic::{
        BEDROCK_ANTHROPIC_VERSION, context_management_config,
    };
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn provider() -> AnthropicProvider {
        AnthropicProvider {
            http: reqwest::Client::new(),
            credential: ApiCredential::None,
            base_url: String::new(),
            transport: Transport::Direct,
        }
    }

    fn bedrock_provider() -> AnthropicProvider {
        AnthropicProvider {
            transport: Transport::Bedrock {
                region: "us-east-1".into(),
            },
            credential: ApiCredential::Fixed(crate::vault::SecretString::new(
                "bedrock-api-key-test-harness".into(),
            )),
            ..provider()
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn bedrock_transport_uses_invoke_model_body_url_and_bearer_key() {
        let provider = bedrock_provider();
        let model = "us.anthropic.claude-opus-5-5-v1:0";
        let mut streaming = request(model, Some("high"));
        streaming.stream = true;
        let body = provider.transport_body(&streaming);
        assert_eq!(body["anthropic_version"], BEDROCK_ANTHROPIC_VERSION);
        assert!(body.get("model").is_none());
        assert!(body.get("stream").is_none());
        // Bedrock Claude IDs get the same adaptive-thinking shape.
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(
            provider.messages_url(model, true),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/us.anthropic.claude-opus-5-5-v1%3A0/invoke-with-response-stream"
        );
        let (name, value, _) = provider.auth_header().unwrap();
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer bedrock-api-key-test-harness");

        // The direct transport keeps its body and endpoint.
        let direct = provider_with_key();
        let body = direct.transport_body(&request("claude-sonnet-5", None));
        assert_eq!(body["model"], "claude-sonnet-5");
        assert!(body.get("anthropic_version").is_none());
        assert_eq!(direct.auth_header().unwrap().0, "x-api-key");
    }

    fn provider_with_key() -> AnthropicProvider {
        AnthropicProvider {
            credential: ApiCredential::Fixed(crate::vault::SecretString::new(
                "sk-ant-api03-test".into(),
            )),
            ..provider()
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
            context_editing: false,
            tool_choice_none: false,
        }
    }

    fn execution() -> crate::model_control::ModelExecutionCapability {
        crate::model_control::ModelExecutionCapability::for_test(
            RuntimeExecutionRoute::SessionHarnessAnthropicHttp,
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn anthropic_stream_preserves_hosted_tool_result_for_replay() {
        let server = MockServer::start().await;
        let search_result = json!({
            "type": "web_search_tool_result",
            "tool_use_id": "srvtool-hosted",
            "content": [{"type": "web_search_result", "url": "https://example.com", "title": "Example"}],
            "encrypted_content": "byte-for-byte-encrypted-content",
            "encrypted_index": "byte-for-byte-encrypted-index"
        });
        let stream_body = format!(
            "event: message_start\ndata: {}\n\n\
             event: content_block_start\ndata: {}\n\n\
             event: content_block_stop\ndata: {}\n\n\
             event: content_block_start\ndata: {}\n\n\
             event: content_block_stop\ndata: {}\n\n\
             event: message_delta\ndata: {}\n\n\
             event: message_stop\ndata: {}\n\n",
            json!({"type":"message_start", "message":{"usage":{"input_tokens":12}}}),
            json!({
                "type":"content_block_start", "index":0,
                "content_block":{
                    "type":"server_tool_use", "id":"srvtool-hosted", "name":"web_search",
                    "input":{"query":"harness hosted tools"}
                }
            }),
            json!({"type":"content_block_stop", "index":0}),
            json!({"type":"content_block_start", "index":1, "content_block":search_result}),
            json!({"type":"content_block_stop", "index":1}),
            json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}, "usage":{"output_tokens":4}}),
            json!({"type":"message_stop"})
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(stream_body),
            )
            .mount(&server)
            .await;
        let provider = AnthropicProvider::with_credential(
            server.uri(),
            ApiCredential::explicit(Some("test-key")),
        )
        .unwrap();
        let mut request = request("claude-sonnet-5", None);
        request.stream = true;
        request.tools = provider.web_capabilities("claude-sonnet-5").specs();

        let (chunk_tx, _chunk_rx) = tokio::sync::mpsc::channel(8);
        let response = provider
            .stream_chat(
                &request,
                chunk_tx,
                &tokio_util::sync::CancellationToken::new(),
                execution(),
            )
            .await
            .unwrap();

        assert_eq!(response.tool_calls.len(), 1);
        let call = &response.tool_calls[0];
        assert_eq!(call.id, "srvtool-hosted");
        assert_eq!(call.name, "web_search");
        assert!(call.hosted);
        assert_eq!(call.hosted_result.as_ref().unwrap(), &search_result);

        let mut assistant = ChatMessage::assistant("");
        assistant.tool_calls.push(call.clone());
        let result = ChatMessage::tool_result_blocks(
            &call.id,
            vec![ToolContentBlock::ServerToolResult {
                result: search_result.clone(),
            }],
            false,
        );
        request.messages.extend([assistant, result]);
        let replay_body = provider.build_request_body(&request);
        assert_eq!(
            replay_body["messages"][1],
            json!({
                "role":"assistant", "content":[{
                    "type":"server_tool_use", "id":"srvtool-hosted", "name":"web_search",
                    "input":{"query":"harness hosted tools"}
                }]
            })
        );
        assert_eq!(replay_body["messages"][2]["role"], "assistant");
        assert_eq!(replay_body["messages"][2]["content"][0], search_result);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn anthropic_request_negotiates_explicit_hosted_tools() {
        let mut hosted_request = request("claude-sonnet-5", None);
        hosted_request.tools = provider().web_capabilities("claude-sonnet-5").specs();
        let body = provider().build_request_body(&hosted_request);

        assert_eq!(
            body["tools"],
            json!([
                {"type":"web_search_20250305", "name":"web_search"},
                {"type":"web_fetch_20250910", "name":"web_fetch"}
            ])
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn suffixed_current_models_use_adaptive_thinking() {
        let body = provider().build_request_body(&request("claude-opus-5-20260724", Some("max")));

        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "max");
        assert!(body.get("temperature").is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn current_models_without_effort_omit_thinking_and_output_config() {
        let body = provider().build_request_body(&request("claude-opus-5", None));

        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
        assert!(body.get("temperature").is_none());
    }

    fn editing_request(model: &str, editing: bool) -> ChatRequest {
        ChatRequest {
            context_editing: editing,
            tool_choice_none: false,
            ..request(model, None)
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn context_editing_is_sent_for_claude_on_the_direct_api_only() {
        let direct = provider_with_key();
        let on = editing_request("claude-opus-5-5", true);
        let body = direct.build_request_body(&on);
        assert_eq!(body["context_management"], context_management_config());
        let edit = &body["context_management"]["edits"][0];
        assert_eq!(edit["type"], "clear_tool_uses_20250919");
        assert_eq!(edit["keep"]["type"], "tool_uses");
        assert_eq!(edit["trigger"]["type"], "input_tokens");
        assert_eq!(edit["clear_at_least"]["type"], "input_tokens");
        let beta = direct.beta_headers(&on);
        assert_eq!(beta["anthropic-beta"], CONTEXT_MANAGEMENT_BETA);

        // Session setting off: neither the field nor the header.
        let off = editing_request("claude-opus-5-5", false);
        assert!(
            direct
                .build_request_body(&off)
                .get("context_management")
                .is_none()
        );
        assert!(direct.beta_headers(&off).is_empty());

        // Bedrock does not offer the beta: nothing is sent.
        let bedrock = bedrock_provider();
        let model = "us.anthropic.claude-opus-5-5-v1:0";
        let bedrock_on = editing_request(model, true);
        assert!(
            bedrock
                .transport_body(&bedrock_on)
                .get("context_management")
                .is_none()
        );
        assert!(bedrock.beta_headers(&bedrock_on).is_empty());

        // A non-Claude model id never carries it.
        let other = editing_request("gpt-5", true);
        assert!(
            direct
                .build_request_body(&other)
                .get("context_management")
                .is_none()
        );
        assert!(direct.beta_headers(&other).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn context_editing_beta_header_and_field_reach_the_wire() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(format!(
                        "event: message_start\ndata: {}\n\nevent: message_stop\ndata: {}\n\n",
                        json!({"type":"message_start", "message":{"usage":{"input_tokens":1}}}),
                        json!({"type":"message_stop"})
                    )),
            )
            .mount(&server)
            .await;
        let provider = AnthropicProvider::with_credential(
            server.uri(),
            ApiCredential::explicit(Some("test-key")),
        )
        .unwrap();
        for editing in [true, false] {
            let mut request = editing_request("claude-sonnet-5-5", editing);
            request.stream = true;
            let (tx, _rx) = tokio::sync::mpsc::channel(8);
            provider
                .stream_chat(
                    &request,
                    tx,
                    &tokio_util::sync::CancellationToken::new(),
                    execution(),
                )
                .await
                .unwrap();
        }
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 2);
        let sent = |request: &wiremock::Request| -> (Option<String>, Value) {
            (
                request
                    .headers
                    .get("anthropic-beta")
                    .map(|value| value.to_str().unwrap().to_string()),
                serde_json::from_slice(&request.body).unwrap(),
            )
        };
        let (beta, body) = sent(&received[0]);
        assert_eq!(beta.as_deref(), Some(CONTEXT_MANAGEMENT_BETA));
        assert_eq!(body["context_management"], context_management_config());
        let (beta, body) = sent(&received[1]);
        assert_eq!(beta, None);
        assert!(body.get("context_management").is_none());
    }

    /// The transcript is append-only across a spill and a context-edit cycle:
    /// each request's messages begin with exactly the previous request's
    /// messages, and `context_management` is the same constant every turn, so
    /// the cached prefix is never edited client-side.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn transcript_stays_append_only_across_a_spill_and_a_context_edit_cycle() {
        use crate::session::harness::tools::truncation::spill_or_truncate;
        let provider = provider_with_key();
        let root = tempfile::tempdir().unwrap();
        let cfg = rsi_common::spill::SpillConfig {
            root: root.path().to_path_buf(),
            max_bytes: 8 * 1024,
            max_lines: 200,
            disabled: false,
        };
        let big = (0..4000)
            .map(|n| format!("test case_{n} ... ok\n"))
            .chain(["test case_9 ... FAILED\n".to_string()])
            .collect::<String>();
        let stub = spill_or_truncate(&cfg, "s4", "shell", Some(101), &big, 1 << 20, false).content;
        assert!(stub.starts_with("[rsi-spill s4/1]"));

        let call = |id: &str| {
            let mut assistant = ChatMessage::assistant("");
            assistant.tool_calls.push(ToolCall {
                id: id.into(),
                name: "shell".into(),
                arguments: "{}".into(),
                hosted: false,
                hosted_result: None,
            });
            assistant
        };
        let mut history = vec![ChatMessage::user("run the tests")];
        history.extend([call("t1"), ChatMessage::tool_result("t1", &stub)]);
        history.push(ChatMessage::assistant("one failure, reading it"));
        let turn = |history: &[ChatMessage]| {
            let mut request = editing_request("claude-opus-5-5", true);
            request.messages = history.to_vec();
            provider.build_request_body(&request)
        };
        let first = turn(&history);

        // A later turn: a second spilled result, then a user turn.
        history.push(ChatMessage::user("now the other crate"));
        history.extend([call("t2"), ChatMessage::tool_result("t2", &stub)]);
        history.push(ChatMessage::assistant("done"));
        let second = turn(&history);

        let before = first["messages"].as_array().unwrap();
        let after = second["messages"].as_array().unwrap();
        assert!(after.len() > before.len());
        assert_eq!(&after[..before.len()], &before[..]);
        assert_eq!(first["context_management"], second["context_management"]);
        assert_eq!(first["tools"], second["tools"]);
    }
}
