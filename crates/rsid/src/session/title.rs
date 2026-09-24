//! Title and description generation for sessions (Ollama -> Haiku fallback).

use super::types::{CompletedSession, TrackedSession};
use crate::bus::EventBus;
use crate::error::{DaemonError, Result};
use crate::model_control::{
    AdmissionDecision, ModelAdmissionRequest, admit_invocation, completion_with_wall_time,
    hash_request_fingerprint,
};
use crate::store::Store;
use rsi_common::model_control::{InvocationOwner, ModelInvocationPurpose, ModelUsageConfidence};
use rsi_common::types::{
    ConversationEvent, EventType, Role, Session, SessionKind, SessionProvider,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

/// Result of LLM title+description generation.
pub(crate) struct TitleResult {
    pub title: String,
    pub description: String,
}

/// One durable predicate shared by both generated-title boundaries.
///
/// A populated role and reserved ordinal under a hierarchy parent identify a
/// role-titled Epic lineage. Provider establishment, query text, kind, and
/// generated prose are deliberately irrelevant.
pub(crate) fn is_role_titled_epic_identity(session: &Session) -> bool {
    is_role_titled_epic_identity_parts(
        session.parent_id,
        session.agent_role.as_deref(),
        session.epic_spawn_ordinal,
    )
}

pub(crate) fn is_role_titled_epic_identity_parts(
    parent_id: Option<Uuid>,
    agent_role: Option<&str>,
    epic_spawn_ordinal: Option<u32>,
) -> bool {
    parent_id.is_some()
        && epic_spawn_ordinal.is_some_and(|ordinal| ordinal > 0)
        && agent_role.is_some_and(|role| !role.is_empty())
}

pub(crate) fn should_enqueue_initial_title_generation(
    session_kind: SessionKind,
    parent_id: Option<Uuid>,
    agent_role: Option<&str>,
    epic_spawn_ordinal: Option<u32>,
) -> bool {
    !matches!(session_kind, SessionKind::TaskRabbit | SessionKind::Bug)
        && !is_role_titled_epic_identity_parts(parent_id, agent_role, epic_spawn_ordinal)
}

pub(crate) fn should_enqueue_title_refinement(
    session_kind: SessionKind,
    role_titled_identity: bool,
    event_count: usize,
    title_is_none: bool,
) -> bool {
    !matches!(session_kind, SessionKind::TaskRabbit | SessionKind::Bug)
        && !role_titled_identity
        && event_count >= 3
        && title_is_none
}

#[cfg(test)]
mod identity_policy_tests {
    use super::*;

    #[test]
    fn title_generation_suppression_covers_both_invocation_boundaries() {
        let parent_id = Some(Uuid::new_v4());
        assert!(!should_enqueue_initial_title_generation(
            SessionKind::Task,
            parent_id,
            Some("Reviewer"),
            Some(4),
        ));
        assert!(!should_enqueue_title_refinement(
            SessionKind::Task,
            true,
            3,
            true,
        ));

        assert!(should_enqueue_initial_title_generation(
            SessionKind::Standard,
            None,
            None,
            None,
        ));
        assert!(should_enqueue_title_refinement(
            SessionKind::Standard,
            false,
            3,
            true,
        ));
        assert!(should_enqueue_initial_title_generation(
            SessionKind::Task,
            parent_id,
            None,
            Some(4),
        ));
        assert!(!is_role_titled_epic_identity_parts(
            parent_id,
            Some("Reviewer"),
            None,
        ));
    }
}

/// Build conversation context string from events, reusable by both prompt builders.
fn build_conversation_context(events: &[&ConversationEvent]) -> String {
    let messages: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == EventType::Message)
        .collect();
    if messages.is_empty() {
        return String::new();
    }

    let mut context = String::from("\n\nConversation context (last messages):\n");
    let mut context_budget = 600usize;
    for event in messages {
        let role_str = match event.role {
            Some(Role::User) => "User",
            Some(Role::Assistant) => "Assistant",
            _ => continue,
        };
        let content = if event.content.len() > 200 {
            let mut end = 200;
            while !event.content.is_char_boundary(end) {
                end -= 1;
            }
            &event.content[..end]
        } else {
            &event.content
        };
        let line = format!("{}: {}\n", role_str, content);
        if line.len() > context_budget {
            break;
        }
        context_budget -= line.len();
        context.push_str(&line);
    }
    context
}

/// Truncate a query string to a maximum byte length at a char boundary.
fn truncate_query(query: &str, max_bytes: usize) -> &str {
    if query.len() > max_bytes {
        let mut end = max_bytes;
        while !query.is_char_boundary(end) {
            end -= 1;
        }
        &query[..end]
    } else {
        query
    }
}

/// Hard ceiling on a generated session title, in words, INCLUDING the leading
/// role word. The operator reads the session list at a glance; a title that
/// wraps is a title that is not read.
pub(crate) const TITLE_MAX_WORDS: usize = 5;

/// Defensive byte ceiling for a normalized title. Word-count alone does not
/// bound a pathological single "word" pasted out of a stack trace.
const TITLE_MAX_BYTES: usize = 72;

/// Role word forced onto any session that holds lead authority over a
/// container row. Lead sessions are never named for the slice's craft; they
/// are named for the authority they carry.
pub(crate) const LEAD_ROLE_WORD: &str = "Demiurge";

/// Role word used when nothing in the model output survives normalization.
const FALLBACK_ROLE_WORD: &str = "Agent";

/// Build a combined title+description prompt that returns both in structured
/// format.
///
/// `forced_role` pins the title's leading role word (see [`LEAD_ROLE_WORD`]).
/// It is a *hint* here and an *invariant* in [`normalize_title`]: the prompt
/// asks, the normalizer enforces. That split is deliberate — this prompt is
/// answered by whatever title model the operator configured (local Ollama, a
/// third-party API, or the Claude CLI fallback), and instruction-following
/// across that range is not uniform. Nothing about the title contract may
/// depend on which backend answered.
pub(crate) fn build_title_and_description_prompt(
    query: &str,
    conversation_context: Option<&[&ConversationEvent]>,
    forced_role: Option<&str>,
) -> String {
    let truncated_query = truncate_query(query, 400);

    let mut prompt = String::with_capacity(2000);
    prompt.push_str(
        "You are naming an AI coding-agent session. Generate BOTH a SHORT title AND a detailed \
         description.\n\n\
         Output format (use these exact labels):\n\
         TITLE: <Role: subject>\n\
         DESCRIPTION: <one paragraph>\n\n",
    );

    prompt.push_str("TITLE rules:\n");
    prompt.push_str(&format!(
        "- {TITLE_MAX_WORDS} words MAXIMUM, in total, including the role word. Shorter is better.\n"
    ));
    match forced_role {
        Some(role) => {
            prompt.push_str(&format!(
                "- The first word MUST be exactly \"{role}\". This session holds lead authority \
                 over a container, so its role word is fixed. Do not substitute any other role.\n"
            ));
        }
        None => {
            prompt.push_str(
                "- The first word is the session's role, i.e. what kind of agent this is: \
                 Implementer, Researcher, Planner, Reviewer, Verifier, Debugger, Refactorer, \
                 Orchestrator, and so on. If the prompt does not state a role, infer the best \
                 fit; if none of the usual roles fit, invent the single word that fits best.\n",
            );
        }
    }
    prompt.push_str(
        "- Then a colon, then the slice, phase, or ticket identifier the session was assigned \
         if the prompt names one.\n\
         - If no slice was assigned, use 1-4 words naming what the session is working on.\n\
         - If there is no slice and no clear subject, output the role word alone.\n\
         - Title Case. No quotes, no trailing punctuation, no sentences, and no filler words \
           such as \"Session\", \"Task\", or \"Agent\".\n\n",
    );

    prompt.push_str("TITLE examples:\n");
    match forced_role {
        Some(role) => {
            prompt.push_str(&format!(
                "TITLE: {role}: S3 Custody Fence\n\
                 TITLE: {role}: Title Pipeline\n\
                 TITLE: {role}\n\n"
            ));
        }
        None => {
            prompt.push_str(
                "TITLE: Implementer: S3 Custody Fence\n\
                 TITLE: Researcher: Title Generation Pipeline\n\
                 TITLE: Reviewer: RSI-014\n\
                 TITLE: Debugger\n\n",
            );
        }
    }

    prompt.push_str(
        "DESCRIPTION rules:\n\
         - A full paragraph (3-5 sentences, 200-500 characters) describing what this session is \
           doing, the technical approach, key files or systems involved, and the expected \
           outcome.\n\
         - No quotes around the text.\n\n\
         Output ONLY the two labeled lines. No preamble, no explanation.\n\n",
    );

    prompt.push_str("Initial prompt:\n");
    prompt.push_str(truncated_query);

    if let Some(events) = conversation_context {
        prompt.push_str(&build_conversation_context(events));
    }

    prompt
}

/// Trim quoting, emphasis, and stray punctuation from one output token.
fn clean_token(token: &str) -> &str {
    token.trim_matches(|c: char| !c.is_alphanumeric())
}

/// Normalize one candidate role word: first token, punctuation stripped,
/// first letter upper-cased. `None` when nothing usable remains.
fn role_word(raw: &str) -> Option<String> {
    let token = clean_token(raw.split_whitespace().next()?);
    let mut chars = token.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().chain(chars).collect::<String>())
}

/// Take at most `max_words` usable words from the subject half of a title.
fn subject_words(raw: &str, max_words: usize) -> String {
    let mut words = Vec::with_capacity(max_words);
    for token in raw.split_whitespace() {
        if words.len() == max_words {
            break;
        }
        let cleaned = token.trim_matches(|c: char| {
            !c.is_alphanumeric() && !matches!(c, '-' | '_' | '#' | '/' | '.')
        });
        if !cleaned.is_empty() {
            words.push(cleaned);
        }
    }
    words.join(" ")
}

/// Clamp a string to `max_bytes` on a char boundary.
fn clamp_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].trim_end()
}

/// Enforce the title contract on raw model output, whatever produced it.
///
/// The shape is `Role: subject`, at most [`TITLE_MAX_WORDS`] words including
/// the role word. `forced_role` overrides whatever role the model chose, which
/// is what makes lead authority ([`LEAD_ROLE_WORD`]) an invariant rather than
/// a request. Runs on every backend path, so a local model that ignores the
/// format and a hosted model that follows it converge on the same shape.
fn normalize_title(raw: &str, forced_role: Option<&str>) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let line = line
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '#'))
        .trim();

    // Some models re-emit the label inside the value ("TITLE: Title: Foo").
    let line = line
        .strip_prefix("Title:")
        .or_else(|| line.strip_prefix("title:"))
        .unwrap_or(line)
        .trim();

    // With a colon the split is explicit; without one the first word is the
    // model's role word and the remainder is the subject.
    let (role_part, subject_part) = match line.split_once(':') {
        Some(parts) => parts,
        None => line.split_once(char::is_whitespace).unwrap_or((line, "")),
    };

    let role = forced_role
        .and_then(role_word)
        .or_else(|| role_word(role_part))
        .unwrap_or_else(|| FALLBACK_ROLE_WORD.to_string());
    let subject = subject_words(subject_part, TITLE_MAX_WORDS.saturating_sub(1));

    let title = if subject.is_empty() {
        role
    } else {
        format!("{role}: {subject}")
    };
    clamp_bytes(&title, TITLE_MAX_BYTES).to_string()
}

/// Parse structured "TITLE: ...\nDESCRIPTION: ..." output from the LLM and
/// enforce the title contract via [`normalize_title`].
/// Falls back gracefully: if only a title is found, description is empty.
fn parse_title_and_description(raw: &str, forced_role: Option<&str>) -> Option<TitleResult> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Try structured format first
    if let Some(title_start) = trimmed.find("TITLE:") {
        let after_title = trimmed[title_start + 6..].trim_start();
        let (title_text, description_text) =
            if let Some(desc_start) = after_title.find("DESCRIPTION:") {
                let title = after_title[..desc_start].trim();
                let desc = after_title[desc_start + 12..].trim();
                (title, desc)
            } else {
                (after_title.trim(), "")
            };

        let title = normalize_title(title_text, forced_role);
        if !title.is_empty() {
            return Some(TitleResult {
                title,
                description: description_text.to_string(),
            });
        }
    }

    // Fallback: treat entire output as the title (backwards compatible).
    let title = normalize_title(trimmed, forced_role);
    if title.is_empty() {
        return None;
    }
    Some(TitleResult {
        title,
        description: String::new(),
    })
}

/// Resolve the forced role word for a session, from live lead authority.
///
/// A session holds lead authority when its own parent row names it in
/// `lead_session_id`. Lead candidates are always direct children of the
/// container they lead (`validate_lead_candidate`), so the parent row is the
/// only place that authority can live — no scan is needed. Authority is read
/// from the runtime/store projection, never from anything the session itself
/// supplied.
pub(super) async fn forced_role_for_session(
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    completed: &Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    store: &Arc<Mutex<Store>>,
    session_id: Uuid,
    parent_id: Option<Uuid>,
) -> Option<&'static str> {
    let parent_id = parent_id?;
    let parent = super::queries::get_session_snapshot(active, completed, store, parent_id).await?;
    (parent.lead_session_id == Some(session_id)).then_some(LEAD_ROLE_WORD)
}

/// Generate both title and description from query and optional context.
pub(crate) async fn generate_title_and_description(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    session_id: Uuid,
    query: &str,
    conversation_context: Option<&[&ConversationEvent]>,
    forced_role: Option<&str>,
    ollama_model: &str,
    fallback_model: &str,
    provider: SessionProvider,
    base_url: Option<String>,
    api_key: Option<String>,
) -> Result<TitleResult> {
    let prompt = build_title_and_description_prompt(query, conversation_context, forced_role);
    generate_title_and_description_from_prompt(
        store,
        event_bus,
        session_id,
        &prompt,
        forced_role,
        ollama_model,
        fallback_model,
        provider,
        base_url,
        api_key,
    )
    .await
}

/// Send a pre-built title+description prompt to the LLM backend.
pub(crate) async fn generate_title_and_description_from_prompt(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    session_id: Uuid,
    prompt: &str,
    forced_role: Option<&str>,
    ollama_model: &str,
    fallback_model: &str,
    provider: SessionProvider,
    base_url: Option<String>,
    api_key: Option<String>,
) -> Result<TitleResult> {
    let target = crate::memory::llm::MemoryLlmTarget {
        provider,
        model: ollama_model.to_string(),
        base_url,
        api_key,
    };
    let http = reqwest::Client::new();
    if crate::memory::llm::uses_native_ollama(&target)? {
        match generate_ollama_admitted(
            store,
            event_bus,
            session_id,
            &http,
            prompt,
            512,
            ollama_model,
        )
        .await
        {
            Ok(raw) => {
                if let Some(result) = parse_title_and_description(&raw, forced_role) {
                    return Ok(result);
                }
                tracing::debug!(
                    "Ollama returned unparseable title+description, falling back to CLI"
                );
            }
            Err(e) => {
                tracing::debug!(error = %e, "Ollama title+description generation failed, falling back to CLI");
            }
        }
        let fallback_target = crate::memory::llm::MemoryLlmTarget {
            provider: SessionProvider::Claude,
            model: fallback_model.to_string(),
            base_url: None,
            api_key: None,
        };
        let raw = crate::memory::llm::admit_and_generate_text(
            store,
            event_bus,
            title_request(
                session_id,
                crate::memory::llm::provider_label(&fallback_target)?,
                fallback_model.to_string(),
                crate::memory::llm::backend_label(&fallback_target)?.to_string(),
                prompt,
            ),
            &fallback_target,
            prompt,
            512,
            "Title generation",
        )
        .await?;
        return parse_title_and_description(&raw, forced_role)
            .ok_or_else(|| DaemonError::Store("Empty title+description generated".into()));
    }

    let raw = crate::memory::llm::admit_and_generate_text(
        store,
        event_bus,
        title_request(
            session_id,
            crate::memory::llm::provider_label(&target)?,
            target.model.clone(),
            crate::memory::llm::backend_label(&target)?.to_string(),
            prompt,
        ),
        &target,
        prompt,
        512,
        "Title generation",
    )
    .await?;
    parse_title_and_description(&raw, forced_role)
        .ok_or_else(|| DaemonError::Store("Empty title+description generated".into()))
}

/// LLM generation via shared Ollama HTTP client.
/// Uses a short timeout so fallback is fast when Ollama isn't running.
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
        std::time::Duration::from_secs(30),
    )
    .await
    .map_err(|e| DaemonError::Store(e.to_string()))?;
    let text = raw.trim().to_string();
    if text.is_empty() {
        return Err(DaemonError::Store("Ollama returned empty response".into()));
    }
    Ok(text)
}

fn title_request(
    session_id: Uuid,
    provider: String,
    model: String,
    backend: String,
    prompt: &str,
) -> ModelAdmissionRequest {
    let dedup_key = crate::model_control::stable_dedup_key(
        "session-title",
        &[&session_id.to_string(), &provider, &model, &backend, prompt],
    );
    let request_fingerprint = hash_request_fingerprint(&[&provider, &model, &backend, prompt]);
    let expected_usage = crate::model_control::explicit_expected_usage(
        ModelInvocationPurpose::SessionTitle,
        Some(provider.as_str()),
        Some(backend.as_str()),
        Some(model.as_str()),
    );
    ModelAdmissionRequest {
        purpose: ModelInvocationPurpose::SessionTitle,
        provider: Some(provider),
        model: Some(model),
        backend: Some(backend),
        effort: None,
        trigger: "session_title".to_string(),
        owner: InvocationOwner {
            session_id: Some(session_id),
            ..InvocationOwner::default()
        },
        dedup_key: Some(dedup_key),
        request_fingerprint: Some(request_fingerprint),
        parent_invocation_id: None,
        retry_of_invocation_id: None,
        expected_usage: Some(expected_usage),
        baseline_input_tokens: 0,
        baseline_output_tokens: 0,
        baseline_cache_creation_tokens: 0,
        baseline_cache_read_tokens: 0,
        baseline_reasoning_tokens: 0,
        baseline_embedding_input_count: 0,
        baseline_wall_time_ms: 0,
    }
}

async fn generate_ollama_admitted(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    session_id: Uuid,
    http: &reqwest::Client,
    prompt: &str,
    num_predict: u32,
    model: &str,
) -> Result<String> {
    let request = title_request(
        session_id,
        "Local".to_string(),
        model.to_string(),
        "ollama".to_string(),
        prompt,
    );
    let permit = match admit_invocation(store, request, event_bus).await? {
        AdmissionDecision::Admitted(permit) => permit,
        AdmissionDecision::Duplicate { invocation_id } => {
            return Err(DaemonError::PolicyDenied(format!(
                "duplicate title generation suppressed ({invocation_id})"
            )));
        }
    };
    let started_at = Instant::now();
    let result = generate_ollama(http, prompt, num_predict, model).await;
    let completion = match &result {
        Ok(_) => completion_with_wall_time(started_at, None, ModelUsageConfidence::Partial),
        Err(_) => completion_with_wall_time(
            started_at,
            Some("ollama_failed".to_string()),
            ModelUsageConfidence::Partial,
        ),
    };
    crate::model_control::settle_result(
        store,
        &permit,
        completion,
        result,
        "Title generation",
        event_bus,
    )
    .await
}

/// LLM generation via headless Claude CLI (fallback).
async fn generate_cli_fallback(prompt: &str, model: &str) -> Result<String> {
    let output = tokio::process::Command::new("claude")
        .args([
            "-p",
            prompt,
            "--model",
            model,
            "--output-format",
            "text",
            "--no-session-persistence",
            "--permission-mode",
            "bypassPermissions",
        ])
        .output()
        .await
        .map_err(|e| DaemonError::Store(format!("Title generation failed: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            format!("exit status {}; stderr: {}", output.status, stderr)
        };
        return Err(DaemonError::Store(format!(
            "Title generation fallback failed using Claude model '{}': {}",
            model, detail
        )));
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return Err(DaemonError::Store("Empty title generated".into()));
    }

    Ok(text)
}

#[cfg(test)]
mod title_shape_tests {
    //! The title contract is enforced after generation, not requested during
    //! it, so every backend — local Ollama, a hosted OpenAI/Anthropic-shaped
    //! API, or the Claude CLI fallback — converges on the same shape. These
    //! tests therefore exercise `parse_title_and_description` directly with
    //! raw text of the kind each backend actually returns.
    use super::{
        LEAD_ROLE_WORD, TITLE_MAX_WORDS, build_title_and_description_prompt,
        parse_title_and_description,
    };

    fn title_of(raw: &str, forced_role: Option<&str>) -> String {
        parse_title_and_description(raw, forced_role)
            .expect("non-empty output parses")
            .title
    }

    fn word_count(title: &str) -> usize {
        title.split_whitespace().count()
    }

    #[test]
    fn structured_output_keeps_role_and_slice() {
        let title = title_of(
            "TITLE: Implementer: S3 Custody Fence\nDESCRIPTION: Adds a fence.",
            None,
        );
        assert_eq!(title, "Implementer: S3 Custody Fence");
    }

    #[test]
    fn sentence_title_is_clamped_to_the_word_budget() {
        // The pre-change prompt asked for 150-250 characters of prose; a model
        // still answering that way must be reduced to the short form.
        let raw = "TITLE: Researcher: Investigates the session title generation \
                   pipeline end to end and reports where the prompt is built. \
                   It also covers the fallback path.\nDESCRIPTION: x";
        let title = title_of(raw, None);
        assert_eq!(title, "Researcher: Investigates the session title");
        assert!(
            word_count(&title) <= TITLE_MAX_WORDS,
            "title {title:?} exceeds {TITLE_MAX_WORDS} words",
        );
    }

    #[test]
    fn missing_colon_promotes_the_first_word_to_the_role() {
        let title = title_of("TITLE: Debugger stall detector regression", None);
        assert_eq!(title, "Debugger: stall detector regression");
    }

    #[test]
    fn role_only_output_survives_as_the_whole_title() {
        assert_eq!(title_of("TITLE: Reviewer", None), "Reviewer");
    }

    #[test]
    fn quoting_and_emphasis_are_stripped_and_role_is_capitalized() {
        assert_eq!(
            title_of("TITLE: \"implementer: RSI-014 model routing\"", None),
            "Implementer: RSI-014 model routing",
        );
    }

    #[test]
    fn unlabeled_backend_output_is_normalized_the_same_way() {
        // Small local models frequently drop the TITLE:/DESCRIPTION: labels.
        // That path must land on the same contract as the structured one.
        let title = title_of("Planner: phase two rollout plan for the daemon", None);
        assert_eq!(title, "Planner: phase two rollout plan");
        assert!(word_count(&title) <= TITLE_MAX_WORDS);
    }

    #[test]
    fn lead_authority_replaces_whatever_role_the_model_chose() {
        let title = title_of(
            "TITLE: Implementer: S3 Custody Fence\nDESCRIPTION: x",
            Some(LEAD_ROLE_WORD),
        );
        assert_eq!(title, "Demiurge: S3 Custody Fence");
    }

    #[test]
    fn lead_authority_applies_to_unlabeled_and_role_only_output() {
        assert_eq!(
            title_of("Orchestrator", Some(LEAD_ROLE_WORD)),
            "Demiurge",
            "a role-only answer still takes the lead role word",
        );
        assert_eq!(
            title_of("TITLE: coordinating the burn-down", Some(LEAD_ROLE_WORD)),
            "Demiurge: the burn-down",
        );
    }

    #[test]
    fn empty_output_yields_no_title() {
        assert!(parse_title_and_description("   \n  ", None).is_none());
    }

    #[test]
    fn description_is_preserved_alongside_the_short_title() {
        let parsed = parse_title_and_description(
            "TITLE: Verifier: manifest coverage\nDESCRIPTION: Walks the \
             verification manifest and confirms each declared key is covered.",
            None,
        )
        .expect("parses");
        assert_eq!(parsed.title, "Verifier: manifest coverage");
        assert!(
            parsed
                .description
                .starts_with("Walks the verification manifest"),
            "description text preserved: {:?}",
            parsed.description,
        );
    }

    #[test]
    fn prompt_states_the_word_budget_and_the_forced_lead_role() {
        let plain = build_title_and_description_prompt("do the thing", None, None);
        assert!(plain.contains(&format!("{TITLE_MAX_WORDS} words MAXIMUM")));
        assert!(plain.contains("TITLE: Implementer: S3 Custody Fence"));

        let lead = build_title_and_description_prompt("do the thing", None, Some(LEAD_ROLE_WORD));
        assert!(
            lead.contains("The first word MUST be exactly \"Demiurge\""),
            "lead prompt pins the role word",
        );
        assert!(lead.contains("TITLE: Demiurge: S3 Custody Fence"));
    }
}

#[cfg(test)]
mod body_snapshot_tests {
    //! Byte-level regression: the `/api/generate` request body produced for the
    //! title pipeline must match the pre-refactor helper's shape exactly. If
    //! `generate_ollama` or `ollama_client::build_body` drifts, this fails.
    use crate::ollama_client::{GenerateOptions, build_body};

    /// Replicates the `GenerateOptions` block inside `generate_ollama` in this
    /// file so a change to that block will diverge the snapshot.
    fn title_opts(num_predict: u32) -> GenerateOptions {
        GenerateOptions {
            num_predict: Some(num_predict),
            temperature: 0.3,
            think: false,
            keep_alive: None,
        }
    }

    #[test]
    fn title_request_body_matches_pinned_snapshot() {
        let opts = title_opts(512);
        let body = build_body("qwen3:14b", None, "TITLE_PROMPT", &opts, false);

        // Structural snapshot (order-independent).
        let expected = serde_json::json!({
            "model": "qwen3:14b",
            "prompt": "TITLE_PROMPT",
            "stream": false,
            "think": false,
            "options": { "num_predict": 512, "temperature": 0.3f32 },
        });
        assert_eq!(body, expected, "title request body structure drifted");

        // Byte-identical serialization: catches key-order regressions in the
        // shared `serde_json::Map` backend.
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            serde_json::to_string(&expected).unwrap(),
            "title request body serialized bytes drifted",
        );

        // Explicitly assert the optional keys are omitted when `None`.
        assert!(body.get("system").is_none(), "system key must be absent");
        assert!(
            body.get("keep_alive").is_none(),
            "keep_alive must be absent"
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
    // serialise env-var mutation across this binary's HTTP tests. Dropping
    // before await would defeat its purpose.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn title_generate_ollama_posts_expected_body_shape() {
        let _lock = crate::ollama_client::TEST_OLLAMA_URL_LOCK.lock();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/api/generate$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("{\"response\":\"Title: X\\nDesc: Y\",\"done\":true}"),
            )
            .mount(&server)
            .await;
        // SAFETY: TEST_OLLAMA_URL_LOCK is held above.
        unsafe {
            std::env::set_var("RSI_OLLAMA_URL", format!("{}/api/generate", server.uri()));
        }
        let http = reqwest::Client::new();
        super::generate_ollama(&http, "TITLE PROMPT", 512, "qwen3:14b")
            .await
            .expect("generate_ollama should succeed");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "expected exactly one request");
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("body is valid JSON");
        assert_eq!(body["model"], serde_json::json!("qwen3:14b"));
        assert_eq!(body["prompt"], serde_json::json!("TITLE PROMPT"));
        assert_eq!(body["stream"], serde_json::json!(false));
        assert_eq!(body["think"], serde_json::json!(false));
        assert_eq!(body["options"]["num_predict"], serde_json::json!(512));
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
