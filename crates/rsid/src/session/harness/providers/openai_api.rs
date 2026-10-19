//! OpenAI Chat Completions API provider.
//!
//! Extracted from openai.rs::run_agentic_loop with generalized types.
//! Covers OpenAI proper and serves as the base for CompatibleProvider.

// Provider layer is built ahead of resolve_provider() wiring (Phase 6+).
#![allow(dead_code)]

use crate::error::DaemonError;
use crate::model_control::ModelExecutionCapability;
use crate::session::harness::api_key::ApiCredential;
use crate::session::harness::errors::{self, ProviderError, ProviderErrorClass, ProviderErrorExt};
use crate::session::harness::provider::{ApiProvider, Result};
use crate::session::harness::sse::read_openai_sse_stream;
use crate::session::harness::types::*;
use rsid_harness::providers::openai_api as openai_wire;
use serde_json::Value;
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
            .json(&body)
            .timeout(crate::session::harness::net::total_timeout(request.stream));
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
            .json(&body)
            .timeout(crate::session::harness::net::total_timeout(request.stream));
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

        let http = crate::session::harness::net::client()?;

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
        let http = crate::session::harness::net::client()?;

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

    /// The wire shape lives in `rsid-harness`; the route and quirks are resolved here.
    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let reasoning_effort = self.quirks.reasoning_effort;
        openai_wire::build_request_body(
            request,
            &openai_wire::ChatCompletionsWire {
                merge_system_into_user: self.quirks.merge_system_into_user,
                native_tools: self.quirks.native_tools,
                max_tokens_field: self.quirks.max_tokens_field.field_name(),
                tool_choice_none: self.quirks.tool_choice_none,
                disable_streaming: self.quirks.disable_streaming,
                openrouter_reasoning: self.execution_route
                    == crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenRouterHttp,
                reasoning_effort_wire: &|effort| reasoning_effort.wire_value(effort),
            },
        )
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

        openai_wire::parse_chat_response(&json)
            .map_err(|message| DaemonError::Process(message.into()))
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
                    warning: None,
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

        let tool_calls = openai_wire::convert_tool_calls(acc_tool_calls);

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

    fn default_max_output_tokens(&self) -> Option<u32> {
        self.quirks.default_max_output_tokens
    }

    fn accepts_tool_choice_none(&self) -> bool {
        self.quirks.native_tools && self.quirks.tool_choice_none
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
    use serde_json::json;

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

    fn mercury_provider() -> OpenAiApiProvider {
        let entry = crate::session::harness::compatible_table::lookup("mercury-2")
            .expect("the mercury table entry");
        OpenAiApiProvider::with_config(
            entry.base_url.to_string(),
            ApiCredential::None,
            entry.quirks.clone(),
        )
        .unwrap()
    }

    fn probe_spec() -> HarnessToolSpec {
        HarnessToolSpec {
            name: "probe_tool".into(),
            description: "Probe.".into(),
            parameters_json: r#"{"type":"object"}"#.into(),
            freeform: None,
            kind: HarnessToolSpecKind::Function,
        }
    }

    /// #2253: the Mercury route sends `reasoning_effort` and
    /// `max_completion_tokens`, and never `max_tokens`, `diffusing`,
    /// `parallel_tool_calls` or a `developer` role.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn mercury_body_carries_effort_and_completion_cap_and_nothing_it_must_not_get() {
        let mut req = request(vec![
            ChatMessage::system("You are an agent."),
            ChatMessage::user("hi"),
        ]);
        req.model = "mercury-2".into();
        req.max_tokens = Some(16_384);
        req.reasoning_effort = Some("low".into());
        req.tools = vec![probe_spec()];
        req.stream = true;
        let body = mercury_provider().build_request_body(&req);

        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["max_completion_tokens"], 16_384);
        for absent in [
            "max_tokens",
            "diffusing",
            "parallel_tool_calls",
            "reasoning",
            "tool_choice",
            "temperature",
        ] {
            assert!(body.get(absent).is_none(), "{absent} in {body}");
        }
        assert_eq!(body["tools"][0]["function"]["name"], "probe_tool");
        assert!(body["tools"][0]["function"].get("strict").is_none());
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["role"] != "developer")
        );
        // Another route keeps today's body: `max_tokens`, no effort.
        let plain = OpenAiApiProvider::with_config(
            "http://localhost".into(),
            ApiCredential::None,
            ProviderQuirks {
                native_tools: true,
                ..Default::default()
            },
        )
        .unwrap()
        .build_request_body(&req);
        assert_eq!(plain["max_tokens"], 16_384);
        assert!(plain.get("max_completion_tokens").is_none());
        assert!(plain.get("reasoning_effort").is_none());
    }

    /// #2253: RSI efforts map onto Mercury's `instant|low|medium|high`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn mercury_effort_mapping_table() {
        let provider = mercury_provider();
        for (rsi_effort, wire) in [
            ("minimal", Some("instant")),
            ("none", Some("instant")),
            ("low", Some("low")),
            ("medium", Some("medium")),
            ("high", Some("high")),
            ("xhigh", Some("high")),
            ("max", Some("high")),
            ("MAX", Some("high")),
            ("ultra", None),
            ("diffusing", None),
        ] {
            let mut req = request(vec![ChatMessage::user("hi")]);
            req.reasoning_effort = Some(rsi_effort.into());
            let body = provider.build_request_body(&req);
            assert_eq!(
                body.get("reasoning_effort").and_then(Value::as_str),
                wire,
                "RSI effort {rsi_effort}"
            );
        }
        // No session effort, no parameter: Mercury's own default applies.
        let body = provider.build_request_body(&request(vec![ChatMessage::user("hi")]));
        assert!(body.get("reasoning_effort").is_none());

        // The plain OpenAI-style wire has no `instant`.
        let mut req = request(vec![ChatMessage::user("hi")]);
        req.reasoning_effort = Some("minimal".into());
        let openai_style = OpenAiApiProvider::with_config(
            "http://localhost".into(),
            ApiCredential::None,
            ProviderQuirks {
                native_tools: true,
                reasoning_effort: ReasoningEffortWire::OpenAi,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            openai_style.build_request_body(&req)["reasoning_effort"],
            "low"
        );
    }

    /// #2603: the Local/custom helper route asks a reasoning model for no
    /// reasoning with a top-level `reasoning_effort: "none"`; a route without
    /// the quirk sends nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn local_helper_wire_carries_reasoning_effort_none() {
        let mut req = request(vec![ChatMessage::user("title this")]);
        req.model = "gemma4:e4b".into();
        req.max_tokens = Some(200);
        req.reasoning_effort = Some("none".into());
        let local = OpenAiApiProvider::with_config(
            "http://localhost:11434/v1".into(),
            ApiCredential::None,
            ProviderQuirks {
                reasoning_effort: ReasoningEffortWire::OpenAiWithNone,
                ..Default::default()
            },
        )
        .unwrap();
        let body = local.build_request_body(&req);
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["max_tokens"], 200);

        let plain = OpenAiApiProvider::with_config(
            "http://localhost:11434/v1".into(),
            ApiCredential::None,
            ProviderQuirks::default(),
        )
        .unwrap();
        assert!(
            plain
                .build_request_body(&req)
                .get("reasoning_effort")
                .is_none()
        );
    }

    /// #2253: `tool_choice: "none"` goes out only with tools, only when the
    /// loop asks, and only on a route whose quirk allows it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn tool_choice_none_is_gated_by_the_route_quirk_and_needs_tools() {
        let mut req = request(vec![ChatMessage::user("hi")]);
        req.tools = vec![probe_spec()];
        req.tool_choice_none = true;
        let mercury = mercury_provider();
        assert!(mercury.accepts_tool_choice_none());
        assert_eq!(mercury.build_request_body(&req)["tool_choice"], "none");

        // Without the loop asking, or without tools, nothing is sent.
        req.tool_choice_none = false;
        assert!(
            mercury
                .build_request_body(&req)
                .get("tool_choice")
                .is_none()
        );
        req.tool_choice_none = true;
        req.tools.clear();
        assert!(
            mercury
                .build_request_body(&req)
                .get("tool_choice")
                .is_none()
        );

        // A route without the quirk never sends it, and tells the loop so.
        req.tools = vec![probe_spec()];
        let plain = OpenAiApiProvider::with_config(
            "http://localhost".into(),
            ApiCredential::None,
            ProviderQuirks {
                native_tools: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!plain.accepts_tool_choice_none());
        assert!(plain.build_request_body(&req).get("tool_choice").is_none());
        assert_eq!(mercury.default_max_output_tokens(), Some(16_384));
        assert_eq!(plain.default_max_output_tokens(), None);
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
