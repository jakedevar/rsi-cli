//! Dream-cycle observation extractor.
//!
//! Wraps the existing observation extraction pipeline to work within the dream cycle,
//! using the DreamerLlmClient instead of the default Ollama->Haiku fallback.

use crate::error::Result;
use crate::memory::session_text::extract_session_text;
use crate::observation::extractor::{build_extraction_prompt, parse_observations_response};
use rsi_common::types::{ConversationEvent, Observation, ObservationLevel};
use uuid::Uuid;

use super::llm_client::DreamerLlmClient;

pub(crate) const EXTRACTION_SYSTEM_PROMPT: &str = "You are an observation extraction engine. Extract atomic factual observations from the coding session transcript provided. Output format: JSON array of strings, one per observation.";

pub(crate) fn build_dream_extraction_prompt(
    events: &[ConversationEvent],
    query: &str,
    min_events: usize,
    max_input_chars: usize,
) -> Option<String> {
    if events.len() < min_events {
        return None;
    }

    let (session_text, _line_map) = extract_session_text(events)?;
    let truncated = if session_text.len() > max_input_chars {
        let mut end = max_input_chars;
        while !session_text.is_char_boundary(end) {
            end -= 1;
        }
        session_text[..end].to_string()
    } else {
        session_text
    };

    Some(build_extraction_prompt(&truncated, query))
}

pub(crate) fn parse_dream_extraction_response(
    session_id: Uuid,
    project_id: Option<Uuid>,
    raw_response: &str,
) -> Vec<Observation> {
    let observation_strings = parse_observations_response(raw_response);
    if observation_strings.is_empty() {
        return Vec::new();
    }

    let now = chrono::Utc::now();
    observation_strings
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
        .collect()
}

/// Extract explicit observations from a session's conversation events using the dreamer LLM client.
pub async fn extract_observations_with_client(
    llm: &DreamerLlmClient,
    session_id: Uuid,
    project_id: Option<Uuid>,
    query: &str,
    events: &[ConversationEvent],
    min_events: usize,
    max_input_chars: usize,
) -> Result<Vec<Observation>> {
    if events.len() < min_events {
        tracing::debug!(
            session_id = %session_id,
            event_count = events.len(),
            min_events,
            "Dreamer: skipping extraction, below minimum event threshold"
        );
        return Ok(vec![]);
    }

    let Some(prompt) = build_dream_extraction_prompt(events, query, min_events, max_input_chars)
    else {
        tracing::debug!(
            session_id = %session_id,
            "Dreamer: skipping extraction, no extractable text"
        );
        return Ok(vec![]);
    };
    let raw_response = llm.complete(EXTRACTION_SYSTEM_PROMPT, &prompt).await?;
    let observations = parse_dream_extraction_response(session_id, project_id, &raw_response);

    if observations.is_empty() {
        tracing::debug!(
            session_id = %session_id,
            "Dreamer: LLM returned no parseable observations"
        );
        return Ok(vec![]);
    }

    tracing::info!(
        session_id = %session_id,
        count = observations.len(),
        "Dreamer: extracted observations from session"
    );

    Ok(observations)
}
