//! Generalized agent conversation loop.
//!
//! Handles prompt assembly, streaming API calls, tool dispatch, and context compaction.

use crate::claude::StreamEvent;
use crate::error::DaemonError;
use crate::model_control::call_control::{ModelCallControl, ModelCallKind, ModelCallUsage};
use crate::sandbox::{WorktreeDigest, worktree_content_digest};
use crate::session::harness::agent_mail::{HarnessMailBoundary, HarnessMailDelivery};
use crate::session::harness::compaction::{auto_compact, hard_trim};
use crate::session::harness::errors::ProviderError;
use crate::session::harness::normalize::normalize_history;
use crate::session::harness::provider::ApiProvider;
use crate::session::harness::retry;
use crate::session::harness::tools::{HarnessToolRegistry, ToolExecutionMode, arguments};
use crate::session::harness::types::*;
use rsi_common::completion_gates::{COMPLETION_GATES_GATE_TURN_BUDGET, CompletionGates};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Stream event carrying one auto-compaction record (#796); persisted as a
/// `Compressed` conversation event.
pub const COMPACTION_STREAM_EVENT: &str = "compaction";

pub const COMPLETION_GATE_STREAM_EVENT: &str = "completion_gate";

const MAX_PARALLEL_TOOL_CALLS: usize = 8;

struct CompletionGateFailure {
    gate_index: usize,
    before_digest: WorktreeDigest,
    after_digest: WorktreeDigest,
    output: String,
    exit_code: Option<i32>,
    timed_out: bool,
}

enum PreparedToolCall {
    Executable {
        fingerprint: u64,
        args: serde_json::Value,
        repaired: bool,
    },
    Immediate {
        fingerprint: u64,
        message: ChatMessage,
        is_error: bool,
        emit_use: bool,
        emit_result: bool,
        use_input: serde_json::Value,
    },
    Duplicate {
        fingerprint: u64,
        source: usize,
        repaired: bool,
    },
}

/// Prompt for the tool-less final iteration (#1039).
const ITERATION_LIMIT_WRAP_UP_PROMPT: &str = "[harness] This is your last permitted step and \
tools are no longer available. Reply now with your final answer, or with a summary of what is \
done and what remains, in the format your task requires.";

/// Stream event recording the automatic final-answer retry (#1061).
pub(crate) const FINAL_ANSWER_RETRY_STREAM_EVENT: &str = "final_answer_retry";

/// Nudge appended for the one automatic retry of an empty, output-limited
/// final response (#1061).
const FINAL_ANSWER_RETRY_PROMPT: &str = "Your last response was empty because it hit the output \
limit. Make no tool calls; write your final answer now, concisely.";

/// Output-token ceiling for the final-answer retry call (#1061); the normal
/// per-call ceiling is 8192.
const FINAL_ANSWER_RETRY_MAX_TOKENS: u64 = 32_768;

/// Reasoning effort for the final-answer retry call (#1061).
const FINAL_ANSWER_RETRY_EFFORT: &str = "low";

/// Whether a provider stop reason means the output budget ran out (#1061):
/// `length` (OpenAI-compatible, OpenRouter), `max_tokens` (Anthropic),
/// `max_output_tokens` / `incomplete` (Responses API).
fn is_output_limit_stop(reason: Option<&str>) -> bool {
    reason.is_some_and(|reason| {
        matches!(
            reason.trim().to_ascii_lowercase().as_str(),
            "length" | "max_tokens" | "max_output_tokens" | "incomplete"
        )
    })
}

/// Why a harness turn ended without a normal final reply (#1039).
#[derive(Debug, Clone)]
enum HarnessLimitStop {
    /// The iteration budget ran out while the model was still calling tools.
    IterationLimit { max_iterations: u32 },
    /// The model answered with no tool calls and no text.
    EmptyResponse {
        provider_stop_reason: Option<String>,
        reasoning: Option<String>,
    },
}

impl HarnessLimitStop {
    fn code(&self) -> &'static str {
        match self {
            Self::IterationLimit { .. } => "iteration_limit",
            Self::EmptyResponse { .. } => "empty_response",
        }
    }

    fn message(&self) -> String {
        match self {
            Self::IterationLimit { max_iterations } => format!(
                "[harness] Stopped: the turn used its {max_iterations}-step budget without a \
                 final reply. Continue the session to resume from the last tool result."
            ),
            Self::EmptyResponse {
                provider_stop_reason,
                reasoning,
            } => {
                let mut text = format!(
                    "[harness] Stopped: the model returned an empty final response \
                     (provider stop reason: {}). Continue the session to retry.",
                    provider_stop_reason.as_deref().unwrap_or("none")
                );
                if let Some(reasoning) = reasoning.as_deref().map(str::trim)
                    && !reasoning.is_empty()
                {
                    let tail: String = reasoning.chars().take(8000).collect();
                    text.push_str("\n\nLast reasoning from the model:\n");
                    text.push_str(&tail);
                }
                text
            }
        }
    }
}

fn aborted_tool_result() -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error_msg: Some("Tool execution aborted".to_string()),
    }
}

/// Estimate tokens remaining in the context window.
fn estimate_remaining_tokens(history: &[ChatMessage], token_limit: u64) -> u64 {
    let used: u64 = history.iter().map(|m| m.estimated_tokens()).sum();
    token_limit.saturating_sub(used)
}

/// Hash a tool call for dedup.
fn hash_tool_call(name: &str, arguments: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    arguments.hash(&mut hasher);
    hasher.finish()
}

fn tool_result_message(call_id: &str, result: ToolResult) -> ChatMessage {
    let is_error = result.is_error();
    let has_typed_blocks = result.has_typed_blocks();
    let content = if is_error {
        if has_typed_blocks {
            // Typed blocks keep their structure; `error_msg` is set for
            // callers that only read the message.
            result.output
        } else {
            match result.error_msg {
                Some(error) => format!("Error: {error}"),
                None => format!("Error: {}", result.output),
            }
        }
    } else {
        result.output
    };
    if is_error {
        ChatMessage::tool_error_result(call_id, content)
    } else {
        ChatMessage::tool_result(call_id, content)
    }
}

fn hosted_tool_result_message(call: &ToolCall) -> ChatMessage {
    let arguments = serde_json::from_str::<serde_json::Value>(&call.arguments)
        .unwrap_or(serde_json::Value::Null);
    match &call.hosted_result {
        Some(result) => ChatMessage::tool_result_blocks(
            &call.id,
            vec![ToolContentBlock::ServerToolResult {
                result: result.clone(),
            }],
            false,
        ),
        None => ChatMessage::tool_error_result(
            &call.id,
            format!(
                "Error: hosted tool {} returned no result (input: {arguments})",
                call.name
            ),
        ),
    }
}

async fn append_process_notices(
    event_tx: &mpsc::Sender<StreamEvent>,
    history: &mut Vec<ChatMessage>,
    session_tag: &str,
    process_notices: Vec<String>,
) -> Result<(), DaemonError> {
    if process_notices.is_empty() {
        return Ok(());
    }
    let content = process_notices.join("\n");
    event_tx
        .send(StreamEvent {
            event_type: "background_process_notice".into(),
            data: json!({
                "session_id": session_tag,
                "message": content,
            }),
        })
        .await
        .map_err(|_| DaemonError::ChannelClosed)?;
    history.push(ChatMessage::user(content));
    Ok(())
}

#[cfg(test)]
mod tool_content_tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn block_result_survives_loop_handoff_without_exposing_image_in_event_text() {
        let message = tool_result_message(
            "image",
            ToolResult::from_blocks(
                vec![
                    ToolContentBlock::Text {
                        text: "preview".into(),
                    },
                    ToolContentBlock::Image {
                        media_type: "image/png".into(),
                        data: "aGVsbG8=".into(),
                        detail: None,
                        reference: None,
                        width: None,
                        height: None,
                    },
                ],
                true,
            ),
        );
        assert!(message.is_error);
        assert_eq!(message.tool_blocks().len(), 2);
        assert_eq!(message.visible_tool_text(), "preview\n[image: image/png]");
        assert!(!message.visible_tool_text().contains("aGVsbG8="));

        let legacy = tool_result_message(
            "failure",
            ToolResult {
                success: false,
                output: String::new(),
                error_msg: Some("failed".into()),
            },
        );
        assert_eq!(legacy.content, "Error: failed");
    }
}

/// #1000: one call's full prompt. Anthropic reports `input_tokens` without
/// cache reads/writes; the OpenAI-compatible providers (chat completions,
/// Responses) report a `prompt_tokens`/`input_tokens` that already includes
/// cached tokens, with `cache_read_tokens` as a subset of it.
fn full_prompt_tokens(provider_name: &str, usage: &TokenUsage) -> u64 {
    if provider_name == "anthropic" {
        usage
            .prompt_tokens
            .saturating_add(usage.cache_creation_tokens)
            .saturating_add(usage.cache_read_tokens)
    } else {
        usage.prompt_tokens
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_harness_loop(
    provider: Box<dyn ApiProvider>,
    tools: Arc<HarnessToolRegistry>,
    system_prompt: String,
    user_query: String,
    working_dir: PathBuf,
    model: String,
    reasoning_effort: Option<String>,
    event_tx: mpsc::Sender<StreamEvent>,
    cancel: CancellationToken,
    model_call_control: Arc<dyn ModelCallControl>,
    max_iterations: u32,                            // default 25
    token_limit: u64,                               // context window size for this model
    conversation_history: Option<Vec<ChatMessage>>, // for continue/rotation
) -> Result<(), DaemonError> {
    run_harness_loop_with_compact_budget(
        provider,
        tools,
        system_prompt,
        user_query,
        working_dir,
        model,
        reasoning_effort,
        event_tx,
        cancel,
        model_call_control,
        max_iterations,
        token_limit,
        conversation_history,
        None,
        None,
        None,
        true,
    )
    .await
}

/// [`run_harness_loop`] with an absolute live-context budget (#966): history
/// compacts at `min(75% of token_limit, compact_budget)`.
#[allow(clippy::too_many_arguments)]
pub async fn run_harness_loop_with_compact_budget(
    provider: Box<dyn ApiProvider>,
    tools: Arc<HarnessToolRegistry>,
    system_prompt: String,
    user_query: String,
    working_dir: PathBuf,
    model: String,
    reasoning_effort: Option<String>,
    event_tx: mpsc::Sender<StreamEvent>,
    cancel: CancellationToken,
    model_call_control: Arc<dyn ModelCallControl>,
    max_iterations: u32,
    token_limit: u64,
    conversation_history: Option<Vec<ChatMessage>>,
    compact_budget: Option<u64>,
    agent_mail_boundary: Option<Arc<dyn HarnessMailBoundary>>,
    completion_gates: Option<CompletionGates>,
    completion_gates_enabled: bool,
) -> Result<(), DaemonError> {
    let session_tag = uuid::Uuid::new_v4().to_string();
    let _process_turn = tools.process_turn_guard().await;

    // Phase A: Initialize conversation history
    let mut history: Vec<ChatMessage> = Vec::new();
    if let Some(prior) = conversation_history {
        history.extend(prior);
        // Ensure user message is appended if we're not just rotating with an empty query.
        if !user_query.trim().is_empty() {
            history.push(ChatMessage::user(&user_query));
        }
    } else {
        if !system_prompt.is_empty() {
            history.push(ChatMessage::system(&system_prompt));
        }
        history.push(ChatMessage::user(&user_query));
    }

    // Emit init event
    event_tx
        .send(StreamEvent {
            event_type: "system".into(),
            data: json!({
                "subtype": "init",
                "session_id": session_tag,
                "model": model,
            }),
        })
        .await
        .map_err(|_| DaemonError::ChannelClosed)?;

    let process_notices = tools.take_process_notices().await;
    append_process_notices(&event_tx, &mut history, &session_tag, process_notices).await?;

    let mut total_usage = TokenUsage::default();
    let mut hosted_tools_enabled = !provider.web_capabilities(&model).tools.is_empty();
    // Cumulative full-prompt tokens across all model calls in this run.
    // Anthropic: prompt_tokens (uncached) + cache_creation + cache_read.
    // OpenAI-compatible: prompt_tokens (already includes cached).
    // Emitted as "prompt_tokens_total" for the monitor.
    let mut cumulative_prompt_tokens: u64 = 0;
    tools.set_context_usage(0, token_limit);

    // #1039: why the turn ended without a normal final reply, so the stream
    // never shows a bare `Completed` with no assistant output.
    let mut limit_stop: Option<HarnessLimitStop> = None;
    let mut replied = false;
    // #1061: an empty final response cut off by the output limit gets one
    // automatic retry inside the same turn. The retry re-runs the current
    // iteration, so it is granted one extra loop step.
    let mut final_answer_retry_spent = false;
    let mut final_answer_retry_pending = false;
    let mut gate_retry_pending = false;
    let mut completion_gate_attempts = 0_u32;
    let mut last_completion_gate_failure: Option<CompletionGateFailure> = None;
    let completion_gates = completion_gates.filter(|gates| !gates.is_empty());
    let gate_iteration_budget = completion_gates.as_ref().map_or(0, |gates| {
        gates
            .max_attempts
            .saturating_mul(COMPLETION_GATES_GATE_TURN_BUDGET)
    });
    let mut ordinary_iterations = 0_u32;

    // Phase B: Enter tool-call loop
    'turns: for _ in 0..max_iterations
        .saturating_add(2)
        .saturating_add(gate_iteration_budget)
    {
        let iteration = ordinary_iterations;
        let final_answer_retry_turn = std::mem::take(&mut final_answer_retry_pending);
        let gate_retry_turn = std::mem::take(&mut gate_retry_pending);
        if iteration >= max_iterations && !gate_retry_turn && !final_answer_retry_turn {
            break;
        }
        if !final_answer_retry_turn && !gate_retry_turn {
            ordinary_iterations = ordinary_iterations.saturating_add(1);
        }
        // 1. Check cancellation (synchronous poll)
        if cancel.is_cancelled() {
            tracing::info!("Agent loop cancelled at iteration {iteration}");
            model_call_control.fail_pending("interrupted").await?;
            break;
        }

        // Fresh cache for each turn (dedup cache)
        let mut seen_tool_calls: HashMap<u64, (String, bool)> = HashMap::new();
        let compaction_focus = tools.take_compaction_focus().await;

        // 2. Auto-compaction check
        let compacted = auto_compact(
            &mut history,
            &*provider,
            &model,
            token_limit,
            compact_budget,
            compaction_focus.as_deref(),
            &cancel,
            Arc::clone(&model_call_control),
        )
        .await
        .map_err(|error| {
            DaemonError::Process(format!("Harness auto-compaction failed: {error}"))
        })?;
        // #796: persist the compaction boundary so a continued session
        // rebuilds summary plus kept tail, not the full raw transcript.
        if let Some(record) = compacted {
            // #974: a local-truncation fallback stays visible to the operator
            // instead of silently failing the turn.
            if let Some(fallback) = record.fallback.as_deref() {
                tracing::warn!(
                    fallback,
                    messages_before = record.messages_before,
                    "Harness auto-compaction used local truncation"
                );
            }
            event_tx
                .send(StreamEvent {
                    event_type: COMPACTION_STREAM_EVENT.into(),
                    data: json!({
                        "summary": record.summary,
                        "kept_messages": record.kept_messages,
                        "messages_before": record.messages_before,
                        "tokens_before": record.tokens_before,
                        "fallback": record.fallback,
                    }),
                })
                .await
                .map_err(|_| DaemonError::ChannelClosed)?;
        }
        if cancel.is_cancelled() {
            tracing::info!("Agent loop cancelled after compaction at iteration {iteration}");
            model_call_control.fail_pending("interrupted").await?;
            break;
        }

        // #1039: the last permitted iteration is a tool-less wrap-up so a
        // turn that used its whole budget still ends with a final message.
        let wrap_up_turn =
            max_iterations > 1 && iteration + 1 == max_iterations && !final_answer_retry_turn;
        if wrap_up_turn {
            history.push(ChatMessage::user(ITERATION_LIMIT_WRAP_UP_PROMPT));
        }

        // 3. Build ChatRequest
        let repair = normalize_history(&mut history);
        if repair.synthesized != 0 || repair.dropped != 0 {
            event_tx
                .send(StreamEvent {
                    event_type: "history_repaired".into(),
                    data: json!({
                        "synthesized": repair.synthesized,
                        "dropped": repair.dropped,
                    }),
                })
                .await
                .map_err(|_| DaemonError::ChannelClosed)?;
        }
        let effective_max_tokens = estimate_remaining_tokens(&history, token_limit);
        let request_max_tokens = if final_answer_retry_turn {
            effective_max_tokens.min(FINAL_ANSWER_RETRY_MAX_TOKENS)
        } else {
            effective_max_tokens.min(8192)
        };
        let mut tool_specs = if provider.supports_native_tools() {
            tools.specs()
        } else {
            Vec::new()
        };
        if hosted_tools_enabled {
            tool_specs.extend(tools.filter_hosted_specs(provider.web_capabilities(&model).specs()));
        }
        if wrap_up_turn || final_answer_retry_turn {
            tool_specs.clear();
        }
        let mut request = ChatRequest {
            messages: history.clone(),
            model: model.clone(),
            temperature: None,
            max_tokens: Some(request_max_tokens as u32),
            tools: tool_specs,
            stream: provider.supports_streaming(),
            reasoning_effort: if final_answer_retry_turn {
                Some(FINAL_ANSWER_RETRY_EFFORT.to_string())
            } else {
                reasoning_effort.clone()
            },
            context_editing: tools.context_editing_enabled(),
        };

        // 4. Call provider (streaming or blocking)
        let fingerprint_hint = format!(
            "{}:{}:{}:{}",
            request.model,
            request.messages.len(),
            request.max_tokens.unwrap_or_default(),
            request
                .messages
                .last()
                .map(|message| message.content.as_str())
                .unwrap_or_default()
        );
        let mut retries_done = 0;
        let mut prior_invocation_id = None;
        let mut last_retry_error: Option<ProviderError> = None;
        let mut claimed_mail: Option<Box<dyn HarnessMailDelivery>> = None;
        let (response, settlement) = loop {
            let admission = model_call_control
                .admit(
                    ModelCallKind::Primary,
                    &format!("turn:{iteration}:attempt:{retries_done}"),
                    &format!("{fingerprint_hint}:attempt:{retries_done}"),
                    prior_invocation_id,
                )
                .await;
            let admitted_call = match admission {
                Ok(call) => call,
                Err(DaemonError::PolicyDenied(reason)) if retries_done > 0 => {
                    let Some(error) = last_retry_error else {
                        return Err(DaemonError::PolicyDenied(reason));
                    };
                    tracing::info!(%reason, "model retry policy refused another Harness attempt");
                    event_tx
                        .send(StreamEvent {
                            event_type: "provider_error".into(),
                            data: json!({
                                "class": error.class, "http_status": error.http_status,
                                "retry_after_ms": error.retry_after_ms,
                                "detail_code": error.detail_code, "session_id": session_tag,
                            }),
                        })
                        .await
                        .map_err(|_| DaemonError::ChannelClosed)?;
                    return Err(error.into_daemon_error());
                }
                Err(error) => return Err(error),
            };
            let (mut settlement, execution) = admitted_call.into_parts();
            let model_invocation_id = settlement.invocation_id();
            if iteration > 0 && claimed_mail.is_none() {
                if let (Some(boundary), Some(model_invocation_id)) =
                    (&agent_mail_boundary, model_invocation_id)
                {
                    match boundary.claim(model_invocation_id).await {
                        Ok(Some(mut mail)) => {
                            mail.append(&mut history, &mut request);
                            claimed_mail = Some(mail);
                        }
                        Ok(None) => {}
                        Err(error) => tracing::warn!(
                            target: "agent_coordination",
                            session_id = %session_tag,
                            error = %error,
                            "Harness mid-turn mail claim failed; the message stays queued"
                        ),
                    }
                }
            }
            if cancel.is_cancelled() {
                if let Some(mut mail) = claimed_mail.take() {
                    mail.undo_append(&mut history, &mut request);
                    mail.record_rejected("agent_message_cancelled_before_dispatch")
                        .await;
                }
                model_call_control.fail_pending("interrupted").await?;
                break 'turns;
            }
            let emitted_chunk = Arc::new(AtomicBool::new(false));

            let response = if provider.supports_streaming() {
                let (chunk_tx, mut chunk_rx) = mpsc::channel::<StreamChunk>(64);
                let event_tx_clone = event_tx.clone();
                let tag = session_tag.clone();
                let emitted = Arc::clone(&emitted_chunk);

                let forward_task = tokio::spawn(async move {
                    while let Some(chunk) = chunk_rx.recv().await {
                        emitted.store(true, Ordering::Release);
                        if !chunk.delta_text.is_empty() {
                            let _ = event_tx_clone
                                .send(StreamEvent {
                                    event_type: "content_block_delta".into(),
                                    data: json!({
                                        "session_id": tag,
                                        "delta": { "type": "text_delta", "text": chunk.delta_text },
                                    }),
                                })
                                .await;
                        }
                    }
                });

                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        forward_task.abort();
                        let _ = forward_task.await;
                        if let Some(mut mail) = claimed_mail.take() {
                            mail.record_effect_possible().await;
                        }
                        model_call_control.fail(settlement, "interrupted").await?;
                        break 'turns;
                    }
                    result = provider.stream_chat(&request, chunk_tx, &cancel, execution) => {
                        forward_task.abort();
                        let _ = forward_task.await;
                        result
                    }
                }
            } else {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        if let Some(mut mail) = claimed_mail.take() {
                            mail.record_effect_possible().await;
                        }
                        model_call_control.fail(settlement, "interrupted").await?;
                        break 'turns;
                    }
                    result = provider.chat(&request, execution) => result,
                }
            };
            if let Some(mut mail) = claimed_mail.take() {
                mail.record_effect_possible().await;
            }

            let response = match response {
                Err(DaemonError::StreamFallbackRequired(reason)) => {
                    let failed_stream_invocation_id = settlement.invocation_id();
                    model_call_control
                        .fail(settlement, "stream_fallback_required")
                        .await?;
                    tracing::warn!(
                        %reason,
                        iteration,
                        "streaming attempt failed; requesting separately admitted blocking fallback"
                    );
                    if cancel.is_cancelled() {
                        break 'turns;
                    }

                    let fallback_call = model_call_control
                        .admit(
                            ModelCallKind::Primary,
                            &format!("turn:{iteration}:blocking_fallback"),
                            &format!("{fingerprint_hint}:blocking_fallback"),
                            failed_stream_invocation_id,
                        )
                        .await?;
                    let (fallback_settlement, fallback_execution) = fallback_call.into_parts();
                    settlement = fallback_settlement;

                    let mut fallback_request = request.clone();
                    fallback_request.stream = false;
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => {
                            model_call_control.fail(settlement, "interrupted").await?;
                            break 'turns;
                        }
                        result = provider.chat(&fallback_request, fallback_execution) => result,
                    }
                }
                other => other,
            };

            // 5. On provider error: retry logic
            match response {
                Ok(r) => break (r, settlement),
                Err(e) => {
                    let failed_invocation_id = settlement.invocation_id();
                    let provider_error = ProviderError::from_daemon_error(&e);
                    model_call_control
                        .fail(
                            settlement,
                            provider_error
                                .as_ref()
                                .map_or("provider_call_failed", |error| error.detail_code.as_str()),
                        )
                        .await?;
                    if provider_error
                        .as_ref()
                        .is_some_and(|error| error.http_status == Some(400))
                        && request.tools.iter().any(|tool| tool.kind.is_hosted())
                    {
                        let spec = request
                            .tools
                            .iter()
                            .find(|tool| tool.kind.is_hosted())
                            .expect("hosted tool checked above");
                        let failed_call = ToolCall {
                            id: format!("hosted-{}-failed", spec.name),
                            name: spec.name.clone(),
                            arguments: "{}".to_string(),
                            hosted: true,
                            hosted_result: None,
                        };
                        let mut assistant = ChatMessage::assistant("");
                        assistant.tool_calls.push(failed_call.clone());
                        history.push(assistant);
                        history.push(ChatMessage::tool_error_result(
                            &failed_call.id,
                            format!(
                                "Error: hosted tool {} was rejected by the provider",
                                spec.name
                            ),
                        ));
                        event_tx
                            .send(StreamEvent {
                                event_type: "tool_use".into(),
                                data: json!({
                                    "session_id": session_tag,
                                    "name": failed_call.name,
                                    "input": {},
                                    "id": failed_call.id,
                                }),
                            })
                            .await
                            .map_err(|_| DaemonError::ChannelClosed)?;
                        event_tx
                            .send(StreamEvent {
                                event_type: "tool_result".into(),
                                data: json!({
                                    "session_id": session_tag,
                                    "content": format!("Error: hosted tool {} was rejected by the provider", spec.name),
                                    "name": failed_call.name,
                                    "tool_use_id": failed_call.id,
                                    "is_error": true,
                                    "metadata": serde_json::Value::Null,
                                }),
                            })
                            .await
                            .map_err(|_| DaemonError::ChannelClosed)?;
                        hosted_tools_enabled = false;
                        continue 'turns;
                    }
                    if let Some(error) = provider_error.as_ref()
                        && !emitted_chunk.load(Ordering::Acquire)
                        && retry::delay(error, retries_done).is_some()
                    {
                        if retry::wait(error, retries_done, &cancel).await {
                            retries_done += 1;
                            prior_invocation_id = failed_invocation_id;
                            last_retry_error = Some(error.clone());
                            continue;
                        }
                        break 'turns;
                    }
                    tracing::error!(error = %e, iteration, "LLM call failed");

                    event_tx
                        .send(StreamEvent {
                            event_type: if provider_error.is_some() {
                                "provider_error"
                            } else {
                                "system"
                            }
                            .into(),
                            data: provider_error.map_or_else(
                                || {
                                    json!({
                                        "subtype": "error",
                                        "error": e.to_string(),
                                        "session_id": session_tag,
                                    })
                                },
                                |error| {
                                    json!({
                                        "class": error.class,
                                        "http_status": error.http_status,
                                        "retry_after_ms": error.retry_after_ms,
                                        "detail_code": error.detail_code,
                                        "session_id": session_tag,
                                    })
                                },
                            ),
                        })
                        .await
                        .map_err(|_| DaemonError::ChannelClosed)?;

                    return Err(e);
                }
            }
        };

        model_call_control
            .complete(
                settlement,
                ModelCallUsage {
                    input_tokens: Some(response.usage.prompt_tokens),
                    output_tokens: Some(response.usage.completion_tokens),
                    cache_creation_tokens: Some(response.usage.cache_creation_tokens),
                    cache_read_tokens: Some(response.usage.cache_read_tokens),
                    reasoning_tokens: Some(response.usage.reasoning_tokens),
                    estimated_cost_usd: response.usage.cost_usd,
                    ..ModelCallUsage::default()
                },
            )
            .await?;

        // Track tokens
        total_usage.prompt_tokens = response.usage.prompt_tokens;
        total_usage.completion_tokens += response.usage.completion_tokens;
        total_usage.cache_creation_tokens = response.usage.cache_creation_tokens;
        total_usage.cache_read_tokens = response.usage.cache_read_tokens;
        total_usage.reasoning_tokens += response.usage.reasoning_tokens;
        total_usage.total_tokens = total_usage.prompt_tokens + total_usage.completion_tokens;
        cumulative_prompt_tokens = cumulative_prompt_tokens
            .saturating_add(full_prompt_tokens(provider.name(), &response.usage));
        tools.set_context_usage(
            full_prompt_tokens(provider.name(), &response.usage),
            token_limit,
        );

        // 6. Parse tool calls
        if response.tool_calls.is_empty() {
            // Compose final reply
            if response.content.trim().is_empty()
                && !final_answer_retry_spent
                && is_output_limit_stop(response.stop_reason.as_deref())
            {
                // #1061: the last call spent its whole output budget on
                // reasoning. Retry once inside this turn, while the tool
                // results are still in context, with a final-answer nudge.
                final_answer_retry_spent = true;
                final_answer_retry_pending = true;
                history.push(ChatMessage::user(FINAL_ANSWER_RETRY_PROMPT));
                event_tx
                    .send(StreamEvent {
                        event_type: FINAL_ANSWER_RETRY_STREAM_EVENT.into(),
                        data: json!({
                            "session_id": session_tag,
                            "provider_stop_reason": response.stop_reason,
                        }),
                    })
                    .await
                    .map_err(|_| DaemonError::ChannelClosed)?;
                continue 'turns;
            }
            if response.content.trim().is_empty() {
                // #1039: an empty final response is not a completion. Surface
                // what the model did return (its reasoning) so the operator and
                // the parent see a clear terminal reason.
                limit_stop = Some(HarnessLimitStop::EmptyResponse {
                    provider_stop_reason: response.stop_reason.clone(),
                    reasoning: response.reasoning_content.clone(),
                });
            } else {
                if let Some(gates) = completion_gates.as_ref() {
                    if !completion_gates_enabled {
                        event_tx
                            .send(StreamEvent {
                                event_type: COMPLETION_GATE_STREAM_EVENT.into(),
                                data: json!({
                                    "session_id": session_tag,
                                    "gate": null,
                                    "status": "disabled",
                                    "attempt": 0,
                                    "exit": null,
                                    "tree_digest": null,
                                    "tree_digest_after": null,
                                    "skipped": false,
                                    "output": "",
                                    "timed_out": false,
                                }),
                            })
                            .await
                            .map_err(|_| DaemonError::ChannelClosed)?;
                    } else if completion_gate_attempts >= gates.max_attempts {
                        let failure = last_completion_gate_failure.as_ref();
                        event_tx
                            .send(StreamEvent {
                                event_type: COMPLETION_GATE_STREAM_EVENT.into(),
                                data: json!({
                                    "session_id": session_tag,
                                    "gate": failure.map(|failure| gates.gates[failure.gate_index].name.as_str()),
                                    "status": "exhausted",
                                    "attempt": gates.max_attempts,
                                    "exit": failure.and_then(|failure| failure.exit_code),
                                    "tree_digest": failure.and_then(|failure| failure.before_digest.event_value()),
                                    "tree_digest_after": failure.and_then(|failure| failure.after_digest.event_value()),
                                    "skipped": false,
                                    "output": failure.map_or("", |failure| failure.output.as_str()),
                                    "timed_out": failure.is_some_and(|failure| failure.timed_out),
                                }),
                            })
                            .await
                            .map_err(|_| DaemonError::ChannelClosed)?;
                        replied = true;
                        break;
                    } else {
                        let before_digest =
                            worktree_content_digest(&working_dir).map_err(|error| {
                                DaemonError::Process(format!(
                                    "Harness completion gate digest failed: {error}"
                                ))
                            })?;
                        if let Some(failure) = last_completion_gate_failure.as_ref()
                            && (before_digest == failure.before_digest
                                || before_digest == failure.after_digest)
                        {
                            completion_gate_attempts += 1;
                            event_tx
                                .send(StreamEvent {
                                    event_type: COMPLETION_GATE_STREAM_EVENT.into(),
                                    data: json!({
                                        "session_id": session_tag,
                                        "gate": gates.gates[failure.gate_index].name,
                                        "status": "skipped",
                                        "attempt": completion_gate_attempts,
                                        "exit": failure.exit_code,
                                        "tree_digest": before_digest.event_value(),
                                        "tree_digest_after": failure.after_digest.event_value(),
                                        "skipped": true,
                                        "output": failure.output,
                                        "timed_out": failure.timed_out,
                                    }),
                                })
                                .await
                                .map_err(|_| DaemonError::ChannelClosed)?;
                            let reason = if failure.timed_out {
                                "timed out".to_string()
                            } else {
                                format!(
                                    "exit {}",
                                    failure
                                        .exit_code
                                        .map_or("unknown".to_string(), |code| code.to_string())
                                )
                            };
                            let feedback = format!(
                                "completion gate `{}` did not run again because the worktree was \
                                 unchanged after the previous failure ({reason}).\n\n{}",
                                gates.gates[failure.gate_index].name, failure.output,
                            );
                            history.push(ChatMessage::user(rsi_common::daemon_message::wrap(
                                "completion-gate",
                                &feedback,
                            )));
                            gate_retry_pending = true;
                            continue 'turns;
                        }

                        let mut failed = None;
                        for (gate_index, gate) in gates.gates.iter().enumerate() {
                            let outcome = tools
                                .execute_completion_gate(gate, &working_dir, &cancel)
                                .await;
                            if outcome.cancelled {
                                break 'turns;
                            }
                            if !outcome.success {
                                failed = Some((gate_index, outcome));
                                break;
                            }
                        }

                        if let Some((gate_index, outcome)) = failed {
                            let after_digest =
                                worktree_content_digest(&working_dir).map_err(|error| {
                                    DaemonError::Process(format!(
                                        "Harness completion gate digest failed: {error}"
                                    ))
                                })?;
                            completion_gate_attempts += 1;
                            let failure = CompletionGateFailure {
                                gate_index,
                                before_digest,
                                after_digest,
                                output: outcome.output.clone(),
                                exit_code: outcome.exit_code,
                                timed_out: outcome.timed_out,
                            };
                            let reason = if outcome.timed_out {
                                "timed out".to_string()
                            } else {
                                format!(
                                    "exit {}",
                                    outcome
                                        .exit_code
                                        .map_or("unknown".to_string(), |code| code.to_string())
                                )
                            };
                            let feedback = format!(
                                "completion gate `{}` failed ({reason}). Fix the failure and try \
                                 to finish again; this gate will be rechecked at the next final reply.\n\n{}",
                                gates.gates[gate_index].name, outcome.output,
                            );
                            event_tx
                                .send(StreamEvent {
                                    event_type: COMPLETION_GATE_STREAM_EVENT.into(),
                                    data: json!({
                                        "session_id": session_tag,
                                        "gate": gates.gates[gate_index].name,
                                        "status": "failed",
                                        "attempt": completion_gate_attempts,
                                        "exit": outcome.exit_code,
                                        "tree_digest": before_digest.event_value(),
                                        "tree_digest_after": after_digest.event_value(),
                                        "skipped": false,
                                        "output": outcome.output,
                                        "timed_out": outcome.timed_out,
                                    }),
                                })
                                .await
                                .map_err(|_| DaemonError::ChannelClosed)?;
                            history.push(ChatMessage::user(rsi_common::daemon_message::wrap(
                                "completion-gate",
                                &feedback,
                            )));
                            last_completion_gate_failure = Some(failure);
                            gate_retry_pending = true;
                            continue 'turns;
                        }

                        event_tx
                            .send(StreamEvent {
                                event_type: COMPLETION_GATE_STREAM_EVENT.into(),
                                data: json!({
                                    "session_id": session_tag,
                                    "gate": null,
                                    "status": "passed",
                                    "attempt": completion_gate_attempts,
                                    "exit": 0,
                                    "tree_digest": before_digest.event_value(),
                                    "tree_digest_after": null,
                                    "skipped": false,
                                    "output": "",
                                    "timed_out": false,
                                }),
                            })
                            .await
                            .map_err(|_| DaemonError::ChannelClosed)?;
                    }
                }
                event_tx
                    .send(StreamEvent {
                        event_type: "assistant".into(),
                        data: json!({
                            "session_id": session_tag,
                            "message": {
                                "role": "assistant",
                                "content": [{"type": "text", "text": response.content}],
                                "usage": {
                                    "prompt_tokens_total": cumulative_prompt_tokens,
                                    "input_tokens": total_usage.prompt_tokens,
                                    "cache_creation_input_tokens": total_usage.cache_creation_tokens,
                                    "cache_read_input_tokens": total_usage.cache_read_tokens,
                                    "output_tokens": total_usage.completion_tokens,
                                    "reasoning_output_tokens": total_usage.reasoning_tokens,
                                },
                            },
                        }),
                    })
                    .await
                    .map_err(|_| DaemonError::ChannelClosed)?;
            }
            replied = limit_stop.is_none();
            break; // Complete
        }

        // 7. Execute tool calls
        let assistant_msg = ChatMessage {
            role: MessageRole::Assistant,
            content: response.content.clone(),
            tool_call_id: None,
            is_error: false,
            tool_calls: response.tool_calls.clone(),
        };
        history.push(assistant_msg);

        let mut call_index = 0;
        while call_index < response.tool_calls.len() {
            let first_call = &response.tool_calls[call_index];
            let sequential = tools
                .execution_mode(&first_call.name)
                .is_none_or(|mode| mode == ToolExecutionMode::Sequential);
            let batch_limit = if sequential {
                1
            } else {
                MAX_PARALLEL_TOOL_CALLS
            };
            let batch_start = call_index;
            call_index += 1;
            while call_index < response.tool_calls.len() && call_index - batch_start < batch_limit {
                let next_call = &response.tool_calls[call_index];
                let next_sequential = tools
                    .execution_mode(&next_call.name)
                    .is_none_or(|mode| mode == ToolExecutionMode::Sequential);
                if next_sequential != sequential {
                    break;
                }
                call_index += 1;
            }

            let batch = &response.tool_calls[batch_start..call_index];
            if sequential {
                seen_tool_calls.clear();
            }
            let mut prepared: Vec<PreparedToolCall> = Vec::with_capacity(batch.len());
            let mut batch_sources: HashMap<u64, usize> = HashMap::new();
            for tc in batch {
                let fingerprint = hash_tool_call(&tc.name, &tc.arguments);
                let cacheable = !tc.hosted
                    && tools
                        .execution_mode(&tc.name)
                        .is_some_and(|mode| mode == ToolExecutionMode::ParallelSafe);
                if cacheable && let Some(source) = batch_sources.get(&fingerprint) {
                    if let PreparedToolCall::Immediate {
                        message, is_error, ..
                    } = &prepared[*source]
                    {
                        prepared.push(PreparedToolCall::Immediate {
                            fingerprint,
                            message: message.clone(),
                            is_error: *is_error,
                            emit_use: false,
                            emit_result: false,
                            use_input: serde_json::Value::Null,
                        });
                    } else {
                        prepared.push(PreparedToolCall::Duplicate {
                            fingerprint,
                            source: *source,
                            repaired: matches!(
                                prepared[*source],
                                PreparedToolCall::Executable { repaired: true, .. }
                            ),
                        });
                    }
                    continue;
                }

                if tc.hosted {
                    // #792: the provider ran a hosted call; the session policy
                    // still decides whether its result is admitted. A denied or
                    // over-budget call settles as a visible tool-error row.
                    let denial = tools
                        .policy()
                        .and_then(|policy| policy.admit_call(&tc.name, true).err());
                    let message = if let Some(error) = denial {
                        ChatMessage::tool_error_result(&tc.id, format!("Error: {}", error.text()))
                    } else {
                        if let (Some(policy), Some(result)) = (tools.policy(), &tc.hosted_result) {
                            policy.record_result_bytes(result.to_string().len());
                        }
                        hosted_tool_result_message(tc)
                    };
                    let is_error = message.is_error;
                    let use_input = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                        .unwrap_or(serde_json::Value::Null);
                    prepared.push(PreparedToolCall::Immediate {
                        fingerprint,
                        message,
                        is_error,
                        emit_use: true,
                        emit_result: true,
                        use_input,
                    });
                    batch_sources.insert(fingerprint, prepared.len() - 1);
                    continue;
                }

                if cacheable
                    && let Some((cached_result, is_error)) = seen_tool_calls.get(&fingerprint)
                {
                    let message = if *is_error {
                        ChatMessage::tool_error_result(&tc.id, cached_result)
                    } else {
                        ChatMessage::tool_result(&tc.id, cached_result)
                    };
                    prepared.push(PreparedToolCall::Immediate {
                        fingerprint,
                        message,
                        is_error: *is_error,
                        emit_use: false,
                        emit_result: false,
                        use_input: serde_json::Value::Null,
                    });
                    if cacheable {
                        batch_sources.insert(fingerprint, prepared.len() - 1);
                    }
                    continue;
                }

                let parsed = match arguments::parse_tool_arguments(&tc.arguments) {
                    Ok(parsed) => parsed,
                    Err(message) => {
                        // Surface a parse error the model can act on instead of
                        // letting an empty object fail a schema "missing field".
                        let result_message =
                            ChatMessage::tool_error_result(&tc.id, format!("Error: {message}"));
                        prepared.push(PreparedToolCall::Immediate {
                            fingerprint,
                            message: result_message,
                            is_error: true,
                            emit_use: true,
                            emit_result: true,
                            use_input: serde_json::Value::Null,
                        });
                        if cacheable {
                            batch_sources.insert(fingerprint, prepared.len() - 1);
                        }
                        continue;
                    }
                };
                let repaired = parsed.repaired;
                let mut args = parsed.value;
                if let Some(schema) = tools.parameters_json(&tc.name)
                    && let Ok(schema) = serde_json::from_str::<serde_json::Value>(schema)
                {
                    arguments::coerce_with_schema(&mut args, &schema);
                }
                if cacheable {
                    batch_sources.insert(fingerprint, prepared.len());
                }
                prepared.push(PreparedToolCall::Executable {
                    fingerprint,
                    args,
                    repaired,
                });
            }

            let mut tasks = JoinSet::new();
            let mut results: Vec<Option<ToolResult>> = vec![None; prepared.len()];
            for (index, call) in prepared.iter().enumerate() {
                let PreparedToolCall::Executable { args, .. } = call else {
                    continue;
                };
                let tc = &batch[index];
                event_tx
                    .send(StreamEvent {
                        event_type: "tool_use".into(),
                        data: json!({
                            "session_id": session_tag,
                            "name": tc.name,
                            "input": args.clone(),
                            // Join key for pairing this call with its result.
                            "id": tc.id,
                        }),
                    })
                    .await
                    .map_err(|_| DaemonError::ChannelClosed)?;

                let tools = Arc::clone(&tools);
                let name = tc.name.clone();
                let args = args.clone();
                let working_dir = working_dir.clone();
                let cancel = cancel.clone();
                let event_tx = event_tx.clone();
                tasks.spawn(async move {
                    let result = tools
                        .execute_with_context(&name, args, &working_dir, &cancel, Some(event_tx))
                        .await;
                    (index, result)
                });
            }

            while let Some(joined) = tasks.join_next().await {
                if let Ok((index, result)) = joined {
                    results[index] = Some(result);
                }
            }

            let cancelled = cancel.is_cancelled();
            for (index, call) in prepared.into_iter().enumerate() {
                let tc = &batch[index];
                let (result_message, is_error, emit_use, emit_result, use_input) = match call {
                    PreparedToolCall::Executable {
                        fingerprint: _,
                        args,
                        repaired,
                    } => {
                        let result = results[index].clone().unwrap_or_else(aborted_tool_result);
                        let mut message = tool_result_message(&tc.id, result);
                        if repaired && !message.has_typed_tool_blocks() {
                            message.content.push('\n');
                            message.content.push_str(arguments::REPAIR_NOTE);
                        }
                        let is_error = message.is_error;
                        (message, is_error, true, true, args)
                    }
                    PreparedToolCall::Immediate {
                        fingerprint: _,
                        message,
                        is_error,
                        emit_use,
                        emit_result,
                        use_input,
                    } => (message, is_error, emit_use, emit_result, use_input),
                    PreparedToolCall::Duplicate {
                        fingerprint: _,
                        source,
                        repaired,
                    } => {
                        let result = results[source].clone().unwrap_or_else(aborted_tool_result);
                        let mut message = tool_result_message(&tc.id, result);
                        if repaired && !message.has_typed_tool_blocks() {
                            message.content.push('\n');
                            message.content.push_str(arguments::REPAIR_NOTE);
                        }
                        let is_error = message.is_error;
                        (message, is_error, false, true, serde_json::Value::Null)
                    }
                };

                if emit_use
                    && event_tx
                        .send(StreamEvent {
                            event_type: "tool_use".into(),
                            data: json!({
                                "session_id": session_tag,
                                "name": tc.name,
                                "input": use_input,
                                // Join key for pairing this call with its result.
                                "id": tc.id,
                            }),
                        })
                        .await
                        .is_err()
                {
                    return Err(DaemonError::ChannelClosed);
                }
                if emit_result {
                    let result_metadata = result_message.event_metadata();
                    let result_text = result_message.visible_tool_text();
                    event_tx
                        .send(StreamEvent {
                            event_type: "tool_result".into(),
                            data: json!({
                                "session_id": session_tag,
                                "content": result_text,
                                "name": tc.name,
                                "tool_use_id": tc.id,
                                "is_error": is_error,
                                "metadata": result_metadata,
                            }),
                        })
                        .await
                        .map_err(|_| DaemonError::ChannelClosed)?;
                }
                if tools
                    .execution_mode(&tc.name)
                    .is_some_and(|mode| mode == ToolExecutionMode::ParallelSafe)
                {
                    seen_tool_calls.insert(
                        hash_tool_call(&tc.name, &tc.arguments),
                        (result_message.content.clone(), is_error),
                    );
                }
                history.push(result_message);
            }

            if cancelled {
                model_call_control.fail_pending("interrupted").await?;
                break 'turns;
            }
        }

        let process_notices = tools.take_process_notices().await;
        append_process_notices(&event_tx, &mut history, &session_tag, process_notices).await?;

        hard_trim(&mut history, 200); // safety backstop
    }

    // Every non-error exit other than a reply or a cancellation is the
    // iteration budget running out mid tool loop.
    if limit_stop.is_none() && !replied && !cancel.is_cancelled() {
        limit_stop = Some(HarnessLimitStop::IterationLimit { max_iterations });
    }
    if let Some(stop) = limit_stop.as_ref() {
        tracing::warn!(
            session_id = %session_tag,
            stop_reason = stop.code(),
            "Harness turn ended without a final reply"
        );
        event_tx
            .send(StreamEvent {
                event_type: "assistant".into(),
                data: json!({
                    "session_id": session_tag,
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "text", "text": stop.message()}],
                        "usage": {
                            "prompt_tokens_total": cumulative_prompt_tokens,
                            "input_tokens": total_usage.prompt_tokens,
                            "cache_creation_input_tokens": total_usage.cache_creation_tokens,
                            "cache_read_input_tokens": total_usage.cache_read_tokens,
                            "output_tokens": total_usage.completion_tokens,
                            "reasoning_output_tokens": total_usage.reasoning_tokens,
                        },
                    },
                }),
            })
            .await
            .map_err(|_| DaemonError::ChannelClosed)?;
    }

    // Phase C & D: Emit final result
    event_tx
        .send(StreamEvent {
            event_type: "result".into(),
            data: json!({
                "subtype": "turn_completed",
                "stop_reason": limit_stop.as_ref().map_or("end_turn", HarnessLimitStop::code),
                "session_id": session_tag,
                "usage": {
                    "prompt_tokens_total": cumulative_prompt_tokens,
                    "input_tokens": total_usage.prompt_tokens,
                    "cache_creation_input_tokens": total_usage.cache_creation_tokens,
                    "cache_read_input_tokens": total_usage.cache_read_tokens,
                    "output_tokens": total_usage.completion_tokens,
                    "reasoning_output_tokens": total_usage.reasoning_tokens,
                },
            }),
        })
        .await
        .map_err(|_| DaemonError::ChannelClosed)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_control::call_control::StoreBackedModelCallControl;
    use crate::model_control::{AdmissionDecision, admit_invocation};
    use crate::session::harness::agent_mail::{TestHarnessMailBoundary, TestHarnessMailDelivery};
    use crate::session::harness::api_key::ApiCredential;
    use crate::session::harness::providers::openai_api::OpenAiApiProvider;
    use crate::store::Store;
    use rsi_common::completion_gates::CompletionGate;
    use rusqlite::params;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::{Barrier, Mutex};

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn test_estimate_remaining_tokens() {
        let history = vec![
            ChatMessage::system("sys prompt"),      // ~3 tokens
            ChatMessage::user("hello world"),       // ~3 tokens
            ChatMessage::assistant("hi there guy"), // ~3 tokens
        ];
        let remaining = estimate_remaining_tokens(&history, 100);
        // Total estimated ~9 tokens, so remaining should be ~91
        assert!(remaining > 80);
        assert!(remaining < 100);
    }

    struct FakeProvider {
        responses: Mutex<VecDeque<ChatResponse>>,
        streaming: bool,
        calls: AtomicUsize,
    }

    struct CapturingProvider {
        request_efforts: Arc<Mutex<Vec<Option<String>>>>,
        request_messages: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
        responses: Option<Arc<Mutex<VecDeque<ChatResponse>>>>,
    }

    struct GateProvider {
        requests: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
        responses: Arc<Mutex<VecDeque<ChatResponse>>>,
        working_dir: std::path::PathBuf,
        mutate_on_calls: Vec<usize>,
        calls: AtomicUsize,
    }

    struct FallbackProvider {
        stream_calls: Arc<AtomicUsize>,
        chat_calls: Arc<AtomicUsize>,
    }

    struct HostedFallbackProvider {
        requests: Arc<Mutex<Vec<ChatRequest>>>,
        outcomes: Mutex<VecDeque<crate::error::Result<ChatResponse>>>,
    }

    struct CancellingCompactionProvider {
        calls: Arc<AtomicUsize>,
        cancel: CancellationToken,
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    struct InstrumentedTool {
        tool_name: &'static str,
        mode: ToolExecutionMode,
        barrier: Option<Arc<Barrier>>,
        delay: Duration,
        wait_for_cancel: bool,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        trace: Arc<Mutex<Vec<String>>>,
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[async_trait::async_trait]
    impl crate::session::harness::tools::HarnessTool for InstrumentedTool {
        fn name(&self) -> &str {
            self.tool_name
        }

        fn description(&self) -> &str {
            "Instrumented parallel dispatch probe"
        }

        fn parameters_json(&self) -> &str {
            r#"{"type":"object","required":["label"],"properties":{"label":{"type":"string"}}}"#
        }

        fn execution_mode(&self) -> ToolExecutionMode {
            self.mode
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _working_dir: &std::path::Path,
        ) -> ToolResult {
            if self.wait_for_cancel {
                std::future::pending::<()>().await;
                unreachable!("pending cancellation probe returned");
            }
            ToolResult {
                success: true,
                output: "instrumented".to_string(),
                error_msg: None,
            }
        }

        async fn execute_with_context(
            &self,
            args: serde_json::Value,
            context: &crate::session::harness::tools::ToolContext,
        ) -> ToolResult {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            let label = args["label"].as_str().unwrap_or("unlabeled");
            self.trace.lock().await.push(format!("start {label}"));

            let result = if self.wait_for_cancel {
                context.cancel.cancelled().await;
                ToolResult {
                    success: false,
                    output: format!("cancelled {label}"),
                    error_msg: None,
                }
            } else {
                if let Some(barrier) = &self.barrier {
                    barrier.wait().await;
                }
                tokio::time::sleep(self.delay).await;
                ToolResult {
                    success: true,
                    output: format!("read {label}"),
                    error_msg: None,
                }
            };

            self.trace.lock().await.push(format!("finish {label}"));
            self.active.fetch_sub(1, Ordering::SeqCst);
            result
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    struct CancellingProbeTool {
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        trace: Arc<Mutex<Vec<String>>>,
        started: Arc<AtomicUsize>,
        finished: Arc<AtomicUsize>,
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[async_trait::async_trait]
    impl crate::session::harness::tools::HarnessTool for CancellingProbeTool {
        fn name(&self) -> &str {
            "cancelling_probe"
        }

        fn description(&self) -> &str {
            "Instrumented cancellation probe"
        }

        fn parameters_json(&self) -> &str {
            r#"{"type":"object","required":["label"],"properties":{"label":{"type":"string"}}}"#
        }

        fn execution_mode(&self) -> ToolExecutionMode {
            ToolExecutionMode::ParallelSafe
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _working_dir: &std::path::Path,
        ) -> ToolResult {
            std::future::pending::<()>().await;
            unreachable!("pending cancellation probe returned")
        }

        async fn execute_with_context(
            &self,
            args: serde_json::Value,
            context: &crate::session::harness::tools::ToolContext,
        ) -> ToolResult {
            self.started.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            let label = args["label"].as_str().unwrap_or("unlabeled");
            self.trace.lock().await.push(format!("start {label}"));

            context.cancel.cancelled().await;
            self.trace.lock().await.push(format!("finish {label}"));
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.finished.fetch_add(1, Ordering::SeqCst);
            ToolResult {
                success: false,
                output: format!("cancelled {label}"),
                error_msg: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl ApiProvider for FakeProvider {
        async fn chat(
            &self,
            _request: &ChatRequest,
            _execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| DaemonError::Process("missing fake response".to_string()))
        }

        async fn stream_chat(
            &self,
            request: &ChatRequest,
            _chunk_tx: mpsc::Sender<StreamChunk>,
            _cancel: &tokio_util::sync::CancellationToken,
            execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            self.chat(request, execution).await
        }

        fn supports_native_tools(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "fake"
        }

        fn supports_streaming(&self) -> bool {
            self.streaming
        }
    }

    #[async_trait::async_trait]
    impl ApiProvider for CapturingProvider {
        async fn chat(
            &self,
            request: &ChatRequest,
            _execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            self.request_efforts
                .lock()
                .await
                .push(request.reasoning_effort.clone());
            self.request_messages
                .lock()
                .await
                .push(request.messages.clone());
            if let Some(responses) = &self.responses {
                return responses.lock().await.pop_front().ok_or_else(|| {
                    DaemonError::Process("missing captured fake response".to_string())
                });
            }
            Ok(ChatResponse {
                content: "done".to_string(),
                tool_calls: vec![],
                usage: TokenUsage::default(),
                reasoning_content: None,
                stop_reason: None,
            })
        }

        async fn stream_chat(
            &self,
            request: &ChatRequest,
            _chunk_tx: mpsc::Sender<StreamChunk>,
            _cancel: &CancellationToken,
            execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            self.chat(request, execution).await
        }

        fn supports_native_tools(&self) -> bool {
            false
        }

        fn name(&self) -> &str {
            "capturing"
        }

        fn supports_streaming(&self) -> bool {
            false
        }
    }

    #[async_trait::async_trait]
    impl ApiProvider for GateProvider {
        async fn chat(
            &self,
            request: &ChatRequest,
            _execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            self.requests.lock().await.push(request.messages.clone());
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.mutate_on_calls.contains(&call) {
                std::fs::write(self.working_dir.join("tracked.txt"), "gate-pass")?;
            }
            self.responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| DaemonError::Process("missing gate response".to_string()))
        }

        async fn stream_chat(
            &self,
            request: &ChatRequest,
            _chunk_tx: mpsc::Sender<StreamChunk>,
            _cancel: &CancellationToken,
            execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            self.chat(request, execution).await
        }

        fn supports_native_tools(&self) -> bool {
            false
        }

        fn name(&self) -> &str {
            "gate-fake"
        }

        fn supports_streaming(&self) -> bool {
            false
        }
    }

    #[async_trait::async_trait]
    impl ApiProvider for FallbackProvider {
        async fn chat(
            &self,
            _request: &ChatRequest,
            _execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            self.chat_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                content: "fallback complete".to_string(),
                tool_calls: vec![],
                usage: TokenUsage {
                    prompt_tokens: 7,
                    completion_tokens: 2,
                    total_tokens: 9,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    reasoning_tokens: 0,
                    cost_usd: None,
                },
                reasoning_content: None,
                stop_reason: None,
            })
        }

        async fn stream_chat(
            &self,
            _request: &ChatRequest,
            _chunk_tx: mpsc::Sender<StreamChunk>,
            _cancel: &tokio_util::sync::CancellationToken,
            _execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            Err(DaemonError::StreamFallbackRequired(
                "deterministic fake stream failure".to_string(),
            ))
        }

        fn supports_native_tools(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "fallback-fake"
        }

        fn supports_streaming(&self) -> bool {
            true
        }
    }

    #[async_trait::async_trait]
    impl ApiProvider for HostedFallbackProvider {
        async fn chat(
            &self,
            request: &ChatRequest,
            _execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            self.requests.lock().await.push(request.clone());
            self.outcomes
                .lock()
                .await
                .pop_front()
                .expect("hosted fallback response")
        }

        async fn stream_chat(
            &self,
            request: &ChatRequest,
            _chunk_tx: mpsc::Sender<StreamChunk>,
            _cancel: &CancellationToken,
            execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            self.chat(request, execution).await
        }

        fn supports_native_tools(&self) -> bool {
            true
        }

        fn web_capabilities(
            &self,
            model: &str,
        ) -> crate::session::harness::provider::WebCapabilities {
            crate::session::harness::provider::WebCapabilities::anthropic_messages(
                "https://api.anthropic.com",
                model,
            )
        }

        fn name(&self) -> &str {
            "hosted-fallback-fake"
        }

        fn supports_streaming(&self) -> bool {
            false
        }
    }

    #[async_trait::async_trait]
    impl ApiProvider for CancellingCompactionProvider {
        async fn chat(
            &self,
            _request: &ChatRequest,
            _execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.cancel.cancel();
            Ok(ChatResponse {
                content: "compacted".to_string(),
                tool_calls: vec![],
                usage: TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 2,
                    total_tokens: 12,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    reasoning_tokens: 0,
                    cost_usd: None,
                },
                reasoning_content: None,
                stop_reason: None,
            })
        }

        async fn stream_chat(
            &self,
            request: &ChatRequest,
            _chunk_tx: mpsc::Sender<StreamChunk>,
            _cancel: &CancellationToken,
            execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            self.chat(request, execution).await
        }

        fn supports_native_tools(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "cancelling-compaction-fake"
        }

        fn supports_streaming(&self) -> bool {
            false
        }
    }

    #[async_trait::async_trait]
    impl ApiProvider for Arc<FakeProvider> {
        async fn chat(
            &self,
            request: &ChatRequest,
            execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            (**self).chat(request, execution).await
        }

        async fn stream_chat(
            &self,
            request: &ChatRequest,
            chunk_tx: mpsc::Sender<StreamChunk>,
            cancel: &tokio_util::sync::CancellationToken,
            execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<ChatResponse> {
            (**self)
                .stream_chat(request, chunk_tx, cancel, execution)
                .await
        }

        fn supports_native_tools(&self) -> bool {
            (**self).supports_native_tools()
        }

        fn name(&self) -> &str {
            (**self).name()
        }

        fn supports_streaming(&self) -> bool {
            (**self).supports_streaming()
        }
    }

    async fn root_permit(
        store: &Arc<Mutex<Store>>,
        session_id: uuid::Uuid,
    ) -> crate::model_control::AdmissionPermit {
        root_permit_keyed(store, session_id, "root").await
    }

    async fn root_permit_keyed(
        store: &Arc<Mutex<Store>>,
        session_id: uuid::Uuid,
        key_prefix: &str,
    ) -> crate::model_control::AdmissionPermit {
        let request = crate::model_control::ModelAdmissionRequest {
            purpose: rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
            provider: Some("Harness".to_string()),
            model: Some("gpt-5.4".to_string()),
            backend: Some("Harness".to_string()),
            effort: Some("high".to_string()),
            trigger: "test_harness_loop".to_string(),
            owner: rsi_common::model_control::InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            dedup_key: Some(format!("{key_prefix}:{session_id}")),
            request_fingerprint: Some(format!("sha256:{key_prefix}")),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
                Some("Harness"),
                Some("Harness"),
                Some("gpt-5.4"),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let bus = Arc::new(crate::bus::EventBus::new(8));
        match admit_invocation(store, request, &bus)
            .await
            .expect("admit root")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { .. } => panic!("unexpected duplicate root admission"),
        }
    }

    fn controller(
        store: &Arc<Mutex<Store>>,
        session_id: uuid::Uuid,
        permit: crate::model_control::AdmissionPermit,
    ) -> Arc<dyn ModelCallControl> {
        let event_bus = Arc::new(crate::bus::EventBus::new(8));
        let settlements = crate::model_control::call_control::ModelCallSettlementWorker::new(
            Arc::clone(store),
            Arc::clone(&event_bus),
        )
        .expect("settlement worker")
        .handle()
        .expect("settlement producer");
        Arc::new(StoreBackedModelCallControl::new(
            Arc::clone(store),
            event_bus,
            settlements,
            rsi_common::model_control::InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            "Harness",
            Some("gpt-5.4".to_string()),
            "Harness",
            Some("high".to_string()),
            "test_harness_loop",
            permit,
            rsi_common::model_control::ModelInvocationPurpose::SessionHarnessTurn,
            Some(rsi_common::model_control::ModelInvocationPurpose::SessionHarnessCompaction),
            crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
        ))
    }

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
            hosted: false,
            hosted_result: None,
        }
    }

    fn test_harness_mail_boundary(
        message: &str,
    ) -> (
        Arc<std::sync::Mutex<TestHarnessMailDelivery>>,
        TestHarnessMailBoundary,
    ) {
        let delivery = Arc::new(std::sync::Mutex::new(TestHarnessMailDelivery {
            message: message.to_string(),
            ..TestHarnessMailDelivery::default()
        }));
        let boundary = TestHarnessMailBoundary {
            delivery: Some(Arc::clone(&delivery)),
            ..TestHarnessMailBoundary::default()
        };
        (delivery, boundary)
    }

    fn tool_then_final_responses() -> Arc<Mutex<VecDeque<ChatResponse>>> {
        Arc::new(Mutex::new(VecDeque::from([
            ChatResponse {
                content: String::new(),
                tool_calls: vec![tool_call("mail-boundary", "unknown_tool")],
                usage: TokenUsage::default(),
                reasoning_content: None,
                stop_reason: None,
            },
            ChatResponse {
                content: "done".to_string(),
                tool_calls: Vec::new(),
                usage: TokenUsage::default(),
                reasoning_content: None,
                stop_reason: None,
            },
        ])))
    }

    fn hosted_search_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "web_search".into(),
            arguments: r#"{"query":"harness"}"#.into(),
            hosted: true,
            hosted_result: Some(json!({
                "type": "web_search_tool_result",
                "tool_use_id": id,
                "encrypted_content": "encrypted-content",
                "encrypted_index": "encrypted-index"
            })),
        }
    }

    fn scripted_response(tool_calls: Vec<ToolCall>, content: &str) -> ChatResponse {
        ChatResponse {
            content: content.into(),
            tool_calls,
            usage: TokenUsage::default(),
            reasoning_content: None,
            stop_reason: Some("end_turn".into()),
        }
    }

    async fn run_with_policy(
        policy: rsi_common::harness_tool_policy::HarnessToolPolicy,
        responses: Vec<ChatResponse>,
    ) -> (Vec<ChatRequest>, Vec<StreamEvent>) {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = HostedFallbackProvider {
            requests: Arc::clone(&requests),
            outcomes: Mutex::new(responses.into_iter().map(Ok).collect()),
        };
        let mut registry = HarnessToolRegistry::new();
        registry.set_policy(Arc::new(
            crate::session::harness::tools::policy::ToolPolicyRuntime::new(policy),
        ));
        let (event_tx, mut event_rx) = mpsc::channel(128);
        run_harness_loop(
            Box::new(provider),
            Arc::new(registry),
            String::new(),
            "search".to_string(),
            std::env::temp_dir(),
            "claude-sonnet-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            4,
            100_000,
            None,
        )
        .await
        .expect("the session continues past policy refusals");
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let requests = requests.lock().await.clone();
        (requests, events)
    }

    fn tool_results(events: &[StreamEvent]) -> Vec<&StreamEvent> {
        events
            .iter()
            .filter(|event| event.event_type == "tool_result")
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn web_access_disabled_sends_no_hosted_spec_and_denies_a_forced_hosted_call() {
        use rsi_common::harness_tool_policy::{HarnessToolPolicy, WebAccessMode};
        let (requests, events) = run_with_policy(
            HarnessToolPolicy {
                web_access: Some(WebAccessMode::Disabled),
                ..HarnessToolPolicy::default()
            },
            vec![
                // A misbehaving route returns a hosted call although no spec
                // was sent; the session must refuse to admit its result.
                scripted_response(vec![hosted_search_call("forced-1")], ""),
                scripted_response(Vec::new(), "done"),
            ],
        )
        .await;
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert!(
                request.tools.iter().all(|tool| !tool.kind.is_hosted()),
                "no hosted web spec may be sent while web_access is disabled"
            );
        }
        let results = tool_results(&events);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].data["is_error"], true);
        assert!(
            results[0].data["content"]
                .as_str()
                .is_some_and(|text| text.starts_with("Error: tool_policy_denied:"))
        );
        // The provider's hosted payload never reached the model as a result.
        assert!(results[0].data["metadata"]["server_tool_results"].is_null());
        assert!(events.iter().any(|event| event.event_type == "assistant"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn hosted_only_and_enabled_still_advertise_the_hosted_web_specs() {
        use rsi_common::harness_tool_policy::{HarnessToolPolicy, WebAccessMode};
        for mode in [WebAccessMode::HostedOnly, WebAccessMode::Enabled] {
            let (requests, _) = run_with_policy(
                HarnessToolPolicy {
                    web_access: Some(mode),
                    ..HarnessToolPolicy::default()
                },
                vec![scripted_response(Vec::new(), "done")],
            )
            .await;
            let hosted: Vec<_> = requests[0]
                .tools
                .iter()
                .filter(|tool| tool.kind.is_hosted())
                .map(|tool| tool.name.as_str())
                .collect();
            assert_eq!(hosted, ["web_search", "web_fetch"], "{mode:?}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn search_budget_exhaustion_is_a_typed_tool_error_and_the_session_continues() {
        use rsi_common::harness_tool_policy::{HarnessToolPolicy, ToolBudgets};
        let (requests, events) = run_with_policy(
            HarnessToolPolicy {
                budgets: ToolBudgets {
                    max_search_calls: Some(1),
                    ..ToolBudgets::default()
                },
                ..HarnessToolPolicy::default()
            },
            vec![
                scripted_response(vec![hosted_search_call("s1"), hosted_search_call("s2")], ""),
                scripted_response(Vec::new(), "done"),
            ],
        )
        .await;
        let results = tool_results(&events);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].data["is_error"], false);
        assert_eq!(results[1].data["is_error"], true);
        assert!(
            results[1].data["content"]
                .as_str()
                .is_some_and(|text| text.starts_with("Error: tool_budget_exhausted: search_calls"))
        );
        // Once the budget is used up the search spec is no longer sent, while
        // the untouched fetch spec stays.
        let names = |request: &ChatRequest| -> Vec<String> {
            request
                .tools
                .iter()
                .filter(|tool| tool.kind.is_hosted())
                .map(|tool| tool.name.clone())
                .collect()
        };
        assert_eq!(names(&requests[0]), ["web_search", "web_fetch"]);
        assert_eq!(names(&requests[1]), ["web_fetch"]);
        assert!(events.iter().any(|event| event.event_type == "assistant"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn hosted_tool_call_emits_pair_and_preserves_result() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = HostedFallbackProvider {
            requests: Arc::clone(&requests),
            outcomes: Mutex::new(VecDeque::from([
                Ok(ChatResponse {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "srvtool-hosted".into(),
                        name: "web_search".into(),
                        arguments: r#"{"query":"harness"}"#.into(),
                        hosted: true,
                        hosted_result: Some(json!({
                            "type": "web_search_tool_result",
                            "tool_use_id": "srvtool-hosted",
                            "encrypted_content": "encrypted-content",
                            "encrypted_index": "encrypted-index"
                        })),
                    }],
                    usage: TokenUsage::default(),
                    reasoning_content: None,
                    stop_reason: Some("tool_use".into()),
                }),
                Ok(ChatResponse {
                    content: "done".into(),
                    tool_calls: Vec::new(),
                    usage: TokenUsage::default(),
                    reasoning_content: None,
                    stop_reason: Some("end_turn".into()),
                }),
            ])),
        };
        let (event_tx, mut event_rx) = mpsc::channel(64);

        run_harness_loop(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "search".to_string(),
            std::env::temp_dir(),
            "claude-sonnet-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            2,
            100_000,
            None,
        )
        .await
        .expect("hosted tool call completes");

        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let uses: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == "tool_use")
            .collect();
        let results: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == "tool_result")
            .collect();
        assert_eq!(uses.len(), 1);
        assert_eq!(results.len(), 1);
        assert_eq!(uses[0].data["id"], "srvtool-hosted");
        assert_eq!(results[0].data["tool_use_id"], "srvtool-hosted");
        assert_eq!(
            results[0].data["metadata"]["server_tool_results"][0]["encrypted_content"],
            "encrypted-content"
        );
        assert_eq!(
            results[0].data["metadata"]["server_tool_results"][0]["encrypted_index"],
            "encrypted-index"
        );

        let requests = requests.lock().await;
        assert_eq!(
            requests[1].messages[1].tool_calls[0].hosted_result,
            Some(json!({
                "type": "web_search_tool_result",
                "tool_use_id": "srvtool-hosted",
                "encrypted_content": "encrypted-content",
                "encrypted_index": "encrypted-index"
            }))
        );
        assert!(
            requests[1].messages[2]
                .tool_blocks()
                .iter()
                .any(|block| matches!(
                    block,
                    ToolContentBlock::ServerToolResult { result }
                        if result["encrypted_content"] == "encrypted-content"
                            && result["encrypted_index"] == "encrypted-index"
                ))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn hosted_tool_rejection_records_failure_and_continues() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let rejected = ProviderError {
            class: crate::session::harness::errors::ProviderErrorClass::BadRequest,
            http_status: Some(400),
            retry_after_ms: None,
            detail_code: "request_rejected".into(),
        }
        .into_daemon_error();
        let provider = HostedFallbackProvider {
            requests: Arc::clone(&requests),
            outcomes: Mutex::new(VecDeque::from([
                Err(rejected),
                Ok(ChatResponse {
                    content: "recovered".into(),
                    tool_calls: Vec::new(),
                    usage: TokenUsage::default(),
                    reasoning_content: None,
                    stop_reason: Some("end_turn".into()),
                }),
            ])),
        };
        let (event_tx, mut event_rx) = mpsc::channel(64);

        run_harness_loop(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "search".to_string(),
            std::env::temp_dir(),
            "claude-sonnet-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            2,
            100_000,
            None,
        )
        .await
        .expect("hosted-tool rejection must not end the session");

        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let uses: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == "tool_use")
            .collect();
        let results: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == "tool_result")
            .collect();
        assert_eq!(uses.len(), 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].data["is_error"], true);
        assert_eq!(results[0].data["tool_use_id"], uses[0].data["id"]);
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "result"
                    && event.data["subtype"] == "turn_completed")
        );

        let requests = requests.lock().await;
        assert_eq!(requests[0].tools.len(), 2);
        assert_eq!(requests[1].tools.len(), 0);
    }

    async fn captured_harness_request_effort(reasoning_effort: Option<String>) -> Option<String> {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let control = controller(&store, session_id, permit);
        let request_efforts = Arc::new(Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            request_efforts: Arc::clone(&request_efforts),
            request_messages: Arc::new(Mutex::new(Vec::new())),
            responses: None,
        };
        let (event_tx, _event_rx) = mpsc::channel(32);

        run_harness_loop(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            reasoning_effort,
            event_tx,
            CancellationToken::new(),
            control,
            1,
            100_000,
            None,
        )
        .await
        .expect("loop succeeds");

        request_efforts
            .lock()
            .await
            .pop()
            .expect("provider receives primary request")
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn harness_loop_forwards_configured_launch_effort_and_preserves_none() {
        assert_eq!(
            captured_harness_request_effort(Some("xhigh".to_string())).await,
            Some("xhigh".to_string())
        );
        assert_eq!(captured_harness_request_effort(None).await, None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn resumed_orphan_call_is_repaired_before_provider_request() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            request_efforts: Arc::new(Mutex::new(Vec::new())),
            request_messages: Arc::clone(&requests),
            responses: None,
        };
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let mut prior_call = ChatMessage::assistant("calling");
        prior_call
            .tool_calls
            .push(tool_call("orphan", "unknown_tool"));

        run_harness_loop(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "resume".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            1,
            100_000,
            Some(vec![ChatMessage::user("previous"), prior_call]),
        )
        .await
        .expect("loop succeeds");

        let messages = requests.lock().await[0].clone();
        assert_eq!(messages[2].role, MessageRole::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("orphan"));
        assert_eq!(messages[2].content, "aborted");
        assert_eq!(messages[3].content, "resume");
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let repairs: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == "history_repaired")
            .collect();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].data, json!({"synthesized": 1, "dropped": 0}));
    }

    /// #796: a compaction is persisted and a continued session rebuilds the
    /// same summary-plus-tail history the live loop used.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used, clippy::too_many_lines)]
    async fn compaction_is_persisted_and_continuation_rebuilds_summary_and_tail() {
        use rsi_common::types::{ConversationEvent, EventType, Role};
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            request_efforts: Arc::new(Mutex::new(Vec::new())),
            request_messages: Arc::clone(&requests),
            responses: None,
        };
        let (event_tx, mut event_rx) = mpsc::channel(64);
        // A local backend is not paid background work, so its compaction
        // summary call is admitted (paid backends deny it by default).
        let event_bus = Arc::new(crate::bus::EventBus::new(8));
        let settlements = crate::model_control::call_control::ModelCallSettlementWorker::new(
            Arc::clone(&store),
            Arc::clone(&event_bus),
        )
        .expect("settlement worker")
        .handle()
        .expect("settlement producer");
        let control: Arc<dyn ModelCallControl> = Arc::new(StoreBackedModelCallControl::new(
            Arc::clone(&store),
            event_bus,
            settlements,
            rsi_common::model_control::InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            "Local",
            Some("qwen-test".to_string()),
            "ollama",
            None,
            "test_compaction_persistence",
            permit,
            rsi_common::model_control::ModelInvocationPurpose::SessionHarnessTurn,
            Some(rsi_common::model_control::ModelInvocationPurpose::SessionHarnessCompaction),
            crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
        ));
        let prior: Vec<ChatMessage> = (0..155)
            .map(|n| ChatMessage::user(format!("prior {n}")))
            .collect();

        run_harness_loop(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "resume".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            control,
            1,
            100_000,
            Some(prior),
        )
        .await
        .expect("loop succeeds");

        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let compaction = events
            .iter()
            .find(|event| event.event_type == COMPACTION_STREAM_EVENT)
            .expect("the compaction is emitted");
        assert_eq!(compaction.data["summary"], "done");
        assert_eq!(compaction.data["kept_messages"], 20);
        let live = requests.lock().await[1].clone();

        // Persist the transcript as the daemon does: user turns, the
        // compaction boundary, then the assistant reply.
        let mut sequence = 0;
        let mut persisted: Vec<ConversationEvent> = (0..155)
            .map(|n| format!("prior {n}"))
            .chain(std::iter::once("resume".to_string()))
            .map(|content| {
                sequence += 1;
                ConversationEvent {
                    id: 0,
                    session_id,
                    sequence,
                    event_type: EventType::Message,
                    role: Some(Role::User),
                    content,
                    tool_name: None,
                    tool_input: None,
                    created_at: chrono::Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                }
            })
            .collect();
        persisted.extend(
            crate::session::SessionManager::convert_recognized_stream_event(
                compaction,
                session_id,
                &mut sequence,
            )
            .expect("compaction is a recognized stream event"),
        );
        assert_eq!(
            persisted.last().map(|e| e.event_type),
            Some(EventType::Compressed)
        );

        let rebuilt = crate::session::SessionManager::events_to_harness_messages(&persisted);
        // A resumed run has no system prompt: the live request is exactly
        // this history.
        let live_history: Vec<(MessageRole, String)> =
            live.iter().map(|m| (m.role, m.content.clone())).collect();
        let rebuilt_history: Vec<(MessageRole, String)> = rebuilt
            .iter()
            .map(|m| (m.role, m.content.clone()))
            .collect();
        assert_eq!(rebuilt_history, live_history);
        // #1058: the first task message and the latest summarized-region user
        // instruction stay verbatim ahead of the summary.
        assert!(
            live.iter().any(|m| m.content == "prior 0"),
            "the provider request still carries the original task verbatim"
        );
        assert_eq!(rebuilt[0].content, "prior 0");
        assert_eq!(rebuilt[1].content, "prior 135");
        assert_eq!(rebuilt[2].content, "[Conversation summary: done]");
        assert_eq!(rebuilt.len(), 23);
    }

    /// #974 acceptance 3: the default paid controller denies the compaction
    /// summary call in Normal mode, so a 156-message history falls back to local
    /// truncation, the turn completes, and the emitted compaction event carries
    /// the fallback.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn policy_denied_compaction_truncates_and_the_turn_completes() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let control = controller(&store, session_id, permit);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            request_efforts: Arc::new(Mutex::new(Vec::new())),
            request_messages: Arc::clone(&requests),
            responses: None,
        };
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let prior: Vec<ChatMessage> = (0..155)
            .map(|n| ChatMessage::user(format!("prior {n}")))
            .collect();

        run_harness_loop(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "resume".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            control,
            1,
            100_000,
            Some(prior),
        )
        .await
        .expect("a policy-denied summary must not fail the turn");

        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let compaction = events
            .iter()
            .find(|event| event.event_type == COMPACTION_STREAM_EVENT)
            .expect("the truncation fallback is emitted as a compaction event");
        assert_eq!(compaction.data["fallback"], "policy_denied");
        assert_eq!(compaction.data["summary"], serde_json::Value::Null);
        assert_eq!(compaction.data["kept_messages"], 20);
        // Only the primary turn reached the provider; the denied summary never did.
        assert_eq!(requests.lock().await.len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn resumed_late_tool_result_is_relocated_before_provider_request() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            request_efforts: Arc::new(Mutex::new(Vec::new())),
            request_messages: Arc::clone(&requests),
            responses: None,
        };
        let (event_tx, _event_rx) = mpsc::channel(32);
        let mut prior_call = ChatMessage::assistant("calling");
        prior_call
            .tool_calls
            .push(tool_call("late", "unknown_tool"));
        let mut late_result = ChatMessage::tool_error_result("late", "failed");
        late_result.is_error = true;

        run_harness_loop(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "resume".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            1,
            100_000,
            Some(vec![prior_call, ChatMessage::user("middle"), late_result]),
        )
        .await
        .expect("loop succeeds");

        let messages = requests.lock().await[0].clone();
        assert_eq!(messages[0].role, MessageRole::Assistant);
        assert_eq!(messages[1].role, MessageRole::Tool);
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("late"));
        assert!(messages[1].is_error);
        assert_eq!(messages[2].content, "middle");
        assert_eq!(messages[3].content, "resume");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn failing_tool_result_is_flagged_in_the_next_provider_request() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            request_efforts: Arc::new(Mutex::new(Vec::new())),
            request_messages: Arc::clone(&requests),
            responses: Some(Arc::new(Mutex::new(VecDeque::from(vec![
                ChatResponse {
                    content: String::new(),
                    tool_calls: vec![tool_call("failure", "unknown_tool")],
                    usage: TokenUsage::default(),
                    reasoning_content: None,
                    stop_reason: None,
                },
                ChatResponse {
                    content: "done".to_string(),
                    tool_calls: Vec::new(),
                    usage: TokenUsage::default(),
                    reasoning_content: None,
                    stop_reason: None,
                },
            ])))),
        };
        let (event_tx, _event_rx) = mpsc::channel(32);

        run_harness_loop(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "run failing tool".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            2,
            100_000,
            None,
        )
        .await
        .expect("loop succeeds after reporting the tool failure");

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        let failure = requests[1]
            .iter()
            .find(|message| message.role == MessageRole::Tool)
            .expect("second request includes the failed tool result");
        assert_eq!(failure.tool_call_id.as_deref(), Some("failure"));
        assert!(failure.is_error);
        drop(requests);
    }

    /// #1039: a continued harness session is a new run of the same session; its
    /// second model call must not collide with the original run's second call.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn continued_harness_session_gets_fresh_model_invocation_keys() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        for (run, prefix) in ["launch", "continue-1", "continue-2"]
            .into_iter()
            .enumerate()
        {
            let permit = root_permit_keyed(&store, session_id, prefix).await;
            let requests = Arc::new(Mutex::new(Vec::new()));
            let provider = CapturingProvider {
                request_efforts: Arc::new(Mutex::new(Vec::new())),
                request_messages: Arc::clone(&requests),
                responses: Some(tool_then_final_responses()),
            };
            let (event_tx, _event_rx) = mpsc::channel(64);
            let history = (run > 0).then(|| vec![ChatMessage::user("previous")]);
            run_harness_loop(
                Box::new(provider),
                Arc::new(HarnessToolRegistry::new()),
                String::new(),
                format!("run {run}"),
                std::env::temp_dir(),
                "claude-opus-5".to_string(),
                None,
                event_tx,
                CancellationToken::new(),
                controller(&store, session_id, permit),
                4,
                100_000,
                history,
            )
            .await
            .unwrap_or_else(|error| panic!("run {run} must not conflict: {error}"));
            assert_eq!(requests.lock().await.len(), 2, "run {run}");
        }
    }

    fn tool_only_response(id: &str) -> ChatResponse {
        ChatResponse {
            content: String::new(),
            tool_calls: vec![tool_call(id, "unknown_tool")],
            usage: TokenUsage::default(),
            reasoning_content: None,
            stop_reason: None,
        }
    }

    /// Run the loop over scripted responses; returns the provider's captured
    /// requests and every emitted stream event.
    #[allow(clippy::expect_used)]
    async fn run_scripted(
        responses: Vec<ChatResponse>,
        max_iterations: u32,
    ) -> (Vec<Vec<ChatMessage>>, Vec<StreamEvent>) {
        let (requests, events, _efforts) =
            run_scripted_with_efforts(responses, max_iterations).await;
        (requests, events)
    }

    /// Like [`run_scripted`], also returning each request's reasoning effort.
    #[allow(clippy::expect_used)]
    async fn run_scripted_with_efforts(
        responses: Vec<ChatResponse>,
        max_iterations: u32,
    ) -> (Vec<Vec<ChatMessage>>, Vec<StreamEvent>, Vec<Option<String>>) {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let efforts = Arc::new(Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            request_efforts: Arc::clone(&efforts),
            request_messages: Arc::clone(&requests),
            responses: Some(Arc::new(Mutex::new(VecDeque::from(responses)))),
        };
        let (event_tx, mut event_rx) = mpsc::channel(128);
        run_harness_loop(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            max_iterations,
            100_000,
            None,
        )
        .await
        .expect("loop ends without error");
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let requests = requests.lock().await.clone();
        let efforts = efforts.lock().await.clone();
        (requests, events, efforts)
    }

    fn assistant_texts(events: &[StreamEvent]) -> Vec<String> {
        events
            .iter()
            .filter(|event| event.event_type == "assistant")
            .filter_map(|event| {
                event.data["message"]["content"][0]["text"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }

    fn result_stop_reason(events: &[StreamEvent]) -> String {
        events
            .iter()
            .find(|event| event.event_type == "result")
            .and_then(|event| event.data["stop_reason"].as_str())
            .expect("result event carries a stop_reason")
            .to_string()
    }

    fn gate_tree() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("gate worktree");
        std::fs::write(root.path().join("tracked.txt"), "gate-fail").expect("seed tracked file");
        for args in [
            vec!["init".to_string(), ".".to_string()],
            vec!["add".to_string(), "tracked.txt".to_string()],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .expect("git gate fixture")
                .status;
            assert!(status.success(), "git gate fixture failed");
        }
        root
    }

    fn gate(
        name: &str,
        command: &str,
        timeout_secs: u64,
        max_output_bytes: usize,
    ) -> CompletionGate {
        CompletionGate {
            name: name.into(),
            command: command.into(),
            timeout_secs,
            max_output_bytes,
        }
    }

    #[allow(clippy::expect_used)]
    async fn run_gates(
        responses: Vec<ChatResponse>,
        root: &tempfile::TempDir,
        gates: CompletionGates,
        completion_gates_enabled: bool,
        max_iterations: u32,
        mutate_on_calls: &[usize],
    ) -> (Vec<Vec<ChatMessage>>, Vec<StreamEvent>) {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = GateProvider {
            requests: Arc::clone(&requests),
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            working_dir: root.path().to_path_buf(),
            mutate_on_calls: mutate_on_calls.to_vec(),
            calls: AtomicUsize::new(0),
        };
        let (event_tx, mut event_rx) = mpsc::channel(256);
        run_harness_loop_with_compact_budget(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "start".to_string(),
            root.path().to_path_buf(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            max_iterations,
            100_000,
            None,
            None,
            None,
            Some(gates),
            completion_gates_enabled,
        )
        .await
        .expect("gate loop ends without error");
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let requests = requests.lock().await.clone();
        (requests, events)
    }

    fn gate_events(events: &[StreamEvent]) -> Vec<&StreamEvent> {
        events
            .iter()
            .filter(|event| event.event_type == COMPLETION_GATE_STREAM_EVENT)
            .collect()
    }

    fn gate_statuses(events: &[StreamEvent]) -> Vec<&str> {
        gate_events(events)
            .into_iter()
            .filter_map(|event| event.data["status"].as_str())
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn completion_gate_failure_reenters_and_passes_after_a_tree_change() {
        let root = gate_tree();
        let gates = CompletionGates {
            gates: vec![gate("checks", "grep -q gate-pass tracked.txt", 2, 1024)],
            max_attempts: 3,
        };
        let (requests, events) = run_gates(
            vec![
                plain_response(),
                ChatResponse {
                    content: "retry".into(),
                    ..plain_response()
                },
                ChatResponse {
                    content: "final".into(),
                    ..plain_response()
                },
            ],
            &root,
            gates,
            true,
            25,
            &[2],
        )
        .await;

        assert_eq!(requests.len(), 2);
        assert_eq!(gate_statuses(&events), ["failed", "passed"]);
        assert_eq!(gate_events(&events)[0].data["attempt"], 1);
        assert_eq!(gate_events(&events)[1].data["attempt"], 1);
        assert!(requests[1].last().is_some_and(|message| {
            message.role == MessageRole::User
                && message.content.contains("completion-gate")
                && message.content.contains("failed")
        }));
        assert_eq!(assistant_texts(&events), ["retry"]);
        assert_eq!(result_stop_reason(&events), "end_turn");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn an_unchanged_tree_skips_gate_reruns_and_exhausts_attempts() {
        let root = gate_tree();
        let counter = tempfile::tempdir().expect("outside-tree counter");
        let counter_path = counter.path().join("count");
        let command = format!(
            "printf '%s' $(( $({cat} {path} 2>/dev/null || printf 0) + 1 )) > {path}; echo gate-failure; exit 1",
            cat = "cat",
            path = counter_path.to_string_lossy(),
        );
        let gates = CompletionGates {
            gates: vec![gate("checks", &command, 2, 1024)],
            max_attempts: 3,
        };
        let responses = (0..4)
            .map(|index| ChatResponse {
                content: format!("final {index}"),
                ..plain_response()
            })
            .collect();
        let (requests, events) = run_gates(responses, &root, gates, true, 25, &[]).await;

        assert_eq!(requests.len(), 4);
        assert_eq!(
            gate_statuses(&events),
            ["failed", "skipped", "skipped", "exhausted"]
        );
        assert_eq!(gate_events(&events)[1].data["skipped"], true);
        assert_eq!(gate_events(&events)[2].data["attempt"], 3);
        assert_eq!(
            std::fs::read_to_string(counter_path).unwrap_or_default(),
            "1"
        );
        assert_eq!(result_stop_reason(&events), "end_turn");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn completion_gate_retries_have_a_budget_separate_from_ordinary_iterations() {
        let root = gate_tree();
        let gates = CompletionGates {
            gates: vec![gate("checks", "grep -q gate-pass tracked.txt", 2, 1024)],
            max_attempts: 3,
        };
        let (requests, events) = run_gates(
            vec![
                tool_only_response("one"),
                plain_response(),
                ChatResponse {
                    content: "final".into(),
                    ..plain_response()
                },
            ],
            &root,
            gates,
            true,
            2,
            &[3],
        )
        .await;

        assert_eq!(requests.len(), 3);
        assert_eq!(gate_statuses(&events), ["failed", "passed"]);
        assert_eq!(result_stop_reason(&events), "end_turn");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn a_tool_call_on_a_gate_retry_consumes_the_ordinary_budget() {
        let root = gate_tree();
        let gates = CompletionGates {
            gates: vec![gate("checks", "exit 1", 2, 1024)],
            max_attempts: 3,
        };
        let (requests, events) = run_gates(
            vec![
                tool_only_response("one"),
                plain_response(),
                tool_only_response("gate-retry-tool"),
            ],
            &root,
            gates,
            true,
            2,
            &[],
        )
        .await;

        assert_eq!(requests.len(), 3);
        assert_eq!(gate_statuses(&events), ["failed"]);
        assert_eq!(result_stop_reason(&events), "iteration_limit");
        let tool_use_ids: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == "tool_use")
            .map(|event| event.data["id"].as_str().unwrap_or_default())
            .collect();
        let mut unique_tool_use_ids = tool_use_ids.clone();
        unique_tool_use_ids.sort_unstable();
        unique_tool_use_ids.dedup();
        assert_eq!(unique_tool_use_ids.len(), 2);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "tool_result")
                .count(),
            2
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn completion_gate_timeouts_and_output_caps_are_settled_failures() {
        let root = gate_tree();
        let timeout_gates = CompletionGates {
            gates: vec![gate("timeout", "sleep 3", 1, 1024)],
            max_attempts: 1,
        };
        let (_requests, timeout_events) = run_gates(
            vec![plain_response(), plain_response()],
            &root,
            timeout_gates,
            true,
            25,
            &[],
        )
        .await;
        assert_eq!(gate_statuses(&timeout_events), ["failed", "exhausted"]);
        assert_eq!(gate_events(&timeout_events)[0].data["timed_out"], true);
        assert_eq!(gate_events(&timeout_events)[0].data["output"], "");
        assert_eq!(result_stop_reason(&timeout_events), "end_turn");

        let root = gate_tree();
        let output_gates = CompletionGates {
            gates: vec![gate("output", "printf '%s\\n' $(seq 1 64)", 2, 128)],
            max_attempts: 1,
        };
        let (_requests, output_events) = run_gates(
            vec![plain_response(), plain_response()],
            &root,
            output_gates,
            true,
            25,
            &[],
        )
        .await;
        let failure = gate_events(&output_events)[0];
        assert_eq!(failure.data["status"], "failed");
        assert!(failure.data["output"].as_str().unwrap().len() <= 128);
        assert!(
            failure.data["output"]
                .as_str()
                .unwrap()
                .contains("[truncated:")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn the_completion_gate_kill_switch_is_visible_and_does_not_run_commands() {
        let root = gate_tree();
        let gates = CompletionGates {
            gates: vec![gate("checks", "exit 1", 2, 1024)],
            max_attempts: 3,
        };
        let (requests, events) =
            run_gates(vec![plain_response()], &root, gates, false, 25, &[]).await;

        assert_eq!(requests.len(), 1);
        assert_eq!(gate_statuses(&events), ["disabled"]);
        assert_eq!(assistant_texts(&events), ["done"]);
        assert_eq!(result_stop_reason(&events), "end_turn");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn completion_gate_commands_run_in_the_sandbox_with_a_scrubbed_environment() {
        let root = gate_tree();
        let gates = CompletionGates {
            gates: vec![gate(
                "confinement",
                "pwd > observed.txt; if env | grep -q '^RSI_SESSION_TOKEN='; then printf '\\ntoken-present' >> observed.txt; fi",
                2,
                1024,
            )],
            max_attempts: 1,
        };
        let (_requests, events) =
            run_gates(vec![plain_response()], &root, gates, true, 25, &[]).await;

        assert_eq!(gate_statuses(&events), ["passed"]);
        let observed = std::fs::read_to_string(root.path().join("observed.txt")).unwrap();
        assert_eq!(observed.trim(), root.path().to_string_lossy());
        assert!(!observed.contains("token-present"));
    }

    /// #1039: the last permitted iteration is a tool-less wrap-up, so a turn
    /// that used its whole budget still ends with a final assistant message.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn iteration_budget_ends_with_a_wrap_up_reply() {
        let (requests, events) = run_scripted(
            vec![
                tool_only_response("a"),
                ChatResponse {
                    content: "summary".to_string(),
                    ..plain_response()
                },
            ],
            2,
        )
        .await;
        assert_eq!(requests.len(), 2);
        let last = requests[1].last().expect("wrap-up prompt");
        assert_eq!(last.role, MessageRole::User);
        assert!(last.content.contains("last permitted step"));
        assert_eq!(assistant_texts(&events), vec!["summary".to_string()]);
        assert_eq!(result_stop_reason(&events), "end_turn");
    }

    /// #1039: a model that ignores the wrap-up and keeps calling tools ends
    /// with a clear limit message and terminal reason, not a silent completion.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn iteration_limit_without_reply_states_the_terminal_reason() {
        let (_requests, events) =
            run_scripted(vec![tool_only_response("a"), tool_only_response("b")], 2).await;
        let texts = assistant_texts(&events);
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("[harness] Stopped"));
        assert!(texts[0].contains("2-step budget"));
        assert_eq!(result_stop_reason(&events), "iteration_limit");
    }

    /// #1050: the configured cap is the number of model steps a turn takes;
    /// the last one is the wrap-up, whatever the cap is.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn configured_iteration_cap_is_honoured() {
        for cap in [3_u32, 6] {
            let mut responses: Vec<ChatResponse> = (0..cap - 1)
                .map(|step| tool_only_response(&format!("call-{step}")))
                .collect();
            responses.push(ChatResponse {
                content: "summary".to_string(),
                ..plain_response()
            });
            let (requests, events) = run_scripted(responses, cap).await;
            assert_eq!(requests.len(), cap as usize, "cap {cap}");
            assert_eq!(assistant_texts(&events), vec!["summary".to_string()]);
            assert_eq!(result_stop_reason(&events), "end_turn");
        }
        let (requests, events) = run_scripted(
            (0..4)
                .map(|n| tool_only_response(&format!("t{n}")))
                .collect(),
            4,
        )
        .await;
        assert_eq!(requests.len(), 4);
        assert_eq!(result_stop_reason(&events), "iteration_limit");
    }

    /// #1039: an empty final response (reasoning only, no text) is reported
    /// with its reasoning and a terminal reason instead of ending silently.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn empty_final_response_states_the_terminal_reason() {
        let (_requests, events) = run_scripted(
            vec![
                tool_only_response("a"),
                ChatResponse {
                    content: String::new(),
                    reasoning_content: Some("the answer is 42".to_string()),
                    stop_reason: Some("stop".to_string()),
                    ..plain_response()
                },
            ],
            5,
        )
        .await;
        let texts = assistant_texts(&events);
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("empty final response"));
        assert!(texts[0].contains("provider stop reason: stop"));
        assert!(texts[0].contains("the answer is 42"));
        assert_eq!(result_stop_reason(&events), "empty_response");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    fn empty_response_with(stop_reason: &str) -> ChatResponse {
        ChatResponse {
            content: String::new(),
            reasoning_content: Some("long analysis".to_string()),
            stop_reason: Some(stop_reason.to_string()),
            ..plain_response()
        }
    }

    /// #1061: an empty final response cut off by the output limit is retried
    /// once, in the same turn, with a final-answer nudge and a lower effort.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn empty_length_final_response_is_retried_once_with_a_nudge() {
        let (requests, events, efforts) = run_scripted_with_efforts(
            vec![
                tool_only_response("a"),
                empty_response_with("length"),
                ChatResponse {
                    content: "the review".to_string(),
                    stop_reason: Some("stop".to_string()),
                    ..plain_response()
                },
            ],
            5,
        )
        .await;
        assert_eq!(requests.len(), 3);
        let nudge = requests[2].last().expect("retry nudge");
        assert_eq!(nudge.role, MessageRole::User);
        assert_eq!(nudge.content, FINAL_ANSWER_RETRY_PROMPT);
        // The retry still carries the tool result from earlier in the turn.
        assert!(
            requests[2]
                .iter()
                .any(|message| message.role == MessageRole::Tool)
        );
        assert_eq!(
            efforts[2].as_deref(),
            Some(FINAL_ANSWER_RETRY_EFFORT),
            "retry lowers the reasoning effort"
        );
        assert_ne!(efforts[1].as_deref(), Some(FINAL_ANSWER_RETRY_EFFORT));
        assert_eq!(assistant_texts(&events), vec!["the review".to_string()]);
        assert_eq!(result_stop_reason(&events), "end_turn");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == FINAL_ANSWER_RETRY_STREAM_EVENT)
                .count(),
            1,
            "the retry is recorded exactly once"
        );
    }

    /// #1061: at most one automatic retry; a second empty response keeps the
    /// #1039 terminal state and message.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn second_empty_length_response_keeps_the_terminal_state() {
        let (requests, events) = run_scripted(
            vec![
                tool_only_response("a"),
                empty_response_with("length"),
                empty_response_with("length"),
            ],
            5,
        )
        .await;
        assert_eq!(requests.len(), 3);
        let texts = assistant_texts(&events);
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains("empty final response"));
        assert!(texts[0].contains("provider stop reason: length"));
        assert_eq!(result_stop_reason(&events), "empty_response");
    }

    /// #1061: an empty response whose stop reason is not an output limit is
    /// not retried.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn empty_non_length_final_response_is_not_retried() {
        let (requests, events) = run_scripted(
            vec![tool_only_response("a"), empty_response_with("stop")],
            5,
        )
        .await;
        assert_eq!(requests.len(), 2);
        assert!(
            !events
                .iter()
                .any(|event| event.event_type == FINAL_ANSWER_RETRY_STREAM_EVENT)
        );
        assert_eq!(result_stop_reason(&events), "empty_response");
    }

    /// #1061: the retry also runs when the empty response was the wrap-up
    /// step, which used the whole iteration budget.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn empty_length_wrap_up_response_is_retried_once() {
        let (requests, events) = run_scripted(
            vec![
                tool_only_response("a"),
                empty_response_with("length"),
                ChatResponse {
                    content: "summary".to_string(),
                    ..plain_response()
                },
            ],
            2,
        )
        .await;
        assert_eq!(requests.len(), 3);
        assert_eq!(assistant_texts(&events), vec!["summary".to_string()]);
        assert_eq!(result_stop_reason(&events), "end_turn");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn harness_mail_is_not_claimed_on_the_initial_model_request() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            request_efforts: Arc::new(Mutex::new(Vec::new())),
            request_messages: Arc::clone(&requests),
            responses: None,
        };
        let boundary = TestHarnessMailBoundary::default();
        let claim_count = Arc::clone(&boundary.claims);
        let (event_tx, _event_rx) = mpsc::channel(64);

        run_harness_loop_with_compact_budget(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            2,
            100_000,
            None,
            None,
            Some(Arc::new(boundary)),
            None,
            true,
        )
        .await
        .expect("loop succeeds");

        assert_eq!(requests.lock().await.len(), 1);
        assert_eq!(claim_count.load(Ordering::SeqCst), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn harness_mail_reaches_the_next_model_request_once() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            request_efforts: Arc::new(Mutex::new(Vec::new())),
            request_messages: Arc::clone(&requests),
            responses: Some(tool_then_final_responses()),
        };
        let (delivery, boundary) = test_harness_mail_boundary("steering update");
        let claim_count = Arc::clone(&boundary.claims);
        let (event_tx, _event_rx) = mpsc::channel(64);

        run_harness_loop_with_compact_budget(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            2,
            100_000,
            None,
            None,
            Some(Arc::new(boundary)),
            None,
            true,
        )
        .await
        .expect("loop succeeds");

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        let injected = requests[1]
            .last()
            .expect("the next request contains the mail boundary");
        assert_eq!(injected.role, MessageRole::User);
        assert_eq!(injected.content, "steering update");
        assert_eq!(claim_count.load(Ordering::SeqCst), 1);
        let delivery = delivery
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(delivery.appended, 1);
        assert_eq!(delivery.history_appends, 1);
        assert_eq!(delivery.effect_possible, 1);
        assert_eq!(delivery.undone, 0);
        assert_eq!(delivery.rejected, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn harness_mail_cancelled_before_dispatch_is_undone_and_rejected() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            request_efforts: Arc::new(Mutex::new(Vec::new())),
            request_messages: Arc::clone(&requests),
            responses: Some(tool_then_final_responses()),
        };
        let (delivery, mut boundary) = test_harness_mail_boundary("steering update");
        let cancel = CancellationToken::new();
        boundary.cancel_on_claim = Some(cancel.clone());
        let (event_tx, _event_rx) = mpsc::channel(64);

        run_harness_loop_with_compact_budget(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            cancel,
            controller(&store, session_id, permit),
            2,
            100_000,
            None,
            None,
            Some(Arc::new(boundary)),
            None,
            true,
        )
        .await
        .expect("loop exits without a provider dispatch");

        assert_eq!(requests.lock().await.len(), 1);
        let delivery = delivery
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(delivery.appended, 1);
        assert_eq!(delivery.undone, 1);
        assert_eq!(delivery.rejected, 1);
        assert_eq!(delivery.effect_possible, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn harness_loop_records_three_attempts_in_non_streaming_mode() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let control = controller(&store, session_id, permit);
        let provider = FakeProvider {
            responses: Mutex::new(VecDeque::from(vec![
                ChatResponse {
                    content: String::new(),
                    tool_calls: vec![tool_call("1", "unknown_tool")],
                    usage: TokenUsage {
                        prompt_tokens: 11,
                        completion_tokens: 2,
                        total_tokens: 13,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        reasoning_tokens: 0,
                        cost_usd: None,
                    },
                    reasoning_content: None,
                    stop_reason: None,
                },
                ChatResponse {
                    content: String::new(),
                    tool_calls: vec![tool_call("2", "unknown_tool")],
                    usage: TokenUsage {
                        prompt_tokens: 12,
                        completion_tokens: 2,
                        total_tokens: 14,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        reasoning_tokens: 0,
                        cost_usd: None,
                    },
                    reasoning_content: None,
                    stop_reason: None,
                },
                ChatResponse {
                    content: "done".to_string(),
                    tool_calls: vec![],
                    usage: TokenUsage {
                        prompt_tokens: 13,
                        completion_tokens: 3,
                        total_tokens: 16,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        reasoning_tokens: 0,
                        cost_usd: None,
                    },
                    reasoning_content: None,
                    stop_reason: None,
                },
            ])),
            streaming: false,
            calls: AtomicUsize::new(0),
        };
        let tools = Arc::new(HarnessToolRegistry::new());
        let (event_tx, _event_rx) = mpsc::channel(32);
        run_harness_loop(
            Box::new(provider),
            tools,
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "gpt-5.4".to_string(),
            None,
            event_tx,
            tokio_util::sync::CancellationToken::new(),
            control,
            8,
            100_000,
            None,
        )
        .await
        .expect("loop succeeds");

        let guard = store.lock().await;
        let count: i64 = guard
            .conn
            .query_row("SELECT COUNT(*) FROM model_invocations", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(count, 3);
        let child_rows: i64 = guard
            .conn
            .query_row(
                "SELECT COUNT(*) FROM model_invocations
                 WHERE purpose = 'session.harness.turn'
                   AND parent_invocation_id IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("child count");
        assert_eq!(child_rows, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn harness_loop_records_three_attempts_in_streaming_mode() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let control = controller(&store, session_id, permit);
        let provider = FakeProvider {
            responses: Mutex::new(VecDeque::from(vec![
                ChatResponse {
                    content: String::new(),
                    tool_calls: vec![tool_call("1", "unknown_tool")],
                    usage: TokenUsage {
                        prompt_tokens: 11,
                        completion_tokens: 2,
                        total_tokens: 13,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        reasoning_tokens: 0,
                        cost_usd: None,
                    },
                    reasoning_content: None,
                    stop_reason: None,
                },
                ChatResponse {
                    content: String::new(),
                    tool_calls: vec![tool_call("2", "unknown_tool")],
                    usage: TokenUsage {
                        prompt_tokens: 12,
                        completion_tokens: 2,
                        total_tokens: 14,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        reasoning_tokens: 0,
                        cost_usd: None,
                    },
                    reasoning_content: None,
                    stop_reason: None,
                },
                ChatResponse {
                    content: "done".to_string(),
                    tool_calls: vec![],
                    usage: TokenUsage {
                        prompt_tokens: 13,
                        completion_tokens: 3,
                        total_tokens: 16,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        reasoning_tokens: 0,
                        cost_usd: None,
                    },
                    reasoning_content: None,
                    stop_reason: None,
                },
            ])),
            streaming: true,
            calls: AtomicUsize::new(0),
        };
        let tools = Arc::new(HarnessToolRegistry::new());
        let (event_tx, _event_rx) = mpsc::channel(32);
        run_harness_loop(
            Box::new(provider),
            tools,
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "gpt-5.4".to_string(),
            None,
            event_tx,
            tokio_util::sync::CancellationToken::new(),
            control,
            8,
            100_000,
            None,
        )
        .await
        .expect("loop succeeds");

        let guard = store.lock().await;
        let count: i64 = guard
            .conn
            .query_row("SELECT COUNT(*) FROM model_invocations", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(count, 3);
        let child_rows: i64 = guard
            .conn
            .query_row(
                "SELECT COUNT(*) FROM model_invocations
                 WHERE purpose = 'session.harness.turn'
                   AND parent_invocation_id IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("child count");
        assert_eq!(child_rows, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn harness_stream_fallback_requires_a_second_admission() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let root_invocation_id = permit.invocation_id();
        {
            let guard = store.lock().await;
            guard
                .conn
                .execute(
                    "INSERT INTO model_budget_policies (
                        policy_key, scope_kind, scope_id, max_retries, updated_at
                     ) VALUES (?1, 'retry', ?2, 1, datetime('now'))",
                    rusqlite::params![
                        format!("retry-budget:{root_invocation_id}"),
                        root_invocation_id.to_string()
                    ],
                )
                .expect("retry policy");
        }
        let control = controller(&store, session_id, permit);
        let stream_calls = Arc::new(AtomicUsize::new(0));
        let chat_calls = Arc::new(AtomicUsize::new(0));
        let provider = FallbackProvider {
            stream_calls: Arc::clone(&stream_calls),
            chat_calls: Arc::clone(&chat_calls),
        };
        let tools = Arc::new(HarnessToolRegistry::new());
        let (event_tx, _event_rx) = mpsc::channel(32);

        run_harness_loop(
            Box::new(provider),
            tools,
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "gpt-5.4".to_string(),
            None,
            event_tx,
            tokio_util::sync::CancellationToken::new(),
            control,
            8,
            100_000,
            None,
        )
        .await
        .expect("separately admitted fallback succeeds");

        assert_eq!(stream_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chat_calls.load(Ordering::SeqCst), 1);
        let guard = store.lock().await;
        let total: i64 = guard
            .conn
            .query_row("SELECT COUNT(*) FROM model_invocations", [], |row| {
                row.get(0)
            })
            .expect("invocation count");
        let failed_stream: (String, Option<String>) = guard
            .conn
            .query_row(
                "SELECT id, parent_invocation_id FROM model_invocations
                 WHERE status = 'failed' AND error_class = 'stream_fallback_required'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("failed stream row");
        let completed_child: (Option<String>, Option<String>) = guard
            .conn
            .query_row(
                "SELECT parent_invocation_id, retry_of_invocation_id
                 FROM model_invocations
                 WHERE status = 'completed' AND parent_invocation_id IS NOT NULL",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("completed fallback row");
        assert_eq!(total, 2);
        assert_eq!(failed_stream.0, root_invocation_id.to_string());
        assert_eq!(failed_stream.1, None);
        assert_eq!(
            completed_child.0.as_deref(),
            Some(root_invocation_id.to_string().as_str())
        );
        assert_eq!(
            completed_child.1.as_deref(),
            Some(failed_stream.0.as_str()),
            "blocking fallback retries the failed stream invocation"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn cancellation_during_compaction_prevents_the_followup_turn_send() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let root_invocation_id = permit.invocation_id();
        let event_bus = Arc::new(crate::bus::EventBus::new(8));
        let settlements = crate::model_control::call_control::ModelCallSettlementWorker::new(
            Arc::clone(&store),
            Arc::clone(&event_bus),
        )
        .expect("settlement worker")
        .handle()
        .expect("settlement producer");
        let control: Arc<dyn ModelCallControl> = Arc::new(StoreBackedModelCallControl::new(
            Arc::clone(&store),
            event_bus,
            settlements,
            rsi_common::model_control::InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            "Local",
            Some("qwen-test".to_string()),
            "ollama",
            None,
            "test_cancellation_during_compaction",
            permit,
            rsi_common::model_control::ModelInvocationPurpose::SessionHarnessTurn,
            Some(rsi_common::model_control::ModelInvocationPurpose::SessionHarnessCompaction),
            crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
        ));
        let cancel = CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = CancellingCompactionProvider {
            calls: Arc::clone(&calls),
            cancel: cancel.clone(),
        };
        let mut history = vec![ChatMessage::system("system")];
        for idx in 0..160 {
            history.push(ChatMessage::user(format!(
                "message {idx} {}",
                "x".repeat(64)
            )));
        }
        let (event_tx, _event_rx) = mpsc::channel(32);

        run_harness_loop(
            Box::new(provider),
            Arc::new(HarnessToolRegistry::new()),
            String::new(),
            String::new(),
            std::env::temp_dir(),
            "qwen-test".to_string(),
            None,
            event_tx,
            cancel,
            control,
            2,
            100,
            Some(history),
        )
        .await
        .expect("cancellation after compaction exits cleanly");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only the compaction request may reach the fake provider"
        );
        let guard = store.lock().await;
        let running: i64 = guard
            .conn
            .query_row(
                "SELECT COUNT(*) FROM model_invocations WHERE status = 'running'",
                [],
                |row| row.get(0),
            )
            .expect("running count");
        let root: (String, Option<String>) = guard
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations WHERE id = ?1",
                rusqlite::params![root_invocation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("root row");
        assert_eq!(running, 0);
        assert_eq!(root.0, "failed");
        assert_eq!(root.1.as_deref(), Some("interrupted"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn harness_loop_denies_third_backend_call_before_execution() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        {
            let guard = store.lock().await;
            guard
                .conn
                .execute(
                    "INSERT INTO model_budget_policies (
                        policy_key, scope_kind, scope_id, max_calls, updated_at
                     ) VALUES (?1, 'session', ?2, 2, datetime('now'))",
                    params![
                        format!("session-max-calls:{session_id}"),
                        session_id.to_string()
                    ],
                )
                .expect("policy insert");
        }
        let permit = root_permit(&store, session_id).await;
        let control = controller(&store, session_id, permit);
        let provider = Arc::new(FakeProvider {
            responses: Mutex::new(VecDeque::from(vec![
                ChatResponse {
                    content: String::new(),
                    tool_calls: vec![tool_call("1", "unknown_tool")],
                    usage: TokenUsage {
                        prompt_tokens: 11,
                        completion_tokens: 2,
                        total_tokens: 13,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        reasoning_tokens: 0,
                        cost_usd: None,
                    },
                    reasoning_content: None,
                    stop_reason: None,
                },
                ChatResponse {
                    content: String::new(),
                    tool_calls: vec![tool_call("2", "unknown_tool")],
                    usage: TokenUsage {
                        prompt_tokens: 12,
                        completion_tokens: 2,
                        total_tokens: 14,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        reasoning_tokens: 0,
                        cost_usd: None,
                    },
                    reasoning_content: None,
                    stop_reason: None,
                },
                ChatResponse {
                    content: "should_not_run".to_string(),
                    tool_calls: vec![],
                    usage: TokenUsage {
                        prompt_tokens: 13,
                        completion_tokens: 3,
                        total_tokens: 16,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        reasoning_tokens: 0,
                        cost_usd: None,
                    },
                    reasoning_content: None,
                    stop_reason: None,
                },
            ])),
            streaming: true,
            calls: AtomicUsize::new(0),
        });
        let tools = Arc::new(HarnessToolRegistry::new());
        let (event_tx, _event_rx) = mpsc::channel(32);
        let error = run_harness_loop(
            Box::new(Arc::clone(&provider)),
            tools,
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "gpt-5.4".to_string(),
            None,
            event_tx,
            tokio_util::sync::CancellationToken::new(),
            control,
            8,
            100_000,
            None,
        )
        .await
        .expect_err("third call denied");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));
        assert!(error.to_string().contains("denied"));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

        let guard = store.lock().await;
        let denied_rows: i64 = guard
            .conn
            .query_row(
                "SELECT COUNT(*) FROM model_invocations
                 WHERE purpose = 'session.harness.turn'
                   AND admission_status = 'denied'",
                [],
                |row| row.get(0),
            )
            .expect("denied rows");
        assert_eq!(denied_rows, 1);
    }

    /// #1000: one call's full prompt. Anthropic reports input without cache;
    /// OpenAI-compatible providers already include cached tokens.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[test]
    fn full_prompt_tokens_counts_cached_tokens_once_per_provider_shape() {
        let usage = TokenUsage {
            prompt_tokens: 1_000,
            completion_tokens: 50,
            total_tokens: 1_050,
            cache_creation_tokens: 200,
            cache_read_tokens: 700,
            reasoning_tokens: 0,
            cost_usd: None,
        };
        assert_eq!(super::full_prompt_tokens("anthropic", &usage), 1_900);
        assert_eq!(super::full_prompt_tokens("openai_responses", &usage), 1_000);
        assert_eq!(super::full_prompt_tokens("openai", &usage), 1_000);
    }

    /// #783: a tool that records the arguments the loop dispatched to it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    struct RecordingTool {
        tool_name: &'static str,
        schema: &'static str,
        seen: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[async_trait::async_trait]
    impl crate::session::harness::tools::HarnessTool for RecordingTool {
        fn name(&self) -> &str {
            self.tool_name
        }

        fn description(&self) -> &str {
            "Record the arguments the loop dispatched"
        }

        fn parameters_json(&self) -> &str {
            self.schema
        }

        async fn execute(
            &self,
            args: serde_json::Value,
            _working_dir: &std::path::Path,
        ) -> ToolResult {
            self.seen.lock().await.push(args.clone());
            ToolResult {
                success: true,
                output: format!("recorded {args}"),
                error_msg: None,
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    fn tool_call_response(id: &str, name: &str, arguments: &str) -> ChatResponse {
        ChatResponse {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: arguments.to_string(),
                hosted: false,
                hosted_result: None,
            }],
            usage: TokenUsage::default(),
            reasoning_content: None,
            stop_reason: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    fn plain_response() -> ChatResponse {
        ChatResponse {
            content: "done".to_string(),
            tool_calls: Vec::new(),
            usage: TokenUsage::default(),
            reasoning_content: None,
            stop_reason: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    fn labeled_tool_calls_response(calls: &[(&str, &str, &str)]) -> ChatResponse {
        ChatResponse {
            content: String::new(),
            tool_calls: calls
                .iter()
                .map(|(id, name, label)| ToolCall {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    arguments: json!({ "label": label }).to_string(),
                    hosted: false,
                    hosted_result: None,
                })
                .collect(),
            usage: TokenUsage::default(),
            reasoning_content: None,
            stop_reason: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    fn tool_result_events(rx: &mut mpsc::Receiver<StreamEvent>) -> Vec<serde_json::Value> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| event.event_type == "tool_result")
            .map(|event| event.data)
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn parallel_reads_overlap_and_results_stay_in_call_order() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let trace = Arc::new(Mutex::new(Vec::new()));
        let mut registry = HarnessToolRegistry::new();
        registry.register(Arc::new(InstrumentedTool {
            tool_name: "read_file",
            mode: ToolExecutionMode::ParallelSafe,
            barrier: Some(Arc::new(Barrier::new(3))),
            delay: Duration::from_millis(20),
            wait_for_cancel: false,
            active: Arc::clone(&active),
            max_active: Arc::clone(&max_active),
            trace: Arc::clone(&trace),
        }));

        let provider = FakeProvider {
            responses: Mutex::new(VecDeque::from(vec![
                labeled_tool_calls_response(&[
                    ("call-1", "read_file", "a"),
                    ("call-2", "read_file", "b"),
                    ("call-3", "read_file", "c"),
                ]),
                plain_response(),
            ])),
            streaming: false,
            calls: AtomicUsize::new(0),
        };
        let (event_tx, mut event_rx) = mpsc::channel(64);
        run_harness_loop(
            Box::new(provider),
            Arc::new(registry),
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            2,
            100_000,
            None,
        )
        .await
        .expect("loop succeeds");

        assert_eq!(max_active.load(Ordering::SeqCst), 3);
        let results = tool_result_events(&mut event_rx);
        assert_eq!(results.len(), 3);
        let ids: Vec<_> = results
            .iter()
            .map(|result| result["tool_use_id"].as_str())
            .collect();
        assert_eq!(ids, vec![Some("call-1"), Some("call-2"), Some("call-3")]);
        let contents: Vec<_> = results
            .iter()
            .map(|result| result["content"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(contents, vec!["read a", "read b", "read c"]);

        let trace = trace.lock().await;
        let start_positions: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|label| {
                trace
                    .iter()
                    .position(|entry| entry == &format!("start {label}"))
                    .expect("each read starts")
            })
            .collect();
        let first_finish = trace
            .iter()
            .position(|entry| entry.starts_with("finish "))
            .expect("parallel reads finish");
        assert!(start_positions.iter().all(|start| *start < first_finish));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn sequential_tool_never_overlaps_a_parallel_batch() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let trace = Arc::new(Mutex::new(Vec::new()));
        let mut registry = HarnessToolRegistry::new();
        registry.register(Arc::new(InstrumentedTool {
            tool_name: "read_file",
            mode: ToolExecutionMode::ParallelSafe,
            barrier: Some(Arc::new(Barrier::new(2))),
            delay: Duration::from_millis(20),
            wait_for_cancel: false,
            active: Arc::clone(&active),
            max_active: Arc::clone(&max_active),
            trace: Arc::clone(&trace),
        }));
        registry.register(Arc::new(InstrumentedTool {
            tool_name: "sequential_probe",
            mode: ToolExecutionMode::Sequential,
            barrier: None,
            delay: Duration::from_millis(10),
            wait_for_cancel: false,
            active: Arc::clone(&active),
            max_active: Arc::clone(&max_active),
            trace: Arc::clone(&trace),
        }));

        let provider = FakeProvider {
            responses: Mutex::new(VecDeque::from(vec![
                labeled_tool_calls_response(&[
                    ("call-1", "read_file", "a"),
                    ("call-2", "read_file", "b"),
                    ("call-3", "sequential_probe", "s"),
                ]),
                plain_response(),
            ])),
            streaming: false,
            calls: AtomicUsize::new(0),
        };
        let (event_tx, mut event_rx) = mpsc::channel(64);
        run_harness_loop(
            Box::new(provider),
            Arc::new(registry),
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            2,
            100_000,
            None,
        )
        .await
        .expect("loop succeeds");

        let results = tool_result_events(&mut event_rx);
        assert_eq!(results.len(), 3);
        let ids: Vec<_> = results
            .iter()
            .map(|result| result["tool_use_id"].as_str())
            .collect();
        assert_eq!(ids, vec![Some("call-1"), Some("call-2"), Some("call-3")]);

        let trace = trace.lock().await;
        let sequential_start = trace
            .iter()
            .position(|entry| entry == "start s")
            .expect("sequential probe starts");
        let sequential_finish = trace
            .iter()
            .position(|entry| entry == "finish s")
            .expect("sequential probe finishes");
        for label in ["a", "b"] {
            let finish = trace
                .iter()
                .position(|entry| entry == &format!("finish {label}"))
                .expect("parallel read finishes");
            assert!(finish < sequential_start);
        }
        assert_eq!(
            &trace[sequential_start..=sequential_finish],
            &["start s", "finish s"]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn sequential_tool_invalidates_parallel_result_cache() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let trace = Arc::new(Mutex::new(Vec::new()));
        let mut registry = HarnessToolRegistry::new();
        registry.register(Arc::new(InstrumentedTool {
            tool_name: "read_file",
            mode: ToolExecutionMode::ParallelSafe,
            barrier: None,
            delay: Duration::from_millis(1),
            wait_for_cancel: false,
            active: Arc::clone(&active),
            max_active: Arc::clone(&max_active),
            trace: Arc::clone(&trace),
        }));
        registry.register(Arc::new(InstrumentedTool {
            tool_name: "file_edit",
            mode: ToolExecutionMode::Sequential,
            barrier: None,
            delay: Duration::from_millis(1),
            wait_for_cancel: false,
            active: Arc::clone(&active),
            max_active: Arc::clone(&max_active),
            trace: Arc::clone(&trace),
        }));

        let provider = FakeProvider {
            responses: Mutex::new(VecDeque::from(vec![
                labeled_tool_calls_response(&[
                    ("call-1", "read_file", "a"),
                    ("call-2", "file_edit", "a"),
                    ("call-3", "read_file", "a"),
                ]),
                plain_response(),
            ])),
            streaming: false,
            calls: AtomicUsize::new(0),
        };
        let (event_tx, mut event_rx) = mpsc::channel(64);
        run_harness_loop(
            Box::new(provider),
            Arc::new(registry),
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            2,
            100_000,
            None,
        )
        .await
        .expect("loop succeeds");

        let results = tool_result_events(&mut event_rx);
        assert_eq!(results.len(), 3);
        let ids: Vec<_> = results
            .iter()
            .map(|result| result["tool_use_id"].as_str())
            .collect();
        assert_eq!(ids, vec![Some("call-1"), Some("call-2"), Some("call-3")]);
        let trace = trace.lock().await;
        assert_eq!(trace.iter().filter(|entry| entry == &"start a").count(), 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn repeated_sequential_tool_calls_execute_every_time() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let trace = Arc::new(Mutex::new(Vec::new()));
        let mut registry = HarnessToolRegistry::new();
        registry.register(Arc::new(InstrumentedTool {
            tool_name: "shell",
            mode: ToolExecutionMode::Sequential,
            barrier: None,
            delay: Duration::from_millis(1),
            wait_for_cancel: false,
            active: Arc::clone(&active),
            max_active: Arc::clone(&max_active),
            trace: Arc::clone(&trace),
        }));

        let provider = FakeProvider {
            responses: Mutex::new(VecDeque::from(vec![
                labeled_tool_calls_response(&[("call-1", "shell", "a"), ("call-2", "shell", "a")]),
                plain_response(),
            ])),
            streaming: false,
            calls: AtomicUsize::new(0),
        };
        let (event_tx, mut event_rx) = mpsc::channel(64);
        run_harness_loop(
            Box::new(provider),
            Arc::new(registry),
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            2,
            100_000,
            None,
        )
        .await
        .expect("loop succeeds");

        let results = tool_result_events(&mut event_rx);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_use_id"].as_str().unwrap(), "call-1");
        assert_eq!(results[1]["tool_use_id"].as_str().unwrap(), "call-2");
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
        let trace = trace.lock().await;
        assert_eq!(trace.iter().filter(|entry| entry == &"start a").count(), 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn cancellation_mid_batch_settles_every_call_once() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let trace = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let mut registry = HarnessToolRegistry::new();
        registry.register(Arc::new(CancellingProbeTool {
            active: Arc::clone(&active),
            max_active: Arc::clone(&max_active),
            trace: Arc::clone(&trace),
            started: Arc::clone(&started),
            finished: Arc::clone(&finished),
        }));

        let provider = FakeProvider {
            responses: Mutex::new(VecDeque::from(vec![
                labeled_tool_calls_response(&[
                    ("call-1", "cancelling_probe", "a"),
                    ("call-2", "cancelling_probe", "b"),
                    ("call-3", "cancelling_probe", "c"),
                ]),
                plain_response(),
            ])),
            streaming: false,
            calls: AtomicUsize::new(0),
        };
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let loop_cancel = cancel.clone();
        let control = controller(&store, session_id, permit);
        let loop_task = tokio::spawn(async move {
            run_harness_loop(
                Box::new(provider),
                Arc::new(registry),
                String::new(),
                "start".to_string(),
                std::env::temp_dir(),
                "claude-opus-5".to_string(),
                None,
                event_tx,
                loop_cancel,
                control,
                2,
                100_000,
                None,
            )
            .await
        });

        while started.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
        loop_task
            .await
            .expect("loop task joins")
            .expect("loop succeeds after cancellation");

        assert_eq!(started.load(Ordering::SeqCst), 3);
        assert_eq!(finished.load(Ordering::SeqCst), 3);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(max_active.load(Ordering::SeqCst), 3);

        let results = tool_result_events(&mut event_rx);
        assert_eq!(results.len(), 3);
        let ids: Vec<_> = results
            .iter()
            .map(|result| result["tool_use_id"].as_str())
            .collect();
        assert_eq!(ids, vec![Some("call-1"), Some("call-2"), Some("call-3")]);
        assert!(
            results
                .iter()
                .all(|result| result["is_error"] == json!(true))
        );
    }

    /// #783 acceptance 1 and 5: a trailing comma plus a numeric string both
    /// reach the tool, and the result says the arguments were repaired.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn repaired_arguments_dispatch_with_coercion_and_are_flagged() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut registry = HarnessToolRegistry::new();
        registry.register(Arc::new(RecordingTool {
            tool_name: "record",
            schema: r#"{"type":"object","properties":{"count":{"type":"integer"}}}"#,
            seen: Arc::clone(&seen),
        }));

        let provider = FakeProvider {
            responses: Mutex::new(VecDeque::from(vec![
                tool_call_response("call-1", "record", r#"{"count":"7",}"#),
                plain_response(),
            ])),
            streaming: false,
            calls: AtomicUsize::new(0),
        };
        let (event_tx, mut event_rx) = mpsc::channel(64);
        run_harness_loop(
            Box::new(provider),
            Arc::new(registry),
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            2,
            100_000,
            None,
        )
        .await
        .expect("loop succeeds");

        let recorded = seen.lock().await.clone();
        assert_eq!(
            recorded.len(),
            1,
            "the repaired call dispatched exactly once"
        );
        assert_eq!(recorded[0]["count"], json!(7));

        let results = tool_result_events(&mut event_rx);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["is_error"], json!(false));
        let content = results[0]["content"].as_str().unwrap_or_default();
        assert!(content.contains("repaired"), "{content}");
    }

    /// #783 acceptance 2: an unrepairable argument string yields a specific
    /// parse error instead of a downstream schema "missing field" error.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn unrepairable_arguments_surface_invalid_tool_arguments() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut registry = HarnessToolRegistry::new();
        registry.register(Arc::new(RecordingTool {
            tool_name: "record",
            schema: r#"{"type":"object","required":["count"],"properties":{"count":{"type":"integer"}}}"#,
            seen: Arc::clone(&seen),
        }));

        let provider = FakeProvider {
            responses: Mutex::new(VecDeque::from(vec![
                tool_call_response("call-1", "record", "{oops not json"),
                plain_response(),
            ])),
            streaming: false,
            calls: AtomicUsize::new(0),
        };
        let (event_tx, mut event_rx) = mpsc::channel(64);
        run_harness_loop(
            Box::new(provider),
            Arc::new(registry),
            String::new(),
            "start".to_string(),
            std::env::temp_dir(),
            "claude-opus-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            2,
            100_000,
            None,
        )
        .await
        .expect("loop succeeds after reporting the parse error");

        assert!(
            seen.lock().await.is_empty(),
            "the tool must not run on unrepairable arguments"
        );

        let results = tool_result_events(&mut event_rx);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["is_error"], json!(true));
        let content = results[0]["content"].as_str().unwrap_or_default();
        assert!(content.contains("invalid tool arguments:"), "{content}");
        assert!(!content.contains("missing field"), "{content}");
    }
}
