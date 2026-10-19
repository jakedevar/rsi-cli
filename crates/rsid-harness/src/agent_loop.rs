//! The harness turn loop: compaction, request building, retry, tool batching,
//! repeat-failure recovery and completion-gate sequencing.
//!
//! Everything that needs daemon authority (provider credentials, model-call admission
//! and settlement, the tool registry and its policy, agent mail, worktree digests) is
//! reached through the traits in [`crate::ports`]; this module only sequences them.

use crate::compaction::{auto_compact, hard_trim};
use crate::errors::ProviderError;
use crate::normalize::normalize_history;
use crate::ports::{GateDigest, LoopFault, LoopTools, MailBoundary, MailDelivery, ModelPort};
use crate::retry;
use crate::stream::StreamEvent;
use crate::types::*;
use rsi_common::completion_gates::{COMPLETION_GATES_GATE_TURN_BUDGET, CompletionGates};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Per-run inputs of [`run_loop`].
pub struct LoopParams {
    pub system_prompt: String,
    pub user_query: String,
    pub working_dir: PathBuf,
    pub model: String,
    pub reasoning_effort: Option<String>,
    /// Step budget (default 25).
    pub max_iterations: u32,
    /// Context window size for this model.
    pub token_limit: u64,
    /// Prior history for continue/rotation.
    pub conversation_history: Option<Vec<ChatMessage>>,
    /// Absolute live-context budget (#966): history compacts at
    /// `min(75% of token_limit, compact_budget)`.
    pub compact_budget: Option<u64>,
    pub completion_gates: Option<CompletionGates>,
    pub completion_gates_enabled: bool,
}

/// Stream event carrying one auto-compaction record (#796); persisted as a
/// `Compressed` conversation event.
pub const COMPACTION_STREAM_EVENT: &str = "compaction";

pub const COMPLETION_GATE_STREAM_EVENT: &str = "completion_gate";

const MAX_PARALLEL_TOOL_CALLS: usize = 8;

struct CompletionGateFailure {
    gate_index: usize,
    before_digest: GateDigest,
    after_digest: GateDigest,
    output: String,
    exit_code: Option<i32>,
    timed_out: bool,
}

enum PreparedToolCall {
    Executable {
        args: serde_json::Value,
        repaired: bool,
    },
    Immediate {
        message: ChatMessage,
        is_error: bool,
        emit_use: bool,
        emit_result: bool,
        use_input: serde_json::Value,
    },
    Duplicate {
        source: usize,
        repaired: bool,
    },
}

/// Prompt for the tool-less final iteration (#1039).
const ITERATION_LIMIT_WRAP_UP_PROMPT: &str = "[harness] This is your last permitted step and \
tools are no longer available. Reply now with your final answer, or with a summary of what is \
done and what remains, in the format your task requires.";

/// Stream event: a model stream failed after output was forwarded, so that
/// partial output is superseded by a retry of the step (#3163).
pub const STREAM_SUPERSEDED_STREAM_EVENT: &str = "stream_superseded";

/// Stream event recording the automatic final-answer retry (#1061).
pub const FINAL_ANSWER_RETRY_STREAM_EVENT: &str = "final_answer_retry";

/// Nudge appended for the one automatic retry of an empty, output-limited
/// final response (#1061).
pub const FINAL_ANSWER_RETRY_PROMPT: &str = "Your last response was empty because it hit the output \
limit. Make no tool calls; write your final answer now, concisely.";

/// Stream event recording the repeated-failure nudge (#2257).
pub const REPEAT_FAILURE_STREAM_EVENT: &str = "repeat_failure";

/// Identical failures of one call (same tool, arguments and error) in a turn
/// before the model is nudged once (#2257).
const REPEAT_FAILURE_THRESHOLD: u32 = 3;

/// Longest error excerpt quoted in the repeated-failure nudge and event.
const REPEAT_FAILURE_EXCERPT_CHARS: usize = 300;

/// A call that has just failed [`REPEAT_FAILURE_THRESHOLD`] times alike.
struct RepeatFailure {
    name: String,
    count: u32,
    error: String,
}

impl RepeatFailure {
    /// The `[harness]` user message that asks the model to stop repeating.
    fn nudge(&self) -> String {
        format!(
            "[harness] You called `{}` with the same arguments {} times and it failed the same \
             way each time:\n{}\nDo not repeat the call unchanged. Use the error to fix the \
             cause, try a different approach, or tell the user what is blocking you.",
            self.name, self.count, self.error
        )
    }
}

#[derive(Default)]
struct FailureRun {
    error: u64,
    count: u32,
    nudged: bool,
}

/// Counts identical failures of one call across a turn (#2257). A call is its
/// tool name plus raw arguments; a failure is "the same" when its error text
/// matches. A success, or a different error, restarts the count, and one run
/// of failures is nudged once.
#[derive(Default)]
struct RepeatFailureTracker {
    runs: HashMap<u64, FailureRun>,
}

impl RepeatFailureTracker {
    /// Record a settled call: `error` is its error text, or `None` when it
    /// succeeded. Returns the failure to report when this call is the
    /// [`REPEAT_FAILURE_THRESHOLD`]th identical one.
    fn record(
        &mut self,
        name: &str,
        arguments: &str,
        error: Option<&str>,
    ) -> Option<RepeatFailure> {
        let key = hash_tool_call(name, arguments);
        let Some(error) = error else {
            self.runs.remove(&key);
            return None;
        };
        let error_hash = hash_tool_call("", error);
        let run = self.runs.entry(key).or_default();
        if run.count == 0 || run.error != error_hash {
            *run = FailureRun {
                error: error_hash,
                count: 0,
                nudged: false,
            };
        }
        run.count += 1;
        if run.count < REPEAT_FAILURE_THRESHOLD || run.nudged {
            return None;
        }
        run.nudged = true;
        let excerpt: String = error
            .trim_start_matches("Error: ")
            .chars()
            .take(REPEAT_FAILURE_EXCERPT_CHARS)
            .collect();
        Some(RepeatFailure {
            name: name.to_string(),
            count: run.count,
            error: excerpt,
        })
    }
}

/// Give every tool call with no id one, so its result can be paired with it.
/// An id is unique within the run (the run tag plus a counter); a call the
/// provider named is left alone. Returns how many ids were made.
fn synthesize_missing_call_ids(calls: &mut [ToolCall], run_tag: &str, counter: &mut u64) -> usize {
    let mut synthesized = 0;
    for call in calls.iter_mut().filter(|call| call.id.trim().is_empty()) {
        *counter += 1;
        let prefix: String = run_tag.chars().take(8).collect();
        call.id = format!("call_{prefix}_{counter}");
        synthesized += 1;
    }
    synthesized
}

/// Output-token ceiling for the final-answer retry call (#1061); the normal
/// per-call ceiling is [`DEFAULT_CALL_MAX_OUTPUT_TOKENS`] or the route's own
/// default when that is larger.
const FINAL_ANSWER_RETRY_MAX_TOKENS: u64 = 32_768;

/// How long the loop waits for the stream forwarder to relay the chunks still
/// queued when a streamed call returns (#2253).
const FORWARD_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Generic per-call output-token ceiling. A route that declares a larger
/// default output budget (Inception Mercury: 16,384, reasoning included)
/// raises it for that route only (#2253).
const DEFAULT_CALL_MAX_OUTPUT_TOKENS: u64 = 8192;

/// The per-call output-token ceiling for `provider`: the generic ceiling, or
/// the route's default output budget when that is larger (#2253).
pub fn call_output_token_cap(provider: &dyn ModelPort) -> u64 {
    provider
        .default_max_output_tokens()
        .map_or(DEFAULT_CALL_MAX_OUTPUT_TOKENS, |route| {
            u64::from(route).max(DEFAULT_CALL_MAX_OUTPUT_TOKENS)
        })
}

/// Reasoning effort for the final-answer retry call (#1061).
pub const FINAL_ANSWER_RETRY_EFFORT: &str = "low";

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
) -> Result<(), LoopFault> {
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
        .map_err(|_| LoopFault::channel_closed())?;
    history.push(ChatMessage::user(content));
    Ok(())
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
/// Run one harness turn to completion.
pub async fn run_loop(
    provider: &dyn ModelPort,
    tools: Arc<dyn LoopTools>,
    params: LoopParams,
    event_tx: mpsc::Sender<StreamEvent>,
    cancel: CancellationToken,
    agent_mail_boundary: Option<Arc<dyn MailBoundary>>,
) -> Result<(), LoopFault> {
    let LoopParams {
        system_prompt,
        user_query,
        working_dir,
        model,
        reasoning_effort,
        max_iterations,
        token_limit,
        conversation_history,
        compact_budget,
        completion_gates,
        completion_gates_enabled,
    } = params;
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
        .map_err(|_| LoopFault::channel_closed())?;

    let process_notices = tools.take_process_notices().await;
    append_process_notices(&event_tx, &mut history, &session_tag, process_notices).await?;

    let mut total_usage = TokenUsage::default();
    let mut hosted_tools_enabled = !provider.hosted_capabilities(&model).names.is_empty();
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
    // #2257: failures of one call repeat across the model's steps in a turn,
    // and ids made for calls the provider left unnamed stay unique in the run.
    let mut repeat_failures = RepeatFailureTracker::default();
    let mut synthesized_call_ids = 0_u64;

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
            provider.fail_pending("interrupted").await?;
            break;
        }

        // Fresh cache for each turn (dedup cache)
        let mut seen_tool_calls: HashMap<u64, (String, bool)> = HashMap::new();
        let compaction_focus = tools.take_compaction_focus().await;

        // 2. Auto-compaction check
        let compacted = auto_compact(
            &mut history,
            provider,
            &model,
            token_limit,
            compact_budget,
            compaction_focus.as_deref(),
            &cancel,
        )
        .await
        .map_err(|error| LoopFault::process(format!("Harness auto-compaction failed: {error}")))?;
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
                .map_err(|_| LoopFault::channel_closed())?;
        }
        if cancel.is_cancelled() {
            tracing::info!("Agent loop cancelled after compaction at iteration {iteration}");
            provider.fail_pending("interrupted").await?;
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
                .map_err(|_| LoopFault::channel_closed())?;
        }
        let effective_max_tokens = estimate_remaining_tokens(&history, token_limit);
        let request_max_tokens = if final_answer_retry_turn {
            effective_max_tokens.min(FINAL_ANSWER_RETRY_MAX_TOKENS)
        } else {
            effective_max_tokens.min(call_output_token_cap(provider))
        };
        let mut tool_specs = if provider.supports_native_tools() {
            tools.specs()
        } else {
            Vec::new()
        };
        if hosted_tools_enabled {
            // #2250: a hosted tool wins over the native tool of the same name,
            // so the model sees one `web_fetch`. After the provider rejects
            // the hosted tools (below) this branch is skipped and the native
            // spec is what the next request carries.
            let capabilities = provider.hosted_capabilities(&model);
            tool_specs.retain(|spec| !capabilities.names.iter().any(|hosted| *hosted == spec.name));
            tool_specs.extend(tools.filter_hosted_specs(capabilities.specs));
        }
        // The wrap-up and final-answer turns must not call tools. A route that
        // accepts `tool_choice: "none"` keeps `tools` and says so (#2253), so
        // the history's earlier `tool_calls` stay well formed for it; any
        // other route gets no tools, as before.
        let mut tool_choice_none = false;
        if wrap_up_turn || final_answer_retry_turn {
            if provider.accepts_tool_choice_none() && !tool_specs.is_empty() {
                tool_choice_none = true;
            } else {
                tool_specs.clear();
            }
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
            tool_choice_none,
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
        let mut retry_budget = retry::RetryBudget::default();
        let mut prior_invocation_id = None;
        let mut last_retry_error: Option<ProviderError> = None;
        let mut claimed_mail: Option<Box<dyn MailDelivery>> = None;
        let (response, admitted) = loop {
            let admission = provider
                .admit_primary(
                    &format!("turn:{iteration}:attempt:{}", retry_budget.attempts()),
                    &format!("{fingerprint_hint}:attempt:{}", retry_budget.attempts()),
                    prior_invocation_id,
                )
                .await;
            let mut admitted = match admission {
                Ok(call) => call,
                Err(fault)
                    if retry_budget.attempts() > 0 && fault.policy_denied_reason().is_some() =>
                {
                    let reason = fault.policy_denied_reason().unwrap_or_default().to_owned();
                    let Some(error) = last_retry_error else {
                        return Err(fault);
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
                        .map_err(|_| LoopFault::channel_closed())?;
                    return Err(LoopFault::provider(error));
                }
                Err(fault) => return Err(fault),
            };
            let model_invocation_id = admitted.invocation_id();
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
                provider.fail_pending("interrupted").await?;
                break 'turns;
            }
            let emitted_chunk = Arc::new(AtomicBool::new(false));

            let response = if provider.supports_streaming() {
                let (chunk_tx, mut chunk_rx) = mpsc::channel::<StreamChunk>(64);
                let event_tx_clone = event_tx.clone();
                let tag = session_tag.clone();
                let emitted = Arc::clone(&emitted_chunk);

                let mut forward_task = tokio::spawn(async move {
                    while let Some(chunk) = chunk_rx.recv().await {
                        emitted.store(true, Ordering::Release);
                        if let Some(warning) = chunk.warning {
                            let _ = event_tx_clone
                                .send(StreamEvent {
                                    event_type: "system".into(),
                                    data: json!({
                                        "subtype": "provider_warning",
                                        "session_id": tag,
                                        "content": format!("[provider warning] {warning}"),
                                        "warning": warning,
                                    }),
                                })
                                .await;
                        }
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
                        admitted.fail("interrupted").await?;
                        break 'turns;
                    }
                    result = admitted.stream_chat(&request, chunk_tx, &cancel) => {
                        // The provider has dropped its sender, so the forwarder
                        // ends once it has relayed what is queued. Let it, so a
                        // provider warning is never lost to the abort (#2253);
                        // a stuck consumer is cut off after a short bound.
                        if tokio::time::timeout(FORWARD_DRAIN_TIMEOUT, &mut forward_task)
                            .await
                            .is_err()
                        {
                            forward_task.abort();
                            let _ = forward_task.await;
                        }
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
                        admitted.fail("interrupted").await?;
                        break 'turns;
                    }
                    result = admitted.chat(&request) => result,
                }
            };
            if let Some(mut mail) = claimed_mail.take() {
                mail.record_effect_possible().await;
            }

            let response = match response {
                Err(fault) if fault.stream_fallback_reason().is_some() => {
                    let reason = fault
                        .stream_fallback_reason()
                        .unwrap_or_default()
                        .to_owned();
                    let failed_stream_invocation_id = admitted.invocation_id();
                    admitted.fail("stream_fallback_required").await?;
                    tracing::warn!(
                        %reason,
                        iteration,
                        "streaming attempt failed; requesting separately admitted blocking fallback"
                    );
                    if cancel.is_cancelled() {
                        break 'turns;
                    }

                    admitted = provider
                        .admit_primary(
                            &format!("turn:{iteration}:blocking_fallback"),
                            &format!("{fingerprint_hint}:blocking_fallback"),
                            failed_stream_invocation_id,
                        )
                        .await?;

                    let mut fallback_request = request.clone();
                    fallback_request.stream = false;
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => {
                            admitted.fail("interrupted").await?;
                            break 'turns;
                        }
                        result = admitted.chat(&fallback_request) => result,
                    }
                }
                other => other,
            };

            // 5. On provider error: retry logic
            match response {
                Ok(r) => break (r, admitted),
                Err(e) => {
                    let failed_invocation_id = admitted.invocation_id();
                    let provider_error = e.provider_error().cloned();
                    admitted
                        .fail(
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
                            .map_err(|_| LoopFault::channel_closed())?;
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
                            .map_err(|_| LoopFault::channel_closed())?;
                        hosted_tools_enabled = false;
                        continue 'turns;
                    }
                    // A failure before any output retries inside the time
                    // budget; one after output was forwarded restarts the step
                    // from the same pre-call history (#3163).
                    let mid_stream = emitted_chunk.load(Ordering::Acquire);
                    if let Some(error) = provider_error.as_ref()
                        && let Some(delay) = retry_budget.next_delay(error, mid_stream)
                    {
                        if mid_stream {
                            // The partial output is dropped: nothing of it
                            // enters `history`, and consumers are told to
                            // discard what they already saw.
                            event_tx
                                .send(StreamEvent {
                                    event_type: STREAM_SUPERSEDED_STREAM_EVENT.into(),
                                    data: json!({
                                        "session_id": session_tag,
                                        "class": error.class,
                                        "detail_code": error.detail_code,
                                        "retry": retry_budget.attempts() + 1,
                                        "max_stream_retries": retry::MAX_STREAM_RETRIES,
                                    }),
                                })
                                .await
                                .map_err(|_| LoopFault::channel_closed())?;
                        }
                        if retry::wait(delay, &cancel).await {
                            retry_budget.record(delay, mid_stream);
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
                        .map_err(|_| LoopFault::channel_closed())?;

                    return Err(e);
                }
            }
        };

        admitted.complete(&response.usage).await?;

        // Track tokens
        total_usage.prompt_tokens = response.usage.prompt_tokens;
        total_usage.completion_tokens += response.usage.completion_tokens;
        total_usage.cache_creation_tokens = response.usage.cache_creation_tokens;
        total_usage.cache_read_tokens = response.usage.cache_read_tokens;
        total_usage.reasoning_tokens += response.usage.reasoning_tokens;
        total_usage.total_tokens = total_usage.prompt_tokens + total_usage.completion_tokens;
        cumulative_prompt_tokens = cumulative_prompt_tokens.saturating_add(full_prompt_tokens(
            &provider.provider_name(),
            &response.usage,
        ));
        tools.set_context_usage(
            full_prompt_tokens(&provider.provider_name(), &response.usage),
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
                    .map_err(|_| LoopFault::channel_closed())?;
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
                            .map_err(|_| LoopFault::channel_closed())?;
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
                            .map_err(|_| LoopFault::channel_closed())?;
                        replied = true;
                        break;
                    } else {
                        let before_digest =
                            tools.worktree_digest(&working_dir).map_err(|error| {
                                LoopFault::process(format!(
                                    "Harness completion gate digest failed: {error}"
                                ))
                            })?;
                        if let Some(failure) = last_completion_gate_failure.as_ref()
                            && (before_digest.matches(&failure.before_digest)
                                || before_digest.matches(&failure.after_digest))
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
                                .map_err(|_| LoopFault::channel_closed())?;
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
                                tools.worktree_digest(&working_dir).map_err(|error| {
                                    LoopFault::process(format!(
                                        "Harness completion gate digest failed: {error}"
                                    ))
                                })?;
                            completion_gate_attempts += 1;
                            let failure = CompletionGateFailure {
                                gate_index,
                                before_digest: before_digest.clone(),
                                after_digest: after_digest.clone(),
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
                                .map_err(|_| LoopFault::channel_closed())?;
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
                            .map_err(|_| LoopFault::channel_closed())?;
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
                    .map_err(|_| LoopFault::channel_closed())?;
            }
            replied = limit_stop.is_none();
            break; // Complete
        }

        // 7. Execute tool calls
        // #2257: a call with no id gets one before it enters the history, so
        // its result is paired with it and normalization drops neither.
        let mut tool_calls = response.tool_calls.clone();
        let synthesized =
            synthesize_missing_call_ids(&mut tool_calls, &session_tag, &mut synthesized_call_ids);
        if synthesized != 0 {
            tracing::warn!(
                session_id = %session_tag,
                synthesized,
                "Harness model returned tool call(s) with no id; synthesized one"
            );
        }
        let assistant_msg = ChatMessage {
            role: MessageRole::Assistant,
            content: response.content.clone(),
            tool_call_id: None,
            is_error: false,
            tool_calls: tool_calls.clone(),
        };
        history.push(assistant_msg);

        let mut repeat_nudges: Vec<RepeatFailure> = Vec::new();
        let mut call_index = 0;
        while call_index < tool_calls.len() {
            let first_call = &tool_calls[call_index];
            let sequential = tools
                .execution_mode(&first_call.name)
                .is_none_or(|mode| mode == crate::tools::ToolExecutionMode::Sequential);
            let batch_limit = if sequential {
                1
            } else {
                MAX_PARALLEL_TOOL_CALLS
            };
            let batch_start = call_index;
            call_index += 1;
            while call_index < tool_calls.len() && call_index - batch_start < batch_limit {
                let next_call = &tool_calls[call_index];
                let next_sequential = tools
                    .execution_mode(&next_call.name)
                    .is_none_or(|mode| mode == crate::tools::ToolExecutionMode::Sequential);
                if next_sequential != sequential {
                    break;
                }
                call_index += 1;
            }

            let batch = &tool_calls[batch_start..call_index];
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
                        .is_some_and(|mode| mode == crate::tools::ToolExecutionMode::ParallelSafe);
                if cacheable && let Some(source) = batch_sources.get(&fingerprint) {
                    if let PreparedToolCall::Immediate {
                        message, is_error, ..
                    } = &prepared[*source]
                    {
                        prepared.push(PreparedToolCall::Immediate {
                            message: message.clone(),
                            is_error: *is_error,
                            emit_use: false,
                            emit_result: false,
                            use_input: serde_json::Value::Null,
                        });
                    } else {
                        prepared.push(PreparedToolCall::Duplicate {
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
                    let denial = tools.admit_hosted_call(&tc.name).err();
                    let message = if let Some(text) = denial {
                        ChatMessage::tool_error_result(&tc.id, format!("Error: {text}"))
                    } else {
                        if let Some(result) = &tc.hosted_result {
                            tools.record_hosted_result_bytes(result.to_string().len());
                        }
                        hosted_tool_result_message(tc)
                    };
                    let is_error = message.is_error;
                    let use_input = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                        .unwrap_or(serde_json::Value::Null);
                    prepared.push(PreparedToolCall::Immediate {
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

                if tc.name.trim().is_empty() {
                    // #2257: a call with no name cannot run; say so, and list
                    // what the model can call, instead of a bare parse error.
                    prepared.push(PreparedToolCall::Immediate {
                        message: ChatMessage::tool_error_result(
                            &tc.id,
                            format!("Error: {}", tools.unknown_tool_message(&tc.name)),
                        ),
                        is_error: true,
                        emit_use: true,
                        emit_result: true,
                        use_input: serde_json::from_str(&tc.arguments)
                            .unwrap_or(serde_json::Value::Null),
                    });
                    continue;
                }

                let parsed = match crate::arguments::parse_tool_arguments(&tc.arguments) {
                    Ok(parsed) => parsed,
                    Err(message) => {
                        // Surface a parse error the model can act on instead of
                        // letting an empty object fail a schema "missing field".
                        let result_message =
                            ChatMessage::tool_error_result(&tc.id, format!("Error: {message}"));
                        prepared.push(PreparedToolCall::Immediate {
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
                    && let Ok(schema) = serde_json::from_str::<serde_json::Value>(&schema)
                {
                    crate::arguments::coerce_with_schema(&mut args, &schema);
                }
                if cacheable {
                    batch_sources.insert(fingerprint, prepared.len());
                }
                prepared.push(PreparedToolCall::Executable { args, repaired });
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
                    .map_err(|_| LoopFault::channel_closed())?;

                let tools = Arc::clone(&tools);
                let name = tc.name.clone();
                let args = args.clone();
                let working_dir = working_dir.clone();
                let cancel = cancel.clone();
                let event_tx = event_tx.clone();
                tasks.spawn(async move {
                    let result = tools
                        .execute(&name, args, &working_dir, &cancel, event_tx)
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
                    PreparedToolCall::Executable { args, repaired } => {
                        let result = results[index].clone().unwrap_or_else(aborted_tool_result);
                        let mut message = tool_result_message(&tc.id, result);
                        if repaired && !message.has_typed_tool_blocks() {
                            message.content.push('\n');
                            message.content.push_str(crate::arguments::REPAIR_NOTE);
                        }
                        let is_error = message.is_error;
                        // #2279: the spawn loop already announced this call; announcing
                        // it again would emit two `tool_use` events for one id.
                        (message, is_error, false, true, args)
                    }
                    PreparedToolCall::Immediate {
                        message,
                        is_error,
                        emit_use,
                        emit_result,
                        use_input,
                    } => (message, is_error, emit_use, emit_result, use_input),
                    PreparedToolCall::Duplicate { source, repaired } => {
                        let result = results[source].clone().unwrap_or_else(aborted_tool_result);
                        let mut message = tool_result_message(&tc.id, result);
                        if repaired && !message.has_typed_tool_blocks() {
                            message.content.push('\n');
                            message.content.push_str(crate::arguments::REPAIR_NOTE);
                        }
                        let is_error = message.is_error;
                        (message, is_error, false, true, serde_json::Value::Null)
                    }
                };

                if !tc.hosted
                    && !cancelled
                    && let Some(repeat) = repeat_failures.record(
                        &tc.name,
                        &tc.arguments,
                        is_error
                            .then(|| result_message.visible_tool_text())
                            .as_deref(),
                    )
                {
                    repeat_nudges.push(repeat);
                }

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
                    return Err(LoopFault::channel_closed());
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
                        .map_err(|_| LoopFault::channel_closed())?;
                }
                if tools
                    .execution_mode(&tc.name)
                    .is_some_and(|mode| mode == crate::tools::ToolExecutionMode::ParallelSafe)
                {
                    seen_tool_calls.insert(
                        hash_tool_call(&tc.name, &tc.arguments),
                        (result_message.content.clone(), is_error),
                    );
                }
                history.push(result_message);
            }

            if cancelled {
                provider.fail_pending("interrupted").await?;
                break 'turns;
            }
        }

        // #2257: the same call failed the same way three times; say so once,
        // after every result of this step so the call/result pairs stay intact.
        for repeat in repeat_nudges {
            event_tx
                .send(StreamEvent {
                    event_type: REPEAT_FAILURE_STREAM_EVENT.into(),
                    data: json!({
                        "session_id": session_tag,
                        "name": repeat.name,
                        "count": repeat.count,
                        "error": repeat.error,
                    }),
                })
                .await
                .map_err(|_| LoopFault::channel_closed())?;
            history.push(ChatMessage::user(repeat.nudge()));
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
            .map_err(|_| LoopFault::channel_closed())?;
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
        .map_err(|_| LoopFault::channel_closed())?;

    Ok(())
}

#[cfg(test)]
mod tool_content_tests {
    use super::*;

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

#[cfg(test)]
mod recovery_tests {
    use super::*;

    fn fail(tracker: &mut RepeatFailureTracker, args: &str, error: &str) -> Option<RepeatFailure> {
        tracker.record("read_file", args, Some(error))
    }

    #[test]
    fn the_third_identical_failure_nudges_once() {
        let mut tracker = RepeatFailureTracker::default();
        assert!(fail(&mut tracker, "{\"path\":\"a\"}", "Error: no such file").is_none());
        assert!(fail(&mut tracker, "{\"path\":\"a\"}", "Error: no such file").is_none());
        let repeat = fail(&mut tracker, "{\"path\":\"a\"}", "Error: no such file")
            .expect("the third identical failure is reported");
        assert_eq!(repeat.name, "read_file");
        assert_eq!(repeat.count, REPEAT_FAILURE_THRESHOLD);
        assert_eq!(repeat.error, "no such file");
        assert!(
            repeat
                .nudge()
                .starts_with("[harness] You called `read_file`")
        );
        // Further identical failures stay quiet.
        for _ in 0..5 {
            assert!(fail(&mut tracker, "{\"path\":\"a\"}", "Error: no such file").is_none());
        }
    }

    #[test]
    fn a_different_call_error_or_a_success_restarts_the_count() {
        let mut tracker = RepeatFailureTracker::default();
        // Different arguments are different calls.
        for args in ["a", "b", "c", "d"] {
            assert!(fail(&mut tracker, args, "Error: boom").is_none());
        }
        // A different error restarts the run.
        assert!(fail(&mut tracker, "a", "Error: boom").is_none());
        assert!(fail(&mut tracker, "a", "Error: other").is_none());
        assert!(fail(&mut tracker, "a", "Error: boom").is_none());
        // A success in between restarts it too.
        assert!(fail(&mut tracker, "e", "Error: boom").is_none());
        assert!(fail(&mut tracker, "e", "Error: boom").is_none());
        assert!(tracker.record("read_file", "e", None).is_none());
        assert!(fail(&mut tracker, "e", "Error: boom").is_none());
        // And after a restart, three more alike are reported once more.
        assert!(fail(&mut tracker, "e", "Error: boom").is_none());
        assert!(fail(&mut tracker, "e", "Error: boom").is_some());
        // The tool name is part of the call.
        assert!(tracker.record("grep", "e", Some("Error: boom")).is_none());
    }

    #[test]
    fn the_nudge_quotes_a_bounded_error_excerpt() {
        let mut tracker = RepeatFailureTracker::default();
        let long = format!("Error: {}", "x".repeat(2 * REPEAT_FAILURE_EXCERPT_CHARS));
        assert!(fail(&mut tracker, "a", &long).is_none());
        assert!(fail(&mut tracker, "a", &long).is_none());
        let repeat = fail(&mut tracker, "a", &long).expect("reported");
        assert_eq!(repeat.error.chars().count(), REPEAT_FAILURE_EXCERPT_CHARS);
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "read_file".to_string(),
            arguments: "{}".to_string(),
            hosted: false,
            hosted_result: None,
        }
    }

    #[test]
    fn only_calls_without_an_id_get_one_and_ids_never_repeat() {
        let mut counter = 0;
        let mut calls = vec![call("call_9"), call(""), call("  "), call("call_10")];
        let made = synthesize_missing_call_ids(&mut calls, "0123456789abcdef", &mut counter);
        assert_eq!(made, 2);
        assert_eq!(calls[0].id, "call_9");
        assert_eq!(calls[1].id, "call_01234567_1");
        assert_eq!(calls[2].id, "call_01234567_2");
        assert_eq!(calls[3].id, "call_10");
        // A later step continues the counter.
        let mut later = vec![call("")];
        synthesize_missing_call_ids(&mut later, "0123456789abcdef", &mut counter);
        assert_eq!(later[0].id, "call_01234567_3");
    }

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

    /// #1000: one call's full prompt. Anthropic reports input without cache;
    /// OpenAI-compatible providers already include cached tokens.
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
        assert_eq!(full_prompt_tokens("anthropic", &usage), 1_900);
        assert_eq!(full_prompt_tokens("openai_responses", &usage), 1_000);
        assert_eq!(full_prompt_tokens("openai", &usage), 1_000);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod port_tests {
    use super::*;
    use crate::compaction::{CompactionError, SummaryOutcome, SummaryTransport};
    use crate::ports::{Admission, GateOutcome, HostedCapabilities};
    use async_trait::async_trait;
    use std::any::Any;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;
    use uuid::Uuid;

    /// Scripted model: each admission pops the next call result and records settlement.
    struct ScriptedModel {
        results: Mutex<VecDeque<Result<ChatResponse, LoopFault>>>,
        /// Streaming models send each call's partial text before its result.
        partials: Option<Mutex<VecDeque<Vec<String>>>>,
        requests: Arc<Mutex<Vec<ChatRequest>>>,
        settled: Arc<Mutex<Vec<String>>>,
    }

    struct ScriptedAdmission {
        partial: Vec<String>,
        result: Option<Result<ChatResponse, LoopFault>>,
        requests: Arc<Mutex<Vec<ChatRequest>>>,
        settled: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl SummaryTransport for ScriptedModel {
        async fn summarize(
            &self,
            _request: &ChatRequest,
            _key: &str,
            _transcript: &str,
            _cancel: &CancellationToken,
        ) -> Result<SummaryOutcome, CompactionError> {
            Ok(SummaryOutcome::PolicyDenied)
        }
    }

    #[async_trait]
    impl ModelPort for ScriptedModel {
        fn provider_name(&self) -> String {
            "openai".into()
        }
        fn supports_native_tools(&self) -> bool {
            true
        }
        fn supports_streaming(&self) -> bool {
            self.partials.is_some()
        }
        fn accepts_tool_choice_none(&self) -> bool {
            false
        }
        fn default_max_output_tokens(&self) -> Option<u32> {
            None
        }
        fn hosted_capabilities(&self, _model: &str) -> HostedCapabilities {
            HostedCapabilities::default()
        }
        async fn admit_primary(
            &self,
            _dedup: &str,
            _fingerprint: &str,
            _retry_of: Option<Uuid>,
        ) -> Result<Box<dyn Admission>, LoopFault> {
            let result = self.results.lock().unwrap().pop_front();
            let partial = self
                .partials
                .as_ref()
                .and_then(|partials| partials.lock().unwrap().pop_front())
                .unwrap_or_default();
            Ok(Box::new(ScriptedAdmission {
                partial,
                result,
                requests: Arc::clone(&self.requests),
                settled: Arc::clone(&self.settled),
            }))
        }
        async fn fail_pending(&self, _class: &str) -> Result<(), LoopFault> {
            Ok(())
        }
    }

    #[async_trait]
    impl Admission for ScriptedAdmission {
        fn invocation_id(&self) -> Option<Uuid> {
            None
        }
        async fn chat(&mut self, request: &ChatRequest) -> Result<ChatResponse, LoopFault> {
            self.requests.lock().unwrap().push(request.clone());
            self.result.take().expect("one send per admission")
        }
        async fn stream_chat(
            &mut self,
            request: &ChatRequest,
            chunks: mpsc::Sender<StreamChunk>,
            _cancel: &CancellationToken,
        ) -> Result<ChatResponse, LoopFault> {
            for text in std::mem::take(&mut self.partial) {
                let _ = chunks
                    .send(StreamChunk {
                        delta_text: text,
                        tool_call_deltas: Vec::new(),
                        is_final: false,
                        usage: None,
                        stop_reason: None,
                        warning: None,
                    })
                    .await;
            }
            self.chat(request).await
        }
        async fn fail(self: Box<Self>, class: &str) -> Result<(), LoopFault> {
            self.settled.lock().unwrap().push(format!("fail:{class}"));
            Ok(())
        }
        async fn complete(self: Box<Self>, _usage: &TokenUsage) -> Result<(), LoopFault> {
            self.settled.lock().unwrap().push("complete".into());
            Ok(())
        }
    }

    struct EchoTools;

    #[async_trait]
    impl LoopTools for EchoTools {
        async fn process_turn_guard(&self) -> Box<dyn Any + Send + Sync> {
            Box::new(())
        }
        async fn take_process_notices(&self) -> Vec<String> {
            Vec::new()
        }
        fn set_context_usage(&self, _used: u64, _limit: u64) {}
        async fn take_compaction_focus(&self) -> Option<String> {
            None
        }
        fn specs(&self) -> Vec<HarnessToolSpec> {
            Vec::new()
        }
        fn filter_hosted_specs(&self, specs: Vec<HarnessToolSpec>) -> Vec<HarnessToolSpec> {
            specs
        }
        fn context_editing_enabled(&self) -> bool {
            false
        }
        fn execution_mode(&self, name: &str) -> Option<crate::tools::ToolExecutionMode> {
            (name == "echo").then_some(crate::tools::ToolExecutionMode::Sequential)
        }
        fn admit_hosted_call(&self, _name: &str) -> Result<(), String> {
            Ok(())
        }
        fn record_hosted_result_bytes(&self, _bytes: usize) {}
        fn unknown_tool_message(&self, name: &str) -> String {
            format!("Unknown tool: {name}.")
        }
        fn parameters_json(&self, _name: &str) -> Option<String> {
            None
        }
        async fn execute(
            &self,
            _name: &str,
            args: serde_json::Value,
            _working_dir: &Path,
            _cancel: &CancellationToken,
            _events: mpsc::Sender<StreamEvent>,
        ) -> ToolResult {
            ToolResult {
                success: true,
                output: format!("echo {args}"),
                error_msg: None,
            }
        }
        async fn execute_completion_gate(
            &self,
            _gate: &rsi_common::completion_gates::CompletionGate,
            _working_dir: &Path,
            _cancel: &CancellationToken,
        ) -> GateOutcome {
            GateOutcome {
                success: true,
                output: String::new(),
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
            }
        }
        fn worktree_digest(&self, _working_dir: &Path) -> Result<GateDigest, String> {
            Ok(GateDigest::unbounded())
        }
    }

    fn response(content: &str, calls: Vec<ToolCall>) -> ChatResponse {
        ChatResponse {
            content: content.into(),
            tool_calls: calls,
            usage: TokenUsage::default(),
            reasoning_content: None,
            stop_reason: Some("stop".into()),
        }
    }

    fn params() -> LoopParams {
        LoopParams {
            system_prompt: "system".into(),
            user_query: "go".into(),
            working_dir: PathBuf::from("."),
            model: "test-model".into(),
            reasoning_effort: None,
            max_iterations: 5,
            token_limit: 100_000,
            conversation_history: None,
            compact_budget: None,
            completion_gates: None,
            completion_gates_enabled: true,
        }
    }

    type RunOutput = (
        Result<(), LoopFault>,
        Vec<StreamEvent>,
        Vec<ChatRequest>,
        Vec<String>,
    );

    async fn run(results: Vec<Result<ChatResponse, LoopFault>>) -> RunOutput {
        run_script(results, None).await
    }

    /// A streaming model: each call sends its partial text, then fails or succeeds.
    async fn run_streaming(script: Vec<(Vec<&str>, Result<ChatResponse, LoopFault>)>) -> RunOutput {
        let (partials, results): (Vec<_>, Vec<_>) = script
            .into_iter()
            .map(|(partial, result)| {
                (
                    partial.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                    result,
                )
            })
            .unzip();
        run_script(results, Some(partials)).await
    }

    async fn run_script(
        results: Vec<Result<ChatResponse, LoopFault>>,
        partials: Option<Vec<Vec<String>>>,
    ) -> RunOutput {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let settled = Arc::new(Mutex::new(Vec::new()));
        let model = ScriptedModel {
            results: Mutex::new(results.into()),
            partials: partials.map(|partials| Mutex::new(partials.into())),
            requests: Arc::clone(&requests),
            settled: Arc::clone(&settled),
        };
        let (tx, mut rx) = mpsc::channel(256);
        let outcome = run_loop(
            &model,
            Arc::new(EchoTools),
            params(),
            tx,
            CancellationToken::new(),
            None,
        )
        .await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        let requests = requests.lock().unwrap().clone();
        let settled = settled.lock().unwrap().clone();
        (outcome, events, requests, settled)
    }

    #[tokio::test]
    async fn a_tool_call_turn_runs_the_tool_and_feeds_its_result_to_the_next_request() {
        let call = ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            arguments: r#"{"a":1}"#.into(),
            hosted: false,
            hosted_result: None,
        };
        let (outcome, events, requests, settled) = run(vec![
            Ok(response("", vec![call])),
            Ok(response("all done", Vec::new())),
        ])
        .await;
        outcome.unwrap();
        assert_eq!(settled, ["complete", "complete"]);
        assert_eq!(requests.len(), 2);
        let second = &requests[1].messages;
        let tool = second.last().expect("tool result is the last message");
        assert_eq!(tool.role, MessageRole::Tool);
        assert_eq!(tool.tool_call_id.as_deref(), Some("c1"));
        assert!(tool.content.starts_with("echo "));
        let kinds: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(kinds.contains(&"tool_use"));
        assert!(kinds.contains(&"tool_result"));
        assert_eq!(kinds.last(), Some(&"result"));
        let assistant = events
            .iter()
            .rev()
            .find(|event| event.event_type == "assistant")
            .expect("final assistant event");
        assert_eq!(assistant.data["message"]["content"][0]["text"], "all done");
    }

    #[tokio::test]
    async fn an_exhausted_iteration_budget_ends_with_the_limit_message() {
        let call = |n: u32| ToolCall {
            id: format!("c{n}"),
            name: "echo".into(),
            arguments: format!(r#"{{"n":{n}}}"#),
            hosted: false,
            hosted_result: None,
        };
        // Five steps: four tool-calling steps and a wrap-up step that still calls a tool.
        let results = (0..5).map(|n| Ok(response("", vec![call(n)]))).collect();
        let (outcome, events, _requests, _settled) = run(results).await;
        outcome.unwrap();
        let result = events.last().expect("result event");
        assert_eq!(result.event_type, "result");
        assert_eq!(result.data["stop_reason"], "iteration_limit");
    }

    #[tokio::test]
    async fn a_non_retryable_provider_failure_is_reported_and_returned_unchanged() {
        let error = ProviderError {
            class: crate::errors::ProviderErrorClass::Auth,
            http_status: Some(401),
            retry_after_ms: None,
            detail_code: "invalid_key".into(),
        };
        let (outcome, events, requests, settled) =
            run(vec![Err(LoopFault::provider(error.clone()))]).await;
        let fault = outcome.unwrap_err();
        assert_eq!(fault.provider_error(), Some(&error));
        assert_eq!(requests.len(), 1);
        assert_eq!(settled, ["fail:invalid_key"]);
        let reported = events
            .iter()
            .find(|event| event.event_type == "provider_error")
            .expect("provider_error event");
        assert_eq!(reported.data["detail_code"], "invalid_key");
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_with_a_new_admission() {
        tokio::time::pause();
        let transient = LoopFault::provider(ProviderError::transient("server_error"));
        let (outcome, _events, requests, settled) =
            run(vec![Err(transient), Ok(response("recovered", Vec::new()))]).await;
        outcome.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(settled, ["fail:server_error", "complete"]);
    }

    fn provider_failure(
        class: crate::errors::ProviderErrorClass,
        status: Option<u16>,
        retry_after_ms: Option<u64>,
        detail: &str,
    ) -> LoopFault {
        LoopFault::provider(ProviderError {
            class,
            http_status: status,
            retry_after_ms,
            detail_code: detail.into(),
        })
    }

    fn overloaded() -> LoopFault {
        provider_failure(
            crate::errors::ProviderErrorClass::Overloaded,
            Some(503),
            None,
            "provider_overloaded",
        )
    }

    fn superseded(events: &[StreamEvent]) -> Vec<&StreamEvent> {
        events
            .iter()
            .filter(|event| event.event_type == STREAM_SUPERSEDED_STREAM_EVENT)
            .collect()
    }

    /// #3163: a burst of five 503s used to exhaust the four retries.
    #[tokio::test(start_paused = true)]
    async fn five_overloaded_responses_then_a_success_completes_the_step() {
        let mut results: Vec<_> = (0..5).map(|_| Err(overloaded())).collect();
        results.push(Ok(response("recovered", Vec::new())));
        let (outcome, events, requests, settled) = run(results).await;
        outcome.unwrap();
        assert_eq!(requests.len(), 6);
        assert_eq!(
            settled,
            [
                "fail:provider_overloaded",
                "fail:provider_overloaded",
                "fail:provider_overloaded",
                "fail:provider_overloaded",
                "fail:provider_overloaded",
                "complete"
            ]
        );
        assert!(superseded(&events).is_empty(), "no output was streamed");
    }

    /// #3163: retries are sized by time (about two minutes), then the
    /// provider failure surfaces.
    #[tokio::test(start_paused = true)]
    async fn a_failure_burst_is_retried_for_about_two_minutes_then_surfaces() {
        let started = tokio::time::Instant::now();
        let results = (0..64).map(|_| Err(overloaded())).collect();
        let (outcome, events, requests, _settled) = run(results).await;
        let fault = outcome.unwrap_err();
        assert_eq!(
            fault
                .provider_error()
                .map(|error| error.detail_code.as_str()),
            Some("provider_overloaded")
        );
        assert!(requests.len() > 5, "{}", requests.len());
        assert!(requests.len() < 64, "the budget ends the retries");
        let elapsed = started.elapsed();
        assert!(elapsed >= retry::REQUEST_RETRY_BUDGET, "{elapsed:?}");
        assert!(
            elapsed <= retry::REQUEST_RETRY_BUDGET + retry::MAX_BACKOFF,
            "{elapsed:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "provider_error")
        );
    }

    /// #3163: a `Retry-After` above the old 60 s cap is waited out, not cut short.
    #[tokio::test(start_paused = true)]
    async fn a_retry_after_above_the_old_cap_is_waited_in_full() {
        let started = tokio::time::Instant::now();
        let limited = provider_failure(
            crate::errors::ProviderErrorClass::RateLimited,
            Some(429),
            Some(120_000),
            "rate_limited",
        );
        let (outcome, _events, requests, _settled) =
            run(vec![Err(limited), Ok(response("ok", Vec::new()))]).await;
        outcome.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(started.elapsed() >= Duration::from_secs(120));
    }

    /// #3163: a stream that dies after a chunk restarts the step from the
    /// same history and supersedes the partial output.
    #[tokio::test(start_paused = true)]
    async fn a_stream_that_drops_after_one_chunk_is_retried_and_supersedes_the_partial() {
        let dropped = LoopFault::provider(ProviderError::transient("stream_disconnected"));
        let (outcome, events, requests, settled) = run_streaming(vec![
            (vec!["par"], Err(dropped)),
            (vec!["full answer"], Ok(response("full answer", Vec::new()))),
        ])
        .await;
        outcome.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].messages.len(), requests[1].messages.len());
        assert_eq!(
            requests[0].messages.last().map(|m| m.content.clone()),
            requests[1].messages.last().map(|m| m.content.clone())
        );
        assert_eq!(settled, ["fail:stream_disconnected", "complete"]);
        let marks = superseded(&events);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].data["detail_code"], "stream_disconnected");
        // The marker comes after the partial text and before the retry's text.
        let kinds: Vec<(usize, &str)> = events
            .iter()
            .enumerate()
            .filter_map(|(at, event)| match event.event_type.as_str() {
                "content_block_delta" => Some((at, event.data["delta"]["text"].as_str()?)),
                STREAM_SUPERSEDED_STREAM_EVENT => Some((at, "<superseded>")),
                _ => None,
            })
            .collect();
        let texts: Vec<&str> = kinds.iter().map(|(_, text)| *text).collect();
        assert_eq!(texts, ["par", "<superseded>", "full answer"]);
        let assistant = events
            .iter()
            .rev()
            .find(|event| event.event_type == "assistant")
            .expect("final assistant event");
        assert_eq!(
            assistant.data["message"]["content"][0]["text"],
            "full answer"
        );
    }

    /// #3163: an idle-timeout cut-off and an upstream error event retry the same way.
    #[tokio::test(start_paused = true)]
    async fn an_idle_stream_and_an_upstream_error_event_are_retried() {
        let idle = LoopFault::provider(ProviderError::transient("stream_idle_timeout"));
        let upstream = provider_failure(
            crate::errors::ProviderErrorClass::Overloaded,
            None,
            None,
            "stream_overloaded",
        );
        let (outcome, events, requests, settled) = run_streaming(vec![
            (vec!["a"], Err(idle)),
            (vec!["b"], Err(upstream)),
            (vec!["c"], Ok(response("c", Vec::new()))),
        ])
        .await;
        outcome.unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            settled,
            [
                "fail:stream_idle_timeout",
                "fail:stream_overloaded",
                "complete"
            ]
        );
        let details: Vec<&str> = superseded(&events)
            .iter()
            .map(|event| event.data["detail_code"].as_str().unwrap())
            .collect();
        assert_eq!(details, ["stream_idle_timeout", "stream_overloaded"]);
    }

    /// #3163: stream retries are bounded; the sixth failure surfaces.
    #[tokio::test(start_paused = true)]
    async fn stream_retries_are_bounded() {
        let script = (0..8)
            .map(|_| {
                (
                    vec!["x"],
                    Err(LoopFault::provider(ProviderError::transient(
                        "stream_disconnected",
                    ))),
                )
            })
            .collect();
        let (outcome, events, requests, _settled) = run_streaming(script).await;
        assert!(outcome.is_err());
        let retries = usize::try_from(retry::MAX_STREAM_RETRIES).unwrap();
        assert_eq!(requests.len(), retries + 1);
        assert_eq!(superseded(&events).len(), retries);
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "provider_error")
        );
    }
}
