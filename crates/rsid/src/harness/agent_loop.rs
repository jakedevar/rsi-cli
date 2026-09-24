//! Generalized agent conversation loop.
//!
//! Extracted from openai.rs::run_agentic_loop and generalized to work with
//! any ApiProvider. Emits StreamEvents compatible with monitor_session().

use crate::claude::StreamEvent;
use crate::error::DaemonError;
use crate::harness::provider::ApiProvider;
use crate::harness::tools::HarnessToolRegistry;
use crate::harness::types::*;
use crate::model_control::call_control::{ModelCallControl, ModelCallKind, ModelCallUsage};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Configuration for the agent loop.
#[derive(Debug, Clone)]
pub struct AgentLoopConfig {
    pub max_iterations: u32,
    pub max_history_messages: u32,
    pub token_limit: u64,
    pub auto_compact_threshold: f32,
    pub auto_compact_message_threshold: u32,
    pub model: String,
    pub reasoning_effort: Option<String>,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: 25,
            max_history_messages: 50,
            token_limit: 200_000,
            auto_compact_threshold: 0.75,
            auto_compact_message_threshold: 50,
            model: String::new(),
            reasoning_effort: None,
        }
    }
}

/// Run the full agent conversation loop.
///
/// This is the core of the harness. A single call processes one user message
/// through multiple LLM calls with tool execution until completion.
///
/// Emits StreamEvents through `event_tx` for consumption by monitor_session().
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agent_loop(
    provider: &dyn ApiProvider,
    tools: &HarnessToolRegistry,
    config: &AgentLoopConfig,
    system_prompt: Option<&str>,
    user_message: &str,
    prior_history: Option<&[ChatMessage]>,
    working_dir: &Path,
    event_tx: &mpsc::Sender<StreamEvent>,
    cancel: &CancellationToken,
    model_call_control: Arc<dyn ModelCallControl>,
) -> Result<(), DaemonError> {
    let session_tag = uuid::Uuid::new_v4().to_string();

    // Build initial message history
    let mut history: Vec<ChatMessage> = Vec::new();
    if let Some(prior) = prior_history {
        history.extend_from_slice(prior);
    }
    if let Some(sys) = system_prompt {
        // Only add system prompt if not already present
        if !history.iter().any(|m| m.role == MessageRole::System) {
            history.insert(0, ChatMessage::system(sys));
        }
    }
    history.push(ChatMessage::user(user_message));

    // Emit init event
    event_tx
        .send(StreamEvent {
            event_type: "system".into(),
            data: json!({
                "subtype": "init",
                "session_id": session_tag,
                "model": config.model,
            }),
        })
        .await
        .map_err(|_| DaemonError::ChannelClosed)?;

    let tool_specs = tools.specs();
    let mut total_usage = TokenUsage::default();
    let mut seen_tool_calls: HashMap<u64, String> = HashMap::new(); // dedup cache

    for iteration in 0..config.max_iterations {
        if cancel.is_cancelled() {
            tracing::info!("Agent loop cancelled at iteration {iteration}");
            model_call_control.fail_pending("interrupted").await?;
            break;
        }

        // Build request
        let effective_max_tokens = estimate_remaining_tokens(&history, config.token_limit);
        let request = ChatRequest {
            messages: history.clone(),
            model: config.model.clone(),
            temperature: None,
            max_tokens: Some(effective_max_tokens.min(8192) as u32),
            tools: tool_specs.clone(),
            stream: provider.supports_streaming(),
            reasoning_effort: config.reasoning_effort.clone(),
        };

        // Call LLM (streaming preferred)
        let (chunk_tx, mut chunk_rx) = mpsc::channel::<StreamChunk>(64);

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
        if cancel.is_cancelled() {
            model_call_control.fail_pending("interrupted").await?;
            break;
        }
        let admitted_call = model_call_control
            .admit(
                ModelCallKind::Primary,
                &format!("turn:{iteration}"),
                &fingerprint_hint,
                None,
            )
            .await?;
        let (mut settlement, execution) = admitted_call.into_parts();

        let response = if provider.supports_streaming() {
            // Forward streaming chunks as content_block_delta events
            let event_tx_clone = event_tx.clone();
            let tag = session_tag.clone();
            let forward_task = tokio::spawn(async move {
                while let Some(chunk) = chunk_rx.recv().await {
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
                _ = cancel.cancelled() => {
                    forward_task.abort();
                    let _ = forward_task.await;
                    model_call_control.fail(settlement, "interrupted").await?;
                    break;
                }
                result = provider.stream_chat(&request, chunk_tx, execution) => {
                    forward_task.await.ok();
                    result
                }
            }
        } else {
            drop(chunk_tx);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    model_call_control.fail(settlement, "interrupted").await?;
                    break;
                }
                result = provider.chat(&request, execution) => result,
            }
        };

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
                    break;
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
                    _ = cancel.cancelled() => {
                        model_call_control.fail(settlement, "interrupted").await?;
                        break;
                    }
                    result = provider.chat(&fallback_request, fallback_execution) => result,
                }
            }
            other => other,
        };

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                model_call_control
                    .fail(settlement, "provider_call_failed")
                    .await?;
                tracing::error!(error = %e, iteration, "LLM call failed");

                event_tx
                    .send(StreamEvent {
                        event_type: "system".into(),
                        data: json!({
                            "subtype": "error",
                            "error": e.to_string(),
                            "session_id": session_tag,
                        }),
                    })
                    .await
                    .map_err(|_| DaemonError::ChannelClosed)?;

                return Err(e);
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
                    ..ModelCallUsage::default()
                },
            )
            .await?;
        if cancel.is_cancelled() {
            model_call_control.fail_pending("interrupted").await?;
            return Ok(());
        }

        // Track tokens
        total_usage.prompt_tokens = response.usage.prompt_tokens;
        total_usage.completion_tokens += response.usage.completion_tokens;
        total_usage.cache_creation_tokens = response.usage.cache_creation_tokens;
        total_usage.cache_read_tokens = response.usage.cache_read_tokens;
        total_usage.total_tokens = total_usage.prompt_tokens + total_usage.completion_tokens;

        // No tool calls -> final response
        if response.tool_calls.is_empty() {
            // Emit assistant message
            if !response.content.is_empty() {
                event_tx
                    .send(StreamEvent {
                        event_type: "assistant".into(),
                        data: json!({
                            "session_id": session_tag,
                            "message": {
                                "role": "assistant",
                                "content": [{"type": "text", "text": response.content}],
                                "usage": {
                                    "input_tokens": total_usage.prompt_tokens,
                                    "cache_creation_input_tokens": total_usage.cache_creation_tokens,
                                    "cache_read_input_tokens": total_usage.cache_read_tokens,
                                    "output_tokens": total_usage.completion_tokens,
                                },
                            },
                        }),
                    })
                    .await
                    .map_err(|_| DaemonError::ChannelClosed)?;
            }

            // Auto-compact if needed
            maybe_auto_compact(
                provider,
                &mut history,
                config,
                cancel,
                Arc::clone(&model_call_control),
            )
            .await?;

            // Emit result event
            event_tx
                .send(StreamEvent {
                    event_type: "result".into(),
                    data: json!({
                        "subtype": "turn_completed",
                        "session_id": session_tag,
                        "usage": {
                            "input_tokens": total_usage.prompt_tokens,
                            "cache_creation_input_tokens": total_usage.cache_creation_tokens,
                            "cache_read_input_tokens": total_usage.cache_read_tokens,
                            "output_tokens": total_usage.completion_tokens,
                        },
                    }),
                })
                .await
                .map_err(|_| DaemonError::ChannelClosed)?;

            return Ok(());
        }

        // Tool calls present -> execute and loop
        let assistant_msg = ChatMessage {
            role: MessageRole::Assistant,
            content: response.content.clone(),
            tool_call_id: None,
            tool_calls: response.tool_calls.clone(),
        };
        history.push(assistant_msg);

        for tc in &response.tool_calls {
            // Dedup check
            let fingerprint = hash_tool_call(&tc.name, &tc.arguments);
            if let Some(cached_result) = seen_tool_calls.get(&fingerprint) {
                // Return cached result
                history.push(ChatMessage::tool_result(&tc.id, cached_result));
                continue;
            }

            // Emit tool_use event
            let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
            event_tx
                .send(StreamEvent {
                    event_type: "tool_use".into(),
                    data: json!({
                        "session_id": session_tag,
                        "name": tc.name,
                        "input": args,
                    }),
                })
                .await
                .map_err(|_| DaemonError::ChannelClosed)?;

            // Execute tool
            let result = tools.execute(&tc.name, args, working_dir).await;
            let result_text = if result.success {
                result.output
            } else {
                format!("Error: {}", result.error_msg.unwrap_or(result.output))
            };

            // Emit tool_result event
            event_tx
                .send(StreamEvent {
                    event_type: "tool_result".into(),
                    data: json!({
                        "session_id": session_tag,
                        "content": result_text,
                        "name": tc.name,
                    }),
                })
                .await
                .map_err(|_| DaemonError::ChannelClosed)?;

            // Cache and add to history
            seen_tool_calls.insert(fingerprint, result_text.clone());
            history.push(ChatMessage::tool_result(&tc.id, result_text));
        }

        // Trim history if too long
        trim_history(&mut history, config.max_history_messages);
    }

    // Max iterations exhausted -- emit final result
    tracing::warn!(
        max_iterations = config.max_iterations,
        "Agent loop exhausted max iterations"
    );

    event_tx
        .send(StreamEvent {
            event_type: "result".into(),
            data: json!({
                "subtype": "turn_completed",
                "session_id": session_tag,
                "usage": {
                    "input_tokens": total_usage.prompt_tokens,
                    "cache_creation_input_tokens": total_usage.cache_creation_tokens,
                    "cache_read_input_tokens": total_usage.cache_read_tokens,
                    "output_tokens": total_usage.completion_tokens,
                },
            }),
        })
        .await
        .map_err(|_| DaemonError::ChannelClosed)?;

    Ok(())
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

/// Hard trim: keep system prompt + last N messages.
fn trim_history(history: &mut Vec<ChatMessage>, max_messages: u32) {
    let max = max_messages as usize;
    if history.len() <= max {
        return;
    }
    // Keep first message (system prompt) and last max-1 messages
    let keep_from = history.len() - (max - 1);
    let system = history[0].clone();
    let tail: Vec<ChatMessage> = history[keep_from..].to_vec();
    history.clear();
    history.push(system);
    history.extend(tail);
}

/// Force compression: drop everything between system prompt and last 4 messages.
/// Emergency path when context is exhausted. No LLM call.
fn force_compress_history(history: &mut Vec<ChatMessage>) {
    if history.len() <= 5 {
        return;
    }
    let system = history[0].clone();
    let tail: Vec<ChatMessage> = history[history.len() - 4..].to_vec();
    history.clear();
    history.push(system);
    history.push(ChatMessage::user(
        "[Previous conversation truncated due to context exhaustion]",
    ));
    history.extend(tail);
}

/// Auto-compaction: triggered at 75% token usage or 50+ messages.
/// Uses the provider to generate a summary, falls back to truncation.
async fn maybe_auto_compact(
    provider: &dyn ApiProvider,
    history: &mut Vec<ChatMessage>,
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    model_call_control: Arc<dyn ModelCallControl>,
) -> Result<(), DaemonError> {
    let total_tokens: u64 = history.iter().map(|m| m.estimated_tokens()).sum();
    let threshold_tokens = (config.token_limit as f32 * config.auto_compact_threshold) as u64;
    let message_threshold = config.auto_compact_message_threshold as usize;

    if total_tokens < threshold_tokens && history.len() < message_threshold {
        return Ok(());
    }

    tracing::info!(
        total_tokens,
        messages = history.len(),
        "Auto-compacting conversation history"
    );

    // Keep system prompt and last 20 messages
    let keep_recent = 20.min(history.len());
    if history.len() <= keep_recent + 1 {
        return Ok(());
    }

    let to_summarize: Vec<&ChatMessage> = history[1..history.len() - keep_recent].iter().collect();
    if to_summarize.is_empty() {
        return Ok(());
    }

    // Build summary request
    let transcript: String = to_summarize
        .iter()
        .map(|m| {
            format!(
                "[{}]: {}",
                match m.role {
                    MessageRole::System => "system",
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::Tool => "tool",
                },
                &m.content[..m.content.len().min(500)]
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let summary_request = ChatRequest {
        messages: vec![
            ChatMessage::system(
                "Summarize this conversation concisely (max 2000 chars). \
                 Focus on key decisions, actions taken, and current state.",
            ),
            ChatMessage::user(&transcript),
        ],
        model: config.model.clone(),
        temperature: Some(0.2),
        max_tokens: Some(1000),
        tools: Vec::new(),
        stream: false,
        reasoning_effort: None,
    };

    if cancel.is_cancelled() {
        return Ok(());
    }
    let admitted_call = model_call_control
        .admit(
            ModelCallKind::Compaction,
            &format!("messages:{}:tokens:{total_tokens}", history.len()),
            &transcript,
            None,
        )
        .await?;
    let (settlement, execution) = admitted_call.into_parts();

    let summary_result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            model_call_control.fail(settlement, "interrupted").await?;
            return Ok(());
        }
        result = provider.chat(&summary_request, execution) => result,
    };
    let summary = match summary_result {
        Ok(resp) => resp.content,
        Err(e) => {
            model_call_control
                .fail(settlement, "compaction_failed")
                .await?;
            tracing::warn!(error = %e, "Auto-compact LLM summary failed, falling back to truncation");
            // Fallback: just truncate
            trim_history(history, config.max_history_messages);
            return Ok(());
        }
    };

    model_call_control
        .complete(
            settlement,
            ModelCallUsage {
                input_tokens: Some(summary_request.messages[1].estimated_tokens()),
                output_tokens: Some(summary.len().div_ceil(4) as u64),
                ..ModelCallUsage::default()
            },
        )
        .await?;

    // Replace old messages with summary
    let system = history[0].clone();
    let tail: Vec<ChatMessage> = history[history.len() - keep_recent..].to_vec();
    history.clear();
    history.push(system);
    history.push(ChatMessage::user(format!(
        "[Conversation summary]\n{summary}"
    )));
    history.extend(tail);

    tracing::info!(new_len = history.len(), "Auto-compaction complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_control::call_control::{ModelCallControl, StoreBackedModelCallControl};
    use crate::model_control::{AdmissionDecision, admit_invocation};
    use crate::store::Store;
    use rusqlite::params;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    struct FakeProvider {
        responses: Mutex<VecDeque<ChatResponse>>,
        streaming: bool,
        calls: AtomicUsize,
    }

    struct CapturingProvider {
        request_efforts: Arc<Mutex<Vec<Option<String>>>>,
    }

    struct BlockingProvider {
        calls: AtomicUsize,
    }

    struct FallbackProvider {
        stream_calls: AtomicUsize,
        chat_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ApiProvider for FakeProvider {
        async fn chat(
            &self,
            _request: &ChatRequest,
            _execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::harness::provider::Result<ChatResponse> {
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
            execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::harness::provider::Result<ChatResponse> {
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
        ) -> crate::harness::provider::Result<ChatResponse> {
            self.request_efforts
                .lock()
                .await
                .push(request.reasoning_effort.clone());
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
            execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::harness::provider::Result<ChatResponse> {
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
    impl ApiProvider for BlockingProvider {
        async fn chat(
            &self,
            _request: &ChatRequest,
            _execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::harness::provider::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }

        async fn stream_chat(
            &self,
            request: &ChatRequest,
            _chunk_tx: mpsc::Sender<StreamChunk>,
            execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::harness::provider::Result<ChatResponse> {
            self.chat(request, execution).await
        }

        fn supports_native_tools(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "blocking-fake"
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
        ) -> crate::harness::provider::Result<ChatResponse> {
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
                },
                reasoning_content: None,
                stop_reason: None,
            })
        }

        async fn stream_chat(
            &self,
            _request: &ChatRequest,
            _chunk_tx: mpsc::Sender<StreamChunk>,
            _execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::harness::provider::Result<ChatResponse> {
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

    async fn root_permit(
        store: &Arc<Mutex<Store>>,
        session_id: uuid::Uuid,
    ) -> crate::model_control::AdmissionPermit {
        let request = crate::model_control::ModelAdmissionRequest {
            purpose: rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
            provider: Some("Harness".to_string()),
            model: Some("gpt-5.4".to_string()),
            backend: Some("Harness".to_string()),
            effort: Some("high".to_string()),
            trigger: "test_legacy_harness_loop".to_string(),
            owner: rsi_common::model_control::InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            dedup_key: Some(format!("root:{session_id}")),
            request_fingerprint: Some("sha256:root".to_string()),
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
            "test_legacy_harness_loop",
            permit,
            rsi_common::model_control::ModelInvocationPurpose::SessionHarnessTurn,
            Some(rsi_common::model_control::ModelInvocationPurpose::SessionHarnessCompaction),
            crate::model_control::registry::RuntimeExecutionRoute::HarnessOpenAiHttp,
        ))
    }

    async fn captured_harness_request_effort(reasoning_effort: Option<String>) -> Option<String> {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let control = controller(&store, session_id, permit);
        let request_efforts = Arc::new(Mutex::new(Vec::new()));
        let provider = CapturingProvider {
            request_efforts: Arc::clone(&request_efforts),
        };
        let config = AgentLoopConfig {
            model: "claude-opus-5".to_string(),
            token_limit: 100_000,
            max_iterations: 1,
            reasoning_effort,
            ..AgentLoopConfig::default()
        };
        let tools = HarnessToolRegistry::new();
        let (event_tx, _event_rx) = mpsc::channel(32);

        run_agent_loop(
            &provider,
            &tools,
            &config,
            None,
            "start",
            None,
            std::env::temp_dir().as_path(),
            &event_tx,
            &CancellationToken::new(),
            control,
        )
        .await
        .expect("loop succeeds");

        request_efforts
            .lock()
            .await
            .pop()
            .expect("provider receives primary request")
    }

    #[tokio::test]
    async fn legacy_harness_loop_forwards_configured_effort_and_preserves_none() {
        assert_eq!(
            captured_harness_request_effort(Some("xhigh".to_string())).await,
            Some("xhigh".to_string())
        );
        assert_eq!(captured_harness_request_effort(None).await, None);
    }

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
        }
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

    #[test]
    fn test_hash_tool_call_deterministic() {
        let h1 = hash_tool_call("read_file", r#"{"path":"foo.rs"}"#);
        let h2 = hash_tool_call("read_file", r#"{"path":"foo.rs"}"#);
        let h3 = hash_tool_call("read_file", r#"{"path":"bar.rs"}"#);
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_trim_history_preserves_system() {
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("msg1"),
            ChatMessage::user("msg2"),
            ChatMessage::user("msg3"),
            ChatMessage::user("msg4"),
            ChatMessage::user("msg5"),
        ];
        trim_history(&mut history, 3);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].role, MessageRole::System);
        assert_eq!(history[0].content, "system");
        assert_eq!(history[2].content, "msg5");
    }

    #[test]
    fn test_trim_history_no_op_when_under_limit() {
        let mut history = vec![ChatMessage::user("a"), ChatMessage::user("b")];
        trim_history(&mut history, 10);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn test_force_compress_preserves_system_and_tail() {
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("msg1"),
            ChatMessage::user("msg2"),
            ChatMessage::user("msg3"),
            ChatMessage::user("msg4"),
            ChatMessage::user("msg5"),
            ChatMessage::user("msg6"),
        ];
        force_compress_history(&mut history);
        // Should be: system + truncation notice + last 4
        assert_eq!(history.len(), 6);
        assert_eq!(history[0].role, MessageRole::System);
        assert_eq!(history[0].content, "system");
        assert!(history[1].content.contains("truncated"));
        assert_eq!(history[5].content, "msg6");
    }

    #[test]
    fn test_force_compress_no_op_when_short() {
        let mut history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("a"),
            ChatMessage::user("b"),
        ];
        force_compress_history(&mut history);
        assert_eq!(history.len(), 3);
    }

    #[tokio::test]
    async fn legacy_harness_loop_records_three_attempts_in_streaming_mode() {
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
                    },
                    reasoning_content: None,
                    stop_reason: None,
                },
            ])),
            streaming: true,
            calls: AtomicUsize::new(0),
        };
        let tools = HarnessToolRegistry::new();
        let config = AgentLoopConfig {
            model: "gpt-5.4".to_string(),
            token_limit: 100_000,
            ..AgentLoopConfig::default()
        };
        let (event_tx, _event_rx) = mpsc::channel(32);
        run_agent_loop(
            &provider,
            &tools,
            &config,
            None,
            "start",
            None,
            std::env::temp_dir().as_path(),
            &event_tx,
            &tokio_util::sync::CancellationToken::new(),
            control,
        )
        .await
        .expect("loop succeeds");

        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            3,
            "each admitted lineage row authorizes one fake provider send"
        );
        let guard = store.lock().await;
        let count: i64 = guard
            .conn
            .query_row("SELECT COUNT(*) FROM model_invocations", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn legacy_harness_stream_fallback_requires_a_second_admission() {
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
        let provider = FallbackProvider {
            stream_calls: AtomicUsize::new(0),
            chat_calls: AtomicUsize::new(0),
        };
        let tools = HarnessToolRegistry::new();
        let config = AgentLoopConfig {
            model: "gpt-5.4".to_string(),
            token_limit: 100_000,
            ..AgentLoopConfig::default()
        };
        let (event_tx, _event_rx) = mpsc::channel(32);

        run_agent_loop(
            &provider,
            &tools,
            &config,
            None,
            "start",
            None,
            std::env::temp_dir().as_path(),
            &event_tx,
            &tokio_util::sync::CancellationToken::new(),
            control,
        )
        .await
        .expect("separately admitted fallback succeeds");

        assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.chat_calls.load(Ordering::SeqCst), 1);
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

    #[tokio::test]
    async fn legacy_harness_compaction_denial_performs_zero_provider_calls() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        {
            let guard = store.lock().await;
            guard
                .conn
                .execute(
                    "INSERT INTO model_budget_policies (
                        policy_key, scope_kind, scope_id, max_calls, updated_at
                     ) VALUES (?1, 'session', ?2, 1, datetime('now'))",
                    params![
                        format!("session-max-calls:{session_id}"),
                        session_id.to_string()
                    ],
                )
                .expect("policy insert");
        }
        let permit = root_permit(&store, session_id).await;
        let control = controller(&store, session_id, permit);
        let provider = FakeProvider {
            responses: Mutex::new(VecDeque::new()),
            streaming: false,
            calls: AtomicUsize::new(0),
        };
        let config = AgentLoopConfig {
            model: "gpt-5.4".to_string(),
            token_limit: 100_000,
            auto_compact_threshold: 0.0,
            auto_compact_message_threshold: 1,
            ..AgentLoopConfig::default()
        };
        let mut history = vec![ChatMessage::system("system")];
        for idx in 0..60 {
            history.push(ChatMessage::user(format!(
                "message {idx} {}",
                "x".repeat(64)
            )));
        }

        let error = maybe_auto_compact(
            &provider,
            &mut history,
            &config,
            &CancellationToken::new(),
            control,
        )
        .await
        .expect_err("compaction denied by default");
        assert!(error.to_string().contains("denied"));

        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn legacy_harness_cancellation_settles_the_in_flight_attempt() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let invocation_id = permit.invocation_id();
        let control = controller(&store, session_id, permit);
        let provider = BlockingProvider {
            calls: AtomicUsize::new(0),
        };
        let tools = HarnessToolRegistry::new();
        let config = AgentLoopConfig {
            model: "gpt-5.4".to_string(),
            token_limit: 100_000,
            ..AgentLoopConfig::default()
        };
        let (event_tx, _event_rx) = mpsc::channel(32);
        let cancel = CancellationToken::new();
        let working_dir = std::env::temp_dir();

        let cancel_when_sent = async {
            while provider.calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(
            run_agent_loop(
                &provider,
                &tools,
                &config,
                None,
                "start",
                None,
                working_dir.as_path(),
                &event_tx,
                &cancel,
                control,
            ),
            cancel_when_sent,
        );
        result.expect("cancellation exits the loop after settlement");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

        let guard = store.lock().await;
        let row = guard
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations WHERE id = ?1",
                rusqlite::params![invocation_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .expect("settled invocation");
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("interrupted"));
    }
}
