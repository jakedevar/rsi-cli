use crate::error::DaemonError;
use crate::model_control::call_control::{ModelCallControl, ModelCallKind, ModelCallUsage};
use crate::session::harness::provider::ApiProvider;
use crate::session::harness::types::{ChatMessage, ChatRequest, MessageRole};
use anyhow::Result;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn transcript_line(message: &ChatMessage) -> String {
    let role = match message.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };
    // Image payloads belong in provider history, not the text-only summary.
    let content = if message.role == MessageRole::Tool {
        message.visible_tool_text()
    } else {
        message.content.clone()
    };
    let content = if content.len() > 500 {
        let mut end = 500;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}... [truncated]", &content[..end])
    } else {
        content
    };
    format!("[{role}]: {content}")
}

/// What one auto-compaction replaced (#796). The agent loop persists it as a
/// `Compressed` conversation event so a continued session rebuilds the same
/// summary-plus-tail history instead of the full raw transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionRecord {
    /// The summary that replaced the dropped middle; `None` when the summary
    /// call failed and the middle was only truncated.
    pub summary: Option<String>,
    /// Conversation messages kept verbatim after the summary (the system
    /// prompt is not counted).
    pub kept_messages: usize,
    /// History length, including the system prompt, before compaction.
    pub messages_before: usize,
    /// Estimated history tokens before compaction.
    pub tokens_before: u64,
    /// Why the summary was not produced: `None` for a real summary,
    /// `Some("policy_denied")` / `Some("summary_failed")` when the middle was
    /// only truncated locally (#974).
    pub fallback: Option<String>,
}

/// Number of leading `System` messages (the real system prompt). A resumed
/// history rebuilt from persisted events carries none.
fn leading_system_len(history: &[ChatMessage]) -> usize {
    history
        .iter()
        .take_while(|m| m.role == MessageRole::System)
        .count()
}

/// Indices in `history[head..tail_start]` of the messages compaction must
/// never summarize (#1058): the first user message (the task) and the latest
/// user message (the newest instruction, distinct on a continuation).
fn pinned_task_indices(history: &[ChatMessage], head: usize, tail_start: usize) -> Vec<usize> {
    let region = head.min(tail_start)..tail_start;
    let users = || {
        region
            .clone()
            .filter(|&idx| history[idx].role == MessageRole::User)
    };
    let mut pinned: Vec<usize> = users().next().into_iter().collect();
    if let Some(last) = users().last()
        && !pinned.contains(&last)
    {
        pinned.push(last);
    }
    pinned
}

/// The messages compaction would summarize: everything between the system
/// prompt and `tail_start` except the pinned task/instruction messages.
fn summarizable(history: &[ChatMessage], tail_start: usize) -> Vec<&ChatMessage> {
    let head = leading_system_len(history);
    let pinned = pinned_task_indices(history, head, tail_start);
    (head.min(tail_start)..tail_start)
        .filter(|idx| !pinned.contains(idx))
        .map(|idx| &history[idx])
        .collect()
}

/// Rebuild `history` as system prompt, pinned task messages (verbatim), an
/// optional `marker` (the summary), then the tail from `tail_start`. Every
/// compaction path and the persisted-history rebuild go through this, so the
/// original task survives however often history is compacted (#1058).
/// Orphaned tool results at the tail's front are repaired by
/// `normalize_history` before the next provider request.
pub(crate) fn compact_in_place(
    history: &mut Vec<ChatMessage>,
    tail_start: usize,
    marker: Option<ChatMessage>,
) {
    let tail_start = tail_start.min(history.len());
    let head = leading_system_len(history);
    let pinned = pinned_task_indices(history, head, tail_start);
    let mut rebuilt: Vec<ChatMessage> = history[..head.min(tail_start)].to_vec();
    rebuilt.extend(pinned.iter().map(|&idx| history[idx].clone()));
    rebuilt.extend(marker);
    rebuilt.extend(history[tail_start..].iter().cloned());
    *history = rebuilt;
}

/// Drop the summarizable middle, keeping the system prompt, the pinned task
/// messages and the last `keep_recent` messages. Shared by the compaction
/// fallback paths.
fn truncate_middle(
    history: &mut Vec<ChatMessage>,
    keep_recent: usize,
    messages_before: usize,
    tokens_before: u64,
    fallback: &str,
) -> CompactionRecord {
    compact_in_place(history, messages_before - keep_recent, None);
    CompactionRecord {
        summary: None,
        kept_messages: keep_recent,
        messages_before,
        tokens_before,
        fallback: Some(fallback.to_string()),
    }
}

/// Live-context size at which Tier 1 compaction fires: 75% of the model's
/// window, or the absolute budget when that is lower.
pub(crate) fn compaction_threshold_tokens(token_limit: u64, budget_tokens: Option<u64>) -> u64 {
    let window = (token_limit as f32 * 0.75) as u64;
    budget_tokens.map_or(window, |budget| window.min(budget))
}

/// Message-count backstop for Tier 1 compaction. It sits below `hard_trim`'s
/// 200-message cap so the summarizing path runs first, and far above the old
/// 50: at two messages per tool call, 50 compacted a Harness worker every ~25
/// calls (#1058). Real pacing is token-driven (75% of the window or the #966
/// budget, whichever is lower).
pub(crate) const AUTO_COMPACT_MESSAGE_TRIGGER: usize = 150;

/// Tier 1 — Auto-compaction
/// Triggers at 75% token usage, at the absolute `budget_tokens` when that is
/// lower (#966: an OpenRouter step costs roughly its live context), or at
/// [`AUTO_COMPACT_MESSAGE_TRIGGER`] messages. Keeps the system prompt, the
/// first user task message and the latest user instruction verbatim (#1058),
/// and the last 20 messages, and uses the provider to summarize the rest.
/// Falls back to truncation on error.
/// Returns what it replaced, or `None` when nothing was compacted.
pub async fn auto_compact(
    history: &mut Vec<ChatMessage>,
    provider: &dyn ApiProvider,
    model: &str,
    token_limit: u64,
    budget_tokens: Option<u64>,
    focus: Option<&str>,
    cancel: &CancellationToken,
    model_call_control: Arc<dyn ModelCallControl>,
) -> Result<Option<CompactionRecord>> {
    let message_count = history.len();
    let total_tokens: u64 = history.iter().map(|m| m.estimated_tokens()).sum();
    let threshold_tokens = compaction_threshold_tokens(token_limit, budget_tokens);

    if focus.is_none()
        && total_tokens < threshold_tokens
        && message_count < AUTO_COMPACT_MESSAGE_TRIGGER
    {
        return Ok(None);
    }

    tracing::info!(
        total_tokens,
        messages = message_count,
        "Auto-compacting conversation history"
    );

    let keep_recent = 20;
    if message_count <= keep_recent + 1 {
        return Ok(None);
    }

    let to_summarize = summarizable(history, message_count - keep_recent);
    if to_summarize.is_empty() {
        return Ok(None);
    }

    let transcript: String = to_summarize
        .iter()
        .map(|m| transcript_line(m))
        .collect::<Vec<_>>()
        .join("\n");

    let summary_instruction = match focus {
        Some(focus) => format!(
            "Summarize this conversation concisely (max 2000 chars). \
             Focus on key decisions, actions taken, and current state. \
             Preserve the following items: {focus}"
        ),
        None => "Summarize this conversation concisely (max 2000 chars). \
                 Focus on key decisions, actions taken, and current state."
            .to_string(),
    };

    let summary_request = ChatRequest {
        messages: vec![
            ChatMessage::system(&summary_instruction),
            ChatMessage::user(&transcript),
        ],
        model: model.to_string(),
        temperature: Some(0.2),
        max_tokens: Some(2000),
        tools: Vec::new(),
        stream: false,
        reasoning_effort: None,
        context_editing: false,
    };

    if cancel.is_cancelled() {
        return Ok(None);
    }
    // #974: a policy denial (e.g. paid background work disabled by default in
    // Normal mode) means the summary call is never attempted; fall back to
    // local truncation so the turn keeps going. Other admission errors
    // propagate.
    let admitted_call = match model_call_control
        .admit(
            ModelCallKind::Compaction,
            &format!("messages:{message_count}:tokens:{total_tokens}"),
            &transcript,
            None,
        )
        .await
    {
        Ok(call) => call,
        Err(DaemonError::PolicyDenied(reason)) => {
            tracing::warn!(
                %reason,
                "Auto-compact summary denied by model-call policy, falling back to truncation"
            );
            return Ok(Some(truncate_middle(
                history,
                keep_recent,
                message_count,
                total_tokens,
                "policy_denied",
            )));
        }
        Err(error) => return Err(error.into()),
    };
    let (settlement, execution) = admitted_call.into_parts();

    let summary_result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            model_call_control.fail(settlement, "interrupted").await?;
            return Ok(None);
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
                        reasoning_tokens: Some(resp.usage.reasoning_tokens),
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
            return Ok(Some(truncate_middle(
                history,
                keep_recent,
                message_count,
                total_tokens,
                "summary_failed",
            )));
        }
    };

    // Replace the summarized middle with a single system summary message; the
    // task and latest instruction stay verbatim ahead of it.
    compact_in_place(
        history,
        message_count - keep_recent,
        Some(ChatMessage::system(&format!(
            "[Conversation summary: {}]",
            summary
        ))),
    );

    tracing::info!(new_len = history.len(), "Auto-compaction complete");
    Ok(Some(CompactionRecord {
        summary: Some(summary),
        kept_messages: keep_recent,
        messages_before: message_count,
        tokens_before: total_tokens,
        fallback: None,
    }))
}

/// Tier 2 — Force compaction
/// Emergency compaction when context is exhausted. Keeps the system prompt,
/// the pinned task messages (#1058) and the last 4 messages, dropping
/// everything in between. No LLM call.
pub fn force_compact(history: &mut Vec<ChatMessage>) {
    if history.len() <= 5 {
        return;
    }
    let tail_start = history.len() - 4;
    compact_in_place(
        history,
        tail_start,
        Some(ChatMessage::user(
            "[Previous conversation truncated due to context exhaustion]",
        )),
    );
}

/// Tier 3 — Hard trim
/// Cap at max_history_messages. Drops oldest messages after the system prompt
/// and the pinned task messages (#1058).
pub fn hard_trim(history: &mut Vec<ChatMessage>, max_messages: usize) {
    if history.len() <= max_messages {
        return;
    }
    // Reserve room for the system prompt and up to two pinned messages.
    let head = leading_system_len(history);
    let tail_len = max_messages.saturating_sub(head + 2).max(1);
    compact_in_place(history, history.len() - tail_len, None);
    history.shrink_to_fit();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_control::call_control::StoreBackedModelCallControl;
    use crate::model_control::{AdmissionDecision, admit_invocation};
    use crate::session::harness::provider::ApiProvider;
    use crate::session::harness::types::ToolContentBlock;
    use crate::store::Store;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::{Mutex, mpsc};

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn compaction_transcript_summarizes_image_without_its_payload() {
        let message = ChatMessage::tool_result_blocks(
            "image-call",
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
            false,
        );
        assert_eq!(
            transcript_line(&message),
            "[tool]: preview\n[image: image/png]"
        );
        let unicode = ChatMessage::user(format!("{}é", "x".repeat(499)));
        assert!(transcript_line(&unicode).ends_with("... [truncated]"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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
        hard_trim(&mut history, 5);
        // System prompt, the pinned task (msg1) and latest instruction, tail.
        let contents: Vec<&str> = history.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(history[0].role, MessageRole::System);
        assert_eq!(contents, ["system", "msg1", "msg3", "msg4", "msg5"]);
        assert_eq!(history.capacity(), history.len());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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
        // System prompt, task (msg1), latest summarized instruction (msg2),
        // truncation marker, last four.
        assert_eq!(history.len(), 8);
        assert_eq!(history[0].role, MessageRole::System);
        assert_eq!(history[0].content, "system");
        assert_eq!(history[1].content, "msg1");
        assert_eq!(history[2].content, "msg2");
        assert!(history[3].content.contains("truncated"));
        assert_eq!(history[7].content, "msg6");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    #[allow(clippy::expect_used)]
    fn force_compact_preserves_error_flag_in_retained_history() {
        let mut history = vec![
            ChatMessage::system("system"),
            ChatMessage::user("msg1"),
            ChatMessage::user("msg2"),
            ChatMessage::tool_error_result("call-1", "failed"),
            ChatMessage::user("msg4"),
            ChatMessage::user("msg5"),
            ChatMessage::user("msg6"),
        ];

        force_compact(&mut history);

        let retained_error = history
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("call-1"))
            .expect("recent tool error remains in compacted history");
        assert!(retained_error.is_error);
    }

    struct FakeProvider {
        calls: AtomicUsize,
        prompts: Mutex<Vec<String>>,
        responses: Mutex<VecDeque<crate::session::harness::types::ChatResponse>>,
    }

    #[async_trait::async_trait]
    impl ApiProvider for FakeProvider {
        async fn chat(
            &self,
            request: &crate::session::harness::types::ChatRequest,
            _execution: crate::model_control::ModelExecutionCapability,
        ) -> crate::error::Result<crate::session::harness::types::ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.prompts.lock().await.push(
                request
                    .messages
                    .iter()
                    .map(|message| message.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
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

    /// Model-call control for a paid Harness session in Normal mode: it
    /// denies background compaction, so a fired trigger surfaces as an error
    /// and an unfired one as `Ok(None)`, both without a provider call.
    async fn denying_compaction_control() -> Arc<dyn ModelCallControl> {
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
        Arc::new(StoreBackedModelCallControl::new(
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
        ))
    }

    fn fake_provider() -> FakeProvider {
        FakeProvider {
            calls: AtomicUsize::new(0),
            prompts: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::from(vec![])),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn compaction_threshold_is_the_lower_of_the_window_share_and_the_budget() {
        assert_eq!(compaction_threshold_tokens(1_000_000, None), 750_000);
        assert_eq!(
            compaction_threshold_tokens(1_000_000, Some(128_000)),
            128_000
        );
        // A budget above 75% of a small window never raises the trigger.
        assert_eq!(compaction_threshold_tokens(100_000, Some(128_000)), 75_000);
    }

    /// #966: an OpenRouter-sized window (1M) no longer lets live context grow
    /// to 750k first; compaction is attempted once the budget is crossed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn auto_compact_fires_at_the_absolute_budget_below_the_window_share() {
        let mut history = vec![ChatMessage::system("system")];
        for idx in 0..30 {
            history.push(ChatMessage::user(format!(
                "message {idx} {}",
                "x".repeat(4_000)
            )));
        }
        let live: u64 = history.iter().map(ChatMessage::estimated_tokens).sum();
        assert!(live > 20_000 && live < 64_000, "fixture size {live}");
        let before = history.clone();

        for quiet_budget in [None, Some(64_000)] {
            let provider = fake_provider();
            let compacted = auto_compact(
                &mut history,
                &provider,
                "deepseek/deepseek-v4.1-flash",
                1_000_000,
                quiet_budget,
                None,
                &tokio_util::sync::CancellationToken::new(),
                denying_compaction_control().await,
            )
            .await
            .expect("below the trigger nothing is compacted");
            assert!(compacted.is_none());
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        }
        assert_eq!(history.len(), before.len());

        let provider = fake_provider();
        let record = auto_compact(
            &mut history,
            &provider,
            "deepseek/deepseek-v4.1-flash",
            1_000_000,
            Some(20_000),
            None,
            &tokio_util::sync::CancellationToken::new(),
            denying_compaction_control().await,
        )
        .await
        .expect("the budget trigger reaches the compaction gate")
        .expect("a denied summary still yields a truncation record");
        assert_eq!(record.summary, None);
        assert_eq!(record.fallback.as_deref(), Some("policy_denied"));
        assert_eq!(record.kept_messages, 20);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn auto_compact_denial_performs_zero_provider_calls() {
        let control = denying_compaction_control().await;
        let provider = fake_provider();
        let mut history = vec![ChatMessage::system("system")];
        for idx in 0..160 {
            history.push(ChatMessage::user(format!(
                "message {idx} {}",
                "x".repeat(64)
            )));
        }
        let record = auto_compact(
            &mut history,
            &provider,
            "gpt-5.4",
            100_000,
            None,
            None,
            &tokio_util::sync::CancellationToken::new(),
            control,
        )
        .await
        .expect("compaction denied by default still succeeds via truncation")
        .expect("the trigger produced a truncation record");
        assert_eq!(record.summary, None);
        assert_eq!(record.fallback.as_deref(), Some("policy_denied"));
        assert_eq!(record.kept_messages, 20);
        // System prompt, pinned first task + latest summarized instruction,
        // last 20.
        assert_eq!(history.len(), 23);
        assert_eq!(history[0].content, "system");
        assert!(history[1].content.starts_with("message 0 "));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn focused_compaction_preserves_named_items_in_the_provider_summary() {
        let mut history = vec![ChatMessage::system("system")];
        for idx in 0..30 {
            history.push(ChatMessage::user(format!("message {idx}")));
        }
        let provider = FakeProvider {
            calls: AtomicUsize::new(0),
            prompts: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::from(vec![
                crate::session::harness::types::ChatResponse {
                    content: "Keep the failing tests and the remaining migration steps."
                        .to_string(),
                    tool_calls: Vec::new(),
                    usage: Default::default(),
                    reasoning_content: None,
                    stop_reason: None,
                },
            ])),
        };

        let record = auto_compact(
            &mut history,
            &provider,
            "gpt-5.4",
            1_000_000,
            None,
            Some("Preserve the failing tests and remaining migration steps"),
            &tokio_util::sync::CancellationToken::new(),
            Arc::new(crate::model_control::call_control::NoopModelCallControl),
        )
        .await
        .expect("focused compaction succeeds")
        .expect("the focus forces compaction");

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        let prompt = provider
            .prompts
            .lock()
            .await
            .first()
            .cloned()
            .expect("summary prompt");
        assert!(prompt.contains("Preserve the failing tests and remaining migration steps"));
        let summary = record.summary.expect("provider summary");
        assert!(summary.contains("failing tests"));
        assert!(summary.contains("remaining migration steps"));
        assert!(history.iter().any(|m| m.content.contains(&summary)));
    }

    fn task_history(task: &str, pairs: usize) -> Vec<ChatMessage> {
        task_history_with_ids(task, pairs, task)
    }

    fn task_history_with_ids(task: &str, pairs: usize, id_prefix: &str) -> Vec<ChatMessage> {
        let mut history = vec![ChatMessage::system("system"), ChatMessage::user(task)];
        for idx in 0..pairs {
            let mut call = ChatMessage::assistant(format!("step {idx}"));
            call.tool_calls = vec![crate::session::harness::types::ToolCall {
                id: format!("{id_prefix}-{idx}"),
                name: "read".to_string(),
                arguments: "{}".to_string(),
                hosted: false,
                hosted_result: None,
            }];
            history.push(call);
            history.push(ChatMessage::tool_result(
                format!("{id_prefix}-{idx}"),
                format!("result {idx}"),
            ));
        }
        history
    }

    /// The pairing repair the loop runs before each request leaves a history
    /// that a second pass finds fully valid, and the task survives it.
    fn assert_pairing_valid(history: &[ChatMessage], task: &str) {
        let mut repaired = history.to_vec();
        crate::session::harness::normalize::normalize_history(&mut repaired);
        assert_eq!(repaired[1].content, task);
        let mut again = repaired.clone();
        let repair = crate::session::harness::normalize::normalize_history(&mut again);
        assert_eq!((repair.synthesized, repair.dropped), (0, 0));
        assert_eq!(again.len(), repaired.len());
    }

    /// #1058: however many times history is compacted, the first user task
    /// message stays verbatim right after the system prompt.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn forced_compactions_keep_the_original_task_verbatim() {
        let task = "Review commits abc123 and def456; report two findings.";
        for summarize in [false, true] {
            let mut history = task_history(task, 40);
            let provider = FakeProvider {
                calls: AtomicUsize::new(0),
                prompts: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::new()),
            };
            for round in 0..6 {
                let control: Arc<dyn ModelCallControl> = if summarize {
                    Arc::new(crate::model_control::call_control::NoopModelCallControl)
                } else {
                    denying_compaction_control().await
                };
                // Fresh work between compactions, as in a long turn.
                history
                    .extend(task_history_with_ids("unused", 30, &format!("r{round}")).split_off(2));
                if summarize {
                    provider.responses.lock().await.push_back(
                        crate::session::harness::types::ChatResponse {
                            content: format!("summary {round}"),
                            tool_calls: Vec::new(),
                            usage: Default::default(),
                            reasoning_content: None,
                            stop_reason: None,
                        },
                    );
                }
                let record = auto_compact(
                    &mut history,
                    &provider,
                    "gpt-5.4",
                    1_000_000,
                    None,
                    Some("force a compaction"),
                    &tokio_util::sync::CancellationToken::new(),
                    control,
                )
                .await
                .expect("compaction succeeds")
                .expect("focus forces compaction");
                assert_eq!(record.summary.is_some(), summarize);
                assert_eq!(history[0].content, "system");
                assert_eq!(history[1].role, MessageRole::User);
                assert_eq!(history[1].content, task, "round {round}");
                // The summary never carries the pinned task back through the
                // summarizer as transcript.
                let prompts = provider.prompts.lock().await;
                if let Some(prompt) = prompts.last().filter(|_| summarize) {
                    assert!(!prompt.contains(task));
                }
                drop(prompts);
                assert_pairing_valid(&history, task);
            }
        }
    }

    /// #1058: a continuation's latest user instruction survives compaction
    /// verbatim next to the original task.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn compaction_keeps_the_latest_instruction_and_the_task() {
        let mut history = task_history("original task", 30);
        history.push(ChatMessage::user("continuation instruction"));
        history.extend(task_history_with_ids("unused", 30, "later").split_off(2));
        let tail_start = history.len() - 20;
        compact_in_place(
            &mut history,
            tail_start,
            Some(ChatMessage::system("[Conversation summary: s]")),
        );
        let contents: Vec<&str> = history.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            &contents[..4],
            [
                "system",
                "original task",
                "continuation instruction",
                "[Conversation summary: s]"
            ]
        );
        assert_eq!(history.len(), 4 + 20);

        let mut trimmed = task_history("original task", 60);
        hard_trim(&mut trimmed, 30);
        assert!(trimmed.len() <= 30);
        assert_eq!(trimmed[1].content, "original task");
    }

    /// #1058: the message-count trigger is a high backstop below `hard_trim`'s
    /// cap, not a compaction every ~25 tool calls.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn message_count_trigger_does_not_fire_at_fifty_messages() {
        let mut history = task_history("task", 30);
        assert!(history.len() > 50 && history.len() < AUTO_COMPACT_MESSAGE_TRIGGER);
        let provider = fake_provider();
        let compacted = auto_compact(
            &mut history,
            &provider,
            "gpt-5.4",
            1_000_000,
            None,
            None,
            &tokio_util::sync::CancellationToken::new(),
            denying_compaction_control().await,
        )
        .await
        .expect("no compaction");
        assert!(compacted.is_none());
        assert!(
            AUTO_COMPACT_MESSAGE_TRIGGER < 200,
            "must stay below hard_trim"
        );
    }
}
