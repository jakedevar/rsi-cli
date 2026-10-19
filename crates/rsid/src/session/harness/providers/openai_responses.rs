//! `OpenAI` Responses wire for Bedrock's bearer-authenticated endpoint.

use crate::error::DaemonError;
use crate::model_control::ModelExecutionCapability;
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::session::harness::api_key::ApiCredential;
use crate::session::harness::errors::{self, ProviderError, ProviderErrorClass, ProviderErrorExt};
use crate::session::harness::provider::{ApiProvider, Result, WebCapabilities};
use crate::session::harness::types::{ChatRequest, ChatResponse, StreamChunk};
use rsid_harness::idle::{STREAM_IDLE_TIMEOUT, with_idle_timeout};
use rsid_harness::providers::openai_responses as openai_responses_wire;
use serde_json::Value;
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
        let http = crate::session::harness::net::client()?;
        Ok(Self {
            http,
            base_url,
            credential,
        })
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
            .json(&openai_responses_wire::request_body(request))
            .timeout(crate::session::harness::net::total_timeout(request.stream));
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

fn stream_error(error: openai_responses_wire::ResponsesError) -> DaemonError {
    match error {
        openai_responses_wire::ResponsesError::Provider(error) => error.into_daemon_error(),
        openai_responses_wire::ResponsesError::Process(message) => DaemonError::Process(message),
    }
}

fn completed_response(response: &Value) -> Result<ChatResponse> {
    openai_responses_wire::completed_response(response).map_err(ProviderError::into_daemon_error)
}

async fn read_response_stream(
    response: reqwest::Response,
    chunks: &mpsc::Sender<StreamChunk>,
    cancel: &CancellationToken,
) -> Result<ChatResponse> {
    let (stream, idle) = with_idle_timeout(response.bytes_stream(), STREAM_IDLE_TIMEOUT);
    openai_responses_wire::read_response_stream(stream, chunks, cancel)
        .await
        .map_err(|error| match error {
            openai_responses_wire::ResponsesError::Provider(error) => {
                openai_responses_wire::ResponsesError::Provider(idle.classify(error))
            }
            other => other,
        })
        .map_err(stream_error)
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
    use crate::session::harness::types::{
        ChatMessage, HarnessToolSpec, HarnessToolSpecKind, ToolCall, ToolContentBlock,
    };
    use serde_json::json;
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
            context_editing: false,
            tool_choice_none: false,
        }
    }

    fn execution() -> ModelExecutionCapability {
        ModelExecutionCapability::for_test(RuntimeExecutionRoute::SessionHarnessOpenAiHttp)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn responses_request_negotiates_and_replays_hosted_web_search() {
        let provider = provider_for_request_body();
        let mut request = request(vec![ChatMessage::user("search")], false);
        request.tools = provider
            .web_capabilities("global.openai.gpt-5.6-sol")
            .specs();
        let body = openai_responses_wire::request_body(&request);
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
        let replay = openai_responses_wire::request_body(&request);
        assert_eq!(
            replay["input"][0],
            json!({"role":"assistant", "content":"searching"})
        );
        assert_eq!(replay["input"][1], search_call);
        assert_eq!(replay["input"].as_array().unwrap().len(), 2);
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
