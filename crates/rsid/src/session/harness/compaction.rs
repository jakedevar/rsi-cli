use crate::model_control::call_control::{ModelCallControl, ModelCallKind, ModelCallUsage};
use crate::session::harness::provider::ApiProvider;
use crate::session::harness::types::{ChatMessage, ChatRequest, MessageRole};
use anyhow::Result;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Tier 1 — Auto-compaction
/// Triggers at 75% token usage or 50+ messages. Keeps the system prompt and last 20 messages,
/// and uses the provider to summarize the middle. Falls back to truncation on error.
pub async fn auto_compact(
    history: &mut Vec<ChatMessage>,
    provider: &dyn ApiProvider,
    model: &str,
    token_limit: u64,
    cancel: &CancellationToken,
    model_call_control: Arc<dyn ModelCallControl>,
) -> Result<()> {
    let message_count = history.len();
    let total_tokens: u64 = history.iter().map(|m| m.estimated_tokens()).sum();
    let threshold_tokens = (token_limit as f32 * 0.75) as u64;

    if total_tokens < threshold_tokens && message_count < 50 {
        return Ok(());
    }

    tracing::info!(
        total_tokens,
        messages = message_count,
        "Auto-compacting conversation history"
    );

    let keep_recent = 20;
    if message_count <= keep_recent + 1 {
        return Ok(());
    }

    let to_summarize: Vec<&ChatMessage> = history[1..message_count - keep_recent].iter().collect();
    if to_summarize.is_empty() {
        return Ok(());
    }

    let transcript: String = to_summarize
        .iter()
        .map(|m| {
            let role_str = match m.role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
            };
            let content = if m.content.len() > 500 {
                format!("{}... [truncated]", &m.content[..500])
            } else {
                m.content.clone()
            };
            format!("[{}]: {}", role_str, content)
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
        model: model.to_string(),
        temperature: Some(0.2),
        max_tokens: Some(2000),
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
            &format!("messages:{message_count}:tokens:{total_tokens}"),
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
        },
        res = provider.chat(&summary_request, execution) => res,
    };

    let summary = match summary_result {
        Ok(resp) => {
            model_call_control
                .complete(
                    settlement,
                    ModelCallUsage {
                        input_tokens: Some(resp.usage.prompt_tokens),
                        output_tokens: Some(resp.usage.completion_tokens),
                        cache_creation_tokens: Some(resp.usage.cache_creation_tokens),
                        cache_read_tokens: Some(resp.usage.cache_read_tokens),
                        ..ModelCallUsage::default()
                    },
                )
                .await?;
            resp.content
        }
        Err(e) => {
            model_call_control
                .fail(settlement, "compaction_failed")
                .await?;
            tracing::warn!(error = %e, "Auto-compact LLM summary failed, falling back to truncation");
            // On LLM failure, fall back to local truncation (just drop the middle)
            let system = history[0].clone();
            let tail = history[message_count - keep_recent..].to_vec();
            history.clear();
            history.push(system);
            history.extend(tail);
            return Ok(());
        }
    };

    // Replace the middle with a single ChatMessage::system("[Conversation summary: ...]")
    let system = history[0].clone();
    let tail = history[message_count - keep_recent..].to_vec();
    history.clear();
    history.push(system);
    history.push(ChatMessage::system(&format!(
        "[Conversation summary: {}]",
        summary
    )));
    history.extend(tail);

    tracing::info!(new_len = history.len(), "Auto-compaction complete");
    Ok(())
}

/// Tier 2 — Force compaction
/// Emergency compaction when context is exhausted. Keeps system prompt and last 4 messages,
/// dropping everything in between. No LLM call.
pub fn force_compact(history: &mut Vec<ChatMessage>) {
    if history.len() <= 5 {
        return;
    }
    let system = history[0].clone();
    let tail = history[history.len() - 4..].to_vec();
    history.clear();
    history.push(system);
    history.push(ChatMessage::user(
        "[Previous conversation truncated due to context exhaustion]",
    ));
    history.extend(tail);
}

/// Tier 3 — Hard trim
/// Cap at max_history_messages. Drops oldest messages after system prompt.
pub fn hard_trim(history: &mut Vec<ChatMessage>, max_messages: usize) {
    if history.len() <= max_messages {
        return;
    }
    let keep_from = history.len() - (max_messages - 1);
    let system = history[0].clone();
    let tail = history[keep_from..].to_vec();
    history.clear();
    history.push(system);
    history.extend(tail);
    history.shrink_to_fit();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_control::call_control::StoreBackedModelCallControl;
    use crate::model_control::{AdmissionDecision, admit_invocation};
    use crate::session::harness::provider::ApiProvider;
    use crate::store::Store;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::{Mutex, mpsc};

    #[test]
    fn test_hard_trim() {
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("msg1"),
            ChatMessage::user("msg2"),
            ChatMessage::user("msg3"),
            ChatMessage::user("msg4"),
            ChatMessage::user("msg5"),
        ];
        hard_trim(&mut history, 3);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].role, MessageRole::System);
        assert_eq!(history[0].content, "system");
        assert_eq!(history[2].content, "msg5");
        assert_eq!(history.capacity(), 3);
    }

    #[test]
    fn test_force_compact() {
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("msg1"),
            ChatMessage::user("msg2"),
            ChatMessage::user("msg3"),
            ChatMessage::user("msg4"),
            ChatMessage::user("msg5"),
            ChatMessage::user("msg6"),
        ];
        force_compact(&mut history);
        assert_eq!(history.len(), 6);
        assert_eq!(history[0].role, MessageRole::System);
        assert_eq!(history[0].content, "system");
        assert!(history[1].content.contains("truncated"));
        assert_eq!(history[5].content, "msg6");
    }

    struct FakeProvider {
        calls: AtomicUsize,
        responses: Mutex<VecDeque<crate::session::harness::types::ChatResponse>>,
    }

    #[async_trait::async_trait]
    impl ApiProvider for FakeProvider {
        async fn chat(
            &self,
            _request: &crate::session::harness::types::ChatRequest,
            _execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<crate::session::harness::types::ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses.lock().await.pop_front().ok_or_else(|| {
                crate::error::DaemonError::Process("missing fake response".to_string())
            })
        }

        async fn stream_chat(
            &self,
            request: &crate::session::harness::types::ChatRequest,
            _chunk_tx: mpsc::Sender<crate::session::harness::types::StreamChunk>,
            _cancel: &tokio_util::sync::CancellationToken,
            execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<crate::session::harness::types::ChatResponse> {
            self.chat(request, execution).await
        }

        fn supports_native_tools(&self) -> bool {
            false
        }

        fn name(&self) -> &str {
            "fake"
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
            trigger: "test_harness_compaction".to_string(),
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

    #[tokio::test]
    async fn auto_compact_denial_performs_zero_provider_calls() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let event_bus = Arc::new(crate::bus::EventBus::new(8));
        let settlements = crate::model_control::call_control::ModelCallSettlementWorker::new(
            Arc::clone(&store),
            Arc::clone(&event_bus),
        )
        .expect("settlement worker")
        .handle()
        .expect("settlement producer");
        let control = Arc::new(StoreBackedModelCallControl::new(
            Arc::clone(&store),
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
            "test_harness_compaction",
            permit,
            rsi_common::model_control::ModelInvocationPurpose::SessionHarnessTurn,
            Some(rsi_common::model_control::ModelInvocationPurpose::SessionHarnessCompaction),
            crate::model_control::registry::RuntimeExecutionRoute::SessionHarnessOpenAiHttp,
        ));
        let provider = FakeProvider {
            calls: AtomicUsize::new(0),
            responses: Mutex::new(VecDeque::from(vec![])),
        };
        let mut history = vec![ChatMessage::system("system")];
        for idx in 0..60 {
            history.push(ChatMessage::user(format!(
                "message {idx} {}",
                "x".repeat(64)
            )));
        }
        let error = auto_compact(
            &mut history,
            &provider,
            "gpt-5.4",
            100_000,
            &tokio_util::sync::CancellationToken::new(),
            control,
        )
        .await
        .expect_err("compaction denied by default");
        assert!(error.to_string().contains("background"));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    }
}
