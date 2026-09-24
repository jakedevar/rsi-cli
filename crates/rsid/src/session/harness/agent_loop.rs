//! Generalized agent conversation loop.
//!
//! Handles prompt assembly, streaming API calls, tool dispatch, and context compaction.

use crate::claude::StreamEvent;
use crate::error::DaemonError;
use crate::model_control::call_control::{ModelCallControl, ModelCallKind, ModelCallUsage};
use crate::session::harness::compaction::{auto_compact, hard_trim};
use crate::session::harness::normalize::normalize_history;
use crate::session::harness::provider::ApiProvider;
use crate::session::harness::tools::HarnessToolRegistry;
use crate::session::harness::types::*;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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
    let session_tag = uuid::Uuid::new_v4().to_string();

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

    let mut total_usage = TokenUsage::default();

    // Phase B: Enter tool-call loop
    'turns: for iteration in 0..max_iterations {
        // 1. Check cancellation (synchronous poll)
        if cancel.is_cancelled() {
            tracing::info!("Agent loop cancelled at iteration {iteration}");
            model_call_control.fail_pending("interrupted").await?;
            break;
        }

        // Fresh cache for each turn (dedup cache)
        let mut seen_tool_calls: HashMap<u64, String> = HashMap::new();

        // 2. Auto-compaction check
        auto_compact(
            &mut history,
            &*provider,
            &model,
            token_limit,
            &cancel,
            Arc::clone(&model_call_control),
        )
        .await
        .map_err(|error| {
            DaemonError::Process(format!("Harness auto-compaction failed: {error}"))
        })?;
        if cancel.is_cancelled() {
            tracing::info!("Agent loop cancelled after compaction at iteration {iteration}");
            model_call_control.fail_pending("interrupted").await?;
            break;
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
        let request = ChatRequest {
            messages: history.clone(),
            model: model.clone(),
            temperature: None,
            max_tokens: Some(effective_max_tokens.min(8192) as u32),
            tools: if provider.supports_native_tools() {
                tools.specs()
            } else {
                vec![]
            },
            stream: provider.supports_streaming(),
            reasoning_effort: reasoning_effort.clone(),
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
            let (chunk_tx, mut chunk_rx) = mpsc::channel::<StreamChunk>(64);
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
                result = provider.stream_chat(&request, chunk_tx, &cancel, execution) => {
                    forward_task.abort();
                    let _ = forward_task.await;
                    result
                }
            }
        } else {
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

        // 5. On provider error: retry logic
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

                return Err(DaemonError::Process(format!("Provider error: {}", e)));
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

        // Track tokens
        total_usage.prompt_tokens = response.usage.prompt_tokens;
        total_usage.completion_tokens += response.usage.completion_tokens;
        total_usage.cache_creation_tokens = response.usage.cache_creation_tokens;
        total_usage.cache_read_tokens = response.usage.cache_read_tokens;
        total_usage.total_tokens = total_usage.prompt_tokens + total_usage.completion_tokens;

        // 6. Parse tool calls
        if response.tool_calls.is_empty() {
            // Compose final reply
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
            break; // Complete
        }

        // 7. Execute tool calls
        let assistant_msg = ChatMessage {
            role: MessageRole::Assistant,
            content: response.content.clone(),
            tool_call_id: None,
            tool_calls: response.tool_calls.clone(),
        };
        history.push(assistant_msg);

        for tc in &response.tool_calls {
            let fingerprint = hash_tool_call(&tc.name, &tc.arguments);
            if let Some(cached_result) = seen_tool_calls.get(&fingerprint) {
                history.push(ChatMessage::tool_result(&tc.id, cached_result));
                continue;
            }

            let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or(json!({}));

            event_tx
                .send(StreamEvent {
                    event_type: "tool_use".into(),
                    data: json!({
                        "session_id": session_tag,
                        "name": tc.name,
                        "input": args,
                        // Join key for pairing this call with its result.
                        "id": tc.id,
                    }),
                })
                .await
                .map_err(|_| DaemonError::ChannelClosed)?;

            let result = tools
                .execute_cancellable(&tc.name, args, &working_dir, &cancel)
                .await;
            if cancel.is_cancelled() {
                model_call_control.fail_pending("interrupted").await?;
                break 'turns;
            }
            let result_text = if result.success {
                result.output
            } else {
                format!("Error: {}", result.error_msg.unwrap_or(result.output))
            };

            event_tx
                .send(StreamEvent {
                    event_type: "tool_result".into(),
                    data: json!({
                        "session_id": session_tag,
                        "content": result_text,
                        "name": tc.name,
                        "tool_use_id": tc.id,
                    }),
                })
                .await
                .map_err(|_| DaemonError::ChannelClosed)?;

            seen_tool_calls.insert(fingerprint, result_text.clone());
            history.push(ChatMessage::tool_result(&tc.id, result_text));
        }

        hard_trim(&mut history, 200); // safety backstop
    }

    // Phase C & D: Emit final result
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_control::call_control::StoreBackedModelCallControl;
    use crate::model_control::{AdmissionDecision, admit_invocation};
    use crate::store::Store;
    use rusqlite::params;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

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
    }

    struct FallbackProvider {
        stream_calls: Arc<AtomicUsize>,
        chat_calls: Arc<AtomicUsize>,
    }

    struct CancellingCompactionProvider {
        calls: Arc<AtomicUsize>,
        cancel: CancellationToken,
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
        }
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

    #[tokio::test]
    async fn harness_loop_forwards_configured_launch_effort_and_preserves_none() {
        assert_eq!(
            captured_harness_request_effort(Some("xhigh".to_string())).await,
            Some("xhigh".to_string())
        );
        assert_eq!(captured_harness_request_effort(None).await, None);
    }

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
        for idx in 0..60 {
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
                    content: "should_not_run".to_string(),
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
}
