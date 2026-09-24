//! Shared HTTP client for Ollama `/api/generate`.
//!
//! Single source of truth for Ollama calls from rsid. Replaces the three
//! duplicate `generate_ollama` helpers in title / summarizer / extractor and
//! powers the streaming CompilePrompt pipeline.

use futures::StreamExt;
use std::time::Duration;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Process-wide mutex used by tests that need to set `RSI_OLLAMA_URL`
/// without racing against other parallel tests that read it. Acquire this lock
/// before calling `std::env::set_var` and hold it for the entire test body.
///
/// Lives at module scope (not inside `#[cfg(test)] mod tests`) so it is
/// reachable as `crate::ollama_client::TEST_OLLAMA_URL_LOCK` from any lib-test
/// module in this crate.
#[cfg(test)]
pub(crate) static TEST_OLLAMA_URL_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

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
    model: &str,
    system: Option<&str>,
    prompt: &str,
    opts: GenerateOptions,
    timeout: Duration,
) -> Result<String, OllamaError> {
    #[derive(serde::Deserialize)]
    struct Resp {
        response: String,
    }

    let body = build_body(model, system, prompt, &opts, false);

    let resp = http
        .post(ollama_url()?)
        .timeout(timeout)
        .json(&body)
        .send()
        .await
        .map_err(|e| OllamaError::Request(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(OllamaError::Status(resp.status()));
    }

    let parsed: Resp = resp
        .json()
        .await
        .map_err(|e| OllamaError::Parse(e.to_string()))?;

    if parsed.response.is_empty() {
        return Err(OllamaError::Empty);
    }
    Ok(parsed.response)
}

/// Streaming generate. Invokes `on_token` for each non-empty delta as NDJSON
/// chunks arrive. Returns the full concatenated response on completion.
///
/// Honors `cancel` — if triggered, returns `OllamaError::Cancelled` promptly.
pub async fn generate_stream<F>(
    http: &reqwest::Client,
    model: &str,
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

    let body = build_body(model, system, prompt, &opts, true);

    let request = http.post(ollama_url()?).json(&body);
    let resp = tokio::select! {
        r = request.send() => {
            r.map_err(|e| OllamaError::Request(e.to_string()))?
        }
        _ = cancel.cancelled() => return Err(OllamaError::Cancelled),
    };

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
    let body = build_body(model, None, "", &opts, false);
    let resp = http
        .post(ollama_url()?)
        .timeout(Duration::from_secs(30))
        .json(&body)
        .send()
        .await
        .map_err(|e| OllamaError::Request(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(OllamaError::Status(resp.status()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct OllamaUrlRestore(Option<std::ffi::OsString>);

    impl OllamaUrlRestore {
        fn set(value: &str) -> Self {
            let previous = std::env::var_os("RSI_OLLAMA_URL");
            // SAFETY: callers hold TEST_OLLAMA_URL_LOCK for the full lifetime
            // of this guard, including any awaited request.
            unsafe { std::env::set_var("RSI_OLLAMA_URL", value) };
            Self(previous)
        }
    }

    impl Drop for OllamaUrlRestore {
        fn drop(&mut self) {
            // SAFETY: the owning test still holds TEST_OLLAMA_URL_LOCK.
            unsafe {
                match self.0.take() {
                    Some(value) => std::env::set_var("RSI_OLLAMA_URL", value),
                    None => std::env::remove_var("RSI_OLLAMA_URL"),
                }
            }
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

    #[test]
    fn body_includes_system_when_provided() {
        let opts = GenerateOptions::default();
        let body = build_body("m", Some("SYS"), "P", &opts, true);
        assert_eq!(body["system"], serde_json::json!("SYS"));
        assert_eq!(body["stream"], serde_json::json!(true));
    }

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

    #[test]
    fn native_ollama_url_rejects_remote_hosts_before_io() {
        let error = validate_loopback_ollama_url("https://example.com/api/generate")
            .expect_err("remote native Ollama URL must fail closed");
        assert!(matches!(error, OllamaError::NonLoopback(_)));
    }

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

    #[tokio::test(flavor = "current_thread")]
    async fn remote_native_ollama_generate_is_rejected_before_transport() {
        let _lock = TEST_OLLAMA_URL_LOCK.lock();
        let _restore = OllamaUrlRestore::set("https://example.com/api/generate");
        let error = generate(
            &reqwest::Client::new(),
            "model",
            None,
            "prompt",
            GenerateOptions::default(),
            Duration::from_millis(100),
        )
        .await
        .expect_err("remote native URL");
        assert!(matches!(error, OllamaError::NonLoopback(_)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_native_ollama_generate_remains_operational() {
        let _lock = TEST_OLLAMA_URL_LOCK.lock();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let _restore = OllamaUrlRestore::set(&format!("http://{address}/api/generate"));
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
            "model",
            None,
            "prompt",
            GenerateOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .expect("loopback generate");
        assert_eq!(response, "ok");
        server.await.expect("loopback server");
    }
}
