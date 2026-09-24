//! Session summarization engine.
//!
//! Generates rolling short (every ~20 assistant messages) and long (every ~60)
//! summaries using the same Ollama -> local-model fallback as title generation.

use std::sync::Arc;

use crate::bus::EventBus;
use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use rsi_common::types::{ConversationEvent, EventType, Role};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Thresholds for triggering summarization.
pub(super) const SHORT_SUMMARY_INTERVAL: i32 = 20;
pub(super) const LONG_SUMMARY_INTERVAL: i32 = 60;

/// Maximum output tokens for LLM generation.
const SHORT_MAX_TOKENS: u32 = 512;
const LONG_MAX_TOKENS: u32 = 1500;

/// What summarization action should be taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SummarizeAction {
    None,
    Short,
    Long,
    /// Both short and long thresholds hit simultaneously (at 60, 120, 180, ...).
    Both,
}

/// Check if a summary should be generated based on current assistant message count.
///
/// `assistant_message_count` is the total number of assistant messages received
/// so far in this session (across all continuations).
///
/// `last_short_count` / `last_long_count` are the assistant_message_count values
/// at which the last short/long summaries were generated (None if never generated).
pub(super) fn should_summarize(
    assistant_message_count: i32,
    last_short_count: Option<i32>,
    last_long_count: Option<i32>,
) -> SummarizeAction {
    if assistant_message_count < SHORT_SUMMARY_INTERVAL {
        return SummarizeAction::None;
    }

    let short_due = {
        let last = last_short_count.unwrap_or(0);
        assistant_message_count - last >= SHORT_SUMMARY_INTERVAL
    };
    let long_due = {
        let last = last_long_count.unwrap_or(0);
        assistant_message_count - last >= LONG_SUMMARY_INTERVAL
    };

    match (short_due, long_due) {
        (true, true) => SummarizeAction::Both,
        (true, false) => SummarizeAction::Short,
        (false, true) => SummarizeAction::Long,
        (false, false) => SummarizeAction::None,
    }
}

/// Format conversation events for inclusion in a summarization prompt.
fn format_events_for_prompt(events: &[ConversationEvent], max_chars: usize) -> String {
    let mut buf = String::new();
    let mut remaining = max_chars;

    for event in events {
        if event.event_type != EventType::Message {
            continue;
        }
        let role_str = match event.role {
            Some(Role::User) => "User",
            Some(Role::Assistant) => "Assistant",
            _ => continue,
        };
        let content = if event.content.len() > 400 {
            let mut end = 400;
            while !event.content.is_char_boundary(end) {
                end -= 1;
            }
            &event.content[..end]
        } else {
            &event.content
        };
        let line = format!("{}: {}\n", role_str, content);
        if line.len() > remaining {
            break;
        }
        remaining -= line.len();
        buf.push_str(&line);
    }

    buf
}

/// Generate a short summary from the previous summary + recent messages.
pub(super) async fn generate_short_summary(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    session_id: Uuid,
    previous_summary: Option<&str>,
    recent_events: &[ConversationEvent],
    session_query: &str,
    runtime_config: &Arc<RuntimeConfig>,
) -> Result<String> {
    let formatted_events = format_events_for_prompt(recent_events, 4000);
    if formatted_events.is_empty() {
        return Err(DaemonError::Store("No message events to summarize".into()));
    }

    let truncated_query = truncate_str(session_query, 300);

    let mut prompt = String::with_capacity(5000);
    prompt.push_str(
        "You are summarizing an ongoing AI coding session. Generate a concise rolling summary.\n\n",
    );
    prompt.push_str("Session query: ");
    prompt.push_str(truncated_query);
    prompt.push('\n');

    if let Some(prev) = previous_summary {
        prompt.push_str("\nPrevious short summary:\n");
        prompt.push_str(prev);
        prompt.push('\n');
    }

    prompt.push_str("\nRecent conversation (last ~20 messages):\n");
    prompt.push_str(&formatted_events);
    prompt.push_str(
        "\nWrite a concise summary (2-5 sentences) covering:\n\
         - What was accomplished since the last summary\n\
         - Current state of the work\n\
         - Any open decisions or blockers\n\n\
         Be specific about files, functions, and technical decisions. No preamble.",
    );

    generate_with_fallback(
        store,
        event_bus,
        session_id,
        &prompt,
        SHORT_MAX_TOKENS,
        runtime_config,
    )
    .await
}

/// Generate a long summary from the previous long summary + messages since last.
pub(super) async fn generate_long_summary(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    session_id: Uuid,
    previous_summary: Option<&str>,
    events_since_last: &[ConversationEvent],
    session_query: &str,
    runtime_config: &Arc<RuntimeConfig>,
) -> Result<String> {
    let formatted_events = format_events_for_prompt(events_since_last, 8000);
    if formatted_events.is_empty() {
        return Err(DaemonError::Store("No message events to summarize".into()));
    }

    let truncated_query = truncate_str(session_query, 300);

    let mut prompt = String::with_capacity(10000);
    prompt.push_str(
        "You are generating a comprehensive session history summary for an AI coding session.\n\n",
    );
    prompt.push_str("Session query: ");
    prompt.push_str(truncated_query);
    prompt.push('\n');

    if let Some(prev) = previous_summary {
        prompt.push_str("\nPrevious long summary:\n");
        prompt.push_str(prev);
        prompt.push('\n');
    }

    prompt.push_str("\nMessages since last comprehensive summary:\n");
    prompt.push_str(&formatted_events);
    prompt.push_str(
        "\nWrite a comprehensive summary covering:\n\
         - Key themes and patterns in this session\n\
         - Major technical decisions and their rationale\n\
         - Files and systems modified\n\
         - Current state and open questions\n\
         - What would someone need to know to continue this work\n\n\
         Be thorough but structured. Use bullet points for clarity. No preamble.",
    );

    generate_with_fallback(
        store,
        event_bus,
        session_id,
        &prompt,
        LONG_MAX_TOKENS,
        runtime_config,
    )
    .await
}

/// Truncate a string at a char boundary.
fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Generate text using Ollama -> CLI fallback (same pattern as title.rs).
async fn generate_with_fallback(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    session_id: Uuid,
    prompt: &str,
    max_tokens: u32,
    runtime_config: &Arc<RuntimeConfig>,
) -> Result<String> {
    let local_model = runtime_config.memory_model_local.read().clone();
    let fallback_model = runtime_config.memory_model_fallback.read().clone();
    let http = reqwest::Client::new();
    match generate_ollama(&http, prompt, max_tokens, &local_model).await {
        Ok(text) if !text.is_empty() => return Ok(text),
        Ok(_) => {
            tracing::debug!("Ollama returned empty summary, falling back to local model");
        }
        Err(e) => {
            tracing::debug!(error = %e, "Ollama summarization failed, falling back to local model");
        }
    }

    let fallback_provider = *runtime_config.memory_model_fallback_provider.read();
    let fallback_base_url = runtime_config.memory_model_fallback_base_url.read().clone();
    let fallback_api_key = runtime_config.memory_model_fallback_api_key.read().clone();
    let fallback_target = crate::memory::llm::MemoryLlmTarget {
        provider: fallback_provider,
        model: fallback_model,
        base_url: fallback_base_url,
        api_key: fallback_api_key,
    };
    let provider_label = crate::memory::llm::provider_label(&fallback_target)?;
    let backend_label = crate::memory::llm::backend_label(&fallback_target)?.to_string();
    crate::memory::llm::admit_and_generate_text(
        store,
        event_bus,
        crate::model_control::ModelAdmissionRequest {
            purpose: rsi_common::model_control::ModelInvocationPurpose::SessionSummary,
            provider: Some(provider_label.clone()),
            model: Some(fallback_target.model.clone()),
            backend: Some(backend_label.clone()),
            effort: None,
            trigger: "session_summary".to_string(),
            owner: rsi_common::model_control::InvocationOwner {
                session_id: Some(session_id),
                ..rsi_common::model_control::InvocationOwner::default()
            },
            dedup_key: Some(crate::model_control::stable_dedup_key(
                "session-summary",
                &[
                    &session_id.to_string(),
                    &fallback_target.model,
                    fallback_target.base_url.as_deref().unwrap_or(""),
                    prompt,
                ],
            )),
            request_fingerprint: Some(crate::model_control::hash_request_fingerprint(&[
                &fallback_target.model,
                fallback_target.base_url.as_deref().unwrap_or(""),
                prompt,
            ])),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                rsi_common::model_control::ModelInvocationPurpose::SessionSummary,
                Some(provider_label.as_str()),
                Some(backend_label.as_str()),
                Some(fallback_target.model.as_str()),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        },
        &fallback_target,
        prompt,
        max_tokens,
        "Summary generation",
    )
    .await
}

/// LLM generation via shared Ollama HTTP client.
async fn generate_ollama(
    http: &reqwest::Client,
    prompt: &str,
    num_predict: u32,
    model: &str,
) -> Result<String> {
    let opts = crate::ollama_client::GenerateOptions {
        num_predict: Some(num_predict),
        temperature: 0.3,
        think: false,
        keep_alive: None,
    };
    let raw = crate::ollama_client::generate(
        http,
        model,
        None,
        prompt,
        opts,
        std::time::Duration::from_secs(60),
    )
    .await
    .map_err(|e| DaemonError::Store(e.to_string()))?;
    Ok(raw.trim().to_string())
}

/// Approximate token count from character length (rough heuristic: ~4 chars/token).
pub(super) fn approx_token_count(text: &str) -> u32 {
    (text.len() / 4).max(1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_summarize_below_threshold() {
        assert_eq!(should_summarize(10, None, None), SummarizeAction::None,);
        assert_eq!(should_summarize(19, None, None), SummarizeAction::None,);
    }

    #[test]
    fn test_should_summarize_short_at_threshold() {
        assert_eq!(should_summarize(20, None, None), SummarizeAction::Short,);
    }

    #[test]
    fn test_should_summarize_short_after_previous() {
        // Had a short at count=20, now at 40 -> should trigger again
        assert_eq!(should_summarize(40, Some(20), None), SummarizeAction::Short,);
        // Had a short at count=20, now at 39 -> not yet
        assert_eq!(should_summarize(39, Some(20), None), SummarizeAction::None,);
    }

    #[test]
    fn test_should_summarize_both_at_long_threshold() {
        // At 60, both short and long are due
        assert_eq!(should_summarize(60, Some(40), None), SummarizeAction::Both,);
    }

    #[test]
    fn test_should_summarize_long_only() {
        // Short was just done at 59, but long is due at 60
        assert_eq!(should_summarize(60, Some(59), None), SummarizeAction::Long,);
    }

    #[test]
    fn test_should_summarize_rolling() {
        // After short at 20, long at 60: next short at 80
        assert_eq!(
            should_summarize(80, Some(60), Some(60)),
            SummarizeAction::Short,
        );
        // Next both at 120
        assert_eq!(
            should_summarize(120, Some(100), Some(60)),
            SummarizeAction::Both,
        );
    }

    #[test]
    fn test_approx_token_count() {
        assert_eq!(approx_token_count("hello world"), 2); // 11 chars / 4 = 2
        assert_eq!(approx_token_count(""), 1); // min 1
    }

    #[test]
    fn test_truncate_str() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn test_format_events_for_prompt_empty() {
        let events: Vec<ConversationEvent> = vec![];
        let result = format_events_for_prompt(&events, 1000);
        assert!(result.is_empty());
    }

    #[test]
    fn test_format_events_for_prompt_filters_non_messages() {
        let events = vec![
            ConversationEvent {
                id: 0,
                session_id: uuid::Uuid::new_v4(),
                sequence: 1,
                event_type: EventType::ToolUse,
                role: Some(Role::Assistant),
                content: "tool call".to_string(),
                tool_name: Some("write".to_string()),
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            },
            ConversationEvent {
                id: 0,
                session_id: uuid::Uuid::new_v4(),
                sequence: 2,
                event_type: EventType::Message,
                role: Some(Role::Assistant),
                content: "Hello".to_string(),
                tool_name: None,
                tool_input: None,
                created_at: chrono::Utc::now(),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            },
        ];
        let result = format_events_for_prompt(&events, 1000);
        assert!(!result.contains("tool call"));
        assert!(result.contains("Assistant: Hello"));
    }

    // --- Byte-level regression: summarizer request body shape ---
    //
    // Replicates the `GenerateOptions` block inside `generate_ollama` in this
    // file so any drift in the helper or `ollama_client::build_body` fails the
    // snapshot and surfaces as a regression on the summarizer pipeline.

    fn summarizer_opts(num_predict: u32) -> crate::ollama_client::GenerateOptions {
        crate::ollama_client::GenerateOptions {
            num_predict: Some(num_predict),
            temperature: 0.3,
            think: false,
            keep_alive: None,
        }
    }

    #[test]
    fn summarizer_short_body_matches_pinned_snapshot() {
        // 128 is representative of short-summary call sites.
        let opts = summarizer_opts(128);
        let body = crate::ollama_client::build_body(
            "qwen3:14b",
            None,
            "SUMMARY_PROMPT_SHORT",
            &opts,
            false,
        );
        let expected = serde_json::json!({
            "model": "qwen3:14b",
            "prompt": "SUMMARY_PROMPT_SHORT",
            "stream": false,
            "think": false,
            "options": { "num_predict": 128, "temperature": 0.3f32 },
        });
        assert_eq!(body, expected, "short-summary body structure drifted");
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            serde_json::to_string(&expected).unwrap(),
            "short-summary serialized bytes drifted",
        );
        assert!(body.get("system").is_none());
        assert!(body.get("keep_alive").is_none());
    }

    #[test]
    fn summarizer_long_body_matches_pinned_snapshot() {
        // 1024 is representative of long-summary call sites.
        let opts = summarizer_opts(1024);
        let body = crate::ollama_client::build_body(
            "qwen3:14b",
            None,
            "SUMMARY_PROMPT_LONG",
            &opts,
            false,
        );
        let expected = serde_json::json!({
            "model": "qwen3:14b",
            "prompt": "SUMMARY_PROMPT_LONG",
            "stream": false,
            "think": false,
            "options": { "num_predict": 1024, "temperature": 0.3f32 },
        });
        assert_eq!(body, expected, "long-summary body structure drifted");
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            serde_json::to_string(&expected).unwrap(),
            "long-summary serialized bytes drifted",
        );
    }
}

#[cfg(test)]
mod http_tests {
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// HTTP-level regression: verifies the body sent by `generate_ollama` in
    /// this file. A regression in the `GenerateOptions` construction here would
    /// NOT be caught by the `build_body` unit tests in `ollama_client.rs`.
    //
    // `TEST_OLLAMA_URL_LOCK` is intentionally held across awaits — it exists to
    // serialise env-var mutation across this binary's HTTP tests.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn summarizer_generate_ollama_posts_expected_body_shape() {
        let _lock = crate::ollama_client::TEST_OLLAMA_URL_LOCK.lock();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/api/generate$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("{\"response\":\"A summary.\",\"done\":true}"),
            )
            .mount(&server)
            .await;
        // SAFETY: TEST_OLLAMA_URL_LOCK is held above.
        unsafe {
            std::env::set_var("RSI_OLLAMA_URL", format!("{}/api/generate", server.uri()));
        }
        let http = reqwest::Client::new();
        super::generate_ollama(&http, "SUMMARIZE PROMPT", 1024, "qwen3:14b")
            .await
            .expect("generate_ollama should succeed");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "expected exactly one request");
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("body is valid JSON");
        assert_eq!(body["model"], serde_json::json!("qwen3:14b"));
        assert_eq!(body["prompt"], serde_json::json!("SUMMARIZE PROMPT"));
        assert_eq!(body["stream"], serde_json::json!(false));
        assert_eq!(body["think"], serde_json::json!(false));
        assert_eq!(body["options"]["num_predict"], serde_json::json!(1024));
        let t = body["options"]["temperature"]
            .as_f64()
            .expect("temperature is f64");
        assert!((t - 0.3).abs() < 1e-6, "temperature {t} not ≈ 0.3");
        assert!(body.get("system").is_none(), "system key must be absent");
        assert!(
            body.get("keep_alive").is_none(),
            "keep_alive key must be absent"
        );
    }
}
