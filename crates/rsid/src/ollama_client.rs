//! Shared HTTP client for Ollama `/api/generate`.
//!
//! Single source of truth for Ollama calls from rsid. Replaces the three
//! duplicate `generate_ollama` helpers in title / summarizer / extractor and
//! powers the streaming CompilePrompt pipeline.

use crate::memory::helper_breaker::{Guarded, HelperCall, HelperEndpointBreaker, Outcome};
use futures::StreamExt;
use std::time::Duration;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Base endpoint for Ollama's `/api/generate`. Read from `RSI_OLLAMA_URL`
/// (legacy `MOTHERSHIP_OLLAMA_URL` / `FLYWHEEL_OLLAMA_URL`) on each call
/// (integration tests point this at a `wiremock` server), falling back to the
/// standard local Ollama address. The per-call env read keeps tests that mutate
/// the var observable without `OnceLock` caching across tests — the overhead is
/// ~1 µs per generate call, negligible next to an HTTP round-trip to a language
/// model.
fn ollama_url() -> Result<String, OllamaError> {
    let raw = rsi_common::identity::env_with_legacy(
        "RSI_OLLAMA_URL",
        &["MOTHERSHIP_OLLAMA_URL", "FLYWHEEL_OLLAMA_URL"],
    )
    .unwrap_or_else(|_| "http://localhost:11434/api/generate".to_string());
    validate_loopback_ollama_url(&raw)?;
    Ok(raw)
}

fn validate_loopback_ollama_url(raw: &str) -> Result<(), OllamaError> {
    let url = reqwest::Url::parse(raw)
        .map_err(|error| OllamaError::NonLoopback(format!("{raw}: {error}")))?;
    let host = url
        .host_str()
        .ok_or_else(|| OllamaError::NonLoopback(format!("{raw}: missing host")))?;
    let normalized_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let is_loopback = normalized_host.eq_ignore_ascii_case("localhost")
        || normalized_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !is_loopback {
        return Err(OllamaError::NonLoopback(format!(
            "{raw}: native Ollama model execution is loopback-only; configure a controlled OpenAI-compatible route for remote endpoints"
        )));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum OllamaError {
    #[error("Ollama request failed: {0}")]
    Request(String),
    #[error("Ollama returned status {0}")]
    Status(reqwest::StatusCode),
    #[error("Ollama JSON parse failed: {0}")]
    Parse(String),
    #[error("Ollama returned empty response")]
    Empty,
    #[error("Cancelled")]
    Cancelled,
    #[error("Rejected non-loopback native Ollama URL: {0}")]
    NonLoopback(String),
    /// The endpoint's circuit breaker is open (#261): nothing was sent.
    #[error("{0}")]
    EndpointUnreachable(String),
}

/// Run one Ollama request behind the endpoint's circuit breaker (#261).
async fn behind_breaker<T>(
    url: &str,
    call: &HelperCall<'_>,
    attempt: impl std::future::Future<Output = (Result<T, OllamaError>, Outcome)>,
) -> Result<T, OllamaError> {
    let Some(endpoint) = crate::memory::helper_breaker::loopback_endpoint_key(url) else {
        return attempt.await.0;
    };
    match HelperEndpointBreaker::global()
        .guarded(&endpoint, call, attempt)
        .await
    {
        Guarded::Rejected(message) => Err(OllamaError::EndpointUnreachable(message)),
        Guarded::Ran { value } => value,
    }
}

/// What a failed send says about the endpoint: a connect failure means
/// nothing listens there.
fn send_outcome(error: &reqwest::Error) -> Outcome {
    if error.is_connect() {
        Outcome::Unreachable
    } else {
        Outcome::Unknown
    }
}

#[derive(Debug, Clone)]
pub struct GenerateOptions {
    pub num_predict: Option<u32>,
    pub temperature: f32,
    pub think: bool,
    pub keep_alive: Option<String>,
}

impl Default for GenerateOptions {
    fn default() -> Self {
        Self {
            num_predict: None,
            temperature: 0.3,
            think: false,
            keep_alive: None,
        }
    }
}

/// Build the JSON request body with the exact shape used across the codebase.
///
/// Shape (must match pre-refactor helpers byte-for-byte when `system` is None
/// and `keep_alive` is None):
/// `{"model":..., "prompt":..., "stream":<bool>, "think":<bool>,
///   "options": {"num_predict": N, "temperature": 0.3}}`
/// Extra keys (`system`, `keep_alive`) are only added when `Some`.
pub(crate) fn build_body(
    model: &str,
    system: Option<&str>,
    prompt: &str,
    opts: &GenerateOptions,
    stream: bool,
) -> serde_json::Value {
    let mut options = serde_json::Map::new();
    if let Some(n) = opts.num_predict {
        options.insert("num_predict".to_string(), serde_json::json!(n));
    }
    options.insert(
        "temperature".to_string(),
        serde_json::json!(opts.temperature),
    );

    let mut body = serde_json::Map::new();
    body.insert("model".to_string(), serde_json::json!(model));
    body.insert("prompt".to_string(), serde_json::json!(prompt));
    body.insert("stream".to_string(), serde_json::json!(stream));
    body.insert("think".to_string(), serde_json::json!(opts.think));
    body.insert("options".to_string(), serde_json::Value::Object(options));
    if let Some(s) = system {
        body.insert("system".to_string(), serde_json::json!(s));
    }
    if let Some(k) = &opts.keep_alive {
        body.insert("keep_alive".to_string(), serde_json::json!(k));
    }
    serde_json::Value::Object(body)
}

/// Non-streaming generate. Returns the full response string with no trimming.
/// Callers that previously did `.trim()` should continue to do so.
pub async fn generate(
    http: &reqwest::Client,
    call: &HelperCall<'_>,
    system: Option<&str>,
    prompt: &str,
    opts: GenerateOptions,
    timeout: Duration,
) -> Result<String, OllamaError> {
    #[derive(serde::Deserialize)]
    struct Resp {
        response: String,
    }

    // The breaker key; the request below binds `ollama_url()` itself, the
    // shape the model-control validator proves loopback.
    let url = ollama_url()?;
    let body = build_body(call.model, system, prompt, &opts, false);
    let request = http.post(ollama_url()?).timeout(timeout).json(&body);

    behind_breaker(&url, call, async {
        let resp = match request.send().await {
            Ok(resp) => resp,
            Err(e) => return (Err(OllamaError::Request(e.to_string())), send_outcome(&e)),
        };

        if !resp.status().is_success() {
            return (Err(OllamaError::Status(resp.status())), Outcome::Reachable);
        }

        let parsed: Resp = match resp.json().await {
            Ok(parsed) => parsed,
            Err(e) => return (Err(OllamaError::Parse(e.to_string())), Outcome::Reachable),
        };

        if parsed.response.is_empty() {
            return (Err(OllamaError::Empty), Outcome::Reachable);
        }
        (Ok(parsed.response), Outcome::Reachable)
    })
    .await
}

/// Streaming generate. Invokes `on_token` for each non-empty delta as NDJSON
/// chunks arrive. Returns the full concatenated response on completion.
///
/// Honors `cancel` — if triggered, returns `OllamaError::Cancelled` promptly.
pub async fn generate_stream<F>(
    http: &reqwest::Client,
    call: &HelperCall<'_>,
    system: Option<&str>,
    prompt: &str,
    opts: GenerateOptions,
    mut on_token: F,
    cancel: CancellationToken,
) -> Result<String, OllamaError>
where
    F: FnMut(&str) + Send,
{
    #[derive(serde::Deserialize)]
    struct Chunk {
        #[serde(default)]
        response: String,
        #[serde(default)]
        done: bool,
    }

    let url = ollama_url()?;
    let body = build_body(call.model, system, prompt, &opts, true);

    let request = http.post(ollama_url()?).json(&body);
    let resp = behind_breaker(&url, call, async {
        tokio::select! {
            r = request.send() => match r {
                Ok(resp) => (Ok(resp), Outcome::Reachable),
                Err(e) => (Err(OllamaError::Request(e.to_string())), send_outcome(&e)),
            },
            _ = cancel.cancelled() => (Err(OllamaError::Cancelled), Outcome::Unknown),
        }
    })
    .await?;

    if !resp.status().is_success() {
        return Err(OllamaError::Status(resp.status()));
    }

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut full = String::new();

    loop {
        let next = tokio::select! {
            n = stream.next() => n,
            _ = cancel.cancelled() => return Err(OllamaError::Cancelled),
        };
        match next {
            Some(Ok(bytes)) => {
                buf.push_str(&String::from_utf8_lossy(&bytes));
                // Process each complete NDJSON line
                while let Some(nl) = buf.find('\n') {
                    let line = buf[..nl].trim().to_string();
                    buf.drain(..=nl);
                    if line.is_empty() {
                        continue;
                    }
                    let chunk: Chunk = match serde_json::from_str(&line) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    if !chunk.response.is_empty() {
                        full.push_str(&chunk.response);
                        on_token(&chunk.response);
                    }
                    if chunk.done {
                        return Ok(full);
                    }
                }
            }
            Some(Err(e)) => return Err(OllamaError::Request(e.to_string())),
            None => break,
        }
    }

    // Stream ended without a `done: true` — treat trailing buffer as final line
    let trimmed = buf.trim();
    if !trimmed.is_empty()
        && let Ok(chunk) = serde_json::from_str::<Chunk>(trimmed)
        && !chunk.response.is_empty()
    {
        full.push_str(&chunk.response);
        on_token(&chunk.response);
    }
    Ok(full)
}

/// Warmup: prime the model with an empty prompt and `keep_alive: 60m` so
/// subsequent compile calls avoid cold-load latency.
pub async fn warmup(http: &reqwest::Client, model: &str) -> Result<(), OllamaError> {
    let opts = GenerateOptions {
        num_predict: Some(1),
        temperature: 0.0,
        think: false,
        keep_alive: Some("60m".to_string()),
    };
    let call = HelperCall {
        purpose: rsi_common::model_control::ModelInvocationPurpose::PromptCompile.as_str(),
        setting_key: "prompt_compile_model_local",
        model,
        gate: true,
    };
    let url = ollama_url()?;
    let body = build_body(model, None, "", &opts, false);
    let request = http
        .post(ollama_url()?)
        .timeout(Duration::from_secs(30))
        .json(&body);
    behind_breaker(&url, &call, async {
        let resp = match request.send().await {
            Ok(resp) => resp,
            Err(e) => return (Err(OllamaError::Request(e.to_string())), send_outcome(&e)),
        };
        if !resp.status().is_success() {
            return (Err(OllamaError::Status(resp.status())), Outcome::Reachable);
        }
        (Ok(()), Outcome::Reachable)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    fn test_call(model: &str) -> HelperCall<'_> {
        HelperCall {
            purpose: "session.summary",
            setting_key: "memory_model_local",
            model,
            gate: true,
        }
    }

    /// Regression: the request body produced by `generate` must match the
    /// pre-refactor helpers byte-for-byte when `system`/`keep_alive` are None.
    ///
    /// `GenerateOptions.temperature` is `f32`, and `serde_json::json!` promotes
    /// it to `f64` with the f32→f64 rounding (0.30000001192...). The
    /// pre-refactor helpers inlined `0.3` inside the `json!` macro, which also
    /// produces an `f64`, but with Rust's native `f64` 0.3 (0.29999999999...).
    /// Bodies are therefore structurally identical except for the low bits of
    /// that one value; on the wire Ollama sees both as "0.3". Assert the
    /// non-temperature fields strictly, and assert temperature is ≈ 0.3.
    fn assert_body_shape(body: &serde_json::Value, model: &str, prompt: &str, num_predict: u32) {
        assert_eq!(body["model"], serde_json::json!(model));
        assert_eq!(body["prompt"], serde_json::json!(prompt));
        assert_eq!(body["stream"], serde_json::json!(false));
        assert_eq!(body["think"], serde_json::json!(false));
        assert_eq!(
            body["options"]["num_predict"],
            serde_json::json!(num_predict)
        );
        let t = body["options"]["temperature"].as_f64().expect("f64");
        assert!((t - 0.3).abs() < 1e-6, "temperature {t} not ≈ 0.3");
        assert!(body.get("system").is_none());
        assert!(body.get("keep_alive").is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn body_matches_title_helper() {
        let opts = GenerateOptions {
            num_predict: Some(512),
            temperature: 0.3,
            think: false,
            keep_alive: None,
        };
        let body = build_body("qwen3:14b", None, "TEST PROMPT", &opts, false);
        assert_body_shape(&body, "qwen3:14b", "TEST PROMPT", 512);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn body_matches_summarizer_helper() {
        let opts = GenerateOptions {
            num_predict: Some(1024),
            temperature: 0.3,
            think: false,
            keep_alive: None,
        };
        let body = build_body("qwen3:14b", None, "SUMMARIZE", &opts, false);
        assert_body_shape(&body, "qwen3:14b", "SUMMARIZE", 1024);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn body_matches_extractor_helper() {
        let opts = GenerateOptions {
            num_predict: Some(1024),
            temperature: 0.3,
            think: false,
            keep_alive: None,
        };
        let body = build_body("qwen3:14b", None, "EXTRACT", &opts, false);
        assert_body_shape(&body, "qwen3:14b", "EXTRACT", 1024);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn body_includes_system_when_provided() {
        let opts = GenerateOptions::default();
        let body = build_body("m", Some("SYS"), "P", &opts, true);
        assert_eq!(body["system"], serde_json::json!("SYS"));
        assert_eq!(body["stream"], serde_json::json!(true));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn body_includes_keep_alive_when_provided() {
        let opts = GenerateOptions {
            num_predict: None,
            temperature: 0.3,
            think: false,
            keep_alive: Some("60m".to_string()),
        };
        let body = build_body("m", None, "P", &opts, false);
        assert_eq!(body["keep_alive"], serde_json::json!("60m"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn native_ollama_url_rejects_remote_hosts_before_io() {
        let error = validate_loopback_ollama_url("https://example.com/api/generate")
            .expect_err("remote native Ollama URL must fail closed");
        assert!(matches!(error, OllamaError::NonLoopback(_)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[test]
    fn native_ollama_url_accepts_ipv4_ipv6_and_localhost_loopback() {
        for url in [
            "http://127.0.0.1:11434/api/generate",
            "http://[::1]:11434/api/generate",
            "http://localhost:11434/api/generate",
        ] {
            validate_loopback_ollama_url(url).expect("loopback URL");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "current_thread")]
    async fn remote_native_ollama_generate_is_rejected_before_transport() {
        temp_env::async_with_vars(
            [("RSI_OLLAMA_URL", Some("https://example.com/api/generate"))],
            async move {
                let error = generate(
                    &reqwest::Client::new(),
                    &test_call("model"),
                    None,
                    "prompt",
                    GenerateOptions::default(),
                    Duration::from_millis(100),
                )
                .await
                .expect_err("remote native URL");
                assert!(matches!(error, OllamaError::NonLoopback(_)));
            },
        )
        .await;
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "current_thread")]
    async fn loopback_native_ollama_generate_remains_operational() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        temp_env::async_with_vars([("RSI_OLLAMA_URL", Some(format!("http://{address}/api/generate")))], async move {
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request connection");
            let mut request = [0_u8; 8192];
            let bytes = socket.read(&mut request).await.expect("request bytes");
            assert!(
                std::str::from_utf8(&request[..bytes])
                    .expect("UTF-8 request")
                    .starts_with("POST /api/generate HTTP/1.1")
            );
            let body = r#"{"response":"ok"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("HTTP response");
        });

        let response = generate(
            &reqwest::Client::new(),
            &test_call("model"),
            None,
            "prompt",
            GenerateOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .expect("loopback generate");
        assert_eq!(response, "ok");
        server.await.expect("loopback server");
        })
    .await;
    }

    /// #261: a port nothing listens on opens the endpoint's breaker on the
    /// first refused connect; the next gated call is refused without any
    /// request, naming the setting and the fix; an answer from the endpoint
    /// (here an operator's ungated call) closes it again without a restart.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
    #[tokio::test(flavor = "current_thread")]
    async fn a_refused_endpoint_opens_the_breaker_and_an_answer_closes_it() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        drop(listener);
        let url = format!("http://{address}/api/generate");
        let endpoint = format!("localhost:{}", address.port());
        temp_env::async_with_vars([("RSI_OLLAMA_URL", Some(url))], async move {
            let http = reqwest::Client::new();
            let first = generate(
                &http,
                &test_call("gemma4:e4b"),
                None,
                "prompt",
                GenerateOptions::default(),
                Duration::from_secs(5),
            )
            .await
            .expect_err("nothing listens");
            assert!(matches!(first, OllamaError::Request(_)), "{first:?}");
            let open = HelperEndpointBreaker::global().outages(std::time::Instant::now());
            assert!(
                open.iter().any(|outage| outage.endpoint == endpoint
                    && outage.setting_key == "memory_model_local"),
                "{open:?}"
            );

            let second = generate(
                &http,
                &test_call("gemma4:e4b"),
                None,
                "prompt",
                GenerateOptions::default(),
                Duration::from_secs(5),
            )
            .await
            .expect_err("the breaker refuses");
            let OllamaError::EndpointUnreachable(message) = second else {
                panic!("expected a breaker refusal, got {second:?}");
            };
            assert!(message.contains("memory_model_local=gemma4:e4b"), "{message}");
            assert!(message.contains("ollama pull gemma4:e4b"), "{message}");

            // The endpoint comes back: an ungated call reaches it and closes
            // the breaker.
            let listener = TcpListener::bind(address).await.expect("rebind the port");
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("request connection");
                let mut request = [0_u8; 8192];
                let _ = socket.read(&mut request).await.expect("request bytes");
                let body = r#"{"response":"ok"}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("HTTP response");
            });
            let operator = HelperCall {
                gate: false,
                ..test_call("gemma4:e4b")
            };
            let reply = generate(
                &http,
                &operator,
                None,
                "prompt",
                GenerateOptions::default(),
                Duration::from_secs(5),
            )
            .await
            .expect("the endpoint answers");
            assert_eq!(reply, "ok");
            server.await.expect("loopback server");
            let open = HelperEndpointBreaker::global().outages(std::time::Instant::now());
            assert!(
                open.iter().all(|outage| outage.endpoint != endpoint),
                "{open:?}"
            );
        })
        .await;
    }
}
