//! The harness turn loop facade (#2648): the algorithm lives in `rsid-harness`; this module
//! binds it to the daemon's provider, model-call control, tool registry and agent mail.

use crate::claude::StreamEvent;
use crate::error::DaemonError;
use crate::model_control::call_control::ModelCallControl;
use crate::session::harness::agent_mail::HarnessMailBoundary;
use crate::session::harness::loop_ports::{DaemonMail, DaemonModelPort, DaemonTools, fault_into};
use crate::session::harness::provider::ApiProvider;
use crate::session::harness::tools::HarnessToolRegistry;
use crate::session::harness::types::*;
use rsi_common::completion_gates::CompletionGates;
use rsid_harness::agent_loop::{LoopParams, run_loop};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use rsid_harness::agent_loop::{
    COMPACTION_STREAM_EVENT, COMPLETION_GATE_STREAM_EVENT, FINAL_ANSWER_RETRY_STREAM_EVENT,
    LENGTH_ACT_RETRY_STREAM_EVENT, REPEAT_FAILURE_STREAM_EVENT, STREAM_SUPERSEDED_STREAM_EVENT,
};

#[cfg(test)]
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
    max_iterations: u32,                            // per-turn cap (default 150)
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
        Default::default(),
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
    compaction: rsid_harness::compaction::CompactionPolicy,
    agent_mail_boundary: Option<Arc<dyn HarnessMailBoundary>>,
    completion_gates: Option<CompletionGates>,
    completion_gates_enabled: bool,
) -> Result<(), DaemonError> {
    // #3178: a bench session's live trace rides on its tool registry.
    let trace = tools.trace_sink();
    let port = DaemonModelPort::new(provider, model_call_control, trace.clone());
    run_loop(
        &port,
        Arc::new(DaemonTools(tools, trace)),
        LoopParams {
            system_prompt,
            user_query,
            working_dir,
            model,
            reasoning_effort,
            max_iterations,
            token_limit,
            conversation_history,
            compact_budget,
            compaction,
            completion_gates,
            completion_gates_enabled,
        },
        event_tx,
        cancel,
        agent_mail_boundary.map(|boundary| {
            Arc::new(DaemonMail(boundary)) as Arc<dyn rsid_harness::ports::MailBoundary>
        }),
    )
    .await
    .map_err(fault_into)
}

#[cfg(test)]
pub(in crate::session::harness) mod tests {
    use super::*;
    use crate::model_control::call_control::StoreBackedModelCallControl;
    use crate::model_control::{AdmissionDecision, admit_invocation};
    use crate::session::harness::agent_mail::{TestHarnessMailBoundary, TestHarnessMailDelivery};
    use crate::session::harness::errors::{ProviderError, ProviderErrorExt};
    use crate::session::harness::tools::ToolExecutionMode;
    use crate::store::Store;
    use rsi_common::completion_gates::CompletionGate;
    use rsid_harness::agent_loop::{FINAL_ANSWER_RETRY_EFFORT, FINAL_ANSWER_RETRY_PROMPT};
    use rusqlite::params;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::{Barrier, Mutex};

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
                reasoning_details: None,
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
                reasoning_details: None,
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
                reasoning_details: None,
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

    pub(in crate::session::harness) async fn root_permit(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        session_id: uuid::Uuid,
    ) -> crate::model_control::AdmissionPermit {
        root_permit_keyed(store, session_id, "root").await
    }

    pub(in crate::session::harness) async fn root_permit_keyed(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
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

    pub(in crate::session::harness) fn controller(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
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
                reasoning_details: None,
                stop_reason: None,
            },
            ChatResponse {
                content: "done".to_string(),
                tool_calls: Vec::new(),
                usage: TokenUsage::default(),
                reasoning_content: None,
                reasoning_details: None,
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
            reasoning_details: None,
            stop_reason: Some("end_turn".into()),
        }
    }

    async fn run_with_policy(
        policy: rsi_common::harness_tool_policy::HarnessToolPolicy,
        responses: Vec<ChatResponse>,
    ) -> (Vec<ChatRequest>, Vec<StreamEvent>) {
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
                    reasoning_details: None,
                    stop_reason: Some("tool_use".into()),
                }),
                Ok(ChatResponse {
                    content: "done".into(),
                    tool_calls: Vec::new(),
                    usage: TokenUsage::default(),
                    reasoning_content: None,
                    reasoning_details: None,
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
                    reasoning_details: None,
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

    /// #2250: the route's hosted `web_fetch` and the native tool share a name;
    /// the model sees one. When the provider rejects the hosted tools (400),
    /// the next request carries the native function spec instead.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn hosted_web_fetch_wins_over_the_native_tool_until_the_provider_rejects_it() {
        use crate::session::harness::tools::web_fetch::WebFetchTool;
        use crate::session::harness::types::HarnessToolSpecKind;

        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
                    reasoning_details: None,
                    stop_reason: Some("end_turn".into()),
                }),
            ])),
        };
        let mut registry = HarnessToolRegistry::new();
        registry.register(Arc::new(WebFetchTool::new()));
        let (event_tx, _event_rx) = mpsc::channel(64);

        run_harness_loop(
            Box::new(provider),
            Arc::new(registry),
            String::new(),
            "fetch".to_string(),
            std::env::temp_dir(),
            "claude-sonnet-5".to_string(),
            None,
            event_tx,
            CancellationToken::new(),
            controller(&store, session_id, permit),
            3,
            100_000,
            None,
        )
        .await
        .expect("hosted-tool rejection must not end the session");

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        let fetch_specs = |request: &ChatRequest| -> Vec<bool> {
            request
                .tools
                .iter()
                .filter(|tool| tool.name == "web_fetch")
                .map(|tool| tool.kind.is_hosted())
                .collect()
        };
        // Exactly one `web_fetch`, and it is the provider-hosted one.
        assert_eq!(fetch_specs(&requests[0]), [true]);
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|tool| tool.name == "web_search" && tool.kind.is_hosted())
        );
        // After the 400 the hosted tools are off and the native function
        // spec is what the model now sees.
        assert_eq!(fetch_specs(&requests[1]), [false]);
        let native = requests[1]
            .tools
            .iter()
            .find(|tool| tool.name == "web_fetch")
            .expect("native web_fetch spec");
        assert!(matches!(native.kind, HarnessToolSpecKind::Function));
        assert!(requests[1].tools.iter().all(|tool| !tool.kind.is_hosted()));
    }

    async fn captured_harness_request_effort(reasoning_effort: Option<String>) -> Option<String> {
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let prior: Vec<ChatMessage> = (0..185)
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
        let mut persisted: Vec<ConversationEvent> = (0..185)
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
        assert_eq!(rebuilt[1].content, "prior 165");
        assert_eq!(rebuilt[2].content, "[Conversation summary: done]");
        assert_eq!(rebuilt.len(), 23);
    }

    /// #974 acceptance 3: the default paid controller denies the compaction
    /// summary call in Normal mode, so a 186-message history falls back to local
    /// truncation, the turn completes, and the emitted compaction event carries
    /// the fallback.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn policy_denied_compaction_truncates_and_the_turn_completes() {
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let prior: Vec<ChatMessage> = (0..185)
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
                    reasoning_details: None,
                    stop_reason: None,
                },
                ChatResponse {
                    content: "done".to_string(),
                    tool_calls: Vec::new(),
                    usage: TokenUsage::default(),
                    reasoning_content: None,
                    reasoning_details: None,
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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

    /// Hub #748 strand 1: a direct Harness session's second physical turn
    /// failed with `model invocation dedup conflict for key
    /// session.harness.turn:<sid>:2:root:turn:1` because the child key was
    /// rebuilt from the session id, an ordinal and an iteration hint that all
    /// restart per turn. Two turns of one session, each under its own root
    /// permit and with three model calls (two child admissions), must admit
    /// with distinct child keys while the session stays the owner, and an
    /// exact replay of one child key must still dedup instead of admitting a
    /// second execution.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn second_harness_turn_admits_distinct_child_keys_and_a_replayed_key_dedups() {
        use rsi_common::model_control::ModelInvocationPurpose;

        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
        let session_id = uuid::Uuid::new_v4();
        let mut roots = Vec::new();
        for (turn, prefix) in ["turn-1", "turn-2"].into_iter().enumerate() {
            let permit = root_permit_keyed(&store, session_id, prefix).await;
            roots.push(permit.invocation_id());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let provider = CapturingProvider {
                request_efforts: Arc::new(Mutex::new(Vec::new())),
                request_messages: Arc::clone(&requests),
                responses: Some(Arc::new(Mutex::new(VecDeque::from([
                    tool_only_response(&format!("t{turn}-a")),
                    tool_only_response(&format!("t{turn}-b")),
                    ChatResponse {
                        content: "done".to_string(),
                        tool_calls: Vec::new(),
                        usage: TokenUsage::default(),
                        reasoning_content: None,
                        reasoning_details: None,
                        stop_reason: None,
                    },
                ])))),
            };
            let (event_tx, _event_rx) = mpsc::channel(64);
            let history = (turn > 0).then(|| vec![ChatMessage::user("first turn")]);
            run_harness_loop(
                Box::new(provider),
                Arc::new(HarnessToolRegistry::new()),
                String::new(),
                format!("turn {turn}"),
                std::env::temp_dir(),
                "claude-opus-5".to_string(),
                None,
                event_tx,
                CancellationToken::new(),
                controller(&store, session_id, permit),
                8,
                100_000,
                history,
            )
            .await
            .unwrap_or_else(|error| panic!("turn {turn} must admit: {error}"));
            assert_eq!(requests.lock().await.len(), 3, "turn {turn} made 3 calls");
        }
        assert_ne!(roots[0], roots[1], "each turn has its own root permit");

        let children = store
            .lock()
            .await
            .list_model_invocation_records(
                100,
                false,
                Some(ModelInvocationPurpose::SessionHarnessTurn),
                Some(session_id),
            )
            .expect("list child invocations");
        assert_eq!(children.len(), 4, "two child admissions per turn");
        for child in &children {
            assert_eq!(
                child.owner.session_id,
                Some(session_id),
                "the session stays the authorization owner"
            );
        }
        let mut keys: Vec<String> = children
            .iter()
            .map(|child| child.dedup_key.clone().expect("child dedup key"))
            .collect();
        keys.sort();
        keys.dedup();
        assert_eq!(
            keys.len(),
            4,
            "child keys are distinct across turns: {keys:?}"
        );
        for root in &roots {
            let under_root = children
                .iter()
                .filter(|child| child.parent_invocation_id == Some(*root))
                .count();
            assert_eq!(under_root, 2, "two children under root {root}");
        }

        // An exact replay of a recorded child admission still dedups: the
        // ledger returns the existing invocation and admits nothing new.
        let replayed = children
            .iter()
            .find(|child| child.parent_invocation_id == Some(roots[1]))
            .expect("a turn-2 child");
        let request = crate::model_control::ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::SessionHarnessTurn,
            provider: Some("Harness".to_string()),
            model: Some("gpt-5.4".to_string()),
            backend: Some("Harness".to_string()),
            effort: Some("high".to_string()),
            trigger: "test_harness_loop".to_string(),
            owner: rsi_common::model_control::InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            dedup_key: replayed.dedup_key.clone(),
            request_fingerprint: replayed.request_fingerprint.clone(),
            parent_invocation_id: Some(roots[1]),
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                ModelInvocationPurpose::SessionHarnessTurn,
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
        match admit_invocation(&store, request, &bus)
            .await
            .expect("replay admission")
        {
            AdmissionDecision::Duplicate { invocation_id } => {
                assert_eq!(invocation_id, replayed.id, "the replay names the original");
            }
            AdmissionDecision::Admitted(permit) => panic!(
                "an exact replay of {:?} admitted a new invocation {}",
                replayed.dedup_key,
                permit.invocation_id()
            ),
        }
        let after = store
            .lock()
            .await
            .list_model_invocation_records(
                100,
                false,
                Some(ModelInvocationPurpose::SessionHarnessTurn),
                Some(session_id),
            )
            .expect("list after replay");
        assert_eq!(after.len(), 4, "the replay added no ledger row");
    }

    fn tool_only_response(id: &str) -> ChatResponse {
        ChatResponse {
            content: String::new(),
            tool_calls: vec![tool_call(id, "unknown_tool")],
            usage: TokenUsage::default(),
            reasoning_content: None,
            reasoning_details: None,
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
            Default::default(),
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
        // #2279: one `tool_use` per executed call id, so two calls announce twice.
        let mut unique_tool_use_ids = tool_use_ids.clone();
        unique_tool_use_ids.sort_unstable();
        unique_tool_use_ids.dedup();
        assert_eq!(unique_tool_use_ids.len(), 2);
        assert_eq!(tool_use_ids.len(), 2, "{tool_use_ids:?}");
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
                    reasoning_details: None,
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
            reasoning_details: None,
            stop_reason: Some(stop_reason.to_string()),
            ..plain_response()
        }
    }

    /// #1061, #4028: an empty final response cut off by the output limit is first
    /// recovered by the act retries; once those are spent it is retried once, in
    /// the same turn, with a final-answer nudge and a lower effort.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
    #[tokio::test]
    async fn empty_length_final_response_is_retried_once_with_a_nudge() {
        let (requests, events, efforts) = run_scripted_with_efforts(
            vec![
                tool_only_response("a"),
                empty_response_with("length"),
                empty_response_with("length"),
                empty_response_with("length"),
                ChatResponse {
                    content: "the review".to_string(),
                    stop_reason: Some("stop".to_string()),
                    ..plain_response()
                },
            ],
            6,
        )
        .await;
        assert_eq!(requests.len(), 5);
        let nudge = requests[4].last().expect("retry nudge");
        assert_eq!(nudge.role, MessageRole::User);
        assert_eq!(nudge.content, FINAL_ANSWER_RETRY_PROMPT);
        // The retry still carries the tool result from earlier in the turn.
        assert!(
            requests[4]
                .iter()
                .any(|message| message.role == MessageRole::Tool)
        );
        assert_eq!(
            efforts[4].as_deref(),
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
                empty_response_with("length"),
                empty_response_with("length"),
            ],
            6,
        )
        .await;
        assert_eq!(requests.len(), 5);
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
            Default::default(),
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
            Default::default(),
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
            Default::default(),
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
                    reasoning_details: None,
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
                    reasoning_details: None,
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
                    reasoning_details: None,
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
                    reasoning_details: None,
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
                    reasoning_details: None,
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
                    reasoning_details: None,
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        for idx in 0..185 {
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
                    reasoning_details: None,
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
                    reasoning_details: None,
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
                    reasoning_details: None,
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
            reasoning_details: None,
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
            reasoning_details: None,
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
            reasoning_details: None,
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().expect("store"),
        ));
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
