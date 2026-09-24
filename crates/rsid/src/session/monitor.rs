//! Session monitoring (event stream processing) for SessionManager.

use super::SessionManager;
use super::rotation::PostFinalizeRotationOutcome;
use super::spawn_coordinator::SpawnCoordinator;
use super::spawn_directive::SpawnDirective;
use super::types::{
    CompletedSession, HALT_DIRECTIVE_RE, MonitorBreakReason, PIPELINE_PATH_RE,
    ProcessSettlementMode, ProcessSettlementOutcome, SPAWN_DIRECTIVE_RE, TerminalDecision,
    TerminalEvidence, TerminalFinalizeDecision, TerminalResult, TerminalRetentionReason,
    TerminalRotationAction, TerminalTurnOutcome, TrackedSession, install_context_budget,
};
use crate::bus::DaemonEvent;
use crate::claude::StreamEvent;
use crate::memory::flush::{
    MEMORY_FLUSH_PROMPT, MEMORY_FLUSH_SYSTEM_PROMPT, MemoryFlushSettings, format_flush_prompt,
    should_run_memory_flush,
};
use crate::monitor;
use crate::provider::ProviderSession;
use crate::store::Store;
use rsi_common::closure_kernel::{ConversationEventProducerKindV1, ConversationEventProvenanceV1};
use rsi_common::types::{
    ContextUsageConfidence, ConversationEvent, EventType, Role, Session, SessionProvider,
    SessionStatus, WorkflowStage,
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

use super::rotation_coordinator::{
    RotationAction, RotationEvent, RotationState, is_handoff_file, is_pipeline_artifact,
};
use super::types::PersistenceHandle;

fn capture_spawn_child_handoff(session: &mut Session, action: &RotationAction) {
    // The coordinator is already Completed when it returns SpawnChild.
    if let RotationAction::SpawnChild {
        handoff_filepath: Some(path),
        ..
    } = action
    {
        session.handoff_filepath = Some(path.clone());
    }
}

/// Producer ordering shared by the monitor and its persistence-race tests.
/// Detection and append are one runtime publication. The acknowledged Store
/// operation then invalidates old durable identity before attempting insertion.
async fn persist_tracked_provider_event(
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    persistence: &PersistenceHandle,
    session_id: Uuid,
    expected_generation: u64,
    event: &ConversationEvent,
    provenance: Option<ConversationEventProvenanceV1>,
) -> (
    Option<rsi_common::types::PendingQuestion>,
    crate::error::Result<i64>,
) {
    let detected_question = {
        let mut guard = active.write().await;
        let Some(tracked) = guard
            .get_mut(&session_id)
            .filter(|tracked| tracked.spawn_generation == expected_generation)
        else {
            return (
                None,
                Err(crate::error::DaemonError::SessionNotFound(session_id)),
            );
        };
        let question = super::question::detect(tracked.session.provider, event);
        tracked.events.push(event.clone());
        if let Some(question) = question.as_ref() {
            tracked.pending_question = Some(question.clone());
            tracked.session.pending_question = Some(question.clone());
            if tracked.approval_wait_start.is_none() {
                tracked.approval_wait_start = Some(std::time::Instant::now());
            }
        }
        question
    };
    let persisted = if let Some(question) = detected_question.as_ref() {
        persistence
            .publish_question_event(event.clone(), provenance, question.clone())
            .await
    } else if let Some(provenance) = provenance {
        persistence
            .insert_event_with_provenance(event.clone(), provenance)
            .await
    } else {
        persistence.insert_event(event.clone()).await
    };
    if let Ok(db_id) = persisted {
        let mut guard = active.write().await;
        if let Some(tracked) = guard
            .get_mut(&session_id)
            .filter(|tracked| tracked.spawn_generation == expected_generation)
            && let Some(stored) = tracked.events.iter_mut().rev().find(|stored| {
                stored.id == 0
                    && stored.sequence == event.sequence
                    && stored.tool_use_id == event.tool_use_id
            })
        {
            stored.id = db_id;
        }
    }
    (detected_question, persisted)
}

/// Staleness window: a Claude + Running session that has gone this long
/// without an API-reported usage block flips `Full`/`Partial` → `Stale`.
/// The percentage remains pinned to the last API-reported numerator; the
/// stale flag is a warning marker only, not a switch to a second counting
/// path. 60s is chosen because typical Claude chunk cadence is 2–10s wall
/// time even on long tool chains, so a 60s gap reliably signals "the CLI has
/// gone quiet" rather than "we're between prompt-cache boundaries" (the
/// Anthropic prompt cache TTL is ~5 min).
const USAGE_STALENESS_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
const CODEX_CONTEXT_BASELINE_TOKENS: u64 = 12_000;

pub(super) fn no_idle_outcome_owns_capacity_c5(
    outcome: Option<&crate::session::agent_verbs::MasterNoIdleOutcome>,
) -> bool {
    match outcome {
        Some(crate::session::agent_verbs::MasterNoIdleOutcome::CapacityRecovered {
            settlement,
        }) => {
            settlement.c5_resolution
                != crate::store::capacity_recovery::CapacityC5Resolution::Unrelated
        }
        Some(crate::session::agent_verbs::MasterNoIdleOutcome::TerminalAllowed {
            capacity: Some(settlement),
        }) => {
            settlement.c5_resolution
                != crate::store::capacity_recovery::CapacityC5Resolution::Unrelated
        }
        _ => false,
    }
}

fn workflow_stage_for_pipeline_artifact(path: &str) -> Option<WorkflowStage> {
    let p = std::path::Path::new(path);
    if p.starts_with("thoughts/shared/research") {
        Some(WorkflowStage::ResearchComplete)
    } else if p.starts_with("thoughts/shared/plans") {
        Some(WorkflowStage::PlanComplete)
    } else {
        None
    }
}

fn tool_use_can_create_path(data: &serde_json::Value, filepath: &str) -> bool {
    let name = data
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if name.contains("write") || name.contains("edit") || name == "create_file" {
        return true;
    }

    if !matches!(name.as_str(), "shell" | "bash") {
        return false;
    }

    let command = data
        .get("input")
        .and_then(|v| v.get("command"))
        .or_else(|| data.get("command"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    shell_command_can_create_path(command, filepath)
}

fn shell_command_can_create_path(command: &str, filepath: &str) -> bool {
    if !command.contains(filepath) {
        return false;
    }

    let lower = command.to_ascii_lowercase();
    let writes_to_path = command.match_indices('>').any(|(index, _)| {
        let before = command[..index].chars().last();
        let after = command[index + 1..].trim_start();
        before != Some('2')
            && before != Some('&')
            && !after.starts_with('&')
            && (after.starts_with(filepath)
                || after.starts_with(&format!("'{filepath}'"))
                || after.starts_with(&format!("\"{filepath}\"")))
    });
    let explicit_writer = lower.contains("tee ")
        || lower.contains("apply_patch")
        || lower.contains("touch ")
        || lower.contains("mv ")
        || lower.contains("cp ")
        || lower.contains("install ");
    let read_only = ["sed -n ", "cat ", "head ", "rg ", "grep "]
        .iter()
        .any(|verb| lower.contains(verb));
    if read_only && !writes_to_path && !explicit_writer {
        return false;
    }
    writes_to_path
        || explicit_writer
        || lower.contains("python")
        || lower.contains("node ")
        || lower.contains("ruby")
}

fn assistant_text_declares_handoff_path(text: &str, filepath: &str) -> bool {
    text.lines().any(|line| {
        if !line.contains(filepath) {
            return false;
        }

        let lower = line.to_ascii_lowercase();
        lower.contains("doc_path")
            || ((lower.contains("handoff") || lower.contains("handoffs"))
                && (lower.contains("wrote")
                    || lower.contains("written")
                    || lower.contains("created")
                    || lower.contains("saved")))
    })
}

/// If the session is Claude/Codex + Running and no API usage update has
/// arrived within `USAGE_STALENESS_WINDOW`, downgrade `Full`/`Partial` →
/// `Stale`. Otherwise return the confidence unchanged.
///
/// * Other providers retain their existing BPE/estimator confidence behavior.
/// * Non-Running statuses are exempt — a `Completed`/`Failed`/`Archived`
///   session with an hour-old last-usage is not "stale," it's just done. No
///   signal to raise.
/// * `last_usage_update == None` falls through unchanged — the `Missing`
///   path in `live_context_state` already covers pre-first-turn fallback.
/// * Only `Full`/`Partial` are eligible to flip. `Counted` (already
///   BPE-primary) and `Missing` (no usage yet) stay as-is; `Stale` is
///   idempotent.
fn apply_staleness(
    tracked: &TrackedSession,
    confidence: ContextUsageConfidence,
) -> ContextUsageConfidence {
    if !matches!(tracked.session.provider, SessionProvider::Claude)
        && !is_codex_context_provider(tracked.session.provider)
    {
        return confidence;
    }
    if !matches!(tracked.session.status, SessionStatus::Running) {
        return confidence;
    }
    let Some(last) = tracked.last_usage_update else {
        return confidence;
    };
    if last.elapsed() > USAGE_STALENESS_WINDOW
        && matches!(
            confidence,
            ContextUsageConfidence::Full | ContextUsageConfidence::Partial
        )
    {
        ContextUsageConfidence::Stale
    } else {
        confidence
    }
}

/// Decide whether a stream event is authoritative enough to update the
/// session's model metadata.
///
/// Rationale: some providers can surface auxiliary-model metadata after launch
/// (for example, an internal helper or summarizer model) in events that are
/// not the initial session handshake. If we treat every top-level `model`
/// field as authoritative, a user-selected launch model can be silently
/// clobbered mid-session. We only accept:
/// - the first announced model when the session has no model yet, or
/// - a `system/init` event, which is the provider's launch-time handshake.
fn authoritative_model_update<'a>(
    current_model: Option<&str>,
    stream_event: &'a StreamEvent,
) -> Option<&'a str> {
    let model = stream_event.data.get("model").and_then(|v| v.as_str())?;
    let is_init_event = stream_event.event_type == "system"
        && stream_event.data.get("subtype").and_then(|v| v.as_str()) == Some("init");
    if current_model.is_none() || is_init_event {
        Some(model)
    } else {
        None
    }
}

/// What the provider CLI advertised about itself at `system/init` (V99, P1-A).
///
/// The handshake is the only place the CLI states its own version and the
/// capability tokens it honours. RSI previously read two fields out of init
/// (`model`, `session_id`) and discarded the rest, so every flag it passed was
/// *assumed* supported rather than *known* supported. Recording this is the
/// foundation for gating a later flag on advertised support.
#[derive(Debug, Default, PartialEq, Eq)]
struct ProviderHandshake {
    cli_version: Option<String>,
    capabilities: Vec<String>,
}

/// Parse a `rate_limit_event` into an account-level snapshot (V99, P1-B).
///
/// Returns `None` for any other event and for one carrying no windows.
///
/// The `unifiedWindows` object is ITERATED, never matched against a hardcoded
/// pair of keys: `five_hour` and `seven_day` are the two observed today, but a
/// provider adding a third window should have it captured rather than silently
/// dropped. `resetsAt` is unix epoch seconds and is carried verbatim.
fn parse_rate_limit_event(
    provider: SessionProvider,
    stream_event: &StreamEvent,
) -> Option<rsi_common::rpc::ProviderRateLimitSnapshot> {
    if stream_event.event_type != "rate_limit_event" {
        return None;
    }
    let info = stream_event.data.get("rate_limit_info")?;

    let windows: Vec<rsi_common::rpc::ProviderRateLimitWindow> = info
        .get("unifiedWindows")
        .and_then(|v| v.as_object())
        .map(|windows| {
            windows
                .iter()
                .filter_map(|(window_key, value)| {
                    Some(rsi_common::rpc::ProviderRateLimitWindow {
                        window_key: window_key.clone(),
                        utilization: value.get("utilization")?.as_f64()?,
                        resets_at_epoch: value.get("resetsAt").and_then(|v| v.as_i64()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    if windows.is_empty() {
        return None;
    }

    Some(rsi_common::rpc::ProviderRateLimitSnapshot {
        provider,
        status: info
            .get("status")
            .and_then(|v| v.as_str())
            .map(String::from),
        rate_limit_type: info
            .get("rateLimitType")
            .and_then(|v| v.as_str())
            .map(String::from),
        overage_status: info
            .get("overageStatus")
            .and_then(|v| v.as_str())
            .map(String::from),
        is_using_overage: info
            .get("isUsingOverage")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        observed_at: chrono::Utc::now(),
        windows,
    })
}

/// Extract the handshake from a `system/init` event.
///
/// Returns `None` for any other event, and for an init that advertises neither
/// fact — there is nothing to persist and no reason to spend a write.
fn provider_handshake(stream_event: &StreamEvent) -> Option<ProviderHandshake> {
    let is_init_event = stream_event.event_type == "system"
        && stream_event.data.get("subtype").and_then(|v| v.as_str()) == Some("init");
    if !is_init_event {
        return None;
    }

    let cli_version = stream_event
        .data
        .get("claude_code_version")
        .and_then(|v| v.as_str())
        .map(String::from);
    let capabilities: Vec<String> = stream_event
        .data
        .get("capabilities")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if cli_version.is_none() && capabilities.is_empty() {
        return None;
    }

    Some(ProviderHandshake {
        cli_version,
        capabilities,
    })
}

/// Stream event types that reach [`SessionManager::convert_stream_event`],
/// produce no `ConversationEvent`, and are nevertheless fully handled — the
/// stream loop consumed them earlier in the same iteration.
///
/// This list names the loop's OTHER consumers, not the converter's own match
/// arms; the converter remains the single source of truth for what it maps.
/// Mirrors the Codex path's explicit `"turn.started" => None` no-op arm: a
/// known no-op is silent, an unknown type is not.
const NON_CONVERSATION_STREAM_EVENTS: &[&str] = &[
    // Local / OpenAI-compatible streaming chunks. Counted for tokens in the
    // loop's token-accounting match; the aggregated `assistant` event carries
    // the text that becomes a ConversationEvent.
    "content_block_delta",
    // Account-level plan-window telemetry, persisted by the loop (V99/P1-B).
    "rate_limit_event",
    // Codex context-window usage, consumed by `extract_codex_context_usage`.
    "codex_token_count",
];

/// Ceiling on distinct unrecognized types diagnosed by one monitor run.
///
/// Dedupe alone bounds the log against a chatty type; this bounds memory and
/// the log against a provider emitting unbounded DISTINCT types. A real
/// protocol adds types one at a time, so this is never reached in practice.
const UNRECOGNIZED_STREAM_EVENT_CAP: usize = 32;

/// What [`UnrecognizedStreamEvents`] decided to do with one occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnrecognizedEventAction {
    /// First sighting of a genuinely unrecognized type — log it.
    Report,
    /// Handled elsewhere, or already reported for this session.
    Silent,
    /// The per-run distinct-type budget is exhausted; log the ceiling once.
    CapReached,
}

/// One-diagnostic-per-distinct-type gate for unrecognized stream events.
///
/// Lives on the monitor loop's stack, so it is scoped to exactly one session's
/// run and freed with it — no process-wide map to grow, and no lock on the
/// stream hot path.
#[derive(Debug, Default)]
struct UnrecognizedStreamEvents {
    reported: std::collections::HashSet<String>,
    cap_reported: bool,
}

impl UnrecognizedStreamEvents {
    /// Classify one occurrence, recording it so the next is `Silent`.
    fn classify(&mut self, event_type: &str) -> UnrecognizedEventAction {
        if NON_CONVERSATION_STREAM_EVENTS.contains(&event_type)
            || self.reported.contains(event_type)
        {
            return UnrecognizedEventAction::Silent;
        }
        if self.reported.len() >= UNRECOGNIZED_STREAM_EVENT_CAP {
            if self.cap_reported {
                return UnrecognizedEventAction::Silent;
            }
            self.cap_reported = true;
            return UnrecognizedEventAction::CapReached;
        }
        self.reported.insert(event_type.to_string());
        UnrecognizedEventAction::Report
    }

    /// Emit the diagnostic for an event the converter did not map.
    ///
    /// The Claude stream path previously returned an empty vec here with no
    /// log and no metric, so `rate_limit_event` and `system/api_retry` went
    /// unnoticed until a manual CLI probe found them. The Codex path already
    /// warns; this is the same signal for every other provider.
    fn note(&mut self, session_id: Uuid, event_type: &str) {
        match self.classify(event_type) {
            UnrecognizedEventAction::Report => tracing::warn!(
                session_id = %session_id,
                event_type = %event_type,
                "Provider stream: unrecognized event type — please update SessionManager::convert_stream_event"
            ),
            UnrecognizedEventAction::CapReached => tracing::warn!(
                session_id = %session_id,
                cap = UNRECOGNIZED_STREAM_EVENT_CAP,
                "Provider stream: unrecognized event type diagnostics capped for this session"
            ),
            UnrecognizedEventAction::Silent => {}
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct AuthoritativeModelCandidate {
    prior_model: Option<String>,
    prior_context_window: Option<u64>,
    prior_budget: Option<rsi_common::ResolvedContextBudget>,
    model: String,
    budget: rsi_common::ResolvedContextBudget,
    tuple_changed: bool,
    model_changed: bool,
}

fn authoritative_model_candidate(
    provider: SessionProvider,
    current_model: Option<&str>,
    current_context_window: Option<u64>,
    current_budget: Option<&rsi_common::ResolvedContextBudget>,
    stream_event: &StreamEvent,
) -> Option<AuthoritativeModelCandidate> {
    let model = authoritative_model_update(current_model, stream_event)?.to_string();
    let model_changed = current_model != Some(model.as_str());
    let budget = if model_changed {
        crate::provider_capabilities::resolve_new_incarnation_context_budget_for(
            provider,
            &model,
            current_budget,
        )
    } else if let Some(existing) = current_budget {
        existing.clone()
    } else {
        let mut request = crate::provider_capabilities::ContextBudgetRequest::new(provider, &model);
        request.legacy_stored_tokens = current_context_window;
        crate::provider_capabilities::provider_capabilities().resolve_context_budget(request)
    };
    let prior_model = current_model.map(str::to_owned);
    let tuple_changed = prior_model.as_deref() != Some(model.as_str())
        || current_context_window != Some(budget.active_tokens)
        || current_budget != Some(&budget);

    Some(AuthoritativeModelCandidate {
        prior_model,
        prior_context_window: current_context_window,
        prior_budget: current_budget.cloned(),
        model,
        budget,
        tuple_changed,
        model_changed,
    })
}

fn same_runtime_context_observation(
    prior: Option<&rsi_common::ResolvedContextBudget>,
    next: &rsi_common::ResolvedContextBudget,
) -> bool {
    prior.is_some_and(|prior| {
        prior.active_tokens == next.active_tokens
            && prior.evidence.source == rsi_common::CapabilitySource::RuntimeTelemetry
            && prior.evidence.source_version == next.evidence.source_version
            && prior.evidence.source_digest == next.evidence.source_digest
    })
}

fn resolve_runtime_context_budget(
    provider: SessionProvider,
    model: &str,
    prior: Option<&rsi_common::ResolvedContextBudget>,
    runtime_tokens: u64,
    observed_at: chrono::DateTime<chrono::Utc>,
) -> crate::error::Result<rsi_common::ResolvedContextBudget> {
    if runtime_tokens == 0 {
        return Err(crate::error::DaemonError::Process(
            "provider runtime context window must be positive".to_string(),
        ));
    }
    let mut request = crate::provider_capabilities::ContextBudgetRequest::new(provider, model);
    request.configured_tokens = prior.and_then(|budget| budget.capacity.configured_tokens);
    request.runtime_effective_tokens = Some(runtime_tokens);
    request.observed_at = Some(observed_at);
    request.expected_codex_cli_version =
        prior.and_then(|budget| budget.evidence.source_version.as_deref());
    request.expected_codex_catalog_digest =
        prior.and_then(|budget| budget.evidence.source_digest.as_deref());
    let mut next =
        crate::provider_capabilities::provider_capabilities().resolve_context_budget(request);
    // A process incarnation retains the exact catalog evidence selected at
    // launch even if a newer CLI refresh replaces the process-global cache
    // while this older process is still streaming telemetry.
    if let Some(prior) = prior
        && prior.evidence.source_version.is_some()
        && prior.evidence.source_digest.is_some()
    {
        // Retain catalog facts already bound to this in-memory incarnation.
        // A row rehydrated after daemon restart may omit those descriptive
        // fields, so only overlay facts that were actually persisted in
        // memory and leave exact-cache/official enrichment on `next` intact.
        if prior.capacity.advertised_max_tokens.is_some() {
            next.capacity.advertised_max_tokens = prior.capacity.advertised_max_tokens;
        }
        if prior.capacity.provider_default_tokens.is_some() {
            next.capacity.provider_default_tokens = prior.capacity.provider_default_tokens;
        }
        if prior.capacity.provider_max_tokens.is_some() {
            next.capacity.provider_max_tokens = prior.capacity.provider_max_tokens;
        }
        if prior.capacity.effective_percent.is_some() {
            next.capacity.effective_percent = prior.capacity.effective_percent;
        }
        if prior.capacity.configured_tokens.is_some() {
            next.capacity.configured_tokens = prior.capacity.configured_tokens;
        }
        if prior.capacity.compaction_limit_tokens.is_some() {
            next.capacity.compaction_limit_tokens = prior.capacity.compaction_limit_tokens;
        }
        if prior.capacity.max_output_tokens.is_some() {
            next.capacity.max_output_tokens = prior.capacity.max_output_tokens;
        }
        next.evidence.source_version = prior.evidence.source_version.clone();
        next.evidence.source_digest = prior.evidence.source_digest.clone();
    }
    Ok(next)
}

/// Commit runtime context authority before exposing it to live consumers.
/// Returns None when the monitor generation lost ownership during the write.
async fn persist_runtime_context_observation(
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    persistence: &PersistenceHandle,
    store: &Arc<tokio::sync::Mutex<Store>>,
    session_id: Uuid,
    expected_generation: u64,
    runtime_tokens: u64,
    observed_at: chrono::DateTime<chrono::Utc>,
) -> crate::error::Result<Option<(rsi_common::ResolvedContextBudget, bool)>> {
    let (provider, prior_model, model, prior_context_window, prior_budget) = {
        let active_guard = active.read().await;
        let Some(tracked) = active_guard
            .get(&session_id)
            .filter(|tracked| tracked.spawn_generation == expected_generation)
        else {
            return Ok(None);
        };
        (
            tracked.session.provider,
            tracked.session.model.clone(),
            tracked
                .session
                .model
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            tracked.session.context_window,
            tracked.session.resolved_context_budget.clone(),
        )
    };

    let mut next = resolve_runtime_context_budget(
        provider,
        &model,
        prior_budget.as_ref(),
        runtime_tokens,
        observed_at,
    )?;

    let changed = if same_runtime_context_observation(prior_budget.as_ref(), &next) {
        next.evidence.observed_at = prior_budget
            .as_ref()
            .and_then(|budget| budget.evidence.observed_at.clone());
        false
    } else {
        let persisted = persistence
            .compare_and_update_session_model(
                Arc::clone(store),
                session_id,
                prior_model.clone(),
                prior_context_window,
                prior_budget,
                prior_model,
                Some(next.active_tokens),
                Some(next.clone()),
            )
            .await?;
        if !persisted {
            return Ok(None);
        }
        true
    };

    let mut active_guard = active.write().await;
    let Some(tracked) = active_guard
        .get_mut(&session_id)
        .filter(|tracked| tracked.spawn_generation == expected_generation)
    else {
        return Ok(None);
    };
    install_context_budget(&mut tracked.session, next.clone());
    Ok(Some((next, changed)))
}

/// Emit the warn-only capability-class diagnostic once per declared/model pair.
///
/// This remains independent of whether the authoritative tuple needed a durable
/// write: an accepted exact `system/init` still validates its capability class.
fn warn_capability_class_mismatch(
    event_bus: &crate::bus::EventBus,
    session_id: Uuid,
    declared: Option<rsi_common::types::CapabilityClass>,
    last_mismatch_warn: &mut Option<(rsi_common::types::CapabilityClass, String)>,
    model: &str,
) {
    let Some(declared) = declared else {
        return;
    };
    let Some(actual_class) = rsi_common::types::CapabilityClass::classify(model) else {
        return;
    };
    if actual_class == declared {
        return;
    }
    if last_mismatch_warn
        .as_ref()
        .is_some_and(|(prior_declared, prior_model)| {
            *prior_declared == declared && prior_model == model
        })
    {
        return;
    }

    tracing::warn!(
        session_id = %session_id,
        declared = ?declared,
        actual_class = ?actual_class,
        actual_model = %model,
        "capability class mismatch"
    );
    event_bus.publish(crate::bus::DaemonEvent::SystemMessage {
        level: "warn".to_string(),
        message: format!(
            "session {} declared {:?} but running {} ({:?})",
            session_id, declared, model, actual_class
        ),
    });
    *last_mismatch_warn = Some((declared, model.to_string()));
}

fn is_codex_context_provider(provider: SessionProvider) -> bool {
    matches!(
        provider,
        SessionProvider::Codex
            | SessionProvider::Pioneer
            | SessionProvider::OpenRouter
            | SessionProvider::Bedrock
            | SessionProvider::CodexAppServer
    )
}

fn has_codex_context_observation(tracked: &TrackedSession) -> bool {
    tracked.codex_context_tokens > 0 || tracked.last_usage_update.is_some()
}

fn codex_display_budget_known(budget: &rsi_common::ResolvedContextBudget) -> bool {
    budget.active_tokens > 0
        && matches!(
            budget.evidence.source,
            rsi_common::CapabilitySource::RuntimeTelemetry
                | rsi_common::CapabilitySource::Configured
        )
}

/// Age belongs to the provider observation, not the time we read its file.
fn codex_usage_observed_instant(event: &StreamEvent) -> tokio::time::Instant {
    let age = event
        .data
        .get("observed_at")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|at| {
            (chrono::Utc::now() - at.with_timezone(&chrono::Utc))
                .to_std()
                .unwrap_or_default()
        })
        .unwrap_or_default();
    tokio::time::Instant::now()
        - age.min(USAGE_STALENESS_WINDOW + std::time::Duration::from_secs(1))
}

fn record_codex_context_observation(
    tracked: &mut TrackedSession,
    event: &StreamEvent,
    usage: &monitor::CodexContextUsage,
) {
    tracked.codex_context_tokens = usage.context_tokens;
    tracked.last_usage_update = Some(codex_usage_observed_instant(event));
    tracked.session.input_tokens = Some(usage.context_tokens);
    tracked.session.output_tokens = Some(usage.output_tokens);
    if let Some(cache_read_tokens) = usage.cache_read_tokens {
        tracked.session.total_cache_read_tokens = Some(
            tracked
                .session
                .total_cache_read_tokens
                .unwrap_or(0)
                .saturating_add(cache_read_tokens),
        );
    }
    tracked.daemon_tokens_at_last_api_update =
        tracked.daemon_input_tokens + tracked.daemon_output_tokens;
}

fn cache_read_metadata_changed(current: Option<u64>, last_persisted: Option<u64>) -> bool {
    current.is_some() && current != last_persisted
}

/// Keep #61's baseline and between-report estimator out of raw occupancy display.
fn rotation_context_pct(tracked: &TrackedSession, display_pct: f64) -> f64 {
    if is_codex_context_provider(tracked.session.provider) && has_codex_context_observation(tracked)
    {
        let delta = (tracked.daemon_input_tokens + tracked.daemon_output_tokens)
            .saturating_sub(tracked.daemon_tokens_at_last_api_update);
        if tracked.codex_context_tokens == 0 && delta == 0 {
            return 0.0;
        }
        codex_context_pct_used(
            tracked.codex_context_tokens.saturating_add(delta),
            tracked.context_budget().active_tokens,
        )
    } else if is_codex_context_provider(tracked.session.provider) {
        // Preserve the existing pre-observation rotation policy, while the
        // display correctly remains unknown without provider telemetry.
        context_fill_pct_formula(
            tracked.daemon_input_tokens + tracked.daemon_output_tokens,
            tracked.context_budget().active_tokens,
        )
        .unwrap_or(0.0)
    } else {
        display_pct
    }
}

fn codex_context_pct_used(context_tokens: u64, context_window: u64) -> f64 {
    if context_window <= CODEX_CONTEXT_BASELINE_TOKENS {
        return 100.0;
    }

    let effective_window = context_window - CODEX_CONTEXT_BASELINE_TOKENS;
    let used = context_tokens.saturating_sub(CODEX_CONTEXT_BASELINE_TOKENS);
    let remaining = effective_window.saturating_sub(used);
    let remaining_pct = ((remaining as f64 / effective_window as f64) * 100.0)
        .clamp(0.0, 100.0)
        .round();

    100.0 - remaining_pct
}

/// Raw occupancy shared by live and persisted display. Observation presence
/// is selected by callers; a measured zero is distinct from missing telemetry.
/// Rotation uses `codex_context_pct_used` separately to preserve #61's policy.
pub(super) fn context_fill_pct_formula(numerator: u64, window: u64) -> Option<f64> {
    (window > 0).then(|| (numerator as f64 * 100.0 / window as f64).min(100.0))
}

/// Publish the same measurement presence and raw ratio as the live bus event.
pub(super) fn context_fill_pct_for_tracked(tracked: &TrackedSession) -> Option<f64> {
    let (numerator, pct, confidence, _, _) = live_context_state(tracked);
    let measured = if is_codex_context_provider(tracked.session.provider) {
        confidence != ContextUsageConfidence::Missing
    } else {
        numerator > 0
    };
    measured.then_some(pct)
}

/// Derive `Session.context_fill_pct` for an IDLE/persisted session (no live
/// `TrackedSession`) from its stored token fields. Denominator is resolved
/// through the canonical capability resolver. Codex
/// requires `input_tokens` because the monitor stores the latest Codex
/// current-window numerator there; `total_input_tokens` remains aggregate
/// analytics and can exceed the active context. Other
/// providers prefer API `total_input_tokens`, falling back to daemon BPE total.
/// This is the read-path counterpart of `live_context_state`'s numerator
/// selection, sourced from the daemon through the one shared formula.
pub(super) fn context_fill_pct_from_persisted(session: &Session) -> Option<f64> {
    let budget = crate::provider_capabilities::resolved_context_budget_for_session(session);
    if is_codex_context_provider(session.provider) {
        // Only current-window input is occupancy. Billing/BPE totals cannot
        // establish it, including after restart or compaction.
        if !codex_display_budget_known(&budget) {
            return None;
        }
        return session
            .input_tokens
            .and_then(|tokens| context_fill_pct_formula(tokens, budget.active_tokens));
    }
    let daemon_total =
        session.daemon_input_tokens.unwrap_or(0) + session.daemon_output_tokens.unwrap_or(0);
    let numerator = session
        .total_input_tokens
        .filter(|&t| t > 0)
        .or_else(|| (daemon_total > 0).then_some(daemon_total))?;
    context_fill_pct_formula(numerator, budget.active_tokens)
}

fn live_context_state(tracked: &TrackedSession) -> (u64, f64, ContextUsageConfidence, u64, u64) {
    let context_window = tracked.context_budget().active_tokens;
    let daemon_total = tracked.daemon_input_tokens + tracked.daemon_output_tokens;

    // Stdout fragments cannot estimate unseen tools/system/resumed history.
    if is_codex_context_provider(tracked.session.provider) {
        if !has_codex_context_observation(tracked) {
            return (
                0,
                0.0,
                ContextUsageConfidence::Missing,
                daemon_total,
                context_window,
            );
        }
        let numerator = tracked.codex_context_tokens;
        if !codex_display_budget_known(&tracked.context_budget()) {
            return (
                numerator,
                0.0,
                ContextUsageConfidence::Missing,
                daemon_total,
                context_window,
            );
        }
        let pct = context_fill_pct_formula(numerator, context_window).unwrap_or(0.0);
        return (
            numerator,
            pct,
            apply_staleness(tracked, ContextUsageConfidence::Full),
            daemon_total,
            context_window,
        );
    }
    // Preserve other providers' established API/BPE selection.
    use SessionProvider::*;
    let (numerator, confidence) = match tracked.session.provider {
        Claude if tracked.live_input_tokens > 0 => {
            let applied = apply_staleness(tracked, tracked.live_usage_confidence);
            (tracked.live_input_tokens, applied)
        }
        Antigravity | CodexAppServer | Harness if tracked.live_input_tokens > 0 => {
            let daemon_delta =
                daemon_total.saturating_sub(tracked.daemon_tokens_at_last_api_update);
            (
                tracked.live_input_tokens + daemon_delta,
                tracked.live_usage_confidence,
            )
        }
        _ if daemon_total > 0 => (daemon_total, ContextUsageConfidence::Counted),
        _ => (0, ContextUsageConfidence::Missing),
    };

    let pct = context_fill_pct_formula(numerator, context_window).unwrap_or(0.0);

    (numerator, pct, confidence, daemon_total, context_window)
}

#[derive(Debug)]
struct MemoryFlushTurnCandidate {
    compaction_count: u32,
    total_tokens: u64,
    active_tokens: u64,
    working_dir: std::path::PathBuf,
}

/// Select one pre-compaction memory-flush turn from the live session state.
///
/// This is deliberately an idle-boundary decision: a provider turn must never
/// be overlapped by a synthetic flush. Once context rotation or another
/// lifecycle action owns the session, that transition wins instead. The
/// monitor-local attempt fence bounds failed dispatches, while the tracked
/// compaction count prevents a successful flush from recurring at the same
/// rotation depth.
fn memory_flush_turn_candidate(
    tracked: &TrackedSession,
    settings: &MemoryFlushSettings,
    attempted_compaction_count: Option<u32>,
) -> Option<MemoryFlushTurnCandidate> {
    if tracked.rotation.is_rotating()
        || tracked.interrupt_requested
        || tracked.pending_archive
        || tracked.pending_question.is_some()
        || !matches!(tracked.session.status, SessionStatus::Running)
    {
        return None;
    }

    let current_compaction_count = tracked.session.rotation_depth;
    if attempted_compaction_count == Some(current_compaction_count) {
        return None;
    }

    let total_tokens = live_context_state(tracked).0;
    let context_budget = tracked.context_budget();
    should_run_memory_flush(
        total_tokens,
        &context_budget,
        settings,
        tracked.memory_flush_compaction_count,
        current_compaction_count,
    )
    .then(|| MemoryFlushTurnCandidate {
        compaction_count: current_compaction_count,
        total_tokens,
        active_tokens: context_budget.active_tokens,
        working_dir: tracked.effective_working_dir().to_path_buf(),
    })
}

/// Dispatch at most one memory-flush turn for the current compaction depth.
///
/// `Ok(true)` means a real provider turn was accepted. The attempt fence is
/// advanced before the provider call so a definite or ambiguous send failure
/// cannot create an unbounded retry loop at later idle boundaries.
async fn maybe_start_memory_flush_turn(
    provider_session: &mut dyn ProviderSession,
    active: &Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    session_id: Uuid,
    expected_generation: u64,
    settings: &MemoryFlushSettings,
    attempted_compaction_count: &mut Option<u32>,
) -> crate::error::Result<bool> {
    if !provider_session.supports_multi_turn() {
        return Ok(false);
    }

    let candidate = {
        let active_guard = active.read().await;
        active_guard
            .get(&session_id)
            .filter(|tracked| tracked.spawn_generation == expected_generation)
            .and_then(|tracked| {
                memory_flush_turn_candidate(tracked, settings, *attempted_compaction_count)
            })
    };
    let Some(candidate) = candidate else {
        return Ok(false);
    };

    // This is an attempt fence, not a success claim. Keep it set on error:
    // ProviderSession::start_turn does not offer a general no-effect proof.
    *attempted_compaction_count = Some(candidate.compaction_count);
    let turn_config = crate::provider::TurnConfig {
        input: format!(
            "{}\n\n{}",
            MEMORY_FLUSH_SYSTEM_PROMPT,
            format_flush_prompt(MEMORY_FLUSH_PROMPT)
        ),
        working_dir: Some(candidate.working_dir),
    };
    provider_session.start_turn(&turn_config).await?;

    {
        let mut active_guard = active.write().await;
        if let Some(tracked) = active_guard
            .get_mut(&session_id)
            .filter(|tracked| tracked.spawn_generation == expected_generation)
        {
            tracked.memory_flush_compaction_count = Some(candidate.compaction_count);
            tracked.last_event_at = chrono::Utc::now();
        }
    }

    tracing::info!(
        session_id = %session_id,
        compaction_count = candidate.compaction_count,
        total_tokens = candidate.total_tokens,
        active_tokens = candidate.active_tokens,
        "Started pre-compaction memory-flush turn"
    );
    Ok(true)
}

async fn advance_context_rotation_threshold(
    tracked: &mut TrackedSession,
    session_id: Uuid,
    pct: f64,
    persistence: &PersistenceHandle,
) {
    let pct = rotation_context_pct(tracked, pct);
    let action = context_rotation_threshold_action(tracked, pct);
    if matches!(action, RotationAction::InterruptForRotation) {
        let depth = tracked.session.rotation_depth;
        let rid = tracked
            .rotation
            .rotation_id()
            .unwrap_or("unknown")
            .to_string();
        tracing::info!(
            session_id = %session_id,
            context_pct = pct,
            rotation_depth = depth,
            rotation_id = %rid,
            "Context rotation threshold reached, interrupting for handoff write"
        );
        let _ = tracked.stop_tx.try_send(());
        let metadata = format!("{{\"pct\":{pct:.1}}}");
        let _ = persistence
            .log_rotation_event(
                session_id,
                &rid,
                "pending_interrupt",
                "entered",
                Some(metadata),
            )
            .await;
    }
}

fn context_rotation_threshold_action(tracked: &mut TrackedSession, pct: f64) -> RotationAction {
    // Catalog, repository, and legacy sources are descriptive/degraded and
    // must never trigger threshold rotation.
    if !tracked.authorizes_threshold_rotation() {
        return RotationAction::NoOp;
    }

    tracked
        .rotation
        .advance(RotationEvent::ThresholdCheck { pct })
}

fn inactive_rotation_deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(365 * 24 * 3600)
}

fn current_rotation_deadline(tracked: Option<&TrackedSession>) -> tokio::time::Instant {
    tracked
        .and_then(|session| session.rotation.current_deadline())
        .unwrap_or_else(inactive_rotation_deadline)
}

fn result_evidence(stream_event: &StreamEvent) -> TerminalResult {
    let subtype = stream_event
        .data
        .get("subtype")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let explicit_error = stream_event
        .data
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || stream_event
            .data
            .get("success")
            .and_then(serde_json::Value::as_bool)
            == Some(false)
        || subtype.starts_with("error")
        || subtype.starts_with("failed")
        || subtype == "interrupted";
    if explicit_error {
        TerminalResult::ProviderError
    } else {
        TerminalResult::Success
    }
}

/// Normalized provider terminal failures share the historical
/// `process_error` transport event with raw stderr diagnostics.  Only the
/// former settle a terminal turn; stderr remains a visible nonterminal
/// diagnostic exactly as before.
fn is_terminal_provider_error(stream_event: &StreamEvent) -> bool {
    if stream_event.event_type != "process_error" {
        return false;
    }
    let data = &stream_event.data;
    if data.get("source").and_then(serde_json::Value::as_str) == Some("codex_event")
        && data
            .get("provider_event_type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|event_type| matches!(event_type, "error" | "item.completed.error"))
        && let Some(terminal) = data.get("terminal").and_then(serde_json::Value::as_bool)
    {
        // Codex CLI `error` events and item-level `error` items are
        // retry/warning/diagnostic notices (#603, #662); the subsequent
        // `turn.failed` event or process exit carries terminal evidence. Other
        // Codex event sources (such as App Server compatibility events) retain
        // their existing terminal contract.
        return terminal;
    }
    if data
        .get("terminal")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || data
            .get("error_class")
            .and_then(serde_json::Value::as_str)
            .is_some()
    {
        return true;
    }
    if matches!(
        data.get("turn_status").and_then(serde_json::Value::as_str),
        Some("failed" | "interrupted")
    ) {
        return true;
    }
    matches!(
        data.get("provider_event_type")
            .and_then(serde_json::Value::as_str),
        Some("error" | "turn.failed" | "item.completed.error" | "turn/completed")
    ) || matches!(
        data.get("source").and_then(serde_json::Value::as_str),
        Some("codex_event" | "model_control")
    )
}

/// Maximum characters of provider diagnostic text carried in a stop reason.
const PROVIDER_STOP_REASON_DETAIL_MAX_CHARS: usize = 200;

/// Daemon-owned namespace for Codex terminal errors that have no closed
/// classification. The provider text follows the prefix as bounded detail.
const CODEX_UNCLASSIFIED_STOP_REASON_PREFIX: &str = "provider_error:codex:";

/// Normalize provider diagnostic text into single-line, bounded stop-reason
/// detail: control characters and whitespace runs collapse to one space and
/// the result is truncated on a character boundary.
pub(super) fn bounded_stop_reason_detail(text: &str) -> Option<String> {
    let collapsed = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= PROVIDER_STOP_REASON_DETAIL_MAX_CHARS {
        return Some(collapsed);
    }
    let mut truncated: String = collapsed
        .chars()
        .take(PROVIDER_STOP_REASON_DETAIL_MAX_CHARS)
        .collect();
    truncated.push('…');
    Some(truncated)
}

/// Map provider terminal failures into durable Session stop reasons.
///
/// Closed daemon-owned classifications (usage limit, storage full, ...) are
/// exact identities that retry/capacity consumers match on, so they win.
/// A Codex terminal error with no closed classification carries its real
/// diagnostic text (#662) under the daemon-owned `provider_error:codex:`
/// namespace: the prefix keeps it distinct from every closed identity (which
/// never contains `codex:`), and the text is bounded to one line. Other
/// providers keep the stable `provider_error:unclassified` fallback.
fn terminal_provider_stop_reason(stream_event: &StreamEvent) -> Option<Cow<'static, str>> {
    let data = &stream_event.data;
    if stream_event.event_type != "process_error" {
        return None;
    }
    if data.get("turn_status").and_then(serde_json::Value::as_str) == Some("interrupted")
        || data.get("error_class").and_then(serde_json::Value::as_str) == Some("interrupted")
    {
        return None;
    }
    if data.get("source").and_then(serde_json::Value::as_str) == Some("stderr")
        && data.get("terminal").and_then(serde_json::Value::as_bool) == Some(true)
        && data.get("error_class").and_then(serde_json::Value::as_str)
            == Some(crate::codex::CODEX_STORAGE_FULL_ERROR_CLASS)
        && data
            .get("provider_event_type")
            .and_then(serde_json::Value::as_str)
            == Some(crate::codex::CODEX_STORAGE_FULL_PROVIDER_EVENT_TYPE)
    {
        return Some(Cow::Borrowed(crate::codex::CODEX_STORAGE_FULL_STOP_REASON));
    }
    if data.get("source").and_then(serde_json::Value::as_str) == Some("codex_event")
        && matches!(
            data.get("provider_event_type")
                .and_then(serde_json::Value::as_str),
            Some("error" | "turn.failed")
        )
    {
        match data.get("error_class").and_then(serde_json::Value::as_str) {
            Some(crate::codex::CODEX_USAGE_LIMIT_ERROR_CLASS) => {
                return Some(Cow::Borrowed(crate::codex::CODEX_USAGE_LIMIT_STOP_REASON));
            }
            _ => {}
        }
    }
    if !is_terminal_provider_error(stream_event) {
        return None;
    }
    if data.get("source").and_then(serde_json::Value::as_str) == Some("codex_event")
        && let Some(detail) = data
            .get("error")
            .and_then(serde_json::Value::as_str)
            .and_then(bounded_stop_reason_detail)
    {
        return Some(Cow::Owned(format!(
            "{CODEX_UNCLASSIFIED_STOP_REASON_PREFIX}{detail}"
        )));
    }
    // Every normalized terminal provider error needs a durable classification,
    // even when this provider has no narrower daemon-owned mapping yet. The
    // full diagnostic stays in the conversation event.
    Some(Cow::Borrowed("provider_error:unclassified"))
}

/// Pure terminal truth. Process settlement and stream drain are gates; retry
/// classification is deliberately a later consumer of durable `Failed`.
pub(super) fn terminal_decision(evidence: TerminalEvidence) -> TerminalDecision {
    use crate::store::daemon_settings::AutofileCause;

    if evidence.active_generation != Some(evidence.expected_generation)
        || matches!(
            evidence.settlement,
            ProcessSettlementOutcome::GenerationChanged
        )
    {
        return TerminalDecision::StaleGeneration;
    }
    if !evidence.settlement.is_settled() {
        return TerminalDecision::RetainRunning(TerminalRetentionReason::OwnershipUnsettled);
    }
    if !evidence.stream_drained {
        return TerminalDecision::RetainRunning(if !evidence.process_alive {
            TerminalRetentionReason::ProducerNotClosed
        } else {
            TerminalRetentionReason::StreamNotDrained
        });
    }
    if evidence.process_handle_present && evidence.process_alive {
        return TerminalDecision::RetainRunning(TerminalRetentionReason::ProviderStillAlive);
    }
    if evidence.supports_multi_turn
        && matches!(evidence.turn_outcome, TerminalTurnOutcome::Continued)
    {
        return TerminalDecision::RetainRunning(TerminalRetentionReason::TurnContinues);
    }
    if evidence.pending_archive {
        return TerminalDecision::Finalize(TerminalFinalizeDecision::completed());
    }
    if evidence.stall_interrupted
        || matches!(evidence.break_reason, MonitorBreakReason::StallTimeout)
    {
        return TerminalDecision::Finalize(TerminalFinalizeDecision::failed(
            AutofileCause::StallTimeout,
        ));
    }
    if evidence.interrupt_requested
        || matches!(evidence.break_reason, MonitorBreakReason::Interrupted)
    {
        return TerminalDecision::Finalize(TerminalFinalizeDecision::interrupted());
    }
    if evidence.pending_question {
        return TerminalDecision::Finalize(TerminalFinalizeDecision::waiting_approval());
    }
    if matches!(
        evidence.rotation_action,
        TerminalRotationAction::PostFinalize
    ) || matches!(evidence.break_reason, MonitorBreakReason::Rotation)
    {
        return TerminalDecision::Finalize(TerminalFinalizeDecision::completed());
    }
    if evidence.exit_code.is_some_and(|code| code != 0) {
        return TerminalDecision::Finalize(TerminalFinalizeDecision::failed(
            AutofileCause::NonZeroExit,
        ));
    }
    if matches!(evidence.current_result, TerminalResult::ProviderError) {
        return TerminalDecision::Finalize(TerminalFinalizeDecision::failed(
            AutofileCause::OtherTerminalFailure,
        ));
    }
    if matches!(evidence.current_result, TerminalResult::Success) {
        return TerminalDecision::Finalize(TerminalFinalizeDecision::completed());
    }
    if !evidence.received_any_event && !evidence.prior_meaningful_output {
        return TerminalDecision::Finalize(TerminalFinalizeDecision::failed(
            AutofileCause::NoMeaningfulOutput,
        ));
    }
    if evidence.received_meaningful_output || evidence.prior_meaningful_output {
        return TerminalDecision::Finalize(TerminalFinalizeDecision::completed());
    }
    TerminalDecision::Finalize(TerminalFinalizeDecision::failed(
        AutofileCause::OtherTerminalFailure,
    ))
}

/// A malformed handoff attempt still becomes stale after later tool activity,
/// but only a contract-valid handoff can correct it.
fn uncorrected_post_handoff_tool(events: &[ConversationEvent]) -> bool {
    let mut saw_handoff = false;
    let mut later_tool = false;
    for event in events {
        if event.event_type == EventType::Message && event.role == Some(Role::User) {
            saw_handoff = false;
            later_tool = false;
        } else if event.event_type == EventType::Message && event.role == Some(Role::Assistant) {
            if let Some(first_line) = event.content.lines().find(|line| !line.trim().is_empty()) {
                let looks_like_handoff = first_line
                    .trim_start_matches(|ch: char| ch == '#' || ch.is_whitespace())
                    .starts_with("PIPELINE HANDOFF — ");
                if looks_like_handoff
                    && (!saw_handoff || rsi_common::validate_first_line(first_line).is_ok())
                {
                    saw_handoff = true;
                    later_tool = false;
                }
            }
        } else if saw_handoff
            && matches!(event.event_type, EventType::ToolUse | EventType::ToolResult)
        {
            later_tool = true;
        }
    }
    saw_handoff && later_tool
}

fn guard_terminal_handoff_order(
    decision: TerminalFinalizeDecision,
    evidence: TerminalEvidence,
    events: &[ConversationEvent],
) -> TerminalFinalizeDecision {
    if decision.status == SessionStatus::Completed
        && !evidence.pending_archive
        && evidence.rotation_action == TerminalRotationAction::None
        && !matches!(evidence.break_reason, MonitorBreakReason::Rotation)
        && uncorrected_post_handoff_tool(events)
    {
        // This is a known handoff-contract failure, not a fresh daemon crash
        // to auto-file for every affected worker.
        return TerminalFinalizeDecision {
            status: SessionStatus::Failed,
            c5_failure_cause: None,
        };
    }
    decision
}

fn apply_terminal_handoff_order(
    decision: TerminalFinalizeDecision,
    evidence: TerminalEvidence,
    tracked: &mut TrackedSession,
) -> TerminalFinalizeDecision {
    let guarded = guard_terminal_handoff_order(decision, evidence, &tracked.events);
    if guarded.status == SessionStatus::Failed && decision.status == SessionStatus::Completed {
        tracked.session.stop_reason = Some("terminal_handoff_superseded_by_tool".to_string());
    }
    guarded
}

async fn optional_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

/// Decide whether a delivered idle-boundary outcome must KEEP its arbitration
/// grant outstanding, or release it (C-P2-08).
///
/// Returns `Some(grant)` when the grant must stay outstanding — the delivered
/// turn is in flight and this logical root must not be granted again until it
/// ends — and `None` after releasing it, which makes the root immediately
/// grantable again.
///
/// # Why this is a named function rather than four inline lines
///
/// This is the ONLY place the hold decision is made.
/// [`super::agent_message_delivery::deliver_at_idle_boundary`] takes
/// `grant: &ArbitrationGrant` and therefore *structurally cannot* retain or
/// release it, and the caller below holds the grant in a plain local. Review R5
/// (H21-P2-R5-003) found that the test cited as the arbiter item's evidence
/// **reproduced** this decision inside the test rather than **exercising** it:
/// its blocking assertion would have passed identically with the
/// `deliver_at_idle_boundary` call deleted, because the reservation was already
/// taken by `decide_next_boundary` and nothing in between could release it.
///
/// Extracting the decision here makes conjunct (B) — *the monitor actually keeps
/// a grant outstanding across a delivered turn* — a test's own subject, driven
/// with each [`IdleBoundaryDelivery`] variant against a real registry.
///
/// **Behaviour-preserving by construction:** this is the inline `matches!` /
/// `else` pair moved verbatim, with no change to which outcomes hold and which
/// release.
///
/// [`IdleBoundaryDelivery`]: super::agent_message_delivery::IdleBoundaryDelivery
pub(super) fn retain_or_release(
    outcome: &super::agent_message_delivery::IdleBoundaryDelivery,
    grant: Box<super::agent_message_arbiter::ArbitrationGrant>,
) -> Option<Box<super::agent_message_arbiter::ArbitrationGrant>> {
    if matches!(
        outcome,
        super::agent_message_delivery::IdleBoundaryDelivery::Dispatched
    ) {
        // Held, not released: the delivered turn is now in flight and this root
        // must not be granted again until it ends.
        Some(grant)
    } else {
        grant.release();
        None
    }
}

fn settlement_mode(reason: MonitorBreakReason, supports_multi_turn: bool) -> ProcessSettlementMode {
    match reason {
        MonitorBreakReason::Interrupted | MonitorBreakReason::StallTimeout => {
            ProcessSettlementMode::AlreadyInterrupted
        }
        MonitorBreakReason::Rotation => ProcessSettlementMode::InterruptNow,
        MonitorBreakReason::Result if supports_multi_turn => ProcessSettlementMode::InterruptNow,
        MonitorBreakReason::Result | MonitorBreakReason::StreamClosed => {
            ProcessSettlementMode::AwaitNatural
        }
    }
}

/// Exactly one monitor-owned settlement task exists at a time.  Retaining its
/// join handle means a panic/cancellation is observable and retryable instead
/// of stranding an open receiver behind a detached channel sender.  Dropping a
/// monitor aborts its still-owned task, including daemon shutdown/monitor abort
/// paths.
struct SettlementTaskOwner {
    task: Option<tokio::task::JoinHandle<ProcessSettlementOutcome>>,
}

impl SettlementTaskOwner {
    fn new() -> Self {
        Self { task: None }
    }

    fn is_running(&self) -> bool {
        self.task.is_some()
    }

    fn start(
        &mut self,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        session_id: Uuid,
        expected_generation: u64,
        mode: ProcessSettlementMode,
        natural_grace: std::time::Duration,
    ) {
        debug_assert!(self.task.is_none(), "one settlement owner per monitor");
        self.task = Some(tokio::spawn(async move {
            super::reaper::settle_process_ownership(
                &active,
                session_id,
                expected_generation,
                mode,
                natural_grace,
                super::reaper::TEARDOWN_KILL_GRACE,
                super::reaper::TEARDOWN_KILL_POLL,
            )
            .await
        }));
    }

    async fn join(
        &mut self,
    ) -> std::result::Result<ProcessSettlementOutcome, tokio::task::JoinError> {
        let joined = self.task.as_mut().expect("checked settlement owner").await;
        self.task = None;
        joined
    }
}

impl Drop for SettlementTaskOwner {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub(crate) fn spawn_retry_timer(
    completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    retry_tx: mpsc::Sender<Uuid>,
    session_id: Uuid,
    delay_ms: u64,
    cancel_rx: tokio::sync::oneshot::Receiver<()>,
    cancel_message: &'static str,
) {
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {
                {
                    let mut completed_guard = completed.write().await;
                    if let Some(cs) = completed_guard.get_mut(&session_id) {
                        cs.retry_fired_at = Some(std::time::Instant::now());
                    }
                }
                let _ = retry_tx.send(session_id).await;
            }
            _ = cancel_rx => {
                tracing::info!(session_id = %session_id, "{}", cancel_message);
            }
        }
    });
}

impl SessionManager {
    /// Monitor a session's event stream.
    /// Takes ownership of provider_session to avoid holding locks while waiting for events.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn monitor_session(
        session_id: Uuid,
        expected_generation: u64,
        mut provider_session: Box<dyn ProviderSession>,
        active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
        completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
        event_bus: Arc<crate::bus::EventBus>,
        mut stop_rx: mpsc::Receiver<()>,
        store: Arc<tokio::sync::Mutex<Store>>,
        model_call_settlements: crate::model_control::call_control::ModelCallSettlementHandle,
        persistence: PersistenceHandle,
        initial_sequence: i32,
        context_rotation_enabled: bool,
        socket_path: std::path::PathBuf,
        counter: Arc<monitor::TokenCounter>,
        memory_handle: Option<crate::memory::worker::MemoryHandle>,
        retry_tx: mpsc::Sender<Uuid>,
        tool_registry: Arc<crate::tool_registry::ToolRegistry>,
        mut turn_controller: crate::turn_controller::TurnController,
        runtime_config: Arc<crate::config::RuntimeConfig>,
        spawn_coordinator: Arc<SpawnCoordinator>,
        // A6: token registry, threaded (like `spawn_coordinator`) so the
        // post-finalization rotation actions can re-mint at the G2/G3
        // establishment sites. Pass-through only — the monitor loop itself
        // never touches tokens.
        agent_tokens: Arc<RwLock<super::AgentTokenRegistry>>,
        spawn_epoch: Arc<std::sync::atomic::AtomicU64>,
        agent_message_arbiter: Arc<super::agent_message_arbiter::AgentMessageArbiter>,
        codegraph_handle: Option<crate::codegraph::IndexHandle>,
        custody_runtime: crate::sandbox::custody::CustodyExecutionRuntime,
    ) {
        let mut sequence: i32 = initial_sequence;
        // The invocation is durable before the monitor starts. If legacy or
        // corrupt state lacks it, provider events remain readable but receive
        // no provenance and Closure therefore fails closed.
        let current_model_invocation_id = {
            let store_guard = store.lock().await;
            store_guard
                .session_model_invocation_id(session_id)
                .ok()
                .flatten()
        };
        let approval_lease = {
            let guard = active.read().await;
            guard
                .get(&session_id)
                .filter(|tracked| {
                    tracked.spawn_generation == expected_generation
                        && tracked.session.provider == SessionProvider::CodexAppServer
                })
                .and_then(|_| provider_session.app_server_approval_writer())
                .and_then(|writer| {
                    super::pending_approvals::register_writer(
                        session_id,
                        expected_generation,
                        writer,
                    )
                })
        };
        let mut turn_number: i32 = 0;
        let mut received_any_event = false;
        let mut received_meaningful_output = false;
        let prior_meaningful_output = {
            let active_guard = active.read().await;
            active_guard.get(&session_id).is_some_and(|tracked| {
                tracked.events.iter().any(|event| {
                    event.role == Some(Role::Assistant) && !event.content.trim().is_empty()
                })
            })
        };
        let supports_multi_turn = provider_session.supports_multi_turn();
        let memory_flush_settings = MemoryFlushSettings {
            enabled: memory_handle.is_some(),
            ..MemoryFlushSettings::default()
        };
        let mut memory_flush_attempted_compaction_count: Option<u32> = None;
        let mut terminal_reason: Option<MonitorBreakReason> = None;
        let mut current_result = TerminalResult::None;
        let mut turn_outcome = TerminalTurnOutcome::NotMultiTurn;
        let mut stream_drained = false;
        let mut terminal_deadline: Option<tokio::time::Instant> = None;
        let mut post_settlement_deadline: Option<tokio::time::Instant> = None;
        let mut rotation_deadline_handled = false;
        let mut settlement_owner = SettlementTaskOwner::new();
        let mut settlement_outcome: Option<ProcessSettlementOutcome> = None;
        let mut settlement_retry_count = 0_u8;
        let mut producer_close_recovery_count = 0_u8;
        // The grant backing an IN-FLIGHT delivered message turn (P2-05b).
        //
        // Held here, on the monitor's own stack, for exactly as long as the
        // delivered turn runs. That is what makes "one outstanding grant blocks
        // every other `start_turn`" a property of the running daemon rather
        // than only of the registry: while this is `Some`, a concurrent
        // dispatcher tick sees this logical root as `GrantOutstanding` and
        // cannot plan a second delivery for it.
        //
        // It cannot leak. It is released explicitly at the next idle boundary
        // before the arbiter is consulted again, and `ArbitrationGrant`'s own
        // `Drop` releases it on every other way out of this function — a break,
        // an error return, or a panic unwinding through the loop.
        let mut outstanding_message_grant: Option<
            Box<super::agent_message_arbiter::ArbitrationGrant>,
        > = None;
        // A dead provider can still leave a buggy event producer holding the
        // sender. A later explicit stop is a recoverable operator action: it
        // asks this monitor to relinquish its receiver only after the exact
        // process generation has settled, so the row is not stuck Running.
        let mut abandon_open_producer_on_stop = false;
        let mut ownership_warning_emitted = false;
        let mut current_turn_tools: Vec<String> = Vec::new();
        // B1: one unrecognized-type diagnostic per distinct type, per run.
        // Stack-local, so it is scoped to this session and freed with it.
        let mut unrecognized_stream_events = UnrecognizedStreamEvents::default();
        let mut snapshot_interval = tokio::time::interval(std::time::Duration::from_secs(2));
        snapshot_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_snapshot_tokens: u64 = 0;
        // TD1: last work_time_ms value flushed by the snapshot tick, so the tick
        // can extend its persistence gate to "tokens changed OR work advanced"
        // without persisting on every 2s tick unconditionally.
        let mut last_snapshot_work_ms: Option<u64> = None;
        // Codex cache-read totals can advance while the context numerator stays
        // constant, so include them in the session-metadata flush gate.
        let mut last_snapshot_cache_read_tokens: Option<u64> = None;
        // Cached model string to avoid read-lock on every token usage event.
        let mut cached_model: Option<String> = None;
        // Accumulated assistant content since the last user event, for pipeline path scanning.
        // Reset on each user event so it tracks only the current assistant response.
        let mut accumulated_assistant_content = String::new();

        // --- Summarization state ---
        // Count of assistant messages received in this session (for threshold checks).
        // Initialized from the store for continued sessions.
        let mut assistant_message_count: i32 = if initial_sequence > 0 {
            let store_guard = store.lock().await;
            store_guard
                .count_assistant_messages(session_id)
                .unwrap_or(0)
        } else {
            0
        };
        // Track the assistant_message_count at which the last short/long summaries were generated.
        let (mut last_short_summary_at, mut last_long_summary_at) = {
            let store_guard = store.lock().await;
            let short = store_guard
                .get_latest_summary(session_id, rsi_common::types::SummaryKind::Short)
                .ok()
                .flatten();
            let long = store_guard
                .get_latest_summary(session_id, rsi_common::types::SummaryKind::Long)
                .ok()
                .flatten();
            // For continued sessions, approximate the assistant count at summary time
            // by using covers_through_sequence as a proxy (sequence includes all event types,
            // so this over-estimates, but it's safe — worst case we delay the next summary
            // by a few messages rather than re-triggering).
            (
                short.map(|_| assistant_message_count),
                long.map(|_| assistant_message_count),
            )
        };

        // Update status to Running
        {
            let mut active_guard = active.write().await;
            if let Some(tracked) = active_guard.get_mut(&session_id) {
                let old_status = tracked.session.status;
                tracked.session.status = SessionStatus::Running;
                tracked.session.updated_at = chrono::Utc::now();

                // TD1: open the active-work interval. `base` captures the persisted
                // floor (0 on fresh launch/rotation-child, prior lifetime total on
                // continue) so the fold at Site 2/3 ADDS to prior work rather than
                // overwriting it.
                tracked.work_run_start = Some(std::time::Instant::now());
                tracked.work_time_base_ms = tracked.session.work_time_ms.unwrap_or(0);

                event_bus.publish(DaemonEvent::SessionStatusChanged {
                    session_id,
                    old_status,
                    new_status: SessionStatus::Running,
                });
            }
        }

        // Persist status change to Running
        if let Err(e) = persistence
            .update_status(session_id, SessionStatus::Running)
            .await
        {
            tracing::error!(
                error = %e,
                session_id = %session_id,
                "Failed to persist running status"
            );
        }

        // Main event loop - NO LOCKS held while waiting for events
        let break_reason: MonitorBreakReason = loop {
            if let Some(reason) = terminal_reason {
                let active_generation = {
                    let active_guard = active.read().await;
                    active_guard
                        .get(&session_id)
                        .map(|tracked| tracked.spawn_generation)
                };
                if active_generation != Some(expected_generation) {
                    settlement_outcome = Some(ProcessSettlementOutcome::GenerationChanged);
                    break reason;
                }
                if stream_drained
                    && settlement_outcome.is_some_and(ProcessSettlementOutcome::is_settled)
                {
                    break reason;
                }
            }
            let phase_deadline = if rotation_deadline_handled {
                inactive_rotation_deadline()
            } else {
                let active_guard = active.read().await;
                current_rotation_deadline(active_guard.get(&session_id))
            };
            let candidate_deadline = terminal_deadline;
            let producer_close_deadline = post_settlement_deadline;
            tokio::select! {
                _ = stop_rx.recv() => {
                    tracing::debug!(session_id = %session_id, "Received stop signal");
                    // Distinguish user interrupt from rotation interrupt:
                    // A user interrupt (interrupt_requested=true) always wins,
                    // even if rotation was in progress.
                    let break_reason = {
                        let active_guard = active.read().await;
                        let tracked = active_guard.get(&session_id);
                        let user_interrupted = tracked.map(|t| t.interrupt_requested).unwrap_or(false);
                        let stall_interrupted = tracked.map(|t| t.stall_interrupted).unwrap_or(false);
                        let is_rotation = tracked.map(|t| t.rotation.is_rotating()).unwrap_or(false);
                        if stall_interrupted {
                            MonitorBreakReason::StallTimeout
                        } else if user_interrupted {
                            MonitorBreakReason::Interrupted
                        } else if is_rotation {
                            MonitorBreakReason::Rotation
                        } else {
                            MonitorBreakReason::Interrupted
                        }
                    };
                    terminal_reason = Some(break_reason);
                    terminal_deadline = None;
                    if !stream_drained {
                        abandon_open_producer_on_stop = true;
                    }
                    if abandon_open_producer_on_stop
                        && settlement_outcome
                            .is_some_and(ProcessSettlementOutcome::is_settled)
                    {
                        // The explicit cancellation is the recovery action
                        // for an otherwise-unclosable producer. Dropping the
                        // ProviderSession on monitor return releases our
                        // receiver; terminal status remains Interrupted (or
                        // the higher-priority StallTimeout), never fabricated
                        // Failed/Completed evidence.
                        stream_drained = true;
                        break break_reason;
                    }
                    if !settlement_owner.is_running()
                        && !settlement_outcome.is_some_and(ProcessSettlementOutcome::is_settled)
                    {
                        let mode = if matches!(
                            break_reason,
                            MonitorBreakReason::Interrupted | MonitorBreakReason::StallTimeout
                        ) {
                            ProcessSettlementMode::AlreadyInterrupted
                        } else {
                            ProcessSettlementMode::InterruptNow
                        };
                        settlement_owner.start(
                            active.clone(),
                            session_id,
                            expected_generation,
                            mode,
                            std::time::Duration::ZERO,
                        );
                        settlement_outcome = None;
                    }
                }
                _ = snapshot_interval.tick() => {
                    // Periodic context snapshot for crash recovery.
                    // Context snapshot rows only write when tokens have changed since
                    // the last snapshot. TD1: session-metadata flush is decoupled from
                    // that token gate and additionally fires when work_time_ms advanced,
                    // so a token-idle Running session (e.g. mid-compile, mid-test) still
                    // gets its work-time floor persisted every tick instead of losing the
                    // whole interval on crash.
                    // Consolidate into one WRITE lock: recompute_work_time mutates
                    // tracked state, so this can no longer be a read lock.
                    let (current_tokens, session_clone, work_advanced) = {
                        let mut active_guard = active.write().await;
                        match active_guard.get_mut(&session_id) {
                            Some(t) => {
                                t.recompute_work_time();
                                let work_now = t.session.work_time_ms;
                                let work_advanced = work_now != last_snapshot_work_ms;
                                if work_advanced {
                                    last_snapshot_work_ms = work_now;
                                }
                                let (tokens, pct, confidence, daemon_total, _) = live_context_state(t);
                                if is_codex_context_provider(t.session.provider) {
                                    if !has_codex_context_observation(t) {
                                        t.session.input_tokens = None;
                                    }
                                    if t.session.context_usage_confidence != confidence {
                                        monitor::publish_context_usage(
                                            &event_bus,
                                            session_id,
                                            pct,
                                            tokens,
                                            t.session.output_tokens.unwrap_or(0),
                                            daemon_total,
                                            confidence,
                                            t.context_budget(),
                                        );
                                    }
                                    t.session.context_usage_confidence = confidence;
                                }
                                (tokens, Some(t.session.clone()), work_advanced)
                            }
                            None => (0, None, false),
                        }
                    };
                    let tokens_changed = current_tokens > 0 && current_tokens != last_snapshot_tokens;
                    let current_cache_read_tokens = session_clone
                        .as_ref()
                        .and_then(|session| session.total_cache_read_tokens);
                    let cache_read_changed = cache_read_metadata_changed(
                        current_cache_read_tokens,
                        last_snapshot_cache_read_tokens,
                    );
                    if tokens_changed {
                        last_snapshot_tokens = current_tokens;
                        if let Err(e) =
                            persistence.insert_snapshot(session_id, current_tokens).await
                        {
                            tracing::warn!(
                                session_id = %session_id,
                                error = %e,
                                "Failed to write context snapshot"
                            );
                            event_bus.publish(DaemonEvent::SystemMessage {
                                level: "warn".to_string(),
                                message: format!(
                                    "Context snapshot write failed for {}: {}",
                                    session_id, e
                                ),
                            });
                        }
                    }
                    if tokens_changed || work_advanced || cache_read_changed {
                        // Also persist full session metadata (tokens, context_window,
                        // work_time_ms, etc.) so that TUI polls get accurate data even
                        // after daemon restart.
                        if let Some(session) = session_clone {
                            match persistence.update_session_metadata(session).await {
                                Ok(()) => {
                                    last_snapshot_cache_read_tokens = current_cache_read_tokens;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        error = %e,
                                        "Failed to persist session metadata on snapshot interval"
                                    );
                                    event_bus.publish(DaemonEvent::SystemMessage {
                                        level: "warn".to_string(),
                                        message: format!(
                                            "Session metadata write failed for {}: {}",
                                            session_id, e
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(phase_deadline) => {
                    let (timeout_action, rid, generation_matches) = {
                        let mut active_guard = active.write().await;
                        match active_guard.get_mut(&session_id) {
                            Some(tracked) if tracked.spawn_generation == expected_generation => {
                                let rid = tracked.rotation.rotation_id().map(|s| s.to_string());
                                let action = tracked.rotation.advance(RotationEvent::DeadlineElapsed);
                                (action, rid, true)
                            }
                            Some(_) | None => (RotationAction::NoOp, None, false),
                        }
                    };

                    if !generation_matches {
                        tracing::debug!(
                            session_id = %session_id,
                            expected_generation,
                            "Stale monitor ignored rotation deadline before coordinator advance"
                        );
                        return;
                    }

                    match timeout_action {
                        RotationAction::BreakMonitor {
                            phase,
                            break_reason,
                            kill_process,
                        } => {
                            rotation_deadline_handled = true;
                            persistence
                                .record_session_diagnostic(
                                    session_id,
                                    rsi_common::types::SessionDiagnosticLevelV1::Warn,
                                    "Session rotation deadline expired",
                                )
                                .await;
                            tracing::warn!(
                                session_id = %session_id,
                                phase,
                                kill_process,
                                timeout_break_reason = ?break_reason,
                                "Rotation deadline expired"
                            );
                            if let Some(ref rid) = rid {
                                let _ = persistence.log_rotation_event(
                                    session_id, rid, phase, "timed_out", None,
                                ).await;
                            }
                            terminal_reason = Some(break_reason);
                            terminal_deadline = None;
                            if !settlement_owner.is_running() {
                                // The generation-safe settlement owner does
                                // any deadline escalation after an exact
                                // generation probe; never signal/kill here.
                                settlement_owner.start(
                                    active.clone(),
                                    session_id,
                                    expected_generation,
                                    ProcessSettlementMode::InterruptNow,
                                    std::time::Duration::ZERO,
                                );
                                settlement_outcome = None;
                            }
                        }
                        RotationAction::NoOp => {}
                        unexpected => {
                            tracing::warn!(
                                session_id = %session_id,
                                action = ?unexpected,
                                "Unexpected coordinator action on deadline expiry"
                            );
                        }
                    }
                }
                _ = optional_deadline(candidate_deadline), if candidate_deadline.is_some() => {
                    terminal_deadline = None;
                    if !settlement_owner.is_running()
                        && !settlement_outcome.is_some_and(ProcessSettlementOutcome::is_settled)
                    {
                        settlement_owner.start(
                            active.clone(),
                            session_id,
                            expected_generation,
                            ProcessSettlementMode::InterruptNow,
                            std::time::Duration::ZERO,
                        );
                        settlement_outcome = None;
                    }
                }
                outcome = settlement_owner.join(), if settlement_owner.is_running() => {
                    let outcome = match outcome {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            tracing::error!(
                                session_id = %session_id,
                                ?error,
                                "Settlement owner ended without an outcome; retrying bounded recovery"
                            );
                            if settlement_retry_count < 1 {
                                settlement_retry_count += 1;
                                settlement_owner.start(
                                    active.clone(),
                                    session_id,
                                    expected_generation,
                                    settlement_mode(
                                        terminal_reason.unwrap_or(MonitorBreakReason::StreamClosed),
                                        supports_multi_turn,
                                    ),
                                    std::time::Duration::ZERO,
                                );
                                continue;
                            }
                            ProcessSettlementOutcome::EscalationFailed
                        }
                    };
                    settlement_outcome = Some(outcome);
                    if matches!(outcome, ProcessSettlementOutcome::GenerationChanged) {
                        break terminal_reason.unwrap_or(MonitorBreakReason::StreamClosed);
                    }
                    if outcome.is_settled() {
                        if !stream_drained {
                            if abandon_open_producer_on_stop {
                                // The stop arrived before the exact process
                                // settlement completed. It is now safe to
                                // release the stuck receiver and advance the
                                // explicit interrupt/stall recovery path.
                                stream_drained = true;
                                post_settlement_deadline = None;
                            } else {
                                post_settlement_deadline = Some(
                                    tokio::time::Instant::now()
                                        + super::reaper::TERMINAL_STREAM_CLOSE_GRACE,
                                );
                            }
                        }
                    } else if !ownership_warning_emitted {
                        ownership_warning_emitted = true;
                        tracing::error!(
                            session_id = %session_id,
                            ?outcome,
                            "Provider ownership settlement failed; retaining active session and event receiver"
                        );
                        event_bus.publish(DaemonEvent::SystemMessage {
                            level: "error".to_string(),
                            message: format!(
                                "Provider ownership for session {session_id} is unsettled; session remains Running"
                            ),
                        });
                    }
                }
                _ = optional_deadline(producer_close_deadline), if producer_close_deadline.is_some() => {
                    post_settlement_deadline = None;
                    if !stream_drained {
                        producer_close_recovery_count = producer_close_recovery_count.saturating_add(1);
                        // A dead provider with an open producer is not a
                        // terminal fact. Keep the sole receiver alive and
                        // re-arm bounded close recovery; a later interrupt or
                        // stream close takes the same recovery lane instead of
                        // wedging a detached monitor.
                        post_settlement_deadline = Some(
                            tokio::time::Instant::now()
                                + super::reaper::TERMINAL_STREAM_CLOSE_GRACE,
                        );
                    }
                    if !stream_drained && !ownership_warning_emitted {
                        ownership_warning_emitted = true;
                        tracing::error!(
                            session_id = %session_id,
                            recovery_attempt = producer_close_recovery_count,
                            "Provider process settled but event producers did not close; retaining recoverable active receiver"
                        );
                        event_bus.publish(DaemonEvent::SystemMessage {
                            level: "error".to_string(),
                            message: format!(
                                "Provider event producers for session {session_id} did not quiesce; retaining Running ownership and retrying bounded producer-close recovery (interrupt to end recovery)"
                            ),
                        });
                    }
                }
                event_opt = provider_session.next_event(), if !stream_drained => {
                    match event_opt {
                        Some(stream_event) => {
                            received_any_event = true;

                            if stream_event.event_type == "approval_resolved" {
                                if let Err(error)=super::pending_approvals::resolve_monitor_approval(approval_lease.as_ref(),&active,&persistence,session_id,expected_generation,&mut sequence,&stream_event.data).await {
                                    persistence.record_session_diagnostic(
                                        session_id,
                                        rsi_common::types::SessionDiagnosticLevelV1::Error,
                                        "AppServer request closure persistence or identity recovery failed",
                                    ).await;
                                    tracing::error!(%session_id,%error,"AppServer closure remains unresolved; exact witness blocks answers");
                                    event_bus.publish(DaemonEvent::SystemMessage {level:"error".into(),message:format!("AppServer request closure for {session_id} needs persistence or exact identity recovery: {error}. No response is resent.")});
                                }
                                continue;
                            }
                            if stream_event.event_type == "approval_request" {
                                let invocation = provider_session.app_server_approval_invocation();
                                match super::pending_approvals::publish_monitor_approval(
                                    approval_lease.as_ref(), &active, &persistence, session_id, expected_generation,
                                    current_model_invocation_id, invocation, &mut sequence, &stream_event.data,
                                ).await {
                                    Ok(event) => event_bus.publish(DaemonEvent::ConversationEvent {session_id,event}),
                                    Err(error) => {
                                        tracing::error!(%session_id,%error,"native approval publication remains unresolved");
                                        if approval_lease.as_ref().is_some_and(super::pending_approvals::capacity_sealed) {
                                            current_result=TerminalResult::ProviderError;
                                            turn_outcome=TerminalTurnOutcome::Terminal;
                                            terminal_reason=Some(MonitorBreakReason::Result);
                                            stream_drained=true;
                                            if !settlement_owner.is_running() {
                                                settlement_owner.start(active.clone(),session_id,expected_generation,ProcessSettlementMode::InterruptNow,std::time::Duration::ZERO);
                                            }
                                        }
                                        event_bus.publish(DaemonEvent::SystemMessage {level:"error".into(),message:format!("Pending AppServer approval for {session_id} could not be published: {error}. No automatic answer is allowed.")});
                                    }
                                }
                                continue;
                            }

                            // Update last_event_at for stall detection
                            {
                                let mut active_guard = active.write().await;
                                if let Some(tracked) = active_guard.get_mut(&session_id) {
                                    tracked.last_event_at = chrono::Utc::now();
                                }
                            }

                            // Surface process stderr errors as assistant messages so TUI shows them
                            if stream_event.event_type == "process_error" {
                                let terminal_provider_error = is_terminal_provider_error(&stream_event);
                                if let Some(error_text) = stream_event.data.get("error").and_then(|v| v.as_str()) {
                                    let source = stream_event
                                        .data
                                        .get("source")
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or("provider");
                                    let heading = if source == "stderr" && !terminal_provider_error {
                                        match stream_event
                                            .data
                                            .get("stderr_record_count")
                                            .and_then(serde_json::Value::as_u64)
                                        {
                                            Some(count) if count > 1 => {
                                                format!("Provider diagnostic (stderr; {count} records)")
                                            }
                                            _ => "Provider diagnostic (stderr)".to_string(),
                                        }
                                    } else if source == "codex_event" && !terminal_provider_error {
                                        "Provider diagnostic (codex_event)".to_string()
                                    } else if source == "stderr" {
                                        "Process Error (stderr)".to_string()
                                    } else if source == "codex_event" {
                                        "Process Error (codex_event)".to_string()
                                    } else {
                                        "Process Error (provider)".to_string()
                                    };
                                    if source == "stderr" {
                                        tracing::error!(session_id = %session_id, error = %error_text, "CLI process wrote to stderr");
                                    } else if terminal_provider_error {
                                        tracing::error!(session_id = %session_id, source, error = %error_text, "provider reported a terminal error");
                                    } else {
                                        tracing::warn!(session_id = %session_id, source, error = %error_text, "provider emitted a diagnostic event");
                                    }
                                    sequence += 1;
                                    let error_event = ConversationEvent {
                                        id: 0,
                                        session_id,
                                        sequence,
                                        event_type: EventType::Message,
                                        role: Some(Role::Assistant),
                                        content: format!("**{heading}**\n```\n{}\n```", error_text),
                                        tool_name: None,
                                        tool_input: None,
                                        created_at: chrono::Utc::now(),
                                        offload_id: None,
                                        tool_use_id: None,
                                        metadata: None,
                                    };
                                    {
                                        let mut active_guard = active.write().await;
                                        if let Some(tracked) = active_guard.get_mut(&session_id) {
                                            tracked.events.push(error_event.clone());
                                        }
                                    }
                                    let persisted = if let Some(model_invocation_id) = current_model_invocation_id {
                                        persistence.insert_event_with_provenance(
                                            error_event.clone(),
                                            ConversationEventProvenanceV1 {
                                                producer_kind: ConversationEventProducerKindV1::DaemonProviderDiagnostic,
                                                model_invocation_id,
                                                provider_event_type: stream_event.event_type.clone(),
                                            },
                                        ).await
                                    } else {
                                        persistence.insert_event(error_event.clone()).await
                                    };
                                    if let Ok(db_id) = persisted {
                                        let mut active_guard = active.write().await;
                                        if let Some(tracked) = active_guard.get_mut(&session_id)
                                            && let Some(last_event) = tracked.events.last_mut()
                                        {
                                            last_event.id = db_id;
                                        }
                                    }
                                    event_bus.publish(DaemonEvent::ConversationEvent {
                                        session_id,
                                        event: error_event,
                                    });
                                }
                                if terminal_provider_error {
                                    if let Some(stop_reason) =
                                        terminal_provider_stop_reason(&stream_event)
                                    {
                                        let mut active_guard = active.write().await;
                                        if let Some(tracked) = active_guard.get_mut(&session_id)
                                            && tracked.spawn_generation == expected_generation
                                        {
                                            tracked.session.stop_reason =
                                                Some(stop_reason.into_owned());
                                        }
                                    }
                                    // App-server failed/interrupted turn
                                    // notifications and normalized CLI/task
                                    // errors are terminal provider evidence,
                                    // unlike raw stderr. Preserve the visible
                                    // diagnostic above, then drain/settle the
                                    // exact provider generation.
                                    current_result = TerminalResult::ProviderError;
                                    turn_outcome = TerminalTurnOutcome::Terminal;
                                    terminal_reason = Some(MonitorBreakReason::Result);
                                    terminal_deadline = Some(tokio::time::Instant::now());
                                }
                                continue;
                            }

                            // Update claude_session_id if present (brief lock)
                            let mut sid_to_persist: Option<String> = None;
                            let mut model_to_publish: Option<(
                                String,
                                rsi_common::ResolvedContextBudget,
                            )> = None;
                            if let Some(sid) = stream_event.data.get("session_id").and_then(|v| v.as_str()) {
                                let mut active_guard = active.write().await;
                                if let Some(tracked) = active_guard.get_mut(&session_id)
                                    && tracked.session.claude_session_id.is_none()
                                {
                                    tracked.session.claude_session_id = Some(sid.to_string());
                                    sid_to_persist = Some(sid.to_string());
                                }
                            }

                            // Local never emits a session_id in their output.
                            // Set a synthetic one (the Flywheel UUID) so continue_session() works.
                            {
                                let mut active_guard = active.write().await;
                                if let Some(tracked) = active_guard.get_mut(&session_id)
                                    && tracked.session.claude_session_id.is_none()
                                    && matches!(tracked.session.provider, SessionProvider::Local)
                                {
                                    tracked.session.claude_session_id =
                                        Some(session_id.to_string());
                                    sid_to_persist = Some(session_id.to_string());
                                }
                            }

                            // Capture the authoritative launch model without letting later
                            // auxiliary-model metadata clobber an explicit session selection.
                            if stream_event.data.get("model").is_some() {
                                let refresh_provider = {
                                    let active_guard = active.read().await;
                                    active_guard.get(&session_id).and_then(|tracked| {
                                        if tracked.spawn_generation != expected_generation {
                                            return None;
                                        }
                                        let announced = authoritative_model_update(
                                            tracked.session.model.as_deref(),
                                            &stream_event,
                                        )?;
                                        (tracked.session.model.as_deref() != Some(announced)
                                            || tracked.session.resolved_context_budget.is_none())
                                        .then_some(tracked.session.provider)
                                    })
                                };
                                if let Some(provider) = refresh_provider
                                    && let Err(error) = crate::provider_capabilities::refresh_installed_catalog_for_provider(
                                        provider,
                                        Arc::clone(&runtime_config),
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        %error,
                                        "Installed provider catalog refresh failed on model change; using degraded capability evidence"
                                    );
                                }
                                let candidate = {
                                    let active_guard = active.read().await;
                                    active_guard.get(&session_id).and_then(|tracked| {
                                        (tracked.spawn_generation == expected_generation).then(|| {
                                            authoritative_model_candidate(
                                                tracked.session.provider,
                                                tracked.session.model.as_deref(),
                                                tracked.session.context_window,
                                                tracked
                                                    .session
                                                    .resolved_context_budget
                                                    .as_ref(),
                                                &stream_event,
                                            )
                                        })
                                    })
                                }
                                .flatten();

                                if let Some(candidate) = candidate {
                                    if !candidate.tuple_changed {
                                        let mut active_guard = active.write().await;
                                        if let Some(tracked) = active_guard.get_mut(&session_id)
                                            && tracked.spawn_generation == expected_generation
                                        {
                                            cached_model = Some(candidate.model.clone());
                                            warn_capability_class_mismatch(
                                                &event_bus,
                                                session_id,
                                                tracked.session.capability_class,
                                                &mut tracked.last_mismatch_warn,
                                                &candidate.model,
                                            );
                                        }
                                    } else {
                                        match persistence
                                            .compare_and_update_session_model(
                                            store.clone(),
                                            session_id,
                                            candidate.prior_model.clone(),
                                            candidate.prior_context_window,
                                            candidate.prior_budget.clone(),
                                            Some(candidate.model.clone()),
                                            Some(candidate.budget.active_tokens),
                                            Some(candidate.budget.clone()),
                                        )
                                        .await
                                        {
                                            Ok(true) => {
                                                let mut active_guard = active.write().await;
                                                if let Some(tracked) =
                                                    active_guard.get_mut(&session_id)
                                                    && tracked.spawn_generation
                                                        == expected_generation
                                                {
                                                    tracked.session.model =
                                                        Some(candidate.model.clone());
                                                    install_context_budget(
                                                        &mut tracked.session,
                                                        candidate.budget.clone(),
                                                    );
                                                    cached_model = Some(candidate.model.clone());
                                                    if candidate.model_changed {
                                                        model_to_publish = Some((
                                                            candidate.model.clone(),
                                                            candidate.budget.clone(),
                                                        ));
                                                    }

                                                    warn_capability_class_mismatch(
                                                        &event_bus,
                                                        session_id,
                                                        tracked.session.capability_class,
                                                        &mut tracked.last_mismatch_warn,
                                                        &candidate.model,
                                                    );
                                                }
                                            }
                                            Ok(false) => tracing::debug!(
                                                session_id = %session_id,
                                                expected_generation,
                                                "Stale model/context update lost its durable compare-and-swap"
                                            ),
                                            Err(error) => {
                                                tracing::warn!(
                                                    session_id = %session_id,
                                                    error = %error,
                                                    "Failed to durably persist session model metadata"
                                                );
                                                event_bus.publish(
                                                    crate::bus::DaemonEvent::SystemMessage {
                                                        level: "warn".to_string(),
                                                        message: format!(
                                                            "Failed to durably persist model metadata for session {}",
                                                            session_id
                                                        ),
                                                    },
                                                );
                                            }
                                        }
                                    }
                                }
                            }

                            // V99/P1-A: record what the CLI advertised about
                            // itself. Written on every init rather than latched
                            // on the first — a resumed session re-announces,
                            // and the operator may have upgraded the binary in
                            // between, so the latest handshake is the true one.
                            if let Some(handshake) = provider_handshake(&stream_event) {
                                let generation_matches = {
                                    let active_guard = active.read().await;
                                    active_guard
                                        .get(&session_id)
                                        .is_some_and(|tracked| {
                                            tracked.spawn_generation == expected_generation
                                        })
                                };
                                if generation_matches {
                                    if let Err(error) = persistence
                                        .update_session_provider_handshake(
                                            store.clone(),
                                            session_id,
                                            handshake.cli_version.clone(),
                                            handshake.capabilities.clone(),
                                        )
                                        .await
                                    {
                                        // Telemetry, not conversation: a failed
                                        // write must not take the session down.
                                        tracing::warn!(
                                            session_id = %session_id,
                                            error = %error,
                                            "Failed to persist provider handshake"
                                        );
                                    } else {
                                        let mut active_guard = active.write().await;
                                        if let Some(tracked) = active_guard.get_mut(&session_id)
                                            && tracked.spawn_generation == expected_generation
                                        {
                                            tracked.session.provider_cli_version =
                                                handshake.cli_version;
                                            tracked.session.provider_capabilities =
                                                handshake.capabilities;
                                        }
                                    }
                                }
                            }

                            if let Some((model, resolved_context_budget)) = model_to_publish {
                                event_bus.publish(DaemonEvent::SessionMetadataChanged {
                                    session_id,
                                    model: Some(model),
                                    pinned_at: None,
                                    project_id: None,
                                    parent_id: None,
                                    lead_session_id: None,
                                    testing_needed_at: None,
                                    rotation_disabled_at: None,
                                    resolved_context_budget: Some(resolved_context_budget),
                                });
                            }

                            if let Some(codex_usage) =
                                monitor::extract_codex_context_usage(&stream_event)
                            {
                                let runtime_budget_ready = if let Some(window) =
                                    codex_usage.context_window
                                {
                                    match persist_runtime_context_observation(
                                        &active,
                                        &persistence,
                                        &store,
                                        session_id,
                                        expected_generation,
                                        window,
                                        chrono::Utc::now(),
                                    )
                                    .await
                                    {
                                        Ok(Some(_)) => true,
                                        Ok(None) => false,
                                        Err(error) => {
                                            tracing::warn!(
                                                session_id = %session_id,
                                                %error,
                                                "Failed to durably persist runtime context evidence"
                                            );
                                            event_bus.publish(DaemonEvent::SystemMessage {
                                                level: "warn".to_string(),
                                                message: format!(
                                                    "Failed to durably persist runtime context evidence for session {}",
                                                    session_id
                                                ),
                                            });
                                            false
                                        }
                                    }
                                } else {
                                    true
                                };
                                let context_update = if runtime_budget_ready {
                                    let mut active_guard = active.write().await;
                                    if let Some(tracked) = active_guard.get_mut(&session_id) {
                                        if matches!(
                                            tracked.session.provider,
                                            SessionProvider::Codex
                                                | SessionProvider::Pioneer
                                                | SessionProvider::OpenRouter
                                                | SessionProvider::Bedrock
                                                | SessionProvider::CodexAppServer
                                        )
                                        {
                                            record_codex_context_observation(tracked, &stream_event, &codex_usage);

                                            let (numerator, pct, confidence, daemon_total, ctx_window) =
                                                live_context_state(tracked);
                                            let resolved_context_budget = tracked.context_budget();
                                            tracked.session.context_usage_confidence = confidence;
                                            advance_context_rotation_threshold(
                                                tracked,
                                                session_id,
                                                pct,
                                                &persistence,
                                            )
                                            .await;

                                            Some((
                                                numerator,
                                                pct,
                                                confidence,
                                                daemon_total,
                                                ctx_window,
                                                codex_usage.output_tokens,
                                                resolved_context_budget,
                                            ))
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };

                                if let Some((
                                    numerator,
                                    pct,
                                    confidence,
                                    daemon_total,
                                    _ctx_window,
                                    output_tokens,
                                    resolved_context_budget,
                                )) = context_update
                                {
                                    monitor::publish_context_usage(
                                        &event_bus,
                                        session_id,
                                        pct,
                                        numerator,
                                        output_tokens,
                                        daemon_total,
                                        confidence,
                                        resolved_context_budget,
                                    );
                                }
                            }

                            // Extract token usage from assistant/result events.
                            // Also create and persist a TurnMetric for per-turn analytics.
                            // Live accumulators drive context fill for API-primary providers;
                            // Codex fill uses the token-count branch above.
                            if let Some(usage) = monitor::extract_token_usage(&stream_event) {
                                let has_usage = !matches!(usage.confidence, ContextUsageConfidence::Missing);

                                // Only usage-bearing events represent completed turns.
                                let metric = if has_usage {
                                    turn_number += 1;
                                    let current_model = cached_model.clone();
                                    let metric = monitor::build_turn_metric(
                                        session_id,
                                        turn_number,
                                        &usage,
                                        &current_turn_tools,
                                        current_model,
                                    );
                                    current_turn_tools.clear();
                                    Some(metric)
                                } else {
                                    None
                                };

                                // Update live accumulators, daemon counts, and session tokens
                                let context_update = {
                                    let mut active_guard = active.write().await;
                                    if let Some(tracked) = active_guard.get_mut(&session_id) {
                                        // Daemon-side token counting from raw content.
                                        // Only count `assistant` events for Claude/Codex.
                                        // Local counts via `content_block_delta` (below).
                                        if stream_event.event_type == "assistant"
                                            && !matches!(tracked.session.provider, SessionProvider::Local)
                                        {
                                            tracked.daemon_output_tokens +=
                                                counter.count_assistant_event(&stream_event.data);
                                        }
                                        tracked.session.daemon_input_tokens =
                                            Some(tracked.daemon_input_tokens);
                                        tracked.session.daemon_output_tokens =
                                            Some(tracked.daemon_output_tokens);

                                        // API-reported live accumulators (secondary source).
                                        // Use high-watermark: API total_input can drop when
                                        // ContextUsageConfidence::Partial fires (cache fields
                                        // absent), collapsing to only non-cached input_tokens.
                                        // Never let the accumulated value go backward.
                                        if has_usage {
                                            tracked.live_input_tokens = tracked.live_input_tokens.max(usage.total_input);
                                            tracked.live_output_tokens += usage.output;
                                            // Snapshot daemon total at time of API update for delta-based
                                            // estimation. Used by non-Claude API-primary providers
                                            // (Antigravity / CodexAppServer / Harness) to approximate
                                            // between-turn context growth during tool use. Claude's
                                            // selector ignores this field — the API value is
                                            // authoritative — but the snapshot still updates cheaply on
                                            // every report so the field stays consistent if a future
                                            // provider refactor needs it.
                                            tracked.daemon_tokens_at_last_api_update =
                                                tracked.daemon_input_tokens + tracked.daemon_output_tokens;
                                            // Reset the staleness clock on every usage-bearing chunk.
                                            // `apply_staleness` compares this against a 60s window
                                            // to flip confidence → `Stale` when the CLI has gone
                                            // quiet on a Claude + Running session. The stale flag
                                            // preserves the last API percentage; it does not switch
                                            // Claude to a BPE-driven numerator.
                                            if !is_codex_context_provider(tracked.session.provider) {
                                                tracked.last_usage_update = Some(tokio::time::Instant::now());
                                            }
                                            // Preserve Full confidence if we had it; Partial can't
                                            // downgrade a previously Full reading.
                                            if usage.confidence == ContextUsageConfidence::Full
                                                || tracked.live_usage_confidence != ContextUsageConfidence::Full
                                            {
                                                tracked.live_usage_confidence = usage.confidence;
                                            }
                                        }

                                        // Mirror API values onto Session fields for RPC consumers.
                                        // Missing-usage chunks should not wipe previously recorded totals.
                                        if has_usage {
                                            if is_codex_context_provider(tracked.session.provider) {
                                                tracked.session.input_tokens = has_codex_context_observation(tracked)
                                                    .then_some(tracked.codex_context_tokens);
                                            } else {
                                                tracked.session.input_tokens = Some(usage.total_input);
                                            }
                                            if !is_codex_context_provider(tracked.session.provider) {
                                                tracked.session.output_tokens = Some(usage.output);
                                            }
                                            tracked.session.total_input_tokens =
                                                Some(tracked.live_input_tokens);
                                            tracked.session.total_output_tokens =
                                                Some(tracked.live_output_tokens);
                                            tracked.session.total_cache_creation_tokens = Some(
                                                tracked
                                                    .session
                                                    .total_cache_creation_tokens
                                                    .unwrap_or(0)
                                                    + usage.cache_creation,
                                            );
                                            tracked.session.total_cache_read_tokens = Some(
                                                tracked
                                                    .session
                                                    .total_cache_read_tokens
                                                    .unwrap_or(0)
                                                    + usage.cache_read,
                                            );
                                        }

                                        if let Some(metric) = metric.as_ref() {
                                            if metric.stop_reason.is_some() {
                                                tracked.session.stop_reason =
                                                    metric.stop_reason.clone();
                                            }
                                            tracked.turn_metrics.push(metric.clone());
                                        }

                                        let (numerator, pct, confidence, daemon_total, ctx_window) =
                                            live_context_state(tracked);
                                        tracked.session.context_usage_confidence = confidence;
                                        advance_context_rotation_threshold(
                                            tracked,
                                            session_id,
                                            pct,
                                            &persistence,
                                        )
                                        .await;
                                        Some((
                                            pct,
                                            numerator,
                                            daemon_total,
                                            confidence,
                                            ctx_window,
                                            tracked.context_budget(),
                                        ))
                                    } else {
                                        None
                                    }
                                };

                                // Emit real-time context usage event for TUI.
                                // Pass the same provider-selected numerator that produced `ctx_pct`
                                // so event payloads stay internally consistent.
                                if let Some((
                                    ctx_pct,
                                    ctx_tokens,
                                    daemon_total,
                                    ctx_confidence,
                                    _ctx_window,
                                    resolved_context_budget,
                                )) = context_update
                                    && (has_usage || daemon_total > 0)
                                {
                                    monitor::publish_context_usage(
                                        &event_bus,
                                        session_id,
                                        ctx_pct,
                                        ctx_tokens,
                                        usage.output,
                                        daemon_total,
                                        ctx_confidence,
                                        resolved_context_budget,
                                    );
                                }

                                // Persist turn metric to SQLite
                                if let Some(metric) = metric {
                                    if let Err(e) = persistence.insert_turn_metric(metric).await {
                                        tracing::warn!(
                                            session_id = %session_id,
                                            error = %e,
                                            "Failed to insert turn metric"
                                        );
                                    }
                                }
                            }

                            // Daemon token counting for user events (tool results) and
                            // OpenAI-compat content_block_delta streaming chunks.
                            match stream_event.event_type.as_str() {
                                "user" => {
                                    let delta = counter.count_user_event(&stream_event.data);
                                    if delta > 0 {
                                        let context_update = {
                                            let mut active_guard = active.write().await;
                                            if let Some(tracked) = active_guard.get_mut(&session_id) {
                                                tracked.daemon_input_tokens += delta;
                                                tracked.session.daemon_input_tokens =
                                                    Some(tracked.daemon_input_tokens);
                                                let (numerator, pct, confidence, daemon_total, _) =
                                                    live_context_state(tracked);
                                                Some((
                                                    numerator,
                                                    pct,
                                                    confidence,
                                                    daemon_total,
                                                    tracked.context_budget(),
                                                ))
                                            } else {
                                                None
                                            }
                                        };
                                        if let Some((numerator, pct, confidence, daemon_total, budget)) =
                                            context_update
                                        {
                                            // Send estimated numerator (API baseline + daemon delta)
                                            // so TUI tracks real-time context growth during tool use.
                                            monitor::publish_context_usage(
                                                &event_bus,
                                                session_id,
                                                pct,
                                                numerator,
                                                0,
                                                daemon_total,
                                                confidence,
                                                budget,
                                            );
                                        }
                                    }
                                }
                                "content_block_delta" => {
                                    // Local providers stream text via content_block_delta
                                    if let Some(text) = stream_event
                                        .data
                                        .get("delta")
                                        .and_then(|d| d.get("text"))
                                        .and_then(|v| v.as_str())
                                    {
                                        let delta = counter.count(text);
                                        if delta > 0 {
                                            let context_update = {
                                                let mut active_guard = active.write().await;
                                                if let Some(tracked) =
                                                    active_guard.get_mut(&session_id)
                                                {
                                                    tracked.daemon_output_tokens += delta;
                                                    tracked.session.daemon_output_tokens =
                                                        Some(tracked.daemon_output_tokens);
                                                    let (numerator, pct, confidence, daemon_total, _) =
                                                        live_context_state(tracked);
                                                    Some((
                                                        numerator,
                                                        pct,
                                                        confidence,
                                                        daemon_total,
                                                        tracked.context_budget(),
                                                    ))
                                                } else {
                                                    None
                                                }
                                            };
                                            if let Some((numerator, pct, confidence, daemon_total, budget)) =
                                                context_update
                                            {
                                                // Send estimated numerator (API baseline + daemon delta)
                                                // so TUI tracks real-time context growth during tool use.
                                                monitor::publish_context_usage(
                                                    &event_bus,
                                                    session_id,
                                                    pct,
                                                    numerator,
                                                    0,
                                                    daemon_total,
                                                    confidence,
                                                    budget,
                                                );
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }

                            // Persist claude_session_id to SQLite
                            if let Some(sid_str) = sid_to_persist {
                                if let Err(e) = persistence
                                    .update_claude_session_id(session_id, sid_str.clone())
                                    .await
                                {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        error = %e,
                                        "Failed to persist claude_session_id"
                                    );
                                }
                            }

                            // Collect tool names for per-turn tracking
                            if stream_event.event_type == "tool_use"
                                && let Some(name) = stream_event.data.get("name").and_then(|v| v.as_str())
                            {
                                current_turn_tools.push(name.to_string());
                            }

                            // Phase 6: Dispatch tool_call events for app-server sessions.
                            // The app-server emits "tool_call" (bidirectional RPC) rather than
                            // "tool_use" (CLI stdout). When the provider calls a dynamic tool,
                            // dispatch to the registry and send the result back.
                            if stream_event.event_type == "tool_call" {
                                let call_id = stream_event
                                    .data
                                    .get("call_id")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0);
                                let tool_name = stream_event
                                    .data
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let tool_args = stream_event
                                    .data
                                    .get("arguments")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

                                // Track tool name for per-turn metrics
                                current_turn_tools.push(tool_name.clone());

                                // Dispatch to registry and send result back to provider.
                                // The registry call is async but non-blocking; we await it here
                                // since the provider is waiting on the result before continuing.
                                let registry_result = tool_registry.execute(&tool_name, tool_args).await;
                                match registry_result {
                                    Ok(result) => {
                                        if let Err(e) = provider_session.send_tool_result(call_id, result).await {
                                            tracing::warn!(
                                                session_id = %session_id,
                                                tool = %tool_name,
                                                error = %e,
                                                "Failed to send tool result to provider"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            session_id = %session_id,
                                            tool = %tool_name,
                                            error = %e,
                                            "Tool dispatch failed; sending error result"
                                        );
                                        let error_result = serde_json::json!({
                                            "error": e.to_string()
                                        });
                                        let _ = provider_session.send_tool_result(call_id, error_result).await;
                                    }
                                }
                                // tool_call is an internal protocol event; continue the loop
                                // without converting to a ConversationEvent.
                                continue;
                            }

                            // Debug: log raw stream events to diagnose role attribution
                            tracing::debug!(
                                session_id = %session_id,
                                event_type = %stream_event.event_type,
                                data = %stream_event.data,
                                "Raw stream event from Claude CLI"
                            );

                            // Convert to ConversationEvent(s), store and publish.
                            //
                            // B1: a type the converter does not map produced an
                            // empty vec and NOTHING else — no log, no metric —
                            // so `rate_limit_event` and `system/api_retry` went
                            // unnoticed until a manual CLI probe found them. The
                            // Codex path has always warned here; this is the
                            // same signal, deduped to one line per distinct type
                            // per session so a chatty type cannot flood the log.
                            //
                            // Deliberately NOT a `continue`: the loop's later
                            // stages (rate-limit persistence, pipeline path
                            // detection, result handling) still run for a type
                            // that yields no conversation event.
                            let mut assistant_text_this_event = String::new();
                            let converted = Self::convert_recognized_stream_event(
                                &stream_event,
                                session_id,
                                &mut sequence,
                            )
                            .unwrap_or_else(|| {
                                unrecognized_stream_events
                                    .note(session_id, &stream_event.event_type);
                                Vec::new()
                            });
                            for conv_event in converted {
                                // Update local assistant content accumulator (no lock needed).
                                match conv_event.role {
                                    Some(Role::User) => accumulated_assistant_content.clear(),
                                    Some(Role::Assistant) => {
                                        accumulated_assistant_content.push_str(&conv_event.content);
                                        assistant_text_this_event.push_str(&conv_event.content);
                                        // Track meaningful output locally; flushed to TrackedSession once in post-loop.
                                        if !conv_event.content.is_empty() {
                                            received_meaningful_output = true;
                                        }
                                    }
                                    _ => {}
                                }

                                let provenance = current_model_invocation_id.map(|model_invocation_id| {
                                    let producer_kind = if stream_event.event_type == "assistant"
                                        && conv_event.event_type == EventType::Message
                                        && conv_event.role == Some(Role::Assistant)
                                    {
                                        ConversationEventProducerKindV1::ProviderAssistantOutput
                                    } else {
                                        ConversationEventProducerKindV1::ProviderOther
                                    };
                                    ConversationEventProvenanceV1 {
                                        producer_kind, model_invocation_id,
                                        provider_event_type: stream_event.event_type.clone(),
                                    }
                                });
                                let (detected_question, persisted) = persist_tracked_provider_event(
                                    &active, &persistence, session_id, expected_generation,
                                    &conv_event, provenance,
                                ).await;
                                if let Err(error) = persisted {
                                    tracing::error!(session_id=%session_id, error=%error,
                                        "Failed to persist conversation event; question remains unresolved");
                                }

                                if let Some(question) = detected_question {
                                    // Display remains useful on persistence failure; durable
                                    // answer routing sees the unresolved publication gate.
                                    event_bus.publish(DaemonEvent::SessionQuestionRaised { session_id, question });
                                }

                                event_bus.publish(DaemonEvent::ConversationEvent {
                                    session_id,
                                    event: conv_event.clone(),
                                });

                                // --- Summarization threshold check ---
                                // Only count non-empty assistant messages toward summary thresholds.
                                if conv_event.event_type == EventType::Message
                                    && conv_event.role == Some(Role::Assistant)
                                    && !conv_event.content.is_empty()
                                {
                                    assistant_message_count += 1;

                                    // Skip summarization for TaskRabbit/Bug sessions (ephemeral).
                                    let session_kind = {
                                        let active_guard = active.read().await;
                                        active_guard
                                            .get(&session_id)
                                            .map(|t| t.session.session_kind)
                                            .unwrap_or_default()
                                    };
                                    if !matches!(
                                        session_kind,
                                        rsi_common::types::SessionKind::TaskRabbit
                                            | rsi_common::types::SessionKind::Bug
                                    ) {
                                        let action = super::summarizer::should_summarize(
                                            assistant_message_count,
                                            last_short_summary_at,
                                            last_long_summary_at,
                                        );

                                        if !matches!(action, super::summarizer::SummarizeAction::None) {
                                            let current_count = assistant_message_count;
                                            let current_sequence = sequence;
                                            let store_c = store.clone();
                                            let persistence_c = persistence.clone();
                                            let event_bus_c = event_bus.clone();
                                            let runtime_config_c = runtime_config.clone();
                                            let do_short = matches!(
                                                action,
                                                super::summarizer::SummarizeAction::Short
                                                    | super::summarizer::SummarizeAction::Both
                                            );
                                            let do_long = matches!(
                                                action,
                                                super::summarizer::SummarizeAction::Long
                                                    | super::summarizer::SummarizeAction::Both
                                            );

                                            // Read query from tracked session
                                            let query = {
                                                let active_guard = active.read().await;
                                                active_guard
                                                    .get(&session_id)
                                                    .map(|t| t.session.query.clone())
                                                    .unwrap_or_default()
                                            };

                                            // Update local tracking state immediately
                                            if do_short {
                                                last_short_summary_at = Some(current_count);
                                            }
                                            if do_long {
                                                last_long_summary_at = Some(current_count);
                                            }

                                            // Spawn background summarization task (never blocks monitor)
                                            tokio::spawn(async move {
                                                Self::run_summarization(
                                                    session_id,
                                                    &query,
                                                    current_sequence,
                                                    do_short,
                                                    do_long,
                                                    store_c,
                                                    persistence_c,
                                                    event_bus_c,
                                                    runtime_config_c,
                                                )
                                                .await;
                                            });
                                        }
                                    }
                                }
                            }

                            // Secondary: scan the current assistant message for thoughts/shared/
                            // path mentions. Handoff paths require an explicit declaration so a
                            // stale path quoted from earlier context cannot win the rotation child.
                            if stream_event.event_type == "assistant" {
                                let (needs_artifact, needs_handoff) = {
                                    let active_guard = active.read().await;
                                    let t_opt = active_guard.get(&session_id);
                                    let needs_a = t_opt.map(|t| t.pipeline_artifact.is_none()).unwrap_or(false);
                                    let needs_h = t_opt.map(|t| {
                                        t.rotation.is_rotating() &&
                                        !matches!(t.rotation.state(), RotationState::WritingHandoff { handoff_filepath: Some(_), .. })
                                    }).unwrap_or(false);
                                    (needs_a, needs_h)
                                };

                                if needs_artifact || needs_handoff {
                                    let mut workflow_advances = Vec::new();
                                    for cap in PIPELINE_PATH_RE.find_iter(&assistant_text_this_event) {
                                        let filepath = cap.as_str().to_string();
                                        let is_artifact = is_pipeline_artifact(&filepath);
                                        let is_handoff = is_handoff_file(&filepath);
                                        if !is_artifact && !is_handoff {
                                            continue;
                                        }

                                        // Collect workflow advancement info under write lock, call persistence after.
                                        let workflow_advance = {
                                            let mut active_guard = active.write().await;
                                            let mut wf_advance: Option<(Uuid, WorkflowStage, String)> = None;
                                            if let Some(tracked) = active_guard.get_mut(&session_id) {
                                                if needs_artifact
                                                    && is_artifact
                                                    && tracked.pipeline_artifact.is_none()
                                                {
                                                    tracing::info!(
                                                        session_id = %session_id,
                                                        filepath = %filepath,
                                                        "Detected pipeline artifact from assistant text"
                                                    );
                                                    // Only set pipeline_artifact for non-workflow sessions (legacy compat).
                                                    if tracked.session.workflow_id.is_none() {
                                                        tracked.pipeline_artifact = Some(filepath.clone());
                                                        tracked.session.pipeline_artifact = Some(filepath.clone());
                                                    }

                                                    if let Some(wf_id) = tracked.session.workflow_id
                                                        && let Some(stage) =
                                                            workflow_stage_for_pipeline_artifact(&filepath)
                                                    {
                                                        wf_advance = Some((wf_id, stage, filepath.clone()));
                                                    }
                                                }
                                                if needs_handoff
                                                    && is_handoff
                                                    && assistant_text_declares_handoff_path(
                                                        &assistant_text_this_event,
                                                        &filepath,
                                                    )
                                                {
                                                    tracing::info!(
                                                        session_id = %session_id,
                                                        filepath = %filepath,
                                                        "Detected handoff filepath from assistant text"
                                                    );
                                                    let action = tracked.rotation.advance(
                                                        RotationEvent::HandoffFileDetected {
                                                            path: filepath.clone(),
                                                        },
                                                    );
                                                    debug_assert!(matches!(action, RotationAction::NoOp));
                                                }
                                            }
                                            wf_advance
                                        }; // write lock dropped

                                        if let Some(advance) = workflow_advance {
                                            workflow_advances.push(advance);
                                        }
                                    }

                                    for (wf_id, stage, fp) in workflow_advances {
                                        if let Err(e) =
                                            persistence.update_workflow_stage(wf_id, stage, Some(fp)).await
                                        {
                                            tracing::warn!(error = %e, workflow_id = %wf_id, "Failed to advance workflow stage on artifact detection");
                                        } else {
                                            tracing::info!(workflow_id = %wf_id, ?stage, "Advanced workflow stage on artifact detection (assistant text)");
                                        }
                                    }
                                }

                                // ── /spawn_child directive scan ──
                                // Iterate every `<docregblock>/spawn_child …</docregblock>` block
                                // anchored at line start in the accumulated assistant text. The
                                // SpawnCoordinator validates lead-emitter identity, recursion
                                // depth, and per-Epic rate limits, then enqueues a SpawnRequest
                                // for the daemon main loop to launch via `launch_session`.
                                //
                                // Reliability: malformed directives log a warn and are dropped;
                                // they never crash the monitor loop. Non-lead emitters log at
                                // debug only — there is no user-facing error.
                                for caps in
                                    SPAWN_DIRECTIVE_RE.captures_iter(&accumulated_assistant_content)
                                {
                                    let block = caps.get(0).map(|m| m.as_str()).unwrap_or("");
                                    let block_hash = {
                                        let mut hasher =
                                            std::collections::hash_map::DefaultHasher::new();
                                        block.hash(&mut hasher);
                                        hasher.finish()
                                    };
                                    match SpawnDirective::parse(block) {
                                        Ok(Some(directive)) => {
                                            let kind = directive.kind;
                                            let state = spawn_coordinator
                                                .handle(
                                                    session_id,
                                                    block_hash,
                                                    directive,
                                                    &active,
                                                    &completed,
                                                    &store,
                                                )
                                                .await;
                                            match state {
                                                super::spawn_coordinator::SpawnState::Spawning {
                                                    epic_id,
                                                    ..
                                                } => {
                                                    tracing::info!(
                                                        emitter_id = %session_id,
                                                        epic_id = %epic_id,
                                                        ?kind,
                                                        "spawn_child directive enqueued"
                                                    );
                                                }
                                                super::spawn_coordinator::SpawnState::Rejected { reason } => {
                                                    tracing::debug!(
                                                        emitter_id = %session_id,
                                                        ?kind,
                                                        rejection = %reason,
                                                        "spawn_child directive rejected"
                                                    );
                                                }
                                                _ => {}
                                            }
                                        }
                                        Ok(None) => {
                                            // Block opened with `<docregblock>` but the header
                                            // was not `/spawn_child` — ignored, handled by other
                                            // directive scanners (e.g. handoff).
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                target: "spawn_directive",
                                                emitter_id = %session_id,
                                                error = %e,
                                                "malformed /spawn_child directive — ignored"
                                            );
                                        }
                                    }
                                }
                            }

                            // ── /halt directive scan (P1.11) ──────────────────
                            // Detect `<docregblock>/halt</docregblock>` blocks emitted
                            // by lead sessions to signal loop termination. Publishes
                            // DaemonEvent::HaltDirective so UntilEvaluator can stop
                            // iterating when UntilCondition::LeadHalt is active.
                            if HALT_DIRECTIVE_RE.is_match(&accumulated_assistant_content) {
                                tracing::info!(
                                    session_id = %session_id,
                                    "detected /halt directive in assistant content — publishing HaltDirective"
                                );
                                event_bus.publish(DaemonEvent::HaltDirective {
                                    session_id,
                                    directive: "/halt".to_string(),
                                });
                            }

                            // V99/P1-B: account-level plan-window utilization.
                            // Previously this event fell through to the stream
                            // loop's catch-all and was discarded, so the single
                            // highest-value signal for a multi-session manager
                            // was invisible. It produces no ConversationEvent —
                            // this is telemetry, not conversation.
                            if let Some(provider) = {
                                let active_guard = active.read().await;
                                active_guard.get(&session_id).and_then(|tracked| {
                                    (tracked.spawn_generation == expected_generation)
                                        .then_some(tracked.session.provider)
                                })
                            } && let Some(snapshot) =
                                parse_rate_limit_event(provider, &stream_event)
                            {
                                // Written through `run_store_op` rather than
                                // the PersistenceHandle: the handle's barrier
                                // orders writes to a session row, and this is
                                // account-scoped data that needs no such fence.
                                //
                                // This awaits the daemon-wide store mutex on
                                // the stream loop, which reads like an obvious
                                // candidate for deferral. It is not, and the
                                // reasoning is recorded here so it is not
                                // re-litigated: the PersistenceHandle worker
                                // calls this same `run_store_op` for EVERY
                                // command it drains (`persistence.rs:601+`), so
                                // routing this through the queue would add a
                                // hop and FIFO ordering behind conversation
                                // writes without removing one microsecond of
                                // lock contention. Coalescing unchanged
                                // snapshots would stop refreshing `observed_at`
                                // and turn a freshness reading into a
                                // last-changed one. A detached write would drop
                                // both backpressure and the error path below.
                                // Taking this off the mutex means a real
                                // persistence decision (a dedicated telemetry
                                // writer or a batched flush) that every other
                                // daemon store write would want too.
                                let snapshot_for_store = snapshot.clone();
                                if let Err(error) = super::persistence::run_store_op(
                                    store.clone(),
                                    move |store| {
                                        store.upsert_provider_rate_limit_snapshot(
                                            &snapshot_for_store,
                                            Some(session_id),
                                        )
                                    },
                                )
                                .await
                                {
                                    // Telemetry: a failed write must never take
                                    // the session down.
                                    tracing::warn!(
                                        session_id = %session_id,
                                        error = %error,
                                        "Failed to persist provider rate-limit snapshot"
                                    );
                                }
                                // Publish regardless of the write outcome: the
                                // live TUI reading is worth having even if the
                                // durable copy failed.
                                event_bus.publish(DaemonEvent::ProviderRateLimitUpdated {
                                    snapshot,
                                });
                            }

                            // Pipeline artifact & handoff detection -- fires on ALL sessions, not just rotation sessions.
                            // Detects when model writes to a known pipeline directory using ANY tool.
                            if stream_event.event_type == "tool_use" {
                                let (needs_artifact, needs_handoff) = {
                                    let active_guard = active.read().await;
                                    let t_opt = active_guard.get(&session_id);
                                    let needs_a = t_opt.map(|t| t.pipeline_artifact.is_none()).unwrap_or(false);
                                    let needs_h = t_opt.map(|t| {
                                        t.rotation.is_rotating() &&
                                        !matches!(t.rotation.state(), RotationState::WritingHandoff { handoff_filepath: Some(_), .. })
                                    }).unwrap_or(false);
                                    (needs_a, needs_h)
                                };

                                if needs_artifact || needs_handoff {
                                    let json_str = serde_json::to_string(&stream_event.data).unwrap_or_default();
                                    let mut workflow_advances = Vec::new();
                                    for cap in PIPELINE_PATH_RE.find_iter(&json_str) {
                                        let filepath = cap.as_str().to_string();
                                        let is_artifact = is_pipeline_artifact(&filepath);
                                        let is_handoff = is_handoff_file(&filepath);
                                        if !is_artifact && !is_handoff {
                                            continue;
                                        }

                                        // Collect workflow advancement info under write lock, call persistence after.
                                        let workflow_advance = {
                                            let mut active_guard = active.write().await;
                                            let mut wf_advance: Option<(Uuid, WorkflowStage, String)> = None;
                                            if let Some(tracked) = active_guard.get_mut(&session_id) {
                                                if needs_artifact
                                                    && is_artifact
                                                    && tracked.pipeline_artifact.is_none()
                                                {
                                                    tracing::info!(
                                                        session_id = %session_id,
                                                        filepath = %filepath,
                                                        "Detected pipeline artifact from tool_use JSON"
                                                    );
                                                    // Only set pipeline_artifact for non-workflow sessions (legacy compat).
                                                    if tracked.session.workflow_id.is_none() {
                                                        tracked.pipeline_artifact = Some(filepath.clone());
                                                        tracked.session.pipeline_artifact = Some(filepath.clone());
                                                    }

                                                    if let Some(wf_id) = tracked.session.workflow_id
                                                        && let Some(stage) =
                                                            workflow_stage_for_pipeline_artifact(&filepath)
                                                    {
                                                        wf_advance = Some((wf_id, stage, filepath.clone()));
                                                    }
                                                }
                                                if needs_handoff
                                                    && is_handoff
                                                    && tool_use_can_create_path(
                                                        &stream_event.data,
                                                        &filepath,
                                                    )
                                                {
                                                    tracing::info!(
                                                        session_id = %session_id,
                                                        filepath = %filepath,
                                                        "Detected handoff filepath from tool_use JSON"
                                                    );
                                                    let action = tracked.rotation.advance(
                                                        RotationEvent::HandoffFileDetected {
                                                            path: filepath.clone(),
                                                        },
                                                    );
                                                    debug_assert!(matches!(action, RotationAction::NoOp));
                                                }
                                            }
                                            wf_advance
                                        }; // write lock dropped

                                        if let Some(advance) = workflow_advance {
                                            workflow_advances.push(advance);
                                        }
                                    }

                                    for (wf_id, stage, fp) in workflow_advances {
                                        if let Err(e) =
                                            persistence.update_workflow_stage(wf_id, stage, Some(fp)).await
                                        {
                                            tracing::warn!(error = %e, workflow_id = %wf_id, "Failed to advance workflow stage on artifact detection");
                                        } else {
                                            tracing::info!(workflow_id = %wf_id, ?stage, "Advanced workflow stage on artifact detection (tool_use)");
                                        }
                                    }
                                }
                            }

                            // Handle result event (turn or session complete)
                            if stream_event.event_type == "result" {
                                let result = result_evidence(&stream_event);
                                let session_model = active
                                    .read()
                                    .await
                                    .get(&session_id)
                                    .filter(|tracked| {
                                        tracked.spawn_generation == expected_generation
                                    })
                                    .and_then(|tracked| tracked.session.model.clone());
                                let meta = monitor::extract_result_metadata(
                                    &stream_event,
                                    session_model.as_deref(),
                                );
                                let runtime_budget_changed = if let Some(ctx_window) =
                                    meta.context_window
                                {
                                    match persist_runtime_context_observation(
                                        &active,
                                        &persistence,
                                        &store,
                                        session_id,
                                        expected_generation,
                                        ctx_window,
                                        chrono::Utc::now(),
                                    )
                                    .await
                                    {
                                        Ok(Some((_, changed))) => changed,
                                        Ok(None) => false,
                                        Err(error) => {
                                            tracing::warn!(
                                                session_id = %session_id,
                                                %error,
                                                "Failed to durably persist result context evidence"
                                            );
                                            false
                                        }
                                    }
                                } else {
                                    false
                                };
                                let working_dir = {
                                    let mut active_guard = active.write().await;
                                    let mut wd: Option<std::path::PathBuf> = None;
                                    if let Some(tracked) = active_guard.get_mut(&session_id) {
                                        // The session's own model selects the
                                        // right `modelUsage` entry: a turn that
                                        // ran a subagent reports one entry per
                                        // model, and picking the wrong one binds
                                        // the wrong context window (G-005).
                                        let meta = monitor::extract_result_metadata(
                                            &stream_event,
                                            tracked.session.model.as_deref(),
                                        );
                                        if let Some(duration) = meta.duration_ms {
                                            tracked.session.duration_ms = Some(duration);
                                        }
                                        if let Some(cost) = meta.cost_usd {
                                            tracked.session.cost_usd = Some(cost);
                                        }
                                        if let Some(turns) = meta.num_turns {
                                            tracked.session.num_turns = Some(turns);
                                        }
                                        if let Some(input) = meta.final_input_tokens {
                                            if is_codex_context_provider(tracked.session.provider) {
                                                tracked.session.input_tokens = has_codex_context_observation(tracked)
                                                    .then_some(tracked.codex_context_tokens);
                                            } else {
                                                tracked.session.input_tokens = Some(input);
                                            }
                                            tracked.session.context_usage_confidence = if is_codex_context_provider(tracked.session.provider) {
                                                live_context_state(tracked).2
                                            } else {
                                                ContextUsageConfidence::Full
                                            };
                                        }
                                        if let Some(output) = meta.final_output_tokens {
                                            if !is_codex_context_provider(tracked.session.provider) {
                                                tracked.session.output_tokens = Some(output);
                                            }
                                        }
                                        if let Some(reason) = meta.stop_reason {
                                            tracked.session.stop_reason = Some(reason);
                                        }
                                        // V99/P1-C: richer usage capture. Each
                                        // field is applied only when the result
                                        // actually reported it, so an absent
                                        // counter stays `None` (unmeasured)
                                        // rather than being written as a 0.
                                        if let Some(thinking) = meta.thinking_tokens {
                                            tracked.session.thinking_tokens = Some(thinking);
                                        }
                                        if let Some(tier) = meta.service_tier {
                                            tracked.session.service_tier = Some(tier);
                                        }
                                        if let Some(tokens) = meta.cache_creation_1h_tokens {
                                            tracked.session.cache_creation_1h_tokens = Some(tokens);
                                        }
                                        if let Some(tokens) = meta.cache_creation_5m_tokens {
                                            tracked.session.cache_creation_5m_tokens = Some(tokens);
                                        }
                                        if let Some(count) = meta.permission_denial_count {
                                            tracked.session.permission_denial_count = Some(count);
                                        }
                                        if let Some(stats) = meta.subagent_stats_json {
                                            tracked.session.subagent_stats_json = Some(stats);
                                        }
                                        if let Some(queued) = meta.queued_turn_count {
                                            tracked.session.queued_turn_count = Some(queued);
                                        }
                                        if let Some(reason) = meta.terminal_reason {
                                            tracked.session.terminal_reason = Some(reason);
                                        }
                                        wd = Some(tracked.session.working_dir.clone());
                                    }
                                    wd
                                };

                                if runtime_budget_changed {
                                    let context_update = {
                                        let mut active_guard = active.write().await;
                                        if let Some(tracked) = active_guard
                                            .get_mut(&session_id)
                                            .filter(|tracked| {
                                                tracked.spawn_generation == expected_generation
                                            })
                                        {
                                            let (numerator, pct, confidence, daemon_total, _) =
                                                live_context_state(tracked);
                                            let budget = tracked.context_budget();
                                            advance_context_rotation_threshold(
                                                tracked,
                                                session_id,
                                                pct,
                                                &persistence,
                                            )
                                            .await;
                                            Some((
                                                numerator,
                                                pct,
                                                confidence,
                                                daemon_total,
                                                tracked.session.output_tokens.unwrap_or(0),
                                                budget,
                                            ))
                                        } else {
                                            None
                                        }
                                    };
                                    if let Some((
                                        numerator,
                                        pct,
                                        confidence,
                                        daemon_total,
                                        output_tokens,
                                        budget,
                                    )) = context_update
                                    {
                                        monitor::publish_context_usage(
                                            &event_bus,
                                            session_id,
                                            pct,
                                            numerator,
                                            output_tokens,
                                            daemon_total,
                                            confidence,
                                            budget,
                                        );
                                    }
                                }

                                // A pre-compaction flush is an additional provider turn, so it
                                // can only start at this successful idle boundary. It runs before
                                // the ordinary continuation policy is advanced; the flush result
                                // returns through this same branch, where the tracked compaction
                                // count suppresses recursion and the original policy resumes.
                                if terminal_reason.is_none()
                                    && matches!(result, TerminalResult::Success)
                                {
                                    match maybe_start_memory_flush_turn(
                                        provider_session.as_mut(),
                                        &active,
                                        session_id,
                                        expected_generation,
                                        &memory_flush_settings,
                                        &mut memory_flush_attempted_compaction_count,
                                    )
                                    .await
                                    {
                                        Ok(true) => {
                                            turn_outcome = TerminalTurnOutcome::Continued;
                                            continue;
                                        }
                                        Ok(false) => {}
                                        Err(error) => {
                                            // A flush is protective, not terminal truth. The
                                            // attempt fence prevents retry amplification, while
                                            // the successful user turn continues through its
                                            // normal continuation/finalization path.
                                            tracing::warn!(
                                                session_id = %session_id,
                                                %error,
                                                "Failed to start pre-compaction memory-flush turn"
                                            );
                                            event_bus.publish(DaemonEvent::SystemMessage {
                                                level: "warn".to_string(),
                                                message: format!(
                                                    "Pre-compaction memory flush could not start for session {session_id}: {error}"
                                                ),
                                            });
                                        }
                                    }
                                }

                                // Phase 7: Multi-turn continuation for app-server sessions.
                                // If the provider supports multi-turn and the policy allows more
                                // turns, start the next turn instead of completing the session.
                                if terminal_reason.is_none()
                                    && matches!(result, TerminalResult::Success)
                                    && supports_multi_turn
                                    && turn_controller.turn_completed()
                                {
                                    // ── P2-04 / P2-05b / C-P2-08: idle-boundary delivery ──
                                    //
                                    // The monitor that owns `ProviderSession` is the sole
                                    // idle-boundary arbiter. This is that boundary: a
                                    // successful native result on a multi-turn provider,
                                    // the one place a message turn can be admitted without
                                    // interrupting a provider turn.
                                    //
                                    // BLAST RADIUS. This block sits in a file that runs for
                                    // every session of every provider, but the enclosing
                                    // condition already gates it on `supports_multi_turn`,
                                    // which is `false` by default and `true` for exactly one
                                    // type, `CodexAppServerSession`. Claude, Codex/Pioneer
                                    // CLI, Antigravity, Local, Harness, and any AppServer
                                    // session without mail take a byte-identical path to before.
                                    //
                                    // A grant taken here is now CONSUMED rather than
                                    // released undelivered, and on a successful dispatch it
                                    // is HELD for the duration of the delivered turn. That
                                    // hold is the property C-P2-08 names: while it stands,
                                    // this logical root cannot be granted again, so a
                                    // concurrent dispatcher tick sees `GrantOutstanding`
                                    // instead of planning a duplicate delivery.
                                    let mut delivery = None;
                                    {
                                        // Release the PREVIOUS turn's grant before deciding,
                                        // or the arbiter would hold this root back against
                                        // itself. The delivered turn is over: we are at its
                                        // result boundary.
                                        if let Some(previous) = outstanding_message_grant.take() {
                                            previous.release();
                                        }
                                        // The store guard is scoped to this block and
                                        // dropped before any provider effect. It is
                                        // structurally impossible to hold it across the
                                        // `start_turn` below, because `decide_next_boundary`
                                        // is a plain `fn` and its result borrows nothing
                                        // from the store.
                                        let decision = {
                                            let store_guard = store.lock().await;
                                            super::agent_message_arbiter::decide_next_boundary(
                                                &store_guard,
                                                &agent_message_arbiter,
                                                session_id,
                                                expected_generation,
                                            )
                                        };
                                        match decision {
                                            Ok(super::agent_message_arbiter::BoundaryDecision::DeliverMail(grant)) => {
                                                tracing::info!(
                                                    session_id = %session_id,
                                                    message_id = %grant.message_id(),
                                                    logical_root_session_id = %grant.logical_root_session_id(),
                                                    "Agent message is eligible at this idle boundary; \
                                                     admitting and dispatching it as the next turn"
                                                );
                                                let outcome = super::agent_message_delivery::deliver_at_idle_boundary(
                                                    &store,
                                                    &event_bus,
                                                    &model_call_settlements,
                                                    &active,
                                                    session_id,
                                                    &grant,
                                                    provider_session.as_mut(),
                                                )
                                                .await;
                                                // The hold decision lives in
                                                // `retain_or_release` so it is a test's own
                                                // subject rather than a decision reproduced
                                                // inside a test (H21-P2-R5-003).
                                                outstanding_message_grant =
                                                    retain_or_release(&outcome, grant);
                                                delivery = Some(outcome);
                                            }
                                            Ok(super::agent_message_arbiter::BoundaryDecision::SyntheticContinuation(
                                                declined,
                                            )) => {
                                                tracing::trace!(
                                                    session_id = %session_id,
                                                    ?declined,
                                                    "No agent message granted at idle boundary"
                                                );
                                            }
                                            Err(error) => {
                                                // A failed snapshot must never convert a
                                                // healthy turn boundary into a terminal one:
                                                // no invocation was admitted, no claim was
                                                // attempted, and no byte reached a provider,
                                                // so falling through is provably safe.
                                                tracing::warn!(
                                                    session_id = %session_id,
                                                    error = %error,
                                                    "Idle-boundary mail snapshot failed; \
                                                     continuing with the synthetic continuation"
                                                );
                                            }
                                        }
                                    }

                                    // A delivered message turn IS this session's next turn,
                                    // so the synthetic continuation below must not also run.
                                    // An effect-possible failure must not run it either: the
                                    // plan forbids an effect-possible attempt from starting
                                    // a continuation, because the provider may already be
                                    // acting on the message.
                                    match delivery {
                                        Some(super::agent_message_delivery::IdleBoundaryDelivery::Dispatched) => {
                                            // Same stall-detector reset the synthetic
                                            // continuation performs, for the same reason.
                                            {
                                                let mut active_guard = active.write().await;
                                                if let Some(tracked) = active_guard.get_mut(&session_id) {
                                                    tracked.last_event_at = chrono::Utc::now();
                                                }
                                            }
                                            turn_outcome = TerminalTurnOutcome::Continued;
                                            continue;
                                        }
                                        Some(super::agent_message_delivery::IdleBoundaryDelivery::EffectPossibleTerminal {
                                            error_class,
                                        }) => {
                                            tracing::warn!(
                                                session_id = %session_id,
                                                error_class,
                                                "Agent message dispatch left effect possible; \
                                                 settling terminal without a continuation"
                                            );
                                            // Exactly the terminal state the
                                            // `start_turn` failure branch below sets. The
                                            // `continue` is what skips the synthetic
                                            // continuation; that branch reaches the same
                                            // place by simply having no code after it.
                                            current_result = TerminalResult::ProviderError;
                                            turn_outcome = TerminalTurnOutcome::Terminal;
                                            terminal_reason = Some(MonitorBreakReason::Result);
                                            terminal_deadline = Some(tokio::time::Instant::now());
                                            continue;
                                        }
                                        Some(super::agent_message_delivery::IdleBoundaryDelivery::FellBackToSyntheticContinuation {
                                            reason,
                                        }) => {
                                            tracing::debug!(
                                                session_id = %session_id,
                                                reason,
                                                "No agent message was delivered at this idle \
                                                 boundary; running the synthetic continuation"
                                            );
                                        }
                                        None => {}
                                    }

                                    let continuation_input = format!(
                                        "Turn {} complete. Continue with the task.",
                                        turn_controller.current_turn()
                                    );
                                    let turn_config = crate::provider::TurnConfig {
                                        input: continuation_input,
                                        working_dir: working_dir.clone(),
                                    };
                                    tracing::info!(
                                        session_id = %session_id,
                                        turn = turn_controller.current_turn(),
                                        "Starting continuation turn"
                                    );
                                    // Reset stall detector timestamp to avoid false stall on inter-turn gap
                                    {
                                        let mut active_guard = active.write().await;
                                        if let Some(tracked) = active_guard.get_mut(&session_id) {
                                            tracked.last_event_at = chrono::Utc::now();
                                        }
                                    }
                                    if let Err(e) = provider_session.start_turn(&turn_config).await {
                                        tracing::warn!(
                                            session_id = %session_id,
                                            error = %e,
                                            "Failed to start continuation turn; settling terminal provider error"
                                        );
                                        current_result = TerminalResult::ProviderError;
                                        turn_outcome = TerminalTurnOutcome::Terminal;
                                        terminal_reason = Some(MonitorBreakReason::Result);
                                        terminal_deadline = Some(tokio::time::Instant::now());
                                    } else {
                                        turn_outcome = TerminalTurnOutcome::Continued;
                                        // Continue the loop for the new turn.
                                        continue;
                                    }
                                } else {
                                    current_result = result;
                                    turn_outcome = TerminalTurnOutcome::Terminal;
                                    terminal_reason.get_or_insert(MonitorBreakReason::Result);
                                    terminal_deadline = Some(
                                        if supports_multi_turn
                                            || matches!(result, TerminalResult::ProviderError)
                                        {
                                            tokio::time::Instant::now()
                                        } else {
                                            tokio::time::Instant::now()
                                                + super::reaper::TERMINAL_DRAIN_GRACE
                                        },
                                    );
                                }
                            }
                        }
                        None => {
                            // Stream closed
                            if !received_any_event {
                                tracing::warn!(session_id = %session_id, "Event stream closed with zero events — process likely failed on startup (check stderr)");
                            } else {
                                tracing::debug!(session_id = %session_id, "Event stream closed");
                            }
                            stream_drained = true;
                            if terminal_reason.is_none()
                                && matches!(turn_outcome, TerminalTurnOutcome::Continued)
                            {
                                // A continuation was successfully started, but
                                // its later stream closure is a new terminal
                                // boundary rather than the prior turn boundary.
                                turn_outcome = TerminalTurnOutcome::Terminal;
                            }
                            let reason = *terminal_reason
                                .get_or_insert(MonitorBreakReason::StreamClosed);
                            if !settlement_owner.is_running()
                                && !settlement_outcome
                                    .is_some_and(ProcessSettlementOutcome::is_settled)
                            {
                                let natural_grace = terminal_deadline
                                    .take()
                                    .map(|deadline| {
                                        deadline.saturating_duration_since(
                                            tokio::time::Instant::now(),
                                        )
                                    })
                                    .unwrap_or(super::reaper::TERMINAL_DRAIN_GRACE);
                                settlement_owner.start(
                                    active.clone(),
                                    session_id,
                                    expected_generation,
                                    settlement_mode(reason, supports_multi_turn),
                                    natural_grace,
                                );
                                settlement_outcome = None;
                            }
                        }
                    }
                }
            }
        };

        // ── Pre-finalization: flush monitor-local state and capture coordinator action ──
        // The coordinator must be queried BEFORE finalize_session() removes
        // the TrackedSession from the active map.
        let exit_code_for_retry: Option<i32>;
        // Override break reason for stall-triggered interrupts so they are eligible for retry.
        let break_reason = {
            let active_guard = active.read().await;
            if let Some(tracked) = active_guard.get(&session_id) {
                if tracked.stall_interrupted
                    && matches!(break_reason, MonitorBreakReason::Interrupted)
                {
                    MonitorBreakReason::StallTimeout
                } else {
                    break_reason
                }
            } else {
                break_reason
            }
        };
        let (rotation_action, rotation_id_for_log, evidence) = {
            let mut active_guard = active.write().await;
            match active_guard.get_mut(&session_id) {
                Some(t) if t.spawn_generation == expected_generation => {
                    // Flush monitor-local flags before the single decision
                    // consumes the matching tracked generation.
                    t.received_meaningful_output = received_meaningful_output;
                    let process_handle_present = t.process.is_some();
                    let process_alive = t
                        .process
                        .as_mut()
                        .is_some_and(super::types::ProviderProcess::is_alive);
                    if !process_alive && let Some(process) = t.process.as_mut() {
                        t.exit_code = process.try_exit_status();
                    }
                    exit_code_for_retry = t.exit_code;
                    let action = t
                        .rotation
                        .advance(RotationEvent::MonitorCompleted { break_reason });
                    capture_spawn_child_handoff(&mut t.session, &action);
                    let rid = t.rotation.rotation_id().map(|s| s.to_string());
                    let rotation_action = if matches!(action, RotationAction::NoOp) {
                        TerminalRotationAction::None
                    } else {
                        TerminalRotationAction::PostFinalize
                    };
                    let evidence = TerminalEvidence {
                        expected_generation,
                        active_generation: Some(t.spawn_generation),
                        break_reason,
                        received_any_event,
                        received_meaningful_output,
                        prior_meaningful_output,
                        current_result,
                        process_handle_present,
                        process_alive,
                        exit_code: t.exit_code,
                        stream_drained,
                        settlement: settlement_outcome
                            .unwrap_or(ProcessSettlementOutcome::EscalationFailed),
                        supports_multi_turn,
                        turn_outcome,
                        pending_archive: t.pending_archive,
                        stall_interrupted: t.stall_interrupted,
                        interrupt_requested: t.interrupt_requested,
                        pending_question: t.pending_question.is_some(),
                        rotation_action,
                    };
                    (action, rid, evidence)
                }
                maybe_tracked @ (Some(_) | None) => {
                    exit_code_for_retry = None;
                    (
                        RotationAction::NoOp,
                        None,
                        TerminalEvidence {
                            expected_generation,
                            active_generation: maybe_tracked
                                .as_deref()
                                .map(|tracked| tracked.spawn_generation),
                            break_reason,
                            received_any_event,
                            received_meaningful_output,
                            prior_meaningful_output,
                            current_result,
                            process_handle_present: false,
                            process_alive: false,
                            exit_code: None,
                            stream_drained,
                            settlement: ProcessSettlementOutcome::GenerationChanged,
                            supports_multi_turn,
                            turn_outcome,
                            pending_archive: false,
                            stall_interrupted: false,
                            interrupt_requested: false,
                            pending_question: false,
                            rotation_action: TerminalRotationAction::None,
                        },
                    )
                }
            }
        };
        let mut c5_disposition =
            crate::store::daemon_settings::RecoveryDisposition::NoRecoverySource;
        let finalize_decision = match terminal_decision(evidence) {
            TerminalDecision::StaleGeneration => {
                tracing::warn!(
                    session_id = %session_id,
                    expected_generation,
                    actual_generation = ?evidence.active_generation,
                    "Skipping stale terminal decision for newer active incarnation"
                );
                return;
            }
            TerminalDecision::RetainRunning(reason) => {
                tracing::error!(
                    session_id = %session_id,
                    ?reason,
                    "Terminal evidence was not settled; retaining active session"
                );
                return;
            }
            TerminalDecision::Finalize(decision) => {
                let mut active_guard = active.write().await;
                let Some(tracked) = active_guard.get_mut(&session_id) else {
                    return;
                };
                if tracked.spawn_generation != expected_generation {
                    return;
                }
                apply_terminal_handoff_order(decision, evidence, tracked)
            }
        };
        let Some(finalized_decision) = Self::finalize_session(
            session_id,
            expected_generation,
            finalize_decision,
            active.clone(),
            completed.clone(),
            event_bus.clone(),
            store.clone(),
            persistence.clone(),
            memory_handle.clone(),
            runtime_config.clone(),
        )
        .await
        else {
            return;
        };

        // A lifecycle intent that arrived at the atomic removal boundary wins
        // the original monitor/rotation decision. Do not run a stale rotation
        // side effect after an archive, stall, interrupt, or question override.
        let post_finalize_outcome = if finalized_decision == finalize_decision {
            Self::execute_post_finalization_rotation_action(
                session_id,
                rotation_action,
                rotation_id_for_log,
                active.clone(),
                completed.clone(),
                event_bus.clone(),
                store.clone(),
                model_call_settlements,
                persistence.clone(),
                context_rotation_enabled,
                socket_path,
                counter,
                memory_handle,
                retry_tx.clone(),
                runtime_config.clone(),
                spawn_coordinator.clone(),
                agent_tokens,
                spawn_epoch,
                agent_message_arbiter,
                codegraph_handle,
                custody_runtime,
            )
            .await
        } else {
            PostFinalizeRotationOutcome::ContinueNormalCompletion
        };

        if matches!(
            post_finalize_outcome,
            PostFinalizeRotationOutcome::ContinueNormalCompletion
        ) {
            if let Err(error) = crate::closure_kernel::ingress::capture_for_terminal_session(
                Arc::clone(&store),
                session_id,
            )
            .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "Closure live terminal-output capture deferred to bounded reconciliation"
                );
            }

            // Normal end — check retry eligibility.
            let retry_info = {
                let completed_guard = completed.read().await;
                completed_guard.get(&session_id).map(|cs| {
                    (
                        cs.session.status,
                        cs.session.session_kind,
                        cs.session.max_retries.unwrap_or(0),
                        cs.session.retry_attempt.unwrap_or(0),
                    )
                })
            };

            if let Some((SessionStatus::Failed, session_kind, max_retries, current_attempt)) =
                retry_info
            {
                if max_retries > 0 && current_attempt >= max_retries {
                    c5_disposition =
                        crate::store::daemon_settings::RecoveryDisposition::BudgetExhausted;
                }
                if super::retry_policy::session_retries_allowed(
                    &runtime_config,
                    session_kind,
                    Some(max_retries),
                ) && current_attempt < max_retries
                {
                    // Get events for classification
                    let events = {
                        let g = completed.read().await;
                        g.get(&session_id)
                            .map(|cs| cs.events.clone())
                            .unwrap_or_default()
                    };
                    let reason = classify_retry_eligibility(
                        &break_reason,
                        received_any_event,
                        received_meaningful_output,
                        exit_code_for_retry,
                        &events,
                    );

                    if let Some(retry_reason) = reason {
                        let next_attempt = current_attempt + 1;
                        let max_backoff = runtime_config
                            .retry_max_backoff_ms
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let delay = backoff_ms(next_attempt, max_backoff);

                        tracing::info!(
                            session_id = %session_id,
                            attempt = next_attempt,
                            max_retries = max_retries,
                            backoff_ms = delay,
                            reason = %retry_reason,
                            "Scheduling retry"
                        );

                        event_bus.publish(crate::bus::DaemonEvent::SessionRetrying {
                            session_id,
                            attempt: next_attempt,
                            max_retries,
                            backoff_ms: delay,
                            reason: retry_reason,
                        });

                        // Create cancellation channel
                        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
                        {
                            let mut g = completed.write().await;
                            if let Some(cs) = g.get_mut(&session_id) {
                                cs.retry_cancel = Some(cancel_tx);
                                cs.retry_fired_at = None;
                                cs.session.retry_attempt = Some(next_attempt);
                            }
                        }

                        // Persist retry state so it survives daemon restarts
                        if let Err(e) = persistence
                            .update_retry_state(session_id, Some(next_attempt), Some(max_retries))
                            .await
                        {
                            tracing::warn!(
                                session_id = %session_id,
                                error = %e,
                                "Failed to persist retry state"
                            );
                        }

                        spawn_retry_timer(
                            completed.clone(),
                            retry_tx.clone(),
                            session_id,
                            delay,
                            cancel_rx,
                            "Retry cancelled",
                        );
                    } else {
                        // A retry budget was available but classification
                        // deliberately declined recovery.
                        c5_disposition =
                            crate::store::daemon_settings::RecoveryDisposition::PolicyDeclined;
                    }
                }
            }
        }

        // C5 runs only after rotation and retry disposition has installed its
        // live timer/queue marker. The no-idle service exact-reads the
        // daemon-owned program sentinel before interpreting assistant prose;
        // the guarded C5 service then rechecks persisted and map state and
        // leaves deferred journal rows intact.
        let control = crate::session::agent_verbs::AgentControlHandle::new(
            active,
            completed,
            store,
            event_bus,
            spawn_coordinator,
        );
        let no_idle_outcome = match control
            .enforce_master_no_idle_for_invocation(
                session_id,
                sequence,
                current_model_invocation_id,
                &accumulated_assistant_content,
            )
            .await
        {
            Ok(outcome) => Some(outcome),
            Err(error) => {
                tracing::error!(
                    session_id = %session_id,
                    sequence,
                    error = %error,
                    "master-orchestrate no-idle terminal check failed"
                );
                None
            }
        };
        let capacity_owned_c5 = no_idle_outcome_owns_capacity_c5(no_idle_outcome.as_ref());
        if !capacity_owned_c5 {
            control
                .maybe_autofile_terminal_failure(session_id, c5_disposition)
                .await;
        }
    }

    /// Create a synthetic ConversationEvent for the user's query.
    pub(super) fn create_user_event(
        session_id: Uuid,
        sequence: i32,
        query: &str,
    ) -> ConversationEvent {
        ConversationEvent {
            id: 0, // Placeholder -- DB assigns real ID
            session_id,
            sequence,
            event_type: EventType::Message,
            role: Some(Role::User),
            content: query.to_string(),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    /// Run background summarization for a session.
    /// Called from a detached tokio::spawn so it never blocks the monitor event loop.
    async fn run_summarization(
        session_id: Uuid,
        query: &str,
        current_sequence: i32,
        do_short: bool,
        do_long: bool,
        store: Arc<tokio::sync::Mutex<Store>>,
        persistence: PersistenceHandle,
        event_bus: Arc<crate::bus::EventBus>,
        runtime_config: Arc<crate::config::RuntimeConfig>,
    ) {
        use super::summarizer;
        use rsi_common::types::{SessionSummary, SummaryKind};

        tracing::info!(
            session_id = %session_id,
            do_short,
            do_long,
            current_sequence,
            "Starting background summarization"
        );

        // Load previous summaries and recent events from the store
        let (prev_short, prev_long, events) = {
            let store_guard = store.lock().await;
            let short = store_guard
                .get_latest_summary(session_id, SummaryKind::Short)
                .ok()
                .flatten();
            let long = store_guard
                .get_latest_summary(session_id, SummaryKind::Long)
                .ok()
                .flatten();
            // Load events since the last relevant summary for context
            let since_seq = if do_long {
                long.as_ref().map(|s| s.covers_through_sequence)
            } else {
                short.as_ref().map(|s| s.covers_through_sequence)
            };
            let events = store_guard
                .load_events_since(session_id, since_seq)
                .unwrap_or_default();
            (short, long, events)
        };

        if do_short {
            // For short summary, use events since last short summary
            let since_seq = prev_short.as_ref().map(|s| s.covers_through_sequence);
            let recent_events: Vec<_> = events
                .iter()
                .filter(|e| since_seq.map_or(true, |seq| e.sequence > seq))
                .cloned()
                .collect();

            let prev_content = prev_short.as_ref().map(|s| s.content.as_str());
            match summarizer::generate_short_summary(
                &store,
                &event_bus,
                session_id,
                prev_content,
                &recent_events,
                query,
                &runtime_config,
            )
            .await
            {
                Ok(content) => {
                    let token_count = summarizer::approx_token_count(&content);
                    let summary = SessionSummary {
                        id: 0,
                        session_id,
                        kind: SummaryKind::Short,
                        content: content.clone(),
                        covers_through_sequence: current_sequence,
                        token_count,
                        created_at: chrono::Utc::now(),
                    };
                    if let Err(e) = persistence.insert_session_summary(summary).await {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to persist short summary"
                        );
                    } else {
                        tracing::info!(
                            session_id = %session_id,
                            token_count,
                            "Short summary generated and persisted"
                        );
                        event_bus.publish(DaemonEvent::SessionSummaryUpdated {
                            session_id,
                            kind: SummaryKind::Short,
                            content,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        session_id = %session_id,
                        "Short summary generation failed (will retry at next threshold)"
                    );
                }
            }
        }

        if do_long {
            let prev_content = prev_long.as_ref().map(|s| s.content.as_str());
            match summarizer::generate_long_summary(
                &store,
                &event_bus,
                session_id,
                prev_content,
                &events,
                query,
                &runtime_config,
            )
            .await
            {
                Ok(content) => {
                    let token_count = summarizer::approx_token_count(&content);
                    let summary = SessionSummary {
                        id: 0,
                        session_id,
                        kind: SummaryKind::Long,
                        content: content.clone(),
                        covers_through_sequence: current_sequence,
                        token_count,
                        created_at: chrono::Utc::now(),
                    };
                    if let Err(e) = persistence.insert_session_summary(summary).await {
                        tracing::warn!(
                            error = %e,
                            session_id = %session_id,
                            "Failed to persist long summary"
                        );
                    } else {
                        tracing::info!(
                            session_id = %session_id,
                            token_count,
                            "Long summary generated and persisted"
                        );
                        event_bus.publish(DaemonEvent::SessionSummaryUpdated {
                            session_id,
                            kind: SummaryKind::Long,
                            content,
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        session_id = %session_id,
                        "Long summary generation failed (will retry at next threshold)"
                    );
                }
            }
        }
    }

    /// Convert a raw stream event to ConversationEvents, reporting whether the
    /// event's `type` was recognized at all.
    ///
    /// Returns a Vec because a single assistant stream event may contain both
    /// thinking blocks (emitted as EventType::Thinking) and text blocks
    /// (emitted as EventType::Message). Empty events are filtered out.
    ///
    /// `None` means the converter has no arm for `stream.event_type` — the
    /// caller emits the unrecognized-type diagnostic. `Some(vec![])` means the
    /// type IS recognized and simply carried nothing worth persisting; that
    /// case must stay silent.
    pub(super) fn convert_recognized_stream_event(
        stream: &StreamEvent,
        session_id: Uuid,
        sequence: &mut i32,
    ) -> Option<Vec<ConversationEvent>> {
        let event_type = match stream.event_type.as_str() {
            "assistant" => EventType::Message,
            "user" => {
                // CLI "user" events contain tool result echo-backs in verbose mode.
                // Extract tool_result blocks and emit them as ToolResult events.
                let content_value = stream.data.get("message").and_then(|m| m.get("content"));
                if let Some(arr) = content_value.and_then(|v| v.as_array()) {
                    let mut events = Vec::new();
                    for block in arr {
                        if block.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                            continue;
                        }
                        let tool_use_id = block
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                        let raw_content = block.get("content");

                        // The CLI emits tool_result content either as a bare
                        // string or as an array of content blocks. The array
                        // shape is the common one for Claude Code, and it used
                        // to be dropped on the floor here.
                        match raw_content.map(flatten_tool_content) {
                            Some(Some(flat)) => {
                                if !flat.text.is_empty() {
                                    *sequence += 1;
                                    events.push(ConversationEvent {
                                        id: 0,
                                        session_id,
                                        sequence: *sequence,
                                        event_type: EventType::ToolResult,
                                        role: None,
                                        content: flat.text,
                                        tool_name: None,
                                        tool_input: None,
                                        created_at: chrono::Utc::now(),
                                        offload_id: None,
                                        tool_use_id: tool_use_id.clone(),
                                        metadata: block
                                            .get("is_error")
                                            .and_then(|value| value.as_bool())
                                            .map(|is_error| {
                                                Box::new(
                                                    serde_json::json!({ "is_error": is_error }),
                                                )
                                            }),
                                    });
                                }
                                // Never drop a block we could not render as
                                // text: record it loudly so analysis can see
                                // exactly what arrived.
                                if !flat.unhandled.is_empty() {
                                    events.push(Self::unhandled_block_event(
                                        session_id,
                                        sequence,
                                        tool_use_id,
                                        "tool_result content blocks not renderable as text",
                                        &serde_json::Value::Array(flat.unhandled),
                                    ));
                                }
                            }
                            // Content present but in a shape we do not model.
                            Some(None) => {
                                let raw = raw_content.cloned().unwrap_or(serde_json::Value::Null);
                                events.push(Self::unhandled_block_event(
                                    session_id,
                                    sequence,
                                    tool_use_id,
                                    "tool_result content had unrecognized shape",
                                    &raw,
                                ));
                            }
                            // No `content` key at all -- still a real
                            // tool_result we cannot represent; do not vanish it.
                            None => {
                                events.push(Self::unhandled_block_event(
                                    session_id,
                                    sequence,
                                    tool_use_id,
                                    "tool_result block had no content field",
                                    block,
                                ));
                            }
                        }
                    }
                    return Some(events);
                }
                return Some(Vec::new());
            }
            "thinking" => EventType::Thinking,
            "tool_use" => EventType::ToolUse,
            "tool_result" => EventType::ToolResult,
            "system" => EventType::System,
            "parse_error" => EventType::System, // Surface parse errors as system events
            "result" => return Some(Vec::new()), // Handled separately
            _ => return None,
        };

        let role = stream
            .data
            .get("role")
            .or_else(|| stream.data.get("message").and_then(|m| m.get("role")))
            .and_then(|v| v.as_str())
            .and_then(|r| match r {
                "user" => Some(Role::User),
                "assistant" => Some(Role::Assistant),
                _ => None,
            });

        let content_value = stream
            .data
            .get("content")
            .or_else(|| stream.data.get("message").and_then(|m| m.get("content")));

        // For assistant/user events with content block arrays, separate thinking from text
        if event_type == EventType::Message {
            if let Some(v) = content_value {
                if let Some(arr) = v.as_array() {
                    let mut events = Vec::new();

                    // Extract thinking blocks
                    let thinking: String = arr
                        .iter()
                        .filter_map(|block| {
                            if block.get("type")?.as_str()? == "thinking" {
                                block.get("thinking")?.as_str().map(String::from)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    if !thinking.is_empty() {
                        *sequence += 1;
                        events.push(ConversationEvent {
                            id: 0,
                            session_id,
                            sequence: *sequence,
                            event_type: EventType::Thinking,
                            role,
                            content: thinking,
                            tool_name: None,
                            tool_input: None,
                            created_at: chrono::Utc::now(),
                            offload_id: None,
                            tool_use_id: None,
                            metadata: None,
                        });
                    }

                    // Extract text blocks
                    let text: String = arr
                        .iter()
                        .filter_map(|block| {
                            if block.get("type")?.as_str()? == "text" {
                                block.get("text")?.as_str().map(String::from)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    if !text.is_empty() {
                        *sequence += 1;
                        events.push(ConversationEvent {
                            id: 0,
                            session_id,
                            sequence: *sequence,
                            event_type: EventType::Message,
                            role,
                            content: text,
                            tool_name: None,
                            tool_input: None,
                            created_at: chrono::Utc::now(),
                            offload_id: None,
                            tool_use_id: None,
                            metadata: None,
                        });
                    }

                    // Extract tool_use blocks. The Claude Code CLI's stream-json format
                    // embeds tool calls as {"type":"tool_use","name":...,"input":...}
                    // blocks directly in the assistant message's content array (one per
                    // parallel tool call), rather than as their own top-level stream
                    // event. This branch previously handled only "thinking" and "text"
                    // block types and silently dropped every tool_use block here, which
                    // undercounted (often to zero) the ToolUse events backing the
                    // session-detail "N tool calls" group summary
                    // (`ui::height::count_tool_group_run`).
                    for block in arr {
                        if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                            continue;
                        }
                        let tool_name =
                            block.get("name").and_then(|v| v.as_str()).map(String::from);
                        let tool_input = block.get("input").cloned().map(Box::new);
                        // Each parallel tool call in this array carries its own
                        // `id`; it is the join key the matching tool_result
                        // echoes back as `tool_use_id`.
                        let tool_use_id =
                            block.get("id").and_then(|v| v.as_str()).map(String::from);
                        *sequence += 1;
                        events.push(ConversationEvent {
                            id: 0,
                            session_id,
                            sequence: *sequence,
                            event_type: EventType::ToolUse,
                            role,
                            content: String::new(),
                            tool_name,
                            tool_input,
                            created_at: chrono::Utc::now(),
                            offload_id: None,
                            tool_use_id,
                            metadata: None,
                        });
                    }

                    return Some(events);
                }

                // Simple string content -- emit as message if non-empty
                let text = v
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| v.to_string());
                if text.is_empty() {
                    return Some(Vec::new());
                }
                *sequence += 1;
                return Some(vec![ConversationEvent {
                    id: 0,
                    session_id,
                    sequence: *sequence,
                    event_type: EventType::Message,
                    role,
                    content: text,
                    tool_name: None,
                    tool_input: None,
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                }]);
            }

            // No content at all -- skip
            return Some(Vec::new());
        }

        // Non-message events: extract content normally
        // For thinking events, also check data.message directly
        let content = content_value
            .map(|v| {
                if let Some(s) = v.as_str() {
                    s.to_string()
                } else if let Some(arr) = v.as_array() {
                    arr.iter()
                        .filter_map(|block| {
                            if block.get("type")?.as_str()? == "text" {
                                block.get("text")?.as_str().map(String::from)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    v.to_string()
                }
            })
            .or_else(|| {
                // Some providers send thinking message as a string in data.message
                stream
                    .data
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(String::from)
            })
            .or_else(|| {
                // Antigravity (and potentially other providers) store error text in data.error
                stream
                    .data
                    .get("error")
                    .and_then(|e| e.as_str())
                    .map(String::from)
            })
            .unwrap_or_default();

        let tool_name = stream
            .data
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from);
        let tool_input = stream.data.get("input").cloned().map(Box::new);

        // A content-less `system` row carries nothing a reader can see: no
        // text, no tool name, no input. The Claude CLI emits a stream of them
        // (init/status/hook notices) around every tool call, and persisting
        // them buried the transcript in invisible rows AND split tool grouping
        // — the projection ends a batch on any non-groupable row, so a call and
        // its result were separated and every group collapsed to "1 tool call"
        // instead of the Codex-style "N tool calls". Recognized type, nothing
        // worth persisting: return the silent empty vec, and do not burn a
        // sequence number on a row that will not exist.
        if event_type == EventType::System
            && content.is_empty()
            && tool_name.is_none()
            && tool_input.is_none()
        {
            return Some(Vec::new());
        }

        *sequence += 1;
        Some(vec![ConversationEvent {
            id: 0,
            session_id,
            sequence: *sequence,
            event_type,
            role,
            content,
            tool_name,
            tool_input,
            created_at: chrono::Utc::now(),
            offload_id: None,
            // Top-level "tool_use"/"tool_result" stream events (e.g. the
            // synthetic ones the harness agent loop emits) carry the provider
            // tool-call id under `id` and `tool_use_id` respectively.
            tool_use_id: stream
                .data
                .get("tool_use_id")
                .or_else(|| stream.data.get("id"))
                .and_then(|v| v.as_str())
                .map(String::from),
            metadata: (event_type == EventType::ToolResult)
                .then(|| {
                    stream
                        .data
                        .get("is_error")
                        .and_then(|value| value.as_bool())
                })
                .flatten()
                .map(|is_error| Box::new(serde_json::json!({ "is_error": is_error }))),
        }])
    }

    /// Emit a loud `System` event for an ingest shape we could not model.
    ///
    /// Ingest never discards silently: an unrepresentable block becomes a
    /// visible row carrying the raw JSON verbatim (no truncation) so offline
    /// analysis can tell "nothing happened" apart from "we failed to parse it".
    fn unhandled_block_event(
        session_id: Uuid,
        sequence: &mut i32,
        tool_use_id: Option<String>,
        reason: &str,
        raw: &serde_json::Value,
    ) -> ConversationEvent {
        tracing::warn!(%session_id, reason, "unhandled tool block shape at ingest");
        *sequence += 1;
        ConversationEvent {
            id: 0,
            session_id,
            sequence: *sequence,
            event_type: EventType::System,
            role: None,
            content: format!("[ingest] {reason}: {raw}"),
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id,
            metadata: Some(Box::new(serde_json::json!({
                "ingest_error": reason,
                "raw_block": raw,
            }))),
        }
    }
}

/// Text extracted from a tool_result `content` payload, plus any blocks that
/// could not be rendered as text.
pub(super) struct FlattenedContent {
    pub(super) text: String,
    pub(super) unhandled: Vec<serde_json::Value>,
}

/// Flatten a tool_result `content` payload into text.
///
/// Accepts the two shapes the Claude CLI actually emits: a bare string, or an
/// array of content blocks. Returns `None` for any other shape so the caller
/// can report it loudly rather than dropping it.
pub(super) fn flatten_tool_content(v: &serde_json::Value) -> Option<FlattenedContent> {
    if let Some(s) = v.as_str() {
        return Some(FlattenedContent {
            text: s.to_string(),
            unhandled: Vec::new(),
        });
    }
    let arr = v.as_array()?;
    let mut parts = Vec::new();
    let mut unhandled = Vec::new();
    for block in arr {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => match block.get("text").and_then(|t| t.as_str()) {
                Some(text) => parts.push(text.to_string()),
                None => unhandled.push(block.clone()),
            },
            // Image blocks and any future block type are preserved verbatim
            // for the caller to surface; they are not silently skipped.
            _ => unhandled.push(block.clone()),
        }
    }
    Some(FlattenedContent {
        text: parts.join("\n"),
        unhandled,
    })
}

/// Classify whether a failed session should be retried based on error signals.
/// Returns a descriptive reason string if retryable, None if not.
fn classify_retry_eligibility(
    break_reason: &MonitorBreakReason,
    received_any_event: bool,
    received_meaningful_output: bool,
    exit_code: Option<i32>,
    events: &[ConversationEvent],
) -> Option<String> {
    let classification = crate::model_control::retry::classify_session_retry(
        break_reason,
        received_any_event,
        received_meaningful_output,
        exit_code,
        events,
    );
    if !classification.retryable() {
        return None;
    }
    Some(match classification {
        crate::model_control::retry::RetryClassification::Transient => {
            if matches!(break_reason, MonitorBreakReason::StallTimeout) {
                "stall timeout".to_string()
            } else if !received_any_event {
                "process exited with zero events".to_string()
            } else if !received_meaningful_output {
                "no meaningful output produced".to_string()
            } else if let Some(code) = exit_code.filter(|code| *code != 0) {
                format!("process exited with code {code}")
            } else {
                classification.as_reason().to_string()
            }
        }
        crate::model_control::retry::RetryClassification::RateLimited => {
            "rate limited (429)".to_string()
        }
        crate::model_control::retry::RetryClassification::Overloaded => {
            "API overloaded (529)".to_string()
        }
        _ => classification.as_reason().to_string(),
    })
}

/// Calculate backoff delay in milliseconds for a given retry attempt.
/// Formula: min(10_000 * 2^(attempt-1), max_backoff_ms)
/// attempt=1 -> 10s, attempt=2 -> 20s, attempt=3 -> 40s, attempt=4 -> 80s, capped at max_backoff_ms.
pub(crate) fn backoff_ms(attempt: u8, max_backoff_ms: u64) -> u64 {
    let base: u64 = 10_000;
    let delay = base.saturating_mul(1u64 << (attempt.saturating_sub(1)));
    delay.min(max_backoff_ms)
}

#[cfg(test)]
mod terminal_decision_tests {
    use super::*;
    use crate::store::daemon_settings::AutofileCause;

    fn transcript_event(
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
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        }
    }

    #[test]
    fn issue_97_post_handoff_tool_requires_corrected_terminal_handoff() {
        let mut events = vec![
            transcript_event(
                170,
                EventType::Message,
                Some(Role::Assistant),
                "## PIPELINE HANDOFF — FIX\nstatus: complete",
            ),
            transcript_event(171, EventType::ToolUse, None, ""),
            transcript_event(172, EventType::ToolResult, None, "apply_patch failed"),
            transcript_event(
                173,
                EventType::Message,
                Some(Role::Assistant),
                "Provider diagnostic",
            ),
        ];
        let completed = TerminalFinalizeDecision::completed();
        let failed = guard_terminal_handoff_order(completed, evidence(), &events);
        assert_eq!(failed.status, SessionStatus::Failed);
        assert_eq!(failed.c5_failure_cause, None);

        let mut tracked = super::live_context_state_tests::build_tracked(
            SessionProvider::Claude,
            0,
            0,
            0,
            0,
            ContextUsageConfidence::Missing,
            None,
        );
        tracked.events = events.clone();
        assert_eq!(
            apply_terminal_handoff_order(completed, evidence(), &mut tracked).status,
            SessionStatus::Failed,
        );
        assert_eq!(
            tracked.session.stop_reason.as_deref(),
            Some("terminal_handoff_superseded_by_tool"),
        );

        let mail_child_events = vec![
            transcript_event(
                107,
                EventType::Message,
                Some(Role::Assistant),
                "PIPELINE HANDOFF — IMPLEMENTATION\nstatus: complete",
            ),
            transcript_event(108, EventType::ToolUse, None, ""),
            transcript_event(109, EventType::ToolResult, None, "apply_patch failed"),
        ];
        assert_eq!(
            guard_terminal_handoff_order(completed, evidence(), &mail_child_events).status,
            SessionStatus::Failed,
        );

        let mut archive = evidence();
        archive.pending_archive = true;
        assert_eq!(
            guard_terminal_handoff_order(completed, archive, &events),
            completed,
        );

        let mut post_finalize = evidence();
        post_finalize.rotation_action = TerminalRotationAction::PostFinalize;
        assert_eq!(
            guard_terminal_handoff_order(completed, post_finalize, &events),
            completed,
        );

        let mut rotation = evidence();
        rotation.break_reason = MonitorBreakReason::Rotation;
        assert_eq!(
            guard_terminal_handoff_order(completed, rotation, &events),
            completed
        );

        events.push(transcript_event(
            174,
            EventType::Message,
            Some(Role::Assistant),
            "## PIPELINE HANDOFF — FIX:\nstatus: partial; patch failed",
        ));
        assert_eq!(
            guard_terminal_handoff_order(completed, evidence(), &events).status,
            SessionStatus::Failed,
            "a malformed handoff cannot correct post-handoff tool activity",
        );

        events.push(transcript_event(
            175,
            EventType::Message,
            Some(Role::Assistant),
            "PIPELINE HANDOFF — FIX:\nstatus: partial; patch failed",
        ));
        assert_eq!(
            guard_terminal_handoff_order(completed, evidence(), &events),
            completed
        );

        events.push(transcript_event(
            175,
            EventType::Message,
            Some(Role::User),
            "new turn",
        ));
        events.push(transcript_event(176, EventType::ToolUse, None, ""));
        assert_eq!(
            guard_terminal_handoff_order(completed, evidence(), &events),
            completed
        );

        let late_marker = vec![
            transcript_event(
                177,
                EventType::Message,
                Some(Role::Assistant),
                "\n\n\nPIPELINE HANDOFF — FIX:\nstatus: complete",
            ),
            transcript_event(178, EventType::ToolUse, None, ""),
        ];
        assert_eq!(
            guard_terminal_handoff_order(completed, evidence(), &late_marker).status,
            SessionStatus::Failed,
        );

        for invalid_marker in ["prose\nPIPELINE HANDOFF — FIX:\nstatus: complete"] {
            let events = vec![
                transcript_event(
                    179,
                    EventType::Message,
                    Some(Role::Assistant),
                    invalid_marker,
                ),
                transcript_event(180, EventType::ToolUse, None, ""),
            ];
            assert_eq!(
                guard_terminal_handoff_order(completed, evidence(), &events),
                completed,
                "a malformed first nonblank line is not a worker handoff",
            );
        }
    }

    fn evidence() -> TerminalEvidence {
        TerminalEvidence {
            expected_generation: 7,
            active_generation: Some(7),
            break_reason: MonitorBreakReason::StreamClosed,
            received_any_event: true,
            received_meaningful_output: true,
            prior_meaningful_output: false,
            current_result: TerminalResult::None,
            process_handle_present: true,
            process_alive: false,
            exit_code: Some(0),
            stream_drained: true,
            settlement: ProcessSettlementOutcome::AlreadyExited,
            supports_multi_turn: false,
            turn_outcome: TerminalTurnOutcome::NotMultiTurn,
            pending_archive: false,
            stall_interrupted: false,
            interrupt_requested: false,
            pending_question: false,
            rotation_action: TerminalRotationAction::None,
        }
    }

    #[test]
    fn terminal_decision_matrix_is_exhaustive_and_ordered() {
        let completed = TerminalDecision::Finalize(TerminalFinalizeDecision::completed());
        let interrupted = TerminalDecision::Finalize(TerminalFinalizeDecision::interrupted());
        let waiting = TerminalDecision::Finalize(TerminalFinalizeDecision::waiting_approval());
        let failed = |cause| TerminalDecision::Finalize(TerminalFinalizeDecision::failed(cause));

        let mut cases: Vec<(&str, TerminalEvidence, TerminalDecision)> = Vec::new();

        let mut stale = evidence();
        stale.active_generation = Some(8);
        cases.push(("stale generation", stale, TerminalDecision::StaleGeneration));

        let mut unsettled = evidence();
        unsettled.settlement = ProcessSettlementOutcome::EscalationFailed;
        cases.push((
            "ownership unsettled",
            unsettled,
            TerminalDecision::RetainRunning(TerminalRetentionReason::OwnershipUnsettled),
        ));

        let mut archive = evidence();
        archive.pending_archive = true;
        archive.stall_interrupted = true;
        archive.interrupt_requested = true;
        archive.exit_code = Some(23);
        cases.push(("pending archive wins", archive, completed));

        let mut stall = evidence();
        stall.stall_interrupted = true;
        stall.interrupt_requested = true;
        stall.pending_question = true;
        stall.exit_code = Some(23);
        cases.push(("stall wins", stall, failed(AutofileCause::StallTimeout)));

        let mut interrupt = evidence();
        interrupt.break_reason = MonitorBreakReason::Interrupted;
        interrupt.interrupt_requested = true;
        interrupt.pending_question = true;
        interrupt.exit_code = Some(23);
        cases.push(("interrupt wins", interrupt, interrupted));

        let mut question = evidence();
        question.pending_question = true;
        question.exit_code = Some(23);
        cases.push(("question wins", question, waiting));

        let mut rotation = evidence();
        rotation.break_reason = MonitorBreakReason::Rotation;
        rotation.rotation_action = TerminalRotationAction::PostFinalize;
        rotation.exit_code = Some(23);
        cases.push(("rotation owns outcome", rotation, completed));

        let mut nonzero = evidence();
        nonzero.current_result = TerminalResult::Success;
        nonzero.exit_code = Some(23);
        cases.push((
            "nonzero exit wins over output and result",
            nonzero,
            failed(AutofileCause::NonZeroExit),
        ));

        let mut provider_error = evidence();
        provider_error.exit_code = Some(0);
        provider_error.current_result = TerminalResult::ProviderError;
        cases.push((
            "explicit provider error",
            provider_error,
            failed(AutofileCause::OtherTerminalFailure),
        ));

        let mut dead_zero = evidence();
        dead_zero.received_any_event = false;
        dead_zero.received_meaningful_output = false;
        dead_zero.exit_code = None;
        cases.push((
            "dead zero event",
            dead_zero,
            failed(AutofileCause::NoMeaningfulOutput),
        ));

        let mut quiet_result = evidence();
        quiet_result.received_meaningful_output = false;
        quiet_result.current_result = TerminalResult::Success;
        quiet_result.exit_code = None;
        cases.push(("quiet successful result", quiet_result, completed));

        let mut prior_output = evidence();
        prior_output.received_any_event = false;
        prior_output.received_meaningful_output = false;
        prior_output.prior_meaningful_output = true;
        prior_output.exit_code = None;
        cases.push(("clean prior-output resume", prior_output, completed));

        cases.push(("clean current assistant output", evidence(), completed));

        for (name, evidence, expected) in cases {
            assert_eq!(terminal_decision(evidence), expected, "{name}");
        }
    }

    #[test]
    fn terminal_decision_result_truth_is_not_carried_stop_reason_metadata() {
        let mut no_current_result = evidence();
        no_current_result.received_any_event = false;
        no_current_result.received_meaningful_output = false;
        no_current_result.exit_code = None;
        assert_eq!(
            terminal_decision(no_current_result),
            TerminalDecision::Finalize(TerminalFinalizeDecision::failed(
                AutofileCause::NoMeaningfulOutput,
            )),
            "historical stop_reason is intentionally not an evidence field"
        );

        let mut current_success = no_current_result;
        current_success.received_any_event = true;
        current_success.current_result = TerminalResult::Success;
        assert_eq!(
            terminal_decision(current_success),
            TerminalDecision::Finalize(TerminalFinalizeDecision::completed())
        );
    }

    #[test]
    fn terminal_decision_none_exit_and_turn_ownership_cases() {
        let mut live = evidence();
        live.exit_code = None;
        live.process_alive = true;
        assert_eq!(
            terminal_decision(live),
            TerminalDecision::RetainRunning(TerminalRetentionReason::ProviderStillAlive)
        );

        let mut dead_task = evidence();
        dead_task.exit_code = None;
        dead_task.current_result = TerminalResult::Success;
        assert_eq!(
            terminal_decision(dead_task),
            TerminalDecision::Finalize(TerminalFinalizeDecision::completed())
        );

        let mut no_handle = dead_task;
        no_handle.process_handle_present = false;
        no_handle.settlement = ProcessSettlementOutcome::NoHandle;
        assert_eq!(
            terminal_decision(no_handle),
            TerminalDecision::Finalize(TerminalFinalizeDecision::completed())
        );

        let mut not_drained = dead_task;
        not_drained.stream_drained = false;
        assert_eq!(
            terminal_decision(not_drained),
            TerminalDecision::RetainRunning(TerminalRetentionReason::ProducerNotClosed)
        );

        let mut continuing = dead_task;
        continuing.supports_multi_turn = true;
        continuing.turn_outcome = TerminalTurnOutcome::Continued;
        assert_eq!(
            terminal_decision(continuing),
            TerminalDecision::RetainRunning(TerminalRetentionReason::TurnContinues)
        );

        let mut terminal_app_server = continuing;
        terminal_app_server.turn_outcome = TerminalTurnOutcome::Terminal;
        assert_eq!(
            terminal_decision(terminal_app_server),
            TerminalDecision::Finalize(TerminalFinalizeDecision::completed())
        );
    }
}

#[cfg(test)]
mod settlement_owner_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn settlement_owner_observes_panic_and_cancellation_without_wedging() {
        let mut panicked = SettlementTaskOwner {
            task: Some(tokio::spawn(async {
                panic!("deterministic settlement panic");
            })),
        };
        assert!(
            panicked.join().await.is_err(),
            "panic reaches monitor owner"
        );
        assert!(
            !panicked.is_running(),
            "panic clears completion ownership for retry"
        );

        let mut cancelled = SettlementTaskOwner {
            task: Some(tokio::spawn(async {
                std::future::pending::<ProcessSettlementOutcome>().await
            })),
        };
        cancelled.task.as_ref().expect("owned task").abort();
        assert!(
            cancelled.join().await.is_err(),
            "cancellation reaches monitor owner instead of an open channel"
        );
        assert!(
            !cancelled.is_running(),
            "cancelled ownership clears so monitor recovery can re-arm"
        );
    }

    #[tokio::test]
    async fn monitor_shutdown_drop_aborts_owned_settlement_task() {
        struct AbortMarker(Arc<AtomicBool>);
        impl Drop for AbortMarker {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let aborted = Arc::new(AtomicBool::new(false));
        let task_aborted = Arc::clone(&aborted);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let owner = SettlementTaskOwner {
            task: Some(tokio::spawn(async move {
                let _marker = AbortMarker(task_aborted);
                let _ = started_tx.send(());
                std::future::pending::<ProcessSettlementOutcome>().await
            })),
        };
        started_rx
            .await
            .expect("settlement task entered its owned body");
        drop(owner);
        for _ in 0..100 {
            if aborted.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            aborted.load(Ordering::SeqCst),
            "dropping the monitor-owned settlement handle aborts its task"
        );
    }
}

#[cfg(test)]
mod handoff_path_detection_tests {
    use super::*;
    use serde_json::json;

    const OLD_HANDOFF: &str = "thoughts/shared/handoffs/ENG-general/2026-04-05_21-43-02_old.md";
    const NEW_HANDOFF: &str = "thoughts/shared/handoffs/general/2026-05-23_23-55-15_new.md";

    #[test]
    fn shell_read_of_existing_handoff_is_not_creation_signal() {
        let data = json!({
            "name": "shell",
            "input": {
                "command": format!(
                    "/usr/bin/bash -lc \"sed -n '1,180p' {OLD_HANDOFF}\""
                )
            }
        });

        assert!(!tool_use_can_create_path(&data, OLD_HANDOFF));
    }

    #[test]
    fn shell_write_to_handoff_is_creation_signal() {
        let data = json!({
            "name": "shell",
            "input": {
                "command": format!(
                    "cat > {NEW_HANDOFF} <<'EOF'\n# Handoff\nEOF"
                )
            }
        });

        assert!(tool_use_can_create_path(&data, NEW_HANDOFF));
    }

    #[test]
    fn shell_stderr_redirect_read_is_not_creation_signal() {
        for verb in ["sed -n 1p", "cat", "head -n 1", "rg marker", "grep marker"] {
            for redirect in ["2>/dev/null", "2> /dev/null", ">& /dev/null"] {
                let command = format!("{verb} {OLD_HANDOFF} {redirect}");
                assert!(
                    !shell_command_can_create_path(&command, OLD_HANDOFF),
                    "{command}"
                );
            }
        }
    }

    #[test]
    fn assistant_doc_path_declares_handoff() {
        let text = format!("doc_path: /home/jakedevar/rsi/{NEW_HANDOFF}\nstatus: complete");
        assert!(assistant_text_declares_handoff_path(&text, NEW_HANDOFF));
    }

    #[test]
    fn assistant_listing_handoff_paths_is_not_declaration() {
        let text = format!("{OLD_HANDOFF}\nthoughts/shared/handoffs/general/other.md");
        assert!(!assistant_text_declares_handoff_path(&text, OLD_HANDOFF));
    }
}

#[cfg(test)]
mod model_update_tests {
    use super::*;
    use rsi_common::types::SessionProvider;
    use serde_json::json;

    fn init_event(model: &str) -> StreamEvent {
        StreamEvent {
            event_type: "system".to_string(),
            data: json!({
                "subtype": "init",
                "model": model,
            }),
        }
    }

    #[test]
    fn init_event_can_replace_existing_model_with_provider_reported_model() {
        let event = StreamEvent {
            event_type: "system".to_string(),
            data: json!({
                "subtype": "init",
                "model": "claude-opus-4-8",
            }),
        };

        assert_eq!(
            authoritative_model_update(Some("opus"), &event),
            Some("claude-opus-4-8")
        );
    }

    #[test]
    fn non_init_event_cannot_clobber_existing_model() {
        let event = StreamEvent {
            event_type: "assistant".to_string(),
            data: json!({
                "model": "claude-sonnet-5",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "hello"}],
                },
            }),
        };

        assert_eq!(
            authoritative_model_update(Some("claude-opus-4-8"), &event),
            None
        );
    }

    #[test]
    fn first_reported_model_is_accepted_when_session_model_missing() {
        let event = StreamEvent {
            event_type: "assistant".to_string(),
            data: json!({
                "model": "claude-sonnet-5",
            }),
        };

        assert_eq!(
            authoritative_model_update(None, &event),
            Some("claude-sonnet-5")
        );
    }

    #[test]
    fn candidate_initial_local_default_sets_missing_context_window() {
        let candidate = authoritative_model_candidate(
            SessionProvider::Local,
            None,
            None,
            None,
            &init_event("gemma4:e4b"),
        )
        .expect("missing model accepts provider init");

        assert_eq!(candidate.prior_model, None);
        assert_eq!(candidate.prior_context_window, None);
        assert_eq!(candidate.model, "gemma4:e4b");
        assert_eq!(candidate.budget.active_tokens, 262_144);
        assert!(candidate.tuple_changed);
        assert!(candidate.model_changed);
    }

    #[test]
    fn candidate_init_replacement_resets_context_fallback() {
        let candidate = authoritative_model_candidate(
            SessionProvider::Codex,
            Some("gpt-5-codex"),
            Some(1),
            None,
            &init_event("claude-opus-4-8"),
        )
        .expect("init accepts provider replacement");

        assert_eq!(candidate.prior_model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(candidate.prior_context_window, Some(1));
        assert_eq!(candidate.model, "claude-opus-4-8");
        assert_eq!(candidate.budget.active_tokens, 1_000_000);
        assert!(candidate.tuple_changed);
        assert!(candidate.model_changed);
    }

    #[test]
    fn candidate_model_change_preserves_unprojected_configured_authority() {
        let configured = rsi_common::ResolvedContextBudget::new(
            190_000,
            rsi_common::ContextCapacity::default(),
            rsi_common::CapabilityEvidence {
                source: rsi_common::CapabilitySource::Configured,
                source_version: None,
                source_digest: None,
                observed_at: Some(chrono::Utc::now()),
                confidence: rsi_common::CapabilityConfidence::Authoritative,
            },
        )
        .expect("positive configured budget");
        let candidate = authoritative_model_candidate(
            SessionProvider::Local,
            Some("old-model"),
            Some(190_000),
            Some(&configured),
            &init_event("new-model"),
        )
        .expect("init accepts configured model replacement");

        assert_eq!(candidate.model, "new-model");
        assert_eq!(candidate.budget.active_tokens, 190_000);
        assert_eq!(
            candidate.budget.evidence.source,
            rsi_common::CapabilitySource::Configured
        );
        assert!(candidate.budget.authorizes_threshold_rotation());
        assert!(candidate.model_changed);
        assert!(candidate.tuple_changed);
    }

    #[test]
    fn candidate_same_model_backfills_missing_context_without_model_delta() {
        let candidate = authoritative_model_candidate(
            SessionProvider::Local,
            Some("gemma4:e4b"),
            None,
            None,
            &init_event("gemma4:e4b"),
        )
        .expect("init accepts same model context backfill");

        assert_eq!(candidate.budget.active_tokens, 262_144);
        assert!(candidate.tuple_changed);
        assert!(!candidate.model_changed);
    }

    #[test]
    fn candidate_exact_tuple_is_a_noop() {
        let budget = crate::provider_capabilities::resolve_fresh_context_budget(
            SessionProvider::Local,
            "gemma4:e4b",
            None,
        );
        let candidate = authoritative_model_candidate(
            SessionProvider::Local,
            Some("gemma4:e4b"),
            Some(262_144),
            Some(&budget),
            &init_event("gemma4:e4b"),
        )
        .expect("init accepts exact existing tuple");

        assert!(!candidate.tuple_changed);
        assert!(!candidate.model_changed);
    }

    #[test]
    fn exact_init_capability_mismatch_warns_once_without_a_tuple_change() {
        let session_id = Uuid::new_v4();
        let budget = crate::provider_capabilities::resolve_fresh_context_budget(
            SessionProvider::Codex,
            "gpt-6-astra",
            None,
        );
        let candidate = authoritative_model_candidate(
            SessionProvider::Codex,
            Some("gpt-6-astra"),
            Some(budget.active_tokens),
            Some(&budget),
            &init_event("gpt-6-astra"),
        )
        .expect("exact init remains authoritative");
        assert!(!candidate.tuple_changed);

        let event_bus = crate::bus::EventBus::new(1);
        let mut events = event_bus.subscribe();
        let mut last_mismatch_warn = None;
        for _ in 0..2 {
            warn_capability_class_mismatch(
                &event_bus,
                session_id,
                Some(rsi_common::types::CapabilityClass::LookupFast),
                &mut last_mismatch_warn,
                &candidate.model,
            );
        }

        assert!(matches!(
            events
                .try_recv()
                .expect("first exact-init mismatch warns")
                .as_ref(),
            DaemonEvent::SystemMessage { level, message }
                if level == "warn"
                    && message.contains(&session_id.to_string())
                    && message.contains("gpt-6-astra")
        ));
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(
            last_mismatch_warn,
            Some((
                rsi_common::types::CapabilityClass::LookupFast,
                "gpt-6-astra".to_string()
            ))
        );
        event_bus.unsubscribe();
    }

    #[test]
    fn candidate_rejects_auxiliary_non_init_model() {
        let event = StreamEvent {
            event_type: "assistant".to_string(),
            data: json!({ "model": "auxiliary-model" }),
        };

        assert_eq!(
            authoritative_model_candidate(
                SessionProvider::Local,
                Some("gemma4:e4b"),
                Some(262_144),
                None,
                &event,
            ),
            None
        );
    }

    #[test]
    fn runtime_resolution_rejects_zero_and_pins_incarnation_catalog_evidence() {
        let prior = rsi_common::ResolvedContextBudget::new(
            258_400,
            rsi_common::ContextCapacity {
                provider_default_tokens: Some(272_000),
                provider_max_tokens: Some(872_000),
                effective_percent: Some(95),
                ..rsi_common::ContextCapacity::default()
            },
            rsi_common::CapabilityEvidence {
                source: rsi_common::CapabilitySource::ProviderCatalog,
                source_version: Some("codex-cli 0.155.1".to_string()),
                source_digest: Some(format!("sha256:{}", "b".repeat(64))),
                observed_at: Some(chrono::Utc::now()),
                confidence: rsi_common::CapabilityConfidence::Verified,
            },
        )
        .expect("positive catalog budget");

        assert!(
            resolve_runtime_context_budget(
                SessionProvider::Codex,
                "gpt-6-astra",
                Some(&prior),
                0,
                chrono::Utc::now(),
            )
            .is_err()
        );
        let runtime = resolve_runtime_context_budget(
            SessionProvider::Codex,
            "gpt-6-astra",
            Some(&prior),
            258_400,
            chrono::Utc::now(),
        )
        .expect("positive telemetry resolves");
        assert_eq!(
            runtime.evidence.source,
            rsi_common::CapabilitySource::RuntimeTelemetry
        );
        assert_eq!(
            runtime.evidence.source_version,
            prior.evidence.source_version
        );
        assert_eq!(runtime.evidence.source_digest, prior.evidence.source_digest);
        assert_eq!(
            runtime.capacity.provider_default_tokens,
            prior.capacity.provider_default_tokens
        );
        assert_eq!(
            runtime.capacity.provider_max_tokens,
            prior.capacity.provider_max_tokens
        );
        assert_eq!(
            runtime.capacity.effective_percent,
            prior.capacity.effective_percent
        );
        assert_eq!(runtime.capacity.runtime_effective_tokens, Some(258_400));

        let discovery_only_prior = rsi_common::ResolvedContextBudget::new(
            300_000,
            rsi_common::ContextCapacity::default(),
            rsi_common::CapabilityEvidence {
                source: rsi_common::CapabilitySource::LegacyUnverified,
                source_version: Some("codex-cli 0.153.0".to_string()),
                source_digest: Some(format!("sha256:{}", "a".repeat(64))),
                observed_at: Some(chrono::Utc::now()),
                confidence: rsi_common::CapabilityConfidence::Degraded,
            },
        )
        .expect("positive discovery-only launch budget");
        let runtime = resolve_runtime_context_budget(
            SessionProvider::Codex,
            "gpt-6-astra",
            Some(&discovery_only_prior),
            258_400,
            chrono::Utc::now(),
        )
        .expect("discovery-only incarnation accepts positive telemetry");
        assert_eq!(
            runtime.evidence.source,
            rsi_common::CapabilitySource::RuntimeTelemetry
        );
        assert_eq!(
            runtime.evidence.source_version,
            discovery_only_prior.evidence.source_version
        );
        assert_eq!(
            runtime.evidence.source_digest,
            discovery_only_prior.evidence.source_digest
        );
        assert_eq!(runtime.capacity.provider_default_tokens, None);
        assert_eq!(runtime.capacity.provider_max_tokens, None);
        assert_eq!(runtime.capacity.effective_percent, None);
        assert_eq!(runtime.capacity.advertised_max_tokens, Some(1_050_000));
        assert_eq!(runtime.capacity.max_output_tokens, Some(128_000));
        assert_eq!(runtime.capacity.runtime_effective_tokens, Some(258_400));
    }
}

/// B1: the Claude stream path used to drop an unrecognized event type with no
/// log and no metric, so two live protocol additions (`rate_limit_event` and
/// `system/api_retry`) were invisible until a manual CLI probe found them.
#[cfg(test)]
mod unrecognized_stream_event_tests {
    use super::*;

    #[test]
    fn converter_reports_an_unrecognized_type_as_none() {
        // `None` is what makes the diagnostic possible at all: the caller can
        // now tell "no arm for this type" from "recognized, carried nothing".
        let unknown = StreamEvent {
            event_type: "api_retry".to_string(),
            data: serde_json::json!({"attempt": 1, "max_retries": 5}),
        };
        let mut sequence = 0;
        assert!(
            SessionManager::convert_recognized_stream_event(
                &unknown,
                Uuid::new_v4(),
                &mut sequence
            )
            .is_none(),
            "a type with no converter arm must be reported as unrecognized"
        );
        assert_eq!(sequence, 0, "an unrecognized event consumes no sequence");
    }

    #[test]
    fn converter_reports_a_recognized_but_empty_type_as_some() {
        // `result` is handled by the stream loop separately and yields no
        // conversation event. It must NOT be reported as a blind spot.
        let result = StreamEvent {
            event_type: "result".to_string(),
            data: serde_json::json!({"subtype": "success"}),
        };
        let mut sequence = 0;
        let converted =
            SessionManager::convert_recognized_stream_event(&result, Uuid::new_v4(), &mut sequence)
                .expect("`result` is recognized, so it is not a blind spot");
        assert!(
            converted.is_empty(),
            "`result` is handled by the stream loop and yields no conversation event"
        );
    }

    #[test]
    fn a_content_less_system_event_is_recognized_and_dropped() {
        // The Claude CLI emits these around every tool call. Persisted, they
        // were invisible rows that also split tool grouping, so each call
        // rendered as its own "1 tool call" summary.
        let noise = StreamEvent {
            event_type: "system".to_string(),
            data: serde_json::json!({"subtype": "init", "session_id": "abc123"}),
        };
        let mut sequence = 7;
        let converted =
            SessionManager::convert_recognized_stream_event(&noise, Uuid::new_v4(), &mut sequence)
                .expect("`system` is recognized, so it is not a blind spot");
        assert!(
            converted.is_empty(),
            "a system event with no content, tool name or input carries nothing"
        );
        assert_eq!(sequence, 7, "a dropped event must not consume a sequence");
    }

    #[test]
    fn a_system_event_with_content_is_still_persisted() {
        let real = StreamEvent {
            event_type: "system".to_string(),
            data: serde_json::json!({"content": "context low, compacting"}),
        };
        let mut sequence = 0;
        let converted =
            SessionManager::convert_recognized_stream_event(&real, Uuid::new_v4(), &mut sequence)
                .expect("recognized");
        assert_eq!(converted.len(), 1, "a system row with text must survive");
        assert_eq!(converted[0].event_type, EventType::System);
        assert_eq!(converted[0].content, "context low, compacting");
        assert_eq!(sequence, 1);
    }

    #[test]
    fn a_distinct_unknown_type_is_reported_exactly_once() {
        let mut gate = UnrecognizedStreamEvents::default();
        assert_eq!(
            gate.classify("api_retry"),
            UnrecognizedEventAction::Report,
            "the first sighting is the diagnostic"
        );
        assert_eq!(
            gate.classify("api_retry"),
            UnrecognizedEventAction::Silent,
            "a chatty type must not flood the daemon log"
        );
        assert_eq!(gate.classify("api_retry"), UnrecognizedEventAction::Silent);
        assert_eq!(
            gate.classify("some_other_future_type"),
            UnrecognizedEventAction::Report,
            "dedupe is per type, not a one-shot for the whole session"
        );
    }

    #[test]
    fn types_the_stream_loop_consumes_elsewhere_stay_silent() {
        // These reach the converter and produce no conversation event by
        // design — the loop already handled them. Mirrors the Codex path's
        // explicit `"turn.started" => None` no-op arm.
        let mut gate = UnrecognizedStreamEvents::default();
        for handled in NON_CONVERSATION_STREAM_EVENTS {
            assert_eq!(
                gate.classify(handled),
                UnrecognizedEventAction::Silent,
                "{handled} is consumed by the stream loop and is not a blind spot"
            );
        }
    }

    #[test]
    fn distinct_type_diagnostics_are_capped_and_announce_the_ceiling() {
        let mut gate = UnrecognizedStreamEvents::default();
        for i in 0..UNRECOGNIZED_STREAM_EVENT_CAP {
            assert_eq!(
                gate.classify(&format!("unknown_type_{i}")),
                UnrecognizedEventAction::Report
            );
        }
        assert_eq!(
            gate.classify("one_type_too_many"),
            UnrecognizedEventAction::CapReached,
            "the ceiling is announced rather than reached in silence"
        );
        assert_eq!(
            gate.classify("yet_another_type"),
            UnrecognizedEventAction::Silent,
            "the ceiling notice itself is emitted once"
        );
    }
}

/// V99/P1-A + P1-B: the two receive-side extractors added in this phase.
/// Both fixtures are the verbatim payloads observed from `claude 2.1.259`.
#[cfg(test)]
mod provider_telemetry_tests {
    use super::*;
    use rsi_common::types::SessionProvider;
    use serde_json::json;

    /// Verbatim `system/init` handshake from the probe.
    fn observed_init_event() -> StreamEvent {
        StreamEvent {
            event_type: "system".to_string(),
            data: json!({
                "subtype": "init",
                "model": "claude-opus-5[1m]",
                "session_id": "9d1c1f6a-0c1b-4a53-9d8f-1c2f1a3b4c5d",
                "claude_code_version": "2.1.259",
                "capabilities": [
                    "interrupt_receipt_v1",
                    "interrupt_cancel_queued_v1",
                    "msg_lifecycle_v1"
                ],
            }),
        }
    }

    /// Verbatim `rate_limit_event` from the probe (V-023).
    fn observed_rate_limit_event() -> StreamEvent {
        StreamEvent {
            event_type: "rate_limit_event".to_string(),
            data: json!({
                "session_id": "9d1c1f6a-0c1b-4a53-9d8f-1c2f1a3b4c5d",
                "uuid": "1b2c3d4e-5f60-4712-8394-a5b6c7d8e9f0",
                "rate_limit_info": {
                    "status": "allowed",
                    "resetsAt": 1_788_402_000_i64,
                    "rateLimitType": "five_hour",
                    "overageStatus": "rejected",
                    "overageDisabledReason": "out_of_credits",
                    "isUsingOverage": false,
                    "unifiedWindows": {
                        "five_hour": {"utilization": 0.27, "resetsAt": 1_788_402_000_i64},
                        "seven_day": {"utilization": 0.05, "resetsAt": 1_788_883_200_i64}
                    }
                }
            }),
        }
    }

    #[test]
    fn init_handshake_captures_version_and_capabilities() {
        let handshake = provider_handshake(&observed_init_event()).expect("init yields handshake");
        assert_eq!(handshake.cli_version.as_deref(), Some("2.1.259"));
        assert_eq!(
            handshake.capabilities,
            vec![
                "interrupt_receipt_v1".to_string(),
                "interrupt_cancel_queued_v1".to_string(),
                "msg_lifecycle_v1".to_string(),
            ]
        );
    }

    #[test]
    fn handshake_is_only_read_from_system_init() {
        // A non-init system event, and a non-system event, must both be
        // ignored: only the launch-time handshake is authoritative.
        let not_init = StreamEvent {
            event_type: "system".to_string(),
            data: json!({"subtype": "api_retry", "claude_code_version": "9.9.9"}),
        };
        assert_eq!(provider_handshake(&not_init), None);

        let not_system = StreamEvent {
            event_type: "assistant".to_string(),
            data: json!({"claude_code_version": "9.9.9"}),
        };
        assert_eq!(provider_handshake(&not_system), None);
    }

    #[test]
    fn handshake_advertising_nothing_is_not_persisted() {
        // An init with neither fact gives us nothing to record, so it must not
        // trigger a pointless write.
        let bare_init = StreamEvent {
            event_type: "system".to_string(),
            data: json!({"subtype": "init", "model": "claude-opus-5"}),
        };
        assert_eq!(provider_handshake(&bare_init), None);
    }

    #[test]
    fn handshake_tolerates_a_partial_advertisement() {
        // Version but no capabilities is a real shape for an older CLI.
        let version_only = StreamEvent {
            event_type: "system".to_string(),
            data: json!({"subtype": "init", "claude_code_version": "2.0.0"}),
        };
        let handshake = provider_handshake(&version_only).expect("version alone is worth storing");
        assert_eq!(handshake.cli_version.as_deref(), Some("2.0.0"));
        assert!(handshake.capabilities.is_empty());
    }

    #[test]
    fn rate_limit_event_yields_both_observed_windows() {
        let snapshot =
            parse_rate_limit_event(SessionProvider::Claude, &observed_rate_limit_event())
                .expect("rate_limit_event parses");

        assert_eq!(snapshot.provider, SessionProvider::Claude);
        assert_eq!(snapshot.status.as_deref(), Some("allowed"));
        assert_eq!(snapshot.rate_limit_type.as_deref(), Some("five_hour"));
        assert_eq!(snapshot.overage_status.as_deref(), Some("rejected"));
        assert!(!snapshot.is_using_overage);
        assert_eq!(snapshot.windows.len(), 2);

        let five_hour = snapshot
            .windows
            .iter()
            .find(|w| w.window_key == "five_hour")
            .expect("five_hour window present");
        assert_eq!(five_hour.utilization, 0.27);
        assert_eq!(five_hour.resets_at_epoch, Some(1_788_402_000));

        let seven_day = snapshot
            .windows
            .iter()
            .find(|w| w.window_key == "seven_day")
            .expect("seven_day window present");
        assert_eq!(seven_day.utilization, 0.05);
        assert_eq!(seven_day.resets_at_epoch, Some(1_788_883_200));
    }

    #[test]
    fn rate_limit_windows_are_iterated_not_hardcoded() {
        // A provider adding a third window must have it captured. Hardcoding
        // the two observed keys would silently drop it.
        let mut event = observed_rate_limit_event();
        event.data["rate_limit_info"]["unifiedWindows"]["thirty_day"] =
            json!({"utilization": 0.02, "resetsAt": 1_790_000_000_i64});

        let snapshot = parse_rate_limit_event(SessionProvider::Claude, &event)
            .expect("rate_limit_event parses");
        assert_eq!(snapshot.windows.len(), 3);
        let thirty_day = snapshot
            .windows
            .iter()
            .find(|w| w.window_key == "thirty_day")
            .expect("an unknown third window is captured, not dropped");
        assert_eq!(thirty_day.utilization, 0.02);
    }

    #[test]
    fn peak_window_selects_the_most_consumed() {
        // The status bar shows one window; it must be the one that throttles
        // first, not whichever the map happened to yield first.
        let snapshot =
            parse_rate_limit_event(SessionProvider::Claude, &observed_rate_limit_event())
                .expect("rate_limit_event parses");
        let peak = snapshot.peak_window().expect("a peak window exists");
        assert_eq!(peak.window_key, "five_hour");
        assert_eq!(peak.utilization, 0.27);
    }

    #[test]
    fn non_rate_limit_events_and_empty_windows_are_ignored() {
        let other = StreamEvent {
            event_type: "result".to_string(),
            data: json!({"rate_limit_info": {"unifiedWindows": {}}}),
        };
        assert!(parse_rate_limit_event(SessionProvider::Claude, &other).is_none());

        let mut empty = observed_rate_limit_event();
        empty.data["rate_limit_info"]["unifiedWindows"] = json!({});
        assert!(parse_rate_limit_event(SessionProvider::Claude, &empty).is_none());

        let no_info = StreamEvent {
            event_type: "rate_limit_event".to_string(),
            data: json!({"session_id": "x"}),
        };
        assert!(parse_rate_limit_event(SessionProvider::Claude, &no_info).is_none());
    }

    #[test]
    fn snapshot_provider_follows_the_reporting_session() {
        // The table is keyed by provider so a second CLI reporting windows
        // needs no schema change; the parser must honour the caller's provider.
        let snapshot = parse_rate_limit_event(SessionProvider::Codex, &observed_rate_limit_event())
            .expect("rate_limit_event parses");
        assert_eq!(snapshot.provider, SessionProvider::Codex);
    }
}

#[cfg(test)]
mod live_context_state_tests {
    use super::*;
    use crate::session::rotation_coordinator::RotationCoordinator;
    use rsi_common::types::{Session, SessionKind, SessionStatus};

    fn repository_fallback_budget(active_tokens: u64) -> rsi_common::ResolvedContextBudget {
        rsi_common::ResolvedContextBudget::new(
            active_tokens,
            rsi_common::ContextCapacity::default(),
            rsi_common::CapabilityEvidence {
                source: rsi_common::CapabilitySource::RepositoryFallback,
                source_version: Some("rsi-provider-capabilities-v1".to_string()),
                source_digest: None,
                observed_at: None,
                confidence: rsi_common::CapabilityConfidence::Degraded,
            },
        )
        .expect("positive repository fallback budget")
    }

    /// Build a minimal `TrackedSession` for `live_context_state` assertions.
    ///
    /// Only fields read by `live_context_state` (provider / model / context_window
    /// / live_input_tokens / live_usage_confidence / daemon_input_tokens /
    /// daemon_output_tokens / daemon_tokens_at_last_api_update) are meaningfully
    /// set. All other fields get inert defaults — the test never drives the
    /// monitor loop, so `process: None` and a throwaway stop channel are safe.
    pub(super) fn build_tracked(
        provider: SessionProvider,
        live_input_tokens: u64,
        daemon_input_tokens: u64,
        daemon_output_tokens: u64,
        daemon_tokens_at_last_api_update: u64,
        live_usage_confidence: ContextUsageConfidence,
        context_window: Option<u64>,
    ) -> TrackedSession {
        let session_id = Uuid::new_v4();
        let resolved_context_budget = context_window.map(|active_tokens| {
            let mut request = crate::provider_capabilities::ContextBudgetRequest::new(
                provider,
                "claude-opus-4-7",
            );
            request.runtime_effective_tokens = Some(active_tokens);
            request.observed_at = Some(chrono::Utc::now());
            crate::provider_capabilities::provider_capabilities().resolve_context_budget(request)
        });
        let session = Session {
            context_fill_pct: None,
            id: session_id,
            status: SessionStatus::Running,
            session_kind: SessionKind::Standard,
            provider,
            context_usage_confidence: ContextUsageConfidence::Missing,
            rotation_depth: 0,
            retry_attempt: None,
            max_retries: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            query: "test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            working_dir: std::path::PathBuf::from("/tmp"),
            git_branch: None,
            model: Some("claude-opus-4-7".to_string()),
            claude_session_id: None,
            project_id: None,
            continued_from: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            stop_reason: None,
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            input_tokens: None,
            output_tokens: None,
            context_window,
            resolved_context_budget,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            pending_question: None,
            pending_archive: false,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: None,
            approval_started_at: None,
            work_time_ms: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        };
        let (stop_tx, _stop_rx) = tokio::sync::mpsc::channel(1);
        TrackedSession {
            session,
            spawn_generation: 0,
            events: Vec::new(),
            turn_metrics: Vec::new(),
            process: None,
            deferred_successor_start_gate: None,
            stop_tx,
            interrupt_requested: false,
            pending_archive: false,
            rotation: RotationCoordinator::new(session_id, 0, false),
            live_input_tokens,
            live_output_tokens: 0,
            live_usage_confidence,
            daemon_input_tokens,
            daemon_output_tokens,
            daemon_tokens_at_last_api_update,
            codex_context_tokens: 0,
            pipeline_artifact: None,
            memory_flush_compaction_count: None,
            pending_question: None,
            approval_wait_start: None,
            approval_wait_total_ms: 0,
            work_run_start: None,
            work_time_base_ms: 0,
            received_meaningful_output: false,
            exit_code: None,
            retry_attempt: 0,
            max_retries: 0,
            last_event_at: chrono::Utc::now(),
            stall_interrupted: false,
            last_usage_update: None,
            last_mismatch_warn: None,
            last_classified_at: None,
            classification_count: 0,
            last_verdict: None,
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn spawn_child_persists_handoff_filepath_on_predecessor() {
        let source = include_str!("monitor.rs");
        let decision = source.find("let action = t").unwrap();
        let finalize = source
            .find("let Some(finalized_decision) = Self::finalize_session")
            .unwrap();
        assert!(
            source[decision..finalize]
                .contains("capture_spawn_child_handoff(&mut t.session, &action)"),
            "handoff path must be captured before finalization"
        );
        let mut tracked = build_tracked(
            SessionProvider::Claude,
            0,
            0,
            0,
            0,
            ContextUsageConfidence::Missing,
            None,
        );
        let path = "thoughts/shared/handoffs/general/task.md";
        tracked.rotation = RotationCoordinator::new_writing_handoff(tracked.session.id, 0, true);
        tracked
            .rotation
            .advance(RotationEvent::HandoffFileDetected { path: path.into() });
        let action = tracked.rotation.advance(RotationEvent::MonitorCompleted {
            break_reason: MonitorBreakReason::Result,
        });
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        store.insert_session(&tracked.session).unwrap();
        capture_spawn_child_handoff(&mut tracked.session, &action);
        store.update_session_metadata(&tracked.session).unwrap();
        assert_eq!(
            store
                .get_session(tracked.session.id)
                .unwrap()
                .unwrap()
                .handoff_filepath
                .as_deref(),
            Some(path)
        );
    }

    /// Claude with API data: numerator is exactly `live_input_tokens` —
    /// daemon_delta is NOT added even when daemon counts are nonzero.
    #[test]
    fn test_live_context_state_claude_api_primary_no_delta() {
        let tracked = build_tracked(
            SessionProvider::Claude,
            1000, // live_input_tokens
            500,  // daemon_input_tokens
            200,  // daemon_output_tokens
            0,    // daemon_tokens_at_last_api_update (so daemon_delta=700)
            ContextUsageConfidence::Full,
            None,
        );
        let (numerator, _pct, confidence, _daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(
            numerator, 1000,
            "Claude: numerator must equal live_input_tokens, not live_input_tokens+daemon_delta"
        );
        assert!(matches!(confidence, ContextUsageConfidence::Full));
    }

    /// Codex with token-count data: context fill ignores cumulative
    /// `turn.completed` usage and uses the current-window token-count value.
    #[test]
    fn test_live_context_state_codex_token_count_primary() {
        let mut tracked = build_tracked(
            SessionProvider::Codex,
            463_280, // inflated cumulative turn.completed value; ignored for context fill
            500,
            200,
            700,
            ContextUsageConfidence::Full,
            Some(258_400),
        );
        tracked.codex_context_tokens = 92_833;
        tracked.session.model = Some("gpt-5.5".to_string());

        let (numerator, _pct, confidence, daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(daemon_total, 700);
        assert_eq!(
            numerator, 92_833,
            "Codex: numerator must come from event_msg/token_count total_tokens, not turn.completed"
        );
        assert!(matches!(confidence, ContextUsageConfidence::Full));
    }

    #[test]
    fn codex_cache_read_accounting_stays_separate_from_context_numerator() {
        let mut tracked = build_tracked(
            SessionProvider::OpenRouter,
            463_280,
            500,
            200,
            700,
            ContextUsageConfidence::Full,
            Some(258_400),
        );
        let event = StreamEvent {
            event_type: "codex_token_count".into(),
            data: serde_json::json!({"observed_at":"2026-09-22T21:00:00Z"}),
        };
        let usage = monitor::CodexContextUsage {
            context_tokens: 93_094,
            output_tokens: 261,
            context_window: Some(258_400),
            cache_read_tokens: Some(84_864),
        };

        record_codex_context_observation(&mut tracked, &event, &usage);

        assert_eq!(tracked.session.total_cache_read_tokens, Some(84_864));
        assert_eq!(tracked.codex_context_tokens, 93_094);
        assert_eq!(tracked.session.input_tokens, Some(93_094));

        let next_usage = monitor::CodexContextUsage {
            context_tokens: 100_000,
            output_tokens: 300,
            context_window: Some(258_400),
            cache_read_tokens: Some(297_024),
        };
        record_codex_context_observation(&mut tracked, &event, &next_usage);

        assert_eq!(tracked.session.total_cache_read_tokens, Some(381_888));
        assert_eq!(tracked.codex_context_tokens, 100_000);
    }

    #[test]
    fn cache_read_only_change_requests_session_metadata_persistence() {
        assert!(cache_read_metadata_changed(Some(381_888), Some(300_000)));
        assert!(!cache_read_metadata_changed(Some(381_888), Some(381_888)));
        assert!(!cache_read_metadata_changed(None, None));
    }

    #[test]
    fn test_live_context_state_pioneer_uses_codex_token_count_primary() {
        let mut tracked = build_tracked(
            SessionProvider::Pioneer,
            463_280,
            500,
            200,
            700,
            ContextUsageConfidence::Full,
            Some(258_400),
        );
        tracked.codex_context_tokens = 92_833;
        tracked.session.model = Some("gpt-5.5".to_string());

        let (numerator, _pct, confidence, daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(daemon_total, 700);
        assert_eq!(numerator, 92_833);
        assert!(matches!(confidence, ContextUsageConfidence::Full));
    }

    /// Display keeps the last provider observation; rotation retains its estimator.
    #[test]
    fn test_live_context_state_codex_keeps_provider_reading_between_reports() {
        let mut tracked = build_tracked(
            SessionProvider::Codex,
            463_280,
            500,
            200,
            100,
            ContextUsageConfidence::Full,
            Some(258_400),
        );
        tracked.codex_context_tokens = 92_833;
        tracked.session.model = Some("gpt-5.5".to_string());

        let (numerator, _pct, confidence, daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(daemon_total, 700);
        assert_eq!(numerator, 92_833);
        assert!(matches!(confidence, ContextUsageConfidence::Full));
    }

    #[test]
    fn test_live_context_state_codex_without_token_count_reports_missing() {
        let mut tracked = build_tracked(
            SessionProvider::Codex,
            463_280, // ignored until Codex token-count telemetry arrives
            500,
            200,
            0,
            ContextUsageConfidence::Full,
            None,
        );
        tracked.session.model = Some("gpt-5.5".to_string());

        let (numerator, _pct, confidence, _daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(numerator, 0);
        assert_eq!(context_fill_pct_for_tracked(&tracked), None);
        assert!(matches!(confidence, ContextUsageConfidence::Missing));
    }

    /// Keep descriptive capability metadata without inventing observed capacity.
    #[test]
    fn test_live_context_state_codex_retains_fallback_budget_but_reports_unknown_pct() {
        let mut tracked = build_tracked(
            SessionProvider::Codex,
            129_200,
            0,
            0,
            0,
            ContextUsageConfidence::Full,
            None,
        );
        tracked.session.model = Some("gpt-5.4".to_string());
        tracked.session.context_window = Some(1_050_000); // legacy scalar only
        tracked.session.resolved_context_budget = Some(repository_fallback_budget(272_000));
        tracked.codex_context_tokens = 129_200;

        let (_numerator, pct, _confidence, _daemon_total, window) = live_context_state(&tracked);

        assert_eq!(window, 272_000);
        assert_eq!(pct, 0.0);
        assert_eq!(
            context_fill_pct_for_tracked(&tracked),
            None,
            "descriptive transport budget cannot establish measured occupancy"
        );
    }

    #[test]
    fn live_gpt56_uses_authoritative_runtime_context_window() {
        let mut tracked = build_tracked(
            SessionProvider::Codex,
            34_000,
            0,
            0,
            0,
            ContextUsageConfidence::Full,
            Some(258_400),
        );
        tracked.session.model = Some("gpt-6-astra".to_string());
        tracked.codex_context_tokens = 34_000;

        let (numerator, pct, _confidence, _daemon_total, window) = live_context_state(&tracked);

        assert_eq!(numerator, 34_000);
        assert_eq!(window, 258_400);
        assert!((pct - 34_000.0 * 100.0 / 258_400.0).abs() < 1e-9);
    }

    #[test]
    fn runtime_258400_controls_threshold_rotation() {
        let mut tracked = build_tracked(
            SessionProvider::Codex,
            180_000,
            0,
            0,
            0,
            ContextUsageConfidence::Full,
            Some(258_400),
        );
        tracked.session.model = Some("gpt-6-astra".to_string());
        tracked.codex_context_tokens = 180_000;
        tracked.rotation.set_enabled(true);

        let (_, pct, _, _, window) = live_context_state(&tracked);
        assert_eq!(window, 258_400);
        assert!((pct - 180_000.0 * 100.0 / 258_400.0).abs() < 1e-9);
        let rotation_pct = rotation_context_pct(&tracked, pct);
        assert_eq!(rotation_pct, 68.0);
        assert!(tracked.authorizes_threshold_rotation());
        assert!(matches!(
            context_rotation_threshold_action(&mut tracked, rotation_pct),
            RotationAction::InterruptForRotation
        ));
    }

    #[test]
    fn descriptive_and_degraded_budgets_never_trigger_threshold_rotation() {
        for (source, confidence) in [
            (
                rsi_common::CapabilitySource::ProviderCatalog,
                rsi_common::CapabilityConfidence::Verified,
            ),
            (
                rsi_common::CapabilitySource::RepositoryFallback,
                rsi_common::CapabilityConfidence::Degraded,
            ),
            (
                rsi_common::CapabilitySource::LegacyUnverified,
                rsi_common::CapabilityConfidence::Degraded,
            ),
        ] {
            let mut tracked = build_tracked(
                SessionProvider::Codex,
                180_000,
                0,
                0,
                0,
                ContextUsageConfidence::Full,
                None,
            );
            tracked.session.model = Some("gpt-6-astra".to_string());
            tracked.session.context_window = Some(258_400);
            tracked.session.resolved_context_budget = Some(
                rsi_common::ResolvedContextBudget::new(
                    258_400,
                    rsi_common::ContextCapacity::default(),
                    rsi_common::CapabilityEvidence {
                        source,
                        source_version: None,
                        source_digest: None,
                        observed_at: None,
                        confidence,
                    },
                )
                .expect("positive test budget"),
            );
            tracked.rotation.set_enabled(true);

            assert!(!tracked.authorizes_threshold_rotation());
            assert!(matches!(
                context_rotation_threshold_action(&mut tracked, 68.0),
                RotationAction::NoOp
            ));
            assert!(matches!(tracked.rotation.state(), RotationState::Idle));
        }
    }

    #[tokio::test]
    async fn runtime_context_write_failure_preserves_live_and_durable_tuple() {
        let mut tracked = build_tracked(
            SessionProvider::Codex,
            34_000,
            0,
            0,
            0,
            ContextUsageConfidence::Full,
            Some(272_000),
        );
        tracked.session.model = Some("gpt-6-astra".to_string());
        let session_id = tracked.session.id;
        let prior_budget = tracked
            .session
            .resolved_context_budget
            .clone()
            .expect("fixture starts with a typed budget");
        let directory = tempfile::tempdir().expect("create runtime rollback store");
        let store = Arc::new(tokio::sync::Mutex::new(
            Store::open(&directory.path().join("runtime-rollback.db"))
                .expect("open runtime rollback store"),
        ));
        {
            let store = store.lock().await;
            store
                .insert_session(&tracked.session)
                .expect("insert runtime rollback session");
            store
                .conn
                .execute_batch(
                    "CREATE TRIGGER c4a_runtime_observation_failure\n\
                     BEFORE UPDATE OF context_window ON sessions\n\
                     BEGIN SELECT RAISE(ABORT, 'c4a runtime observation failure'); END;",
                )
                .expect("install runtime rollback trigger");
        }
        let persistence = PersistenceHandle::new(Arc::clone(&store));
        let active = Arc::new(RwLock::new(HashMap::from([(session_id, tracked)])));

        let error = persist_runtime_context_observation(
            &active,
            &persistence,
            &store,
            session_id,
            0,
            258_400,
            chrono::Utc::now(),
        )
        .await
        .expect_err("failed durable write rejects runtime observation");
        assert!(
            error
                .to_string()
                .contains("c4a runtime observation failure")
        );

        let active_guard = active.read().await;
        let live = &active_guard
            .get(&session_id)
            .expect("live session remains")
            .session;
        assert_eq!(live.context_window, Some(272_000));
        assert_eq!(live.resolved_context_budget.as_ref(), Some(&prior_budget));
        drop(active_guard);
        let durable = store
            .lock()
            .await
            .get_session(session_id)
            .expect("read durable session")
            .expect("durable session remains");
        assert_eq!(durable.context_window, Some(272_000));
        let durable_budget = durable
            .resolved_context_budget
            .expect("durable provenance remains");
        assert_eq!(durable_budget.active_tokens, prior_budget.active_tokens);
        assert_eq!(durable_budget.evidence, prior_budget.evidence);
    }

    #[test]
    fn test_live_context_state_codex_includes_system_and_tool_baseline() {
        let mut tracked = build_tracked(
            SessionProvider::Codex,
            12_000,
            0,
            0,
            0,
            ContextUsageConfidence::Full,
            Some(272_000),
        );
        tracked.session.model = Some("gpt-5.4".to_string());
        tracked.codex_context_tokens = 12_000;

        let (_numerator, pct, _confidence, _daemon_total, window) = live_context_state(&tracked);

        assert_eq!(window, 272_000);
        assert!((pct - 12_000.0 * 100.0 / 272_000.0).abs() < 1e-9);
        assert_eq!(rotation_context_pct(&tracked, pct), 0.0);
    }

    #[test]
    fn test_context_fill_pct_for_tracked_includes_codex_baseline() {
        let mut tracked = build_tracked(
            SessionProvider::Codex,
            12_000,
            0,
            0,
            0,
            ContextUsageConfidence::Full,
            Some(258_400),
        );
        tracked.session.model = Some("gpt-5.4".to_string());
        tracked.codex_context_tokens = 12_000;

        assert_eq!(
            context_fill_pct_for_tracked(&tracked),
            Some(12_000.0 * 100.0 / 258_400.0)
        );
    }

    /// API Harness sessions consume the exact typed budget selected at launch.
    #[test]
    fn test_live_context_state_harness_uses_typed_budget() {
        let mut tracked = build_tracked(
            SessionProvider::Harness,
            129_200,
            0,
            0,
            0,
            ContextUsageConfidence::Full,
            Some(1_050_000),
        );
        tracked.session.model = Some("gpt-5.4".to_string());

        let (_numerator, _pct, _confidence, _daemon_total, window) = live_context_state(&tracked);

        assert_eq!(window, 1_050_000);
    }

    /// Antigravity: same delta-estimator behavior as Codex.
    #[test]
    fn test_live_context_state_antigravity_api_primary_with_delta() {
        let tracked = build_tracked(
            SessionProvider::Antigravity,
            1000,
            500,
            200,
            0,
            ContextUsageConfidence::Partial,
            None,
        );
        let (numerator, _pct, confidence, _daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(numerator, 1700);
        assert!(matches!(confidence, ContextUsageConfidence::Partial));
    }

    /// CodexAppServer uses the same native current-window telemetry as Codex
    /// once it receives a `thread/tokenUsage/updated` notification.
    #[test]
    fn test_live_context_state_codexappserver_uses_current_window_usage() {
        let mut tracked = build_tracked(
            SessionProvider::CodexAppServer,
            1000,
            500,
            200,
            0,
            ContextUsageConfidence::Full,
            Some(258_400),
        );
        tracked.codex_context_tokens = 130_492;
        tracked.session.model = Some("gpt-5".to_string());

        let (numerator, pct, confidence, _daemon_total, window) = live_context_state(&tracked);
        assert_eq!(numerator, 130_492);
        assert_eq!(window, 258_400);
        assert!((pct - 130_492.0 * 100.0 / 258_400.0).abs() < 1e-9);
        assert!(matches!(confidence, ContextUsageConfidence::Full));
        assert!(tracked.authorizes_threshold_rotation());
    }

    /// Harness: named-variant guard.
    #[test]
    fn test_live_context_state_harness_preserves_existing_behavior() {
        let tracked = build_tracked(
            SessionProvider::Harness,
            1000,
            500,
            200,
            0,
            ContextUsageConfidence::Full,
            None,
        );
        let (numerator, _pct, _confidence, _daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(numerator, 1700);
    }

    /// Local: BPE-primary — ignores `live_input_tokens`.
    #[test]
    fn test_live_context_state_local_bpe_primary() {
        let tracked = build_tracked(
            SessionProvider::Local,
            1000,
            500,
            200,
            0,
            ContextUsageConfidence::Full,
            None,
        );
        let (numerator, _pct, confidence, _daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(numerator, 700);
        assert!(matches!(confidence, ContextUsageConfidence::Counted));
    }

    /// Claude pre-first-turn (no API data yet): falls back to BPE.
    #[test]
    fn test_live_context_state_claude_bpe_fallback_before_first_turn() {
        let tracked = build_tracked(
            SessionProvider::Claude,
            0, // no API data yet
            300,
            200,
            0,
            ContextUsageConfidence::Missing,
            None,
        );
        let (numerator, _pct, confidence, _daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(numerator, 500, "Claude pre-API: daemon_total");
        assert!(matches!(confidence, ContextUsageConfidence::Counted));
    }

    /// No API data, no daemon counts: (0, Missing).
    #[test]
    fn test_live_context_state_missing_when_nothing_reported() {
        let tracked = build_tracked(
            SessionProvider::Claude,
            0,
            0,
            0,
            0,
            ContextUsageConfidence::Missing,
            None,
        );
        let (numerator, pct, confidence, _daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(numerator, 0);
        assert_eq!(pct, 0.0);
        assert!(matches!(confidence, ContextUsageConfidence::Missing));
    }

    /// Claude consumes the exact typed active-session budget without a second
    /// model lookup at the percentage boundary.
    #[test]
    fn test_live_context_state_claude_uses_typed_200k_budget() {
        let mut tracked = build_tracked(
            SessionProvider::Claude,
            100_000, // 100k tokens
            0,
            0,
            0,
            ContextUsageConfidence::Full,
            Some(200_000), // explicit 200k window
        );
        // Model metadata cannot replace the already resolved 200k denominator.
        tracked.session.model = Some("claude-opus-4-7-200k".to_string());
        let (_numerator, pct, _confidence, _daemon_total, window) = live_context_state(&tracked);
        assert_eq!(window, 200_000);
        assert!(
            (pct - 50.0).abs() < 1e-9,
            "100k / 200k should be 50.0%, got {pct}"
        );
    }

    /// Claude with stale daemon snapshot: snapshot value doesn't leak into
    /// the numerator. Guards against regression where Claude reads
    /// daemon_tokens_at_last_api_update.
    #[test]
    fn test_live_context_state_claude_ignores_daemon_snapshot() {
        let tracked = build_tracked(
            SessionProvider::Claude,
            5_000,
            10_000, // daemon has grown
            2_000,
            8_000, // snapshot at last API report
            ContextUsageConfidence::Full,
            None,
        );
        let (numerator, _pct, _confidence, _daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(
            numerator, 5_000,
            "Claude must use pure live_input_tokens regardless of daemon delta"
        );
    }

    // ── Staleness tests ───────────────────────────────────────────────────

    /// Helper: set `last_usage_update` to a point `secs_ago` seconds in the
    /// past on a pre-built TrackedSession. Built on top of
    /// `tokio::time::Instant::now() - Duration::from_secs(secs_ago)` so tests
    /// can simulate wall-clock gaps without real `sleep` calls.
    fn with_last_usage_secs_ago(tracked: &mut TrackedSession, secs_ago: u64) {
        let now = tokio::time::Instant::now();
        tracked.last_usage_update = Some(now - std::time::Duration::from_secs(secs_ago));
    }

    /// Claude + Running: once >60s has elapsed since the last API usage
    /// update, confidence flips `Full` → `Stale` but the numerator remains
    /// pinned to the last API-reported `live_input_tokens`.
    #[test]
    fn test_stale_confidence_after_60s() {
        let mut tracked = build_tracked(
            SessionProvider::Claude,
            1_000, // live_input_tokens (API value remains authoritative)
            5_000, // daemon_input_tokens must NOT replace the API numerator
            2_000, // daemon_output_tokens
            0,
            ContextUsageConfidence::Full,
            None,
        );
        // Session must be Running for staleness to fire; build_tracked sets that.
        assert!(matches!(tracked.session.status, SessionStatus::Running));
        with_last_usage_secs_ago(&mut tracked, 61);

        let (numerator, _pct, confidence, daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(
            numerator, 1_000,
            "Stale Claude must preserve the last API-reported numerator"
        );
        assert_eq!(daemon_total, 7_000);
        assert!(
            matches!(confidence, ContextUsageConfidence::Stale),
            "Expected Stale confidence, got {:?}",
            confidence
        );
    }

    /// Within the 60s window, a Claude+Running session preserves the
    /// `Full`/`Partial` confidence and the API numerator.
    #[test]
    fn test_no_stale_within_window() {
        let mut tracked = build_tracked(
            SessionProvider::Claude,
            1_000,
            5_000,
            2_000,
            0,
            ContextUsageConfidence::Full,
            None,
        );
        with_last_usage_secs_ago(&mut tracked, 30);
        let (numerator, _pct, confidence, _daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(
            numerator, 1_000,
            "API numerator preserved within 60s window"
        );
        assert!(matches!(confidence, ContextUsageConfidence::Full));
    }

    /// Other BPE/estimator providers never flip to `Stale` regardless of idle time.
    #[test]
    fn test_no_stale_for_other_estimator_providers() {
        for provider in [
            SessionProvider::Antigravity,
            SessionProvider::Harness,
            SessionProvider::Local,
        ] {
            let mut tracked = build_tracked(
                provider,
                1_000,
                5_000,
                2_000,
                0,
                ContextUsageConfidence::Full,
                None,
            );
            with_last_usage_secs_ago(&mut tracked, 3600); // 1 hour idle
            let (_numerator, _pct, confidence, _daemon_total, _window) =
                live_context_state(&tracked);
            assert!(
                !matches!(confidence, ContextUsageConfidence::Stale),
                "Provider {:?} must never be downgraded to Stale, got {:?}",
                provider,
                confidence
            );
        }
    }

    /// Claude but status != Running: stale downgrade does NOT fire. A
    /// Completed session with an hour-old last-usage is done, not stale —
    /// signaling it would be visual noise.
    #[test]
    fn test_no_stale_for_non_running() {
        let mut tracked = build_tracked(
            SessionProvider::Claude,
            1_000,
            5_000,
            2_000,
            0,
            ContextUsageConfidence::Full,
            None,
        );
        tracked.session.status = SessionStatus::Completed;
        with_last_usage_secs_ago(&mut tracked, 3600);
        let (numerator, _pct, confidence, _daemon_total, _window) = live_context_state(&tracked);
        assert_eq!(numerator, 1_000);
        assert!(
            matches!(confidence, ContextUsageConfidence::Full),
            "Completed Claude session must keep Full confidence, got {:?}",
            confidence
        );
    }

    /// Claude + Running pre-first-usage (`last_usage_update == None`): NO
    /// `Stale` downgrade is applied — the existing `Missing`→`daemon_total`
    /// fallback path still covers cold-start because we never had API data to
    /// go stale on.
    #[test]
    fn test_no_stale_pre_first_usage() {
        let tracked = build_tracked(
            SessionProvider::Claude,
            0, // pre-first-turn, so live_input_tokens == 0
            5_000,
            2_000,
            0,
            ContextUsageConfidence::Missing,
            None,
        );
        // last_usage_update defaults to None from build_tracked.
        assert!(tracked.last_usage_update.is_none());
        let (numerator, _pct, confidence, _daemon_total, _window) = live_context_state(&tracked);
        // Pre-first-turn hits the `_ if daemon_total > 0` arm, producing
        // Counted (the BPE-fallback path) — Stale must NOT appear.
        assert_eq!(
            numerator, 7_000,
            "Pre-first-turn falls back to daemon_total"
        );
        assert!(
            matches!(confidence, ContextUsageConfidence::Counted),
            "Pre-first-turn fallback must be Counted, not Stale, got {:?}",
            confidence
        );
    }

    /// `Partial` confidence is eligible for the `Stale` downgrade on the
    /// same rules as `Full` — both represent API-reported data, both should
    /// be marked stale if the API goes quiet.
    #[test]
    fn test_stale_downgrades_partial_too() {
        let mut tracked = build_tracked(
            SessionProvider::Claude,
            1_000,
            5_000,
            2_000,
            0,
            ContextUsageConfidence::Partial,
            None,
        );
        with_last_usage_secs_ago(&mut tracked, 61);
        let (_numerator, _pct, confidence, _daemon_total, _window) = live_context_state(&tracked);
        assert!(
            matches!(confidence, ContextUsageConfidence::Stale),
            "Partial must also downgrade to Stale after 60s, got {:?}",
            confidence
        );
    }

    // ---- Shared context-fill-% formula (single source of truth) ----

    /// Build an idle/persisted `Session` (no live tracking) for read-path tests.
    fn persisted_session(
        provider: SessionProvider,
        model: &str,
        context_window: Option<u64>,
        total_input_tokens: Option<u64>,
        daemon_input_tokens: Option<u64>,
        daemon_output_tokens: Option<u64>,
    ) -> Session {
        let mut session = build_tracked(
            provider,
            0,
            0,
            0,
            0,
            ContextUsageConfidence::Missing,
            context_window,
        )
        .session;
        session.status = SessionStatus::Completed;
        session.model = Some(model.to_string());
        session.resolved_context_budget = context_window.map(|active_tokens| {
            rsi_common::ResolvedContextBudget::new(
                active_tokens,
                rsi_common::ContextCapacity::default(),
                rsi_common::CapabilityEvidence::legacy_unverified(),
            )
            .expect("positive persisted legacy budget")
        });
        session.total_input_tokens = total_input_tokens;
        session.daemon_input_tokens = daemon_input_tokens;
        session.daemon_output_tokens = daemon_output_tokens;
        session
    }

    #[test]
    fn formula_claude_200k_vs_1m() {
        // Same numerator, different windows → different pct (no Codex baseline).
        assert_eq!(context_fill_pct_formula(150_000, 200_000), Some(75.0));
        assert_eq!(context_fill_pct_formula(500_000, 1_000_000), Some(50.0));
    }

    #[test]
    fn formula_100_pct_clamp() {
        // Numerator exceeding the window cannot publish >100%.
        assert_eq!(context_fill_pct_formula(2_000_000, 1_000_000), Some(100.0));
    }

    #[test]
    fn formula_zero_is_measured_but_unknown_window_is_none() {
        // Presence is independent of the well-defined raw ratio for measured zero.
        assert_eq!(context_fill_pct_formula(0, 1_000_000), Some(0.0));
        assert_eq!(context_fill_pct_formula(500, 0), None);
        assert_eq!(context_fill_pct_formula(0, 0), None);
    }

    #[test]
    fn live_context_codex_zero_fraction_stale_and_persisted_parity() {
        let mut tracked = build_tracked(
            SessionProvider::Codex,
            7_979_643,
            500,
            200,
            0,
            ContextUsageConfidence::Full,
            Some(258_400),
        );
        assert_eq!(context_fill_pct_for_tracked(&tracked), None);
        for tokens in [0, 1, 12_000, 85_558, 224_418, 14_589, 30_501] {
            let event = StreamEvent {
                event_type: "codex_token_count".into(),
                data: serde_json::json!({"info":{
                    "last_token_usage":{"total_tokens":tokens},
                    "total_token_usage":{"total_tokens":7_979_643},
                    "model_context_window":258400,
                }}),
            };
            let usage = monitor::extract_codex_context_usage(&event).unwrap();
            record_codex_context_observation(&mut tracked, &event, &usage);
            let (numerator, pct, confidence, _, _) = live_context_state(&tracked);
            assert_eq!(numerator, tokens);
            assert_eq!(confidence, ContextUsageConfidence::Full);
            let expected = Some(tokens as f64 * 100.0 / 258_400.0);
            assert_eq!(Some(pct), expected);
            assert_eq!(context_fill_pct_for_tracked(&tracked), expected);
            assert_eq!(context_fill_pct_from_persisted(&tracked.session), expected);
            with_last_usage_secs_ago(&mut tracked, 61);
            let (stale_tokens, stale_pct, confidence, _, _) = live_context_state(&tracked);
            assert_eq!((stale_tokens, stale_pct), (tokens, pct));
            assert_eq!(confidence, ContextUsageConfidence::Stale);
        }
    }

    #[test]
    fn live_context_rollout_age_survives_late_ingestion() {
        let event = StreamEvent {
            event_type: "codex_token_count".into(),
            data: serde_json::json!({"observed_at":"2020-01-01T00:00:00Z"}),
        };
        assert!(codex_usage_observed_instant(&event).elapsed() > USAGE_STALENESS_WINDOW);
    }

    #[test]
    fn formula_codex_baseline_subtraction() {
        // Codex curve subtracts the 12k baseline from both numerator and window;
        // the straight ratio over the same inputs differs, proving the baseline
        // is applied.
        let codex = Some(codex_context_pct_used(112_000, 212_000));
        assert_eq!(codex, Some(50.0));
        let straight = context_fill_pct_formula(112_000, 212_000).unwrap();
        assert!(
            (straight - 50.0).abs() > 1.0,
            "straight ratio must differ from the Codex baseline curve, got {straight}"
        );
    }

    #[test]
    fn typed_legacy_scalar_is_preserved_without_inventing_repository_origin() {
        let session = persisted_session(
            SessionProvider::Claude,
            "claude-opus-4-7",
            Some(200_000),
            None,
            None,
            None,
        );
        let budget = crate::provider_capabilities::resolved_context_budget_for_session(&session);
        assert_eq!(budget.active_tokens, 200_000);
        assert_eq!(
            budget.evidence.source,
            rsi_common::CapabilitySource::LegacyUnverified
        );
        assert_eq!(
            budget.evidence.confidence,
            rsi_common::CapabilityConfidence::Degraded
        );
    }

    #[test]
    fn persisted_idle_legacy_window_remains_exact_but_degraded() {
        // C3 backfilled this scalar with explicit legacy-unverified provenance.
        // C4a must not silently relabel it as a repository fact on reopen.
        let session = persisted_session(
            SessionProvider::Claude,
            "claude-opus-4-7",
            Some(200_000),
            Some(500_000),
            None,
            None,
        );
        assert_eq!(context_fill_pct_from_persisted(&session), Some(100.0));
    }

    #[test]
    fn persisted_idle_completed_with_no_tokens_is_none() {
        let session = persisted_session(
            SessionProvider::Claude,
            "claude-opus-4-7",
            Some(1_000_000),
            None,
            None,
            None,
        );
        assert_eq!(context_fill_pct_from_persisted(&session), None);
    }

    #[test]
    fn persisted_idle_falls_back_to_daemon_bpe_total() {
        let session = persisted_session(
            SessionProvider::Claude,
            "claude-opus-4-7",
            Some(1_000_000),
            None,
            Some(300_000),
            Some(200_000),
        );
        // daemon_total = 500k over 1M → 50%.
        assert_eq!(context_fill_pct_from_persisted(&session), Some(50.0));
    }

    #[test]
    fn persisted_idle_codex_billing_and_fallback_budget_are_unknown() {
        // A billing total and fallback capacity cannot establish raw occupancy.
        let mut session = persisted_session(
            SessionProvider::Codex,
            "gpt-5.5",
            None,
            Some(142_000),
            None,
            None,
        );
        session.resolved_context_budget = Some(repository_fallback_budget(272_000));
        assert_eq!(
            context_fill_pct_from_persisted(&session),
            None,
            "billing total is not current context"
        );
    }

    #[test]
    fn persisted_idle_codex_prefers_current_context_tokens_over_cumulative_total() {
        let mut session = persisted_session(
            SessionProvider::Codex,
            "gpt-5.5",
            None,
            Some(16_776_424),
            None,
            None,
        );
        session.resolved_context_budget = Some(
            resolve_runtime_context_budget(
                SessionProvider::Codex,
                "gpt-5.5",
                None,
                272_000,
                chrono::Utc::now(),
            )
            .unwrap(),
        );
        // Latest Codex token-count current-window numerator. The cumulative
        // total above would clamp to 100%; use the same raw ratio as live telemetry.
        session.input_tokens = Some(93_094);

        assert_eq!(
            context_fill_pct_from_persisted(&session),
            Some(93_094.0 * 100.0 / 272_000.0)
        );
    }

    #[test]
    fn for_tracked_matches_live_pct_and_blanks_when_no_numerator() {
        let tracked = build_tracked(
            SessionProvider::Claude,
            500_000,
            0,
            0,
            0,
            ContextUsageConfidence::Full,
            Some(1_000_000),
        );
        let (_n, pct, _c, _d, _w) = live_context_state(&tracked);
        assert_eq!(context_fill_pct_for_tracked(&tracked), Some(pct));
        assert_eq!(context_fill_pct_for_tracked(&tracked), Some(50.0));

        // Fresh session with no numerator → None (blank bar).
        let empty = build_tracked(
            SessionProvider::Claude,
            0,
            0,
            0,
            0,
            ContextUsageConfidence::Missing,
            Some(1_000_000),
        );
        assert_eq!(context_fill_pct_for_tracked(&empty), None);
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::session::types::CompletedSession;
    use rsi_common::types::{
        ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::{RwLock, mpsc};

    fn retry_test_session(session_id: Uuid) -> Session {
        let now = chrono::Utc::now();
        Session {
            context_fill_pct: None,
            id: session_id,
            status: SessionStatus::Failed,
            session_kind: SessionKind::Task,
            provider: SessionProvider::Codex,
            context_usage_confidence: ContextUsageConfidence::Missing,
            rotation_depth: 0,
            retry_attempt: Some(1),
            max_retries: Some(2),
            created_at: now,
            updated_at: now,
            pinned_at: None,
            testing_needed_at: None,
            rotation_disabled_at: None,
            query: "retry timer test".to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            description: None,
            short_summary: None,
            working_dir: PathBuf::from("/tmp"),
            git_branch: None,
            model: None,
            claude_session_id: None,
            project_id: None,
            continued_from: None,
            tag: String::new(),
            tags: Vec::new(),
            parent_id: None,
            lead_session_id: None,
            is_eval: false,
            cost_usd: None,
            duration_ms: None,
            num_turns: None,
            input_tokens: None,
            output_tokens: None,
            context_window: None,
            resolved_context_budget: None,
            total_input_tokens: None,
            total_output_tokens: None,
            total_cache_creation_tokens: None,
            total_cache_read_tokens: None,
            stop_reason: None,
            daemon_input_tokens: None,
            daemon_output_tokens: None,
            handoff_filepath: None,
            active_task: None,
            group_id: None,
            scheduled_job_id: None,
            pipeline_artifact: None,
            workflow_id: None,
            workflow_id_override: None,
            pending_question: None,
            pending_archive: false,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            rating: None,
            harness_version_hash: None,
            test_passed: None,
            clippy_passed: None,
            turn_count: None,
            retry_count: None,
            approval_wait_ms: Some(0),
            approval_started_at: None,
            work_time_ms: None,
            sandbox_kind: None,
            sandbox_root: None,
            sandbox_branch: None,
            sandbox_cleanup_state: None,
            capability_class: None,
            topology_node_id: None,
            topology_iteration: 0,
            provider_cli_version: None,
            provider_capabilities: Vec::new(),
            thinking_tokens: None,
            service_tier: None,
            cache_creation_1h_tokens: None,
            cache_creation_5m_tokens: None,
            permission_denial_count: None,
            subagent_stats_json: None,
            queued_turn_count: None,
            terminal_reason: None,
        }
    }

    #[test]
    fn test_backoff_ms_default_cap() {
        let cap = 120_000;
        assert_eq!(backoff_ms(1, cap), 10_000); // 10s
        assert_eq!(backoff_ms(2, cap), 20_000); // 20s
        assert_eq!(backoff_ms(3, cap), 40_000); // 40s
        assert_eq!(backoff_ms(4, cap), 80_000); // 80s
        assert_eq!(backoff_ms(5, cap), 120_000); // capped at 2min
        assert_eq!(backoff_ms(10, cap), 120_000); // still capped
        assert_eq!(backoff_ms(0, cap), 10_000); // edge case: attempt 0 treated as 1
    }

    #[test]
    fn test_backoff_ms_custom_cap() {
        // Lower cap: backoff should never exceed 30s
        assert_eq!(backoff_ms(1, 30_000), 10_000);
        assert_eq!(backoff_ms(2, 30_000), 20_000);
        assert_eq!(backoff_ms(3, 30_000), 30_000); // capped
        assert_eq!(backoff_ms(4, 30_000), 30_000); // still capped
        // Higher cap: backoff can grow beyond default 120s
        assert_eq!(backoff_ms(5, 300_000), 160_000);
        assert_eq!(backoff_ms(6, 300_000), 300_000); // capped at 5min
    }

    #[test]
    fn terminal_monitor_wires_no_idle_after_retry_decision_before_c5() {
        let source = include_str!("monitor.rs");
        let terminal_tail = source
            .split("// C5 runs only after rotation and retry disposition")
            .nth(1)
            .expect("terminal post-finalization tail");
        let no_idle = terminal_tail
            .find(".enforce_master_no_idle_for_invocation(")
            .expect("no-idle interlock call");
        let c5 = terminal_tail
            .find(".maybe_autofile_terminal_failure(session_id, c5_disposition)")
            .expect("C5 terminal issue call");
        assert!(no_idle < c5, "liveness recovery must precede C5 filing");
    }

    #[tokio::test]
    async fn retry_timer_sets_fired_marker_before_enqueue() {
        let session_id = Uuid::new_v4();
        let completed = Arc::new(RwLock::new(HashMap::from([(
            session_id,
            CompletedSession::for_test(retry_test_session(session_id)),
        )])));
        let (retry_tx, mut retry_rx) = mpsc::channel(1);
        let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

        spawn_retry_timer(
            Arc::clone(&completed),
            retry_tx,
            session_id,
            0,
            cancel_rx,
            "test retry cancelled",
        );

        let queued = tokio::time::timeout(std::time::Duration::from_millis(100), retry_rx.recv())
            .await
            .expect("retry timer should enqueue")
            .expect("retry channel open");
        assert_eq!(queued, session_id);
        assert!(
            completed
                .read()
                .await
                .get(&session_id)
                .and_then(|cs| cs.retry_fired_at)
                .is_some(),
            "timer fire must leave an in-memory marker for watch suppression"
        );
    }

    #[test]
    fn test_classify_retry_zero_events() {
        let result = classify_retry_eligibility(
            &MonitorBreakReason::StreamClosed,
            false, // no events
            false, // no meaningful output
            Some(1),
            &[],
        );
        assert!(result.is_some());
        assert!(result.unwrap().contains("zero events"));
    }

    #[test]
    fn test_classify_retry_interrupted_not_retryable() {
        let result =
            classify_retry_eligibility(&MonitorBreakReason::Interrupted, true, true, Some(0), &[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_classify_retry_clean_completion_not_retryable() {
        let result =
            classify_retry_eligibility(&MonitorBreakReason::Result, true, true, Some(0), &[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_classify_retry_stall_timeout_always_retryable() {
        // StallTimeout is retryable regardless of output/events
        let result =
            classify_retry_eligibility(&MonitorBreakReason::StallTimeout, true, true, Some(0), &[]);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "stall timeout");

        // Also retryable with no events
        let result =
            classify_retry_eligibility(&MonitorBreakReason::StallTimeout, false, false, None, &[]);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "stall timeout");
    }
}

#[cfg(test)]
mod pending_question_producer_tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::store::manager_coordinator::ManagerDecisionDeliveryV2;
    use rsi_common::harness_manager::{ConfigureHarnessManagerRequestV1, HarnessManagerConfigV1};
    use rsi_common::harness_manager_v2::*;
    use rsi_common::types::{PendingQuestion, Project, QuestionItem, SessionKind};
    use serde_json::json;
    use std::time::Duration;

    async fn fixture() -> (
        SessionManager,
        tempfile::TempDir,
        HarnessManagerConfigV1,
        Uuid,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("question-producer.db")).unwrap();
        let stamp = chrono::Utc::now();
        let project = Uuid::new_v4();
        store
            .insert_project(&Project {
                id: project,
                name: "Question producer".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: stamp,
                updated_at: stamp,
            })
            .unwrap();
        let mut owner =
            crate::session::agent_verbs::tests::test_session(Uuid::new_v4(), dir.path().to_owned());
        owner.project_id = Some(project);
        owner.session_kind = SessionKind::Standard;
        owner.status = SessionStatus::Completed;
        store.insert_session(&owner).unwrap();
        let mut group = owner.clone();
        group.id = Uuid::new_v4();
        group.session_kind = SessionKind::Group;
        store.insert_session(&group).unwrap();
        let mut epic = owner.clone();
        epic.id = Uuid::new_v4();
        epic.parent_id = Some(group.id);
        epic.session_kind = SessionKind::Epic;
        store.insert_session(&epic).unwrap();
        let mut lead = owner.clone();
        lead.id = Uuid::new_v4();
        lead.session_kind = SessionKind::Feature;
        lead.provider = SessionProvider::Claude;
        lead.parent_id = Some(epic.id);
        lead.status = SessionStatus::WaitingApproval;
        store.insert_session(&lead).unwrap();
        let config = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: owner.id,
                epic_ids: Some(vec![epic.id]),
                expected_row_version: 0,
            })
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: config.row_version,
                expected_policy_version: 0,
                idempotency_key: "question-policy".into(),
                policy: ManagerPolicyV2::default(),
            })
            .unwrap();
        let manager = SessionManager::new(
            Arc::new(EventBus::new(16)),
            store,
            false,
            dir.path().join("daemon.sock"),
            None,
            vec![],
            RuntimeConfig::from_config(&Config::from_env()),
            dir.path().join("sandboxes"),
        )
        .unwrap();
        let id = lead.id;
        manager
            .active
            .write()
            .await
            .insert(id, TrackedSession::new_for_test(lead));
        (manager, dir, config, id)
    }

    fn question() -> PendingQuestion {
        PendingQuestion {
            questions: vec![QuestionItem {
                question: "Use the migration allocation?".into(),
                header: "Migration".into(),
                options: vec![],
                multi_select: false,
            }],
        }
    }

    fn provider_event(session: Uuid, sequence: i32, tool_id: &str) -> ConversationEvent {
        let stream = StreamEvent {
            event_type: "assistant".into(),
            data: json!({"message":{"content":[
                {"type":"tool_use","id":tool_id,"name":"AskUserQuestion","input":question()}
            ]}}),
        };
        let mut prior = sequence - 1;
        let mut events =
            SessionManager::convert_recognized_stream_event(&stream, session, &mut prior).unwrap();
        assert_eq!(events.len(), 1);
        let event = events.pop().unwrap();
        assert_eq!(event.tool_use_id.as_deref(), Some(tool_id));
        event
    }

    async fn publish(
        manager: &SessionManager,
        event: ConversationEvent,
    ) -> crate::error::Result<i64> {
        let (detected, persisted) = persist_tracked_provider_event(
            &manager.active,
            &manager.persistence,
            event.session_id,
            0,
            &event,
            None,
        )
        .await;
        assert_eq!(detected, Some(question()));
        persisted
    }

    async fn answer(
        manager: &SessionManager,
        config: &HarnessManagerConfigV1,
        session: Uuid,
        key: &str,
    ) -> ManagerDecisionDeliveryV2 {
        let store = manager.store.lock().await;
        store.manager_v2_refresh_question_decisions(config).unwrap();
        let decision_key = format!("question:{session}");
        let decision = store
            .manager_v2_record(config, "decision", &decision_key)
            .unwrap()
            .unwrap();
        store
            .manager_v2_prepare_decision_answer(
                &AnswerHarnessManagerDecisionRequestV2 {
                    project_id: config.project_id,
                    fence: ManagerFenceV2 {
                        scope_version: config.row_version,
                        policy_version: 1,
                    },
                    decision_key,
                    expected_row_version: decision.row_version,
                    target_digest: decision.payload["target_digest"].as_str().unwrap().into(),
                    answer: "Use the reserved version".into(),
                    idempotency_key: key.into(),
                },
                |_, _, _, _| panic!("provider question never invokes acceptance"),
            )
            .unwrap();
        store
            .manager_v2_claim_decision_delivery(config, Uuid::new_v4())
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn pending_question_producer_new_same_text_event_blocks_old_answer_before_persistence() {
        let (manager, _dir, config, session) = fixture().await;
        publish(&manager, provider_event(session, 1, "ask-old"))
            .await
            .unwrap();
        let old = answer(&manager, &config, session, "answer-old").await;
        manager.check_manager_decision_runtime(&old).await.unwrap();
        // Hold the actual Store mutex, leaving the new event queued in the
        // persistence worker after the monitor has detected+appended it.
        let store = manager.store.lock().await;
        let event = provider_event(session, 2, "ask-new");
        let active = manager.active.clone();
        let persistence = manager.persistence.clone();
        let producer = tokio::spawn(async move {
            persist_tracked_provider_event(&active, &persistence, session, 0, &event, None).await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if manager
                    .active
                    .read()
                    .await
                    .get(&session)
                    .unwrap()
                    .events
                    .last()
                    .is_some_and(|event| event.tool_use_id.as_deref() == Some("ask-new"))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            store.manager_v2_question_target(session).unwrap(),
            Some(old.target.clone())
        );
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            manager.check_manager_decision_runtime(&old),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("runtime_changed"));
        assert!(
            tokio::time::timeout(
                Duration::from_secs(5),
                manager.clear_delivered_manager_question(&old)
            )
            .await
            .unwrap()
            .is_err()
        );
        drop(store);
        let (detected, persisted) = tokio::time::timeout(Duration::from_secs(5), producer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detected, Some(question()));
        persisted.unwrap();
        assert!(
            manager
                .clear_delivered_manager_question(&old)
                .await
                .is_err()
        );
        let current = manager
            .store
            .lock()
            .await
            .manager_v2_question_target(session)
            .unwrap()
            .unwrap();
        assert_eq!(current["tool_use_id"], "ask-new");
        assert_ne!(current["event_id"], old.target["event_id"]);
    }

    #[tokio::test]
    async fn pending_question_producer_failed_insert_and_quiescent_cache_never_reuse_old_identity()
    {
        let (manager, _dir, config, session) = fixture().await;
        publish(&manager, provider_event(session, 1, "ask-old"))
            .await
            .unwrap();
        let old = answer(&manager, &config, session, "answer-old").await;
        manager.store.lock().await.conn.execute_batch("CREATE TRIGGER fail_new_question BEFORE INSERT ON conversation_events
            WHEN NEW.tool_use_id='ask-new' BEGIN SELECT RAISE(ABORT,'producer insert failpoint'); END;").unwrap();
        assert!(
            publish(&manager, provider_event(session, 2, "ask-new"))
                .await
                .unwrap_err()
                .to_string()
                .contains("producer insert failpoint")
        );
        let tracked = manager.active.write().await.remove(&session).unwrap();
        let mut cached = CompletedSession::for_test(tracked.session);
        cached.events = tracked.events;
        cached.events_hydrated = true;
        assert_eq!(cached.events.last().unwrap().id, 0);
        manager.completed.write().await.insert(session, cached);
        assert!(
            manager
                .check_manager_decision_runtime(&old)
                .await
                .unwrap_err()
                .to_string()
                .contains("runtime_changed")
        );
        assert!(
            manager
                .clear_delivered_manager_question(&old)
                .await
                .is_err()
        );
        let store = manager.store.lock().await;
        assert!(
            store
                .manager_v2_question_target(session)
                .unwrap_err()
                .to_string()
                .contains("identity_unavailable")
        );
        assert_eq!(
            store
                .get_session(session)
                .unwrap()
                .unwrap()
                .pending_question,
            Some(question())
        );
    }

    #[tokio::test]
    async fn pending_question_producer_current_answer_resume_clears_before_new_monitor_question() {
        let (manager, _dir, config, session) = fixture().await;
        publish(&manager, provider_event(session, 1, "ask-current"))
            .await
            .unwrap();
        let delivery = answer(&manager, &config, session, "answer-current").await;
        manager
            .check_manager_decision_runtime(&delivery)
            .await
            .unwrap();
        let tracked = manager.active.write().await.remove(&session).unwrap();
        let mut cached = CompletedSession::for_test(tracked.session);
        cached.events = tracked.events;
        manager.completed.write().await.insert(session, cached);
        manager
            .check_manager_decision_runtime(&delivery)
            .await
            .unwrap();
        let process = super::super::launch::install_controller_candidate_test_process(session);
        tokio::time::timeout(
            Duration::from_secs(30),
            manager.continue_manager_decision(delivery),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            process
                .productive_start_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        {
            let store = manager.store.lock().await;
            assert_eq!(store.manager_v2_question_target(session).unwrap(), None);
            assert_eq!(
                store
                    .get_session(session)
                    .unwrap()
                    .unwrap()
                    .pending_question,
                None
            );
        }
        // Feed the resumed provider's real monitor immediately. Its new
        // same-text question must survive the already completed old clear.
        super::super::launch::send_controller_candidate_test_event(session, StreamEvent {
            event_type: "assistant".into(), data: json!({"message":{"content":[
                {"type":"tool_use","id":"ask-resumed","name":"AskUserQuestion","input":question()}
            ]}}),
        }).await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if manager
                    .store
                    .lock()
                    .await
                    .manager_v2_question_target(session)
                    .ok()
                    .flatten()
                    .is_some_and(|target| target["tool_use_id"] == "ask-resumed")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        manager.interrupt_session(session).await.unwrap();
        super::super::launch::drop_controller_candidate_test_stream(session);
        tokio::time::timeout(Duration::from_secs(10), async {
            while manager.active.read().await.contains_key(&session) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        manager.persistence.barrier().await.unwrap();
        assert!(!process.alive.load(std::sync::atomic::Ordering::SeqCst));
        let store = manager.store.lock().await;
        assert_eq!(
            store.manager_v2_question_target(session).unwrap().unwrap()["tool_use_id"],
            "ask-resumed"
        );
    }
}
