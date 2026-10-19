//! Context compaction facade: the tiers and rebuild rules live in `rsid-harness` (#2648).
//!
//! The daemon owns the summary call's authority: model-call admission, settlement and
//! the provider.

use crate::error::DaemonError;
use crate::model_control::call_control::{ModelCallControl, ModelCallKind, ModelCallUsage};
use crate::session::harness::provider::ApiProvider;
#[cfg(test)]
use crate::session::harness::types::ChatMessage;
use crate::session::harness::types::ChatRequest;
#[cfg(test)]
use anyhow::Result;
use async_trait::async_trait;
use rsid_harness::compaction::{CompactionError, SummaryOutcome, SummaryTransport};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[allow(unused_imports)] // Preserves the old module path.
pub use rsid_harness::compaction::{
    AUTO_COMPACT_MESSAGE_TRIGGER, CompactionRecord, compact_in_place, compaction_threshold_tokens,
    force_compact, hard_trim,
};

fn transport_error(error: DaemonError) -> CompactionError {
    CompactionError(error.to_string())
}

/// Admits, runs and settles the summary call against the session's model-call control.
pub(super) struct DaemonSummaryTransport<'a> {
    pub(super) provider: &'a dyn ApiProvider,
    pub(super) model_call_control: Arc<dyn ModelCallControl>,
}

#[async_trait]
impl SummaryTransport for DaemonSummaryTransport<'_> {
    async fn summarize(
        &self,
        request: &ChatRequest,
        admission_key: &str,
        transcript: &str,
        cancel: &CancellationToken,
    ) -> std::result::Result<SummaryOutcome, CompactionError> {
        let admitted_call = match self
            .model_call_control
            .admit(ModelCallKind::Compaction, admission_key, transcript, None)
            .await
        {
            Ok(call) => call,
            Err(DaemonError::PolicyDenied(reason)) => {
                tracing::warn!(%reason, "Auto-compact summary denied by model-call policy");
                return Ok(SummaryOutcome::PolicyDenied);
            }
            Err(error) => return Err(transport_error(error)),
        };
        let (settlement, execution) = admitted_call.into_parts();

        let summary_result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.model_call_control
                    .fail(settlement, "interrupted")
                    .await
                    .map_err(transport_error)?;
                return Ok(SummaryOutcome::Interrupted);
            },
            res = self.provider.chat(request, execution) => res,
        };

        match summary_result {
            Ok(resp) => {
                self.model_call_control
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
                    .await
                    .map_err(transport_error)?;
                Ok(SummaryOutcome::Summary(resp.content))
            }
            Err(error) => {
                self.model_call_control
                    .fail(settlement, "compaction_failed")
                    .await
                    .map_err(transport_error)?;
                Ok(SummaryOutcome::Failed(error.to_string()))
            }
        }
    }
}

/// Tier 1 — Auto-compaction (see `rsid_harness::compaction::auto_compact`).
#[cfg(test)]
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
    let transport = DaemonSummaryTransport {
        provider,
        model_call_control,
    };
    Ok(rsid_harness::compaction::auto_compact(
        history,
        &transport,
        model,
        token_limit,
        budget_tokens,
        focus,
        cancel,
    )
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_control::call_control::StoreBackedModelCallControl;
    use crate::model_control::{AdmissionDecision, admit_invocation};
    use crate::session::harness::provider::ApiProvider;
    use crate::session::harness::types::MessageRole;
    use crate::store::Store;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::{Mutex, mpsc};

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
        rsid_harness::normalize::normalize_history(&mut repaired);
        assert_eq!(repaired[1].content, task);
        let mut again = repaired.clone();
        let repair = rsid_harness::normalize::normalize_history(&mut again);
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
}
