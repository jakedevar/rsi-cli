//! Observation extraction pipeline: prompt construction, LLM call (Ollama -> local-model fallback),
//! response parsing into structured observations.

use crate::bus::EventBus;
use chrono::Utc;
use rsi_common::types::{ConversationEvent, Observation, ObservationLevel};
use uuid::Uuid;

use std::sync::Arc;

use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::memory::session_text::extract_session_text;
use crate::store::Store;
use tokio::sync::Mutex;

/// Build the extraction prompt from session text and query.
pub(crate) fn build_extraction_prompt(session_text: &str, query: &str) -> String {
    let truncated_query = if query.len() > 400 {
        let mut end = 400;
        while !query.is_char_boundary(end) {
            end -= 1;
        }
        &query[..end]
    } else {
        query
    };

    format!(
        "You are an observation extraction engine. Given a coding session transcript, extract atomic factual observations.\n\n\
         Rules:\n\
         - Each observation is a single, self-contained factual statement\n\
         - Extract 3-15 observations per session (more for longer sessions)\n\
         - Focus on: what was worked on, decisions made, coding patterns, tools used, errors encountered, project state changes\n\
         - Do NOT extract opinions, speculation, or emotional content\n\
         - Each observation should be useful for remembering what happened in this session\n\n\
         Output format: JSON array of strings, one per observation.\n\
         Example: [\"User refactored the overlay system in rsi TUI\", \"Session used ratatui Constraint for layout\", \"User prefers vim-style keybindings\"]\n\n\
         Session query: {truncated_query}\n\n\
         Transcript:\n\
         {session_text}"
    )
}

/// Truncate session text to fit within the character budget.
fn truncate_session_text(text: &str, max_chars: usize) -> &str {
    if text.len() <= max_chars {
        return text;
    }
    let mut end = max_chars;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Parse LLM response into observation strings.
/// Tries JSON array first, falls back to newline-separated plain text.
pub(crate) fn parse_observations_response(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return vec![];
    }

    // Try JSON array parse first
    if let Ok(observations) = serde_json::from_str::<Vec<String>>(trimmed) {
        return observations
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
    }

    // Try to find JSON array embedded in response (LLMs sometimes add preamble)
    if let Some(start) = trimmed.find('[')
        && let Some(end) = trimmed.rfind(']')
        && end > start
    {
        let slice = &trimmed[start..=end];
        if let Ok(observations) = serde_json::from_str::<Vec<String>>(slice) {
            return observations
                .into_iter()
                .filter(|s| !s.trim().is_empty())
                .collect();
        }
    }

    // Fallback: newline-separated plain text, stripping bullets/numbers
    trimmed
        .lines()
        .map(|line| {
            let stripped = line.trim();
            // Strip leading bullets: "- ", "* ", "1. ", "2) ", etc.
            let stripped = stripped
                .strip_prefix("- ")
                .or_else(|| stripped.strip_prefix("* "))
                .unwrap_or(stripped);
            // Strip leading numbered lists: "1. ", "2. ", "1) ", etc.
            let stripped = if stripped.len() > 2 {
                let first_char = stripped.chars().next().unwrap_or(' ');
                if first_char.is_ascii_digit() {
                    stripped
                        .trim_start_matches(|c: char| c.is_ascii_digit())
                        .trim_start_matches(['.', ')'])
                        .trim_start()
                } else {
                    stripped
                }
            } else {
                stripped
            };
            stripped.to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Top-level extraction entry point.
///
/// Extracts session text from conversation events, calls the LLM (Ollama -> local-model fallback),
/// and parses the response into `Observation` structs.
///
/// Returns an empty Vec (not an error) if:
/// - Events are below `min_events` threshold
/// - No extractable text in events
/// - LLM returns no parseable observations
pub async fn extract_observations(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    session_id: Uuid,
    project_id: Option<Uuid>,
    query: &str,
    events: &[ConversationEvent],
    min_events: usize,
    max_input_chars: usize,
    runtime_config: &Arc<RuntimeConfig>,
) -> Result<Vec<Observation>> {
    if events.len() < min_events {
        tracing::debug!(
            session_id = %session_id,
            event_count = events.len(),
            min_events,
            "Skipping observation extraction: below minimum event threshold"
        );
        return Ok(vec![]);
    }

    let (session_text, _line_map) = match extract_session_text(events) {
        Some(result) => result,
        None => {
            tracing::debug!(
                session_id = %session_id,
                "Skipping observation extraction: no extractable text"
            );
            return Ok(vec![]);
        }
    };

    let truncated = truncate_session_text(&session_text, max_input_chars);
    let prompt = build_extraction_prompt(truncated, query);

    let raw_response = call_llm(
        store,
        event_bus,
        session_id,
        project_id,
        &prompt,
        runtime_config,
    )
    .await?;
    let observation_strings = parse_observations_response(&raw_response);

    if observation_strings.is_empty() {
        tracing::debug!(
            session_id = %session_id,
            "LLM returned no parseable observations"
        );
        return Ok(vec![]);
    }

    let now = Utc::now();
    let observations: Vec<Observation> = observation_strings
        .into_iter()
        .map(|content| Observation {
            id: Uuid::new_v4(),
            session_id,
            project_id,
            level: ObservationLevel::Explicit,
            content,
            source_ids: vec![],
            confidence: None,
            times_derived: 1,
            created_at: now,
            updated_at: now,
        })
        .collect();

    tracing::info!(
        session_id = %session_id,
        count = observations.len(),
        "Extracted observations from session"
    );

    Ok(observations)
}

async fn call_llm(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    session_id: Uuid,
    project_id: Option<Uuid>,
    prompt: &str,
    runtime_config: &Arc<RuntimeConfig>,
) -> Result<String> {
    let local_model = runtime_config.memory_model_local.read().clone();
    let fallback_model = runtime_config.memory_model_fallback.read().clone();
    let http = reqwest::Client::new();
    match generate_ollama(&http, prompt, &local_model).await {
        Ok(raw) => return Ok(raw),
        Err(e) => {
            tracing::debug!(
                error = %e,
                "Ollama observation extraction failed, falling back to local model"
            );
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
            purpose: rsi_common::model_control::ModelInvocationPurpose::MemoryObservationExtract,
            provider: Some(provider_label.clone()),
            model: Some(fallback_target.model.clone()),
            backend: Some(backend_label.clone()),
            effort: None,
            trigger: "memory_observation_extract".to_string(),
            owner: rsi_common::model_control::InvocationOwner {
                session_id: Some(session_id),
                project_id,
                ..rsi_common::model_control::InvocationOwner::default()
            },
            dedup_key: Some(crate::model_control::stable_dedup_key(
                "memory-observation",
                &[
                    &session_id.to_string(),
                    &project_id.map(|id| id.to_string()).unwrap_or_default(),
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
                rsi_common::model_control::ModelInvocationPurpose::MemoryObservationExtract,
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
        1024,
        "Observation extraction",
    )
    .await
}

/// LLM generation via shared Ollama HTTP client.
async fn generate_ollama(http: &reqwest::Client, prompt: &str, model: &str) -> Result<String> {
    let opts = crate::ollama_client::GenerateOptions {
        num_predict: Some(1024),
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
    let text = raw.trim().to_string();
    if text.is_empty() {
        return Err(DaemonError::Store("Ollama returned empty response".into()));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::{EventType, Role};

    fn make_event(
        sequence: i32,
        event_type: EventType,
        role: Option<Role>,
        content: &str,
    ) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            session_id: Uuid::nil(),
            sequence,
            event_type,
            role,
            content: content.to_string(),
            tool_name: None,
            tool_input: None,
            created_at: Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    // --- Prompt construction tests ---

    #[test]
    fn test_build_extraction_prompt_contains_key_elements() {
        let prompt = build_extraction_prompt("User: hello\nAssistant: hi", "fix bug");
        assert!(prompt.contains("observation extraction engine"));
        assert!(prompt.contains("fix bug"));
        assert!(prompt.contains("User: hello"));
        assert!(prompt.contains("JSON array"));
    }

    #[test]
    fn test_build_extraction_prompt_truncates_long_query() {
        let long_query = "a".repeat(500);
        let prompt = build_extraction_prompt("text", &long_query);
        // Query should be truncated to 400 chars
        assert!(!prompt.contains(&long_query));
        assert!(prompt.contains(&"a".repeat(400)));
    }

    // --- Truncation tests ---

    #[test]
    fn test_truncate_session_text_short() {
        let text = "short text";
        assert_eq!(truncate_session_text(text, 100), text);
    }

    #[test]
    fn test_truncate_session_text_at_limit() {
        let text = "exactly ten";
        assert_eq!(truncate_session_text(text, 11), text);
    }

    #[test]
    fn test_truncate_session_text_over_limit() {
        let text = "hello world this is long";
        let truncated = truncate_session_text(text, 11);
        assert_eq!(truncated, "hello world");
    }

    #[test]
    fn test_truncate_session_text_unicode_boundary() {
        // Multi-byte unicode: each char is 3 bytes
        let text = "aaaa";
        let truncated = truncate_session_text(text, 3);
        assert_eq!(truncated, "aaa");
    }

    // --- Parsing tests ---

    #[test]
    fn test_parse_valid_json_array() {
        let raw = r#"["Observation one", "Observation two", "Observation three"]"#;
        let obs = parse_observations_response(raw);
        assert_eq!(obs.len(), 3);
        assert_eq!(obs[0], "Observation one");
        assert_eq!(obs[1], "Observation two");
        assert_eq!(obs[2], "Observation three");
    }

    #[test]
    fn test_parse_json_array_with_empty_strings() {
        let raw = r#"["Good observation", "", "Another one", "  "]"#;
        let obs = parse_observations_response(raw);
        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0], "Good observation");
        assert_eq!(obs[1], "Another one");
    }

    #[test]
    fn test_parse_embedded_json_array() {
        let raw = r#"Here are the observations:
["First fact", "Second fact"]
Hope that helps!"#;
        let obs = parse_observations_response(raw);
        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0], "First fact");
    }

    #[test]
    fn test_parse_newline_separated_fallback() {
        let raw = "Observation one\nObservation two\nObservation three";
        let obs = parse_observations_response(raw);
        assert_eq!(obs.len(), 3);
        assert_eq!(obs[0], "Observation one");
    }

    #[test]
    fn test_parse_bullet_list_fallback() {
        let raw = "- First observation\n- Second observation\n- Third observation";
        let obs = parse_observations_response(raw);
        assert_eq!(obs.len(), 3);
        assert_eq!(obs[0], "First observation");
        assert_eq!(obs[1], "Second observation");
    }

    #[test]
    fn test_parse_numbered_list_fallback() {
        let raw = "1. First observation\n2. Second observation\n3. Third observation";
        let obs = parse_observations_response(raw);
        assert_eq!(obs.len(), 3);
        assert_eq!(obs[0], "First observation");
        assert_eq!(obs[1], "Second observation");
    }

    #[test]
    fn test_parse_asterisk_list_fallback() {
        let raw = "* First\n* Second";
        let obs = parse_observations_response(raw);
        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0], "First");
    }

    #[test]
    fn test_parse_empty_response() {
        assert!(parse_observations_response("").is_empty());
        assert!(parse_observations_response("   ").is_empty());
    }

    #[test]
    fn test_parse_malformed_json() {
        // Invalid JSON should fall through to line-based parsing
        let raw = r#"["incomplete array"#;
        let obs = parse_observations_response(raw);
        // Falls back to line parsing; the raw text itself becomes observations
        assert!(!obs.is_empty());
    }

    // --- extract_observations threshold tests ---

    #[tokio::test]
    async fn test_extract_below_min_events_returns_empty() {
        let events = vec![make_event(1, EventType::Message, Some(Role::User), "hello")];
        let rt_cfg = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let result = extract_observations(
            &store,
            &Arc::new(crate::bus::EventBus::new(8)),
            Uuid::new_v4(),
            None,
            "test",
            &events,
            4,
            16_000,
            &rt_cfg,
        )
        .await
        .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_extract_empty_events_returns_empty() {
        let rt_cfg = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let result = extract_observations(
            &store,
            &Arc::new(crate::bus::EventBus::new(8)),
            Uuid::new_v4(),
            None,
            "test",
            &[],
            4,
            16_000,
            &rt_cfg,
        )
        .await
        .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_extract_no_message_events_returns_empty() {
        let events = vec![
            make_event(1, EventType::ToolUse, None, "tool"),
            make_event(2, EventType::ToolResult, None, "result"),
            make_event(3, EventType::System, None, "system"),
            make_event(4, EventType::Thinking, None, "thinking"),
        ];
        let rt_cfg = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let result = extract_observations(
            &store,
            &Arc::new(crate::bus::EventBus::new(8)),
            Uuid::new_v4(),
            None,
            "test",
            &events,
            4,
            16_000,
            &rt_cfg,
        )
        .await
        .unwrap();
        assert!(result.is_empty());
    }

    // --- Byte-level regression: extractor request body shape ---
    //
    // Replicates the `GenerateOptions` block in `generate_ollama` above
    // (hard-coded `num_predict: 1024`). If either this file's helper or
    // `ollama_client::build_body` drifts, the snapshot fails.

    #[test]
    fn extractor_body_matches_pinned_snapshot() {
        let opts = crate::ollama_client::GenerateOptions {
            num_predict: Some(1024),
            temperature: 0.3,
            think: false,
            keep_alive: None,
        };
        let body =
            crate::ollama_client::build_body("qwen3:14b", None, "EXTRACTION_PROMPT", &opts, false);
        let expected = serde_json::json!({
            "model": "qwen3:14b",
            "prompt": "EXTRACTION_PROMPT",
            "stream": false,
            "think": false,
            "options": { "num_predict": 1024, "temperature": 0.3f32 },
        });
        assert_eq!(body, expected, "extractor body structure drifted");
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            serde_json::to_string(&expected).unwrap(),
            "extractor serialized bytes drifted",
        );
        assert!(body.get("system").is_none());
        assert!(body.get("keep_alive").is_none());
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
    async fn extractor_generate_ollama_posts_expected_body_shape() {
        let _lock = crate::ollama_client::TEST_OLLAMA_URL_LOCK.lock();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/api/generate$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("{\"response\":\"[\\\"obs1\\\"]\",\"done\":true}"),
            )
            .mount(&server)
            .await;
        // SAFETY: TEST_OLLAMA_URL_LOCK is held above.
        unsafe {
            std::env::set_var("RSI_OLLAMA_URL", format!("{}/api/generate", server.uri()));
        }
        let http = reqwest::Client::new();
        super::generate_ollama(&http, "EXTRACT PROMPT", "qwen3:14b")
            .await
            .expect("generate_ollama should succeed");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "expected exactly one request");
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("body is valid JSON");
        assert_eq!(body["model"], serde_json::json!("qwen3:14b"));
        assert_eq!(body["prompt"], serde_json::json!("EXTRACT PROMPT"));
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
