//! OpenAI-compatible LLM client for the stall classifier.
//!
//! Forked from `dreamer/llm_client.rs` with a separate struct because:
//! (a) the classifier owns its own API URL / key / model knobs (set by a
//! distinct `RSI_STALL_CLASSIFIER_*` env var family), and (b) timeouts are
//! shorter (default 30s) — classification is on the synchronous nudge path
//! so a 120s dreamer timeout would compound user-visible latency.
//!
//! The two clients can be consolidated later; the plan intentionally
//! forks to keep blast radius small.

use crate::error::{DaemonError, Result};
use crate::model_control::ModelExecutionCapability;
use crate::model_control::registry::RuntimeExecutionRoute;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct StallClassifierLlmClient {
    http: reqwest::Client,
    pub(crate) api_url: String,
    pub(crate) api_key: Option<String>,
    pub(crate) model: String,
    pub(crate) timeout: Duration,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    max_tokens: u32,
    temperature: f64,
    /// Tightens output shape for OpenAI-compatible servers that honor it
    /// (Ollama, vLLM, OpenRouter). Servers that ignore the field still
    /// return text and the parser handles either case.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

#[derive(Debug, Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

impl StallClassifierLlmClient {
    pub fn new(api_url: String, api_key: Option<String>, model: String, timeout: Duration) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_url,
            api_key,
            model,
            timeout,
        }
    }

    /// Make a single chat completion call. Returns the assistant's response
    /// text (which the scheduler then parses as a `ClassifierVerdict` JSON
    /// object).
    pub(crate) async fn complete(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        execution: ModelExecutionCapability,
    ) -> Result<String> {
        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: system_prompt.to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: user_prompt.to_string(),
                },
            ],
            // 512 is plenty for a verdict object — `nudge_prompt` is capped
            // at 500 chars by the prompt itself.
            max_tokens: 512,
            // Low temperature: we want deterministic-ish verdicts, not
            // creative re-interpretation.
            temperature: 0.2,
            response_format: Some(ResponseFormat {
                kind: "json_object".to_string(),
            }),
        };

        let mut req = self
            .http
            .post(&self.api_url)
            .timeout(self.timeout)
            .json(&request);
        if let Some(ref key) = self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }

        let resp = execution
            .bind_http(RuntimeExecutionRoute::StallClassifierHttp, req)
            .send("Stall classifier")
            .await
            .map_err(|e| DaemonError::Process(format!("Stall classifier LLM call failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(DaemonError::Process(format!(
                "Stall classifier LLM call returned {status}: {body}"
            )));
        }

        let parsed: ChatResponse = resp.json().await.map_err(|e| {
            DaemonError::Process(format!(
                "Failed to parse stall classifier LLM response: {e}"
            ))
        })?;

        parsed
            .choices
            .first()
            .map(|c| c.message.content.clone())
            .ok_or_else(|| {
                DaemonError::Process("Stall classifier LLM returned no choices".to_string())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn client_construction_stores_fields() {
        let client = StallClassifierLlmClient::new(
            "http://localhost:11434/v1/chat/completions".to_string(),
            Some("k".to_string()),
            "qwen2.5:7b".to_string(),
            Duration::from_secs(30),
        );
        assert_eq!(client.api_url, "http://localhost:11434/v1/chat/completions");
        assert_eq!(client.model, "qwen2.5:7b");
        assert_eq!(client.api_key.as_deref(), Some("k"));
        assert_eq!(client.timeout, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn admitted_classifier_request_uses_exactly_one_loopback_post() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request connection");
            let mut request = [0_u8; 8192];
            let bytes = socket.read(&mut request).await.expect("request bytes");
            assert!(
                std::str::from_utf8(&request[..bytes])
                    .expect("UTF-8 request")
                    .starts_with("POST /v1/chat/completions HTTP/1.1")
            );
            server_requests.fetch_add(1, Ordering::SeqCst);
            let body = r#"{"choices":[{"message":{"role":"assistant","content":"{\"verdict\":\"Finished\",\"confidence\":1.0}"}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("HTTP response");
        });
        let client = StallClassifierLlmClient::new(
            format!("http://{address}/v1/chat/completions"),
            None,
            "test-model".to_string(),
            Duration::from_secs(5),
        );

        let response = client
            .complete(
                "system",
                "user",
                ModelExecutionCapability::for_test(RuntimeExecutionRoute::StallClassifierHttp),
            )
            .await
            .expect("classifier response");
        assert!(response.contains("\"verdict\":\"Finished\""));
        server.await.expect("loopback server");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
}
