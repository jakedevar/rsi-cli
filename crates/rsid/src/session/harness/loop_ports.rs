//! Daemon implementations of the `rsid-harness` loop ports (#2648).
//!
//! The turn loop lives in `rsid-harness`; every authority it needs is reached through these
//! adapters: provider and credentials, model-call admission and settlement, the tool
//! registry with its policy and process custody, agent mail, and worktree digests.

use crate::bench::trace::TraceSink;
use crate::error::DaemonError;
use crate::model_control::call_control::{
    AdmittedModelCall, ModelCallControl, ModelCallKind, ModelCallSettlement, ModelCallUsage,
};
use crate::sandbox::{WorktreeDigest, worktree_content_digest};
use crate::session::harness::agent_mail::{HarnessMailBoundary, HarnessMailDelivery};
use crate::session::harness::compaction::DaemonSummaryTransport;
use crate::session::harness::errors::{ProviderError, ProviderErrorExt};
use crate::session::harness::provider::ApiProvider;
use crate::session::harness::tools::{HarnessToolRegistry, ToolExecutionMode};
use crate::session::harness::types::{
    ChatMessage, ChatRequest, ChatResponse, HarnessToolSpec, StreamChunk, TokenUsage, ToolResult,
};
use async_trait::async_trait;
use rsi_common::bench::BENCH_TRACE_WRITE_FAILED;
use rsid_harness::compaction::{CompactionError, SummaryOutcome, SummaryTransport};
use rsid_harness::ports::{
    Admission, FaultKind, GateDigest, GateOutcome, HostedCapabilities, LoopFault, LoopTools,
    MailBoundary, MailDelivery, ModelPort,
};
use rsid_harness::stream::StreamEvent;
use std::any::Any;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Describe a daemon error to the loop, keeping the original for the daemon's callers.
pub(super) fn fault_from(error: DaemonError) -> LoopFault {
    let kind = match &error {
        DaemonError::PolicyDenied(reason) => FaultKind::PolicyDenied(reason.clone()),
        DaemonError::StreamFallbackRequired(reason) => {
            FaultKind::StreamFallbackRequired(reason.clone())
        }
        DaemonError::ChannelClosed => FaultKind::ChannelClosed,
        DaemonError::Process(message) => FaultKind::Process(message.clone()),
        _ => FaultKind::Other,
    };
    let provider = ProviderError::from_daemon_error(&error);
    LoopFault::new(kind, error.to_string())
        .with_provider(provider)
        .with_origin(Box::new(error))
}

/// The daemon error behind a loop fault: the original when the daemon attached one.
pub(super) fn fault_into(mut fault: LoopFault) -> DaemonError {
    if let Some(origin) = fault.take_origin()
        && let Ok(error) = origin.downcast::<DaemonError>()
    {
        return *error;
    }
    if let Some(provider) = fault.provider_error() {
        return provider.clone().into_daemon_error();
    }
    match fault.kind() {
        FaultKind::PolicyDenied(reason) => DaemonError::PolicyDenied(reason.clone()),
        FaultKind::StreamFallbackRequired(reason) => {
            DaemonError::StreamFallbackRequired(reason.clone())
        }
        FaultKind::ChannelClosed => DaemonError::ChannelClosed,
        FaultKind::Process(message) => DaemonError::Process(message.clone()),
        FaultKind::Other => DaemonError::Process(fault.to_string()),
    }
}

/// The provider and the session's model-call control.
pub(super) struct DaemonModelPort {
    provider: Arc<dyn ApiProvider>,
    control: Arc<dyn ModelCallControl>,
    /// #3178: a bench session's live trace.
    trace: Option<Arc<TraceSink>>,
}

impl DaemonModelPort {
    pub(super) fn new(
        provider: Box<dyn ApiProvider>,
        control: Arc<dyn ModelCallControl>,
        trace: Option<Arc<TraceSink>>,
    ) -> Self {
        Self {
            provider: Arc::from(provider),
            control,
            trace,
        }
    }
}

#[async_trait]
impl SummaryTransport for DaemonModelPort {
    async fn summarize(
        &self,
        request: &ChatRequest,
        admission_key: &str,
        transcript: &str,
        cancel: &CancellationToken,
    ) -> Result<SummaryOutcome, CompactionError> {
        DaemonSummaryTransport {
            provider: &*self.provider,
            model_call_control: Arc::clone(&self.control),
        }
        .summarize(request, admission_key, transcript, cancel)
        .await
    }
}

#[async_trait]
impl ModelPort for DaemonModelPort {
    fn provider_name(&self) -> String {
        self.provider.name().to_owned()
    }

    fn supports_native_tools(&self) -> bool {
        self.provider.supports_native_tools()
    }

    fn supports_streaming(&self) -> bool {
        self.provider.supports_streaming()
    }

    fn accepts_tool_choice_none(&self) -> bool {
        self.provider.accepts_tool_choice_none()
    }

    fn default_max_output_tokens(&self) -> Option<u32> {
        self.provider.default_max_output_tokens()
    }

    fn hosted_capabilities(&self, model: &str) -> HostedCapabilities {
        let capabilities = self.provider.web_capabilities(model);
        HostedCapabilities {
            names: capabilities
                .tools
                .iter()
                .map(|tool| tool.name.to_owned())
                .collect(),
            specs: capabilities.specs(),
        }
    }

    async fn admit_primary(
        &self,
        dedup_hint: &str,
        fingerprint_hint: &str,
        retry_of: Option<Uuid>,
    ) -> Result<Box<dyn Admission>, LoopFault> {
        let admitted = self
            .control
            .admit(
                ModelCallKind::Primary,
                dedup_hint,
                fingerprint_hint,
                retry_of,
            )
            .await
            .map_err(fault_from)?;
        Ok(Box::new(DaemonAdmission {
            provider: Arc::clone(&self.provider),
            control: Arc::clone(&self.control),
            state: AdmissionState::Admitted(admitted),
            trace: self.trace.clone(),
        }))
    }

    async fn fail_pending(&self, error_class: &str) -> Result<(), LoopFault> {
        self.control
            .fail_pending(error_class)
            .await
            .map_err(fault_from)
    }
}

/// Where one admitted call is: carrying its execution authority, sent (settlement only) or
/// settled.
enum AdmissionState {
    Admitted(AdmittedModelCall),
    Sent(ModelCallSettlement),
    Settled,
}

/// One admitted call: its settlement and the single-use execution capability.
struct DaemonAdmission {
    provider: Arc<dyn ApiProvider>,
    control: Arc<dyn ModelCallControl>,
    state: AdmissionState,
    trace: Option<Arc<TraceSink>>,
}

impl DaemonAdmission {
    /// Take the admitted call out for its one send.
    fn take_call(&mut self) -> Result<AdmittedModelCall, LoopFault> {
        match std::mem::replace(&mut self.state, AdmissionState::Settled) {
            AdmissionState::Admitted(call) => Ok(call),
            other => {
                self.state = other;
                Err(LoopFault::process(
                    "model call execution authority already spent",
                ))
            }
        }
    }

    /// Record a call's outcome in the bench trace, if the session has one.
    fn record_response(&self, result: &std::result::Result<ChatResponse, DaemonError>) {
        let Some(trace) = &self.trace else {
            return;
        };
        match result {
            Ok(response) => trace.model_response(response),
            Err(error) => trace.model_error(&error.to_string()),
        }
    }

    fn take_settlement(&mut self) -> Result<ModelCallSettlement, LoopFault> {
        match std::mem::replace(&mut self.state, AdmissionState::Settled) {
            AdmissionState::Admitted(call) => Ok(call.into_parts().0),
            AdmissionState::Sent(settlement) => Ok(settlement),
            AdmissionState::Settled => Err(LoopFault::process("model call already settled")),
        }
    }
}

#[async_trait]
impl Admission for DaemonAdmission {
    fn invocation_id(&self) -> Option<Uuid> {
        match &self.state {
            AdmissionState::Admitted(call) => call.invocation_id(),
            AdmissionState::Sent(settlement) => settlement.invocation_id(),
            AdmissionState::Settled => None,
        }
    }

    async fn chat(&mut self, request: &ChatRequest) -> Result<ChatResponse, LoopFault> {
        let (settlement, execution) = self.take_call()?.into_parts();
        self.state = AdmissionState::Sent(settlement);
        if let Some(trace) = &self.trace {
            trace.model_request(request);
        }
        let result = self.provider.chat(request, execution).await;
        self.record_response(&result);
        result.map_err(fault_from)
    }

    async fn stream_chat(
        &mut self,
        request: &ChatRequest,
        chunks: mpsc::Sender<StreamChunk>,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse, LoopFault> {
        let (settlement, execution) = self.take_call()?.into_parts();
        self.state = AdmissionState::Sent(settlement);
        if let Some(trace) = &self.trace {
            trace.model_request(request);
        }
        let result = self
            .provider
            .stream_chat(request, chunks, cancel, execution)
            .await;
        self.record_response(&result);
        result.map_err(fault_from)
    }

    async fn fail(mut self: Box<Self>, error_class: &str) -> Result<(), LoopFault> {
        let settlement = self.take_settlement()?;
        self.control
            .fail(settlement, error_class)
            .await
            .map_err(fault_from)
    }

    async fn complete(mut self: Box<Self>, usage: &TokenUsage) -> Result<(), LoopFault> {
        let settlement = self.take_settlement()?;
        self.control
            .complete(
                settlement,
                ModelCallUsage {
                    input_tokens: Some(usage.prompt_tokens),
                    output_tokens: Some(usage.completion_tokens),
                    cache_creation_tokens: Some(usage.cache_creation_tokens),
                    cache_read_tokens: Some(usage.cache_read_tokens),
                    reasoning_tokens: Some(usage.reasoning_tokens),
                    estimated_cost_usd: usage.cost_usd,
                    ..ModelCallUsage::default()
                },
            )
            .await
            .map_err(fault_from)?;
        // #3349: the loop checks trace health only at the next model admission,
        // and a final response has none. A call whose trace line was lost ends
        // the session here, after its usage is recorded, with the typed code.
        if let Some(trace) = &self.trace
            && !trace.healthy()
        {
            return Err(fault_from(DaemonError::PolicyDenied(format!(
                "{BENCH_TRACE_WRITE_FAILED}: a trace line could not be written, so the trial has no full trace"
            ))));
        }
        Ok(())
    }
}

/// The session's tool registry, policy and completion-gate runner.
pub(super) struct DaemonTools(
    pub(super) Arc<HarnessToolRegistry>,
    pub(super) Option<Arc<TraceSink>>,
);

#[async_trait]
impl LoopTools for DaemonTools {
    async fn process_turn_guard(&self) -> Box<dyn Any + Send + Sync> {
        Box::new(self.0.process_turn_guard().await)
    }

    async fn take_process_notices(&self) -> Vec<String> {
        self.0.take_process_notices().await
    }

    fn set_context_usage(&self, used_tokens: u64, token_limit: u64) {
        self.0.set_context_usage(used_tokens, token_limit);
    }

    async fn take_compaction_focus(&self) -> Option<String> {
        self.0.take_compaction_focus().await
    }

    fn specs(&self) -> Vec<HarnessToolSpec> {
        self.0.specs()
    }

    fn filter_hosted_specs(&self, specs: Vec<HarnessToolSpec>) -> Vec<HarnessToolSpec> {
        self.0.filter_hosted_specs(specs)
    }

    fn context_editing_enabled(&self) -> bool {
        self.0.context_editing_enabled()
    }

    fn recovery_max_output_tokens(&self) -> Option<u32> {
        self.0.recovery_max_output_tokens()
    }

    fn execution_mode(&self, name: &str) -> Option<ToolExecutionMode> {
        self.0.execution_mode(name)
    }

    fn admit_hosted_call(&self, name: &str) -> Result<(), String> {
        match self.0.policy() {
            Some(policy) => policy.admit_call(name, true).map_err(|error| error.text()),
            None => Ok(()),
        }
    }

    fn record_hosted_result_bytes(&self, bytes: usize) {
        if let Some(policy) = self.0.policy() {
            policy.record_result_bytes(bytes);
        }
    }

    fn unknown_tool_message(&self, name: &str) -> String {
        self.0.unknown_tool_message(name)
    }

    fn parameters_json(&self, name: &str) -> Option<String> {
        self.0.parameters_json(name).map(str::to_owned)
    }

    async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
        events: mpsc::Sender<StreamEvent>,
    ) -> ToolResult {
        if let Some(trace) = &self.1 {
            trace.tool_call(name, &args);
        }
        let result = self
            .0
            .execute_with_context(name, args, working_dir, cancel, Some(events))
            .await;
        if let Some(trace) = &self.1 {
            trace.tool_result(name, &result);
        }
        result
    }

    async fn execute_completion_gate(
        &self,
        gate: &rsi_common::completion_gates::CompletionGate,
        working_dir: &Path,
        cancel: &CancellationToken,
    ) -> GateOutcome {
        let outcome = self
            .0
            .execute_completion_gate(gate, working_dir, cancel)
            .await;
        GateOutcome {
            success: outcome.success,
            output: outcome.output,
            exit_code: outcome.exit_code,
            timed_out: outcome.timed_out,
            cancelled: outcome.cancelled,
        }
    }

    fn worktree_digest(&self, working_dir: &Path) -> Result<GateDigest, String> {
        worktree_content_digest(working_dir)
            .map(|digest: WorktreeDigest| match digest.event_value() {
                Some(value) => GateDigest::bounded(value),
                None => GateDigest::unbounded(),
            })
            .map_err(|error| error.to_string())
    }
}

/// The session's agent-mail boundary.
pub(super) struct DaemonMail(pub(super) Arc<dyn HarnessMailBoundary>);

#[async_trait]
impl MailBoundary for DaemonMail {
    async fn claim(
        &self,
        model_invocation_id: Uuid,
    ) -> Result<Option<Box<dyn MailDelivery>>, String> {
        match self.0.claim(model_invocation_id).await {
            Ok(Some(delivery)) => Ok(Some(Box::new(DaemonMailDelivery(delivery)))),
            Ok(None) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

struct DaemonMailDelivery(Box<dyn HarnessMailDelivery>);

#[async_trait]
impl MailDelivery for DaemonMailDelivery {
    fn append(&mut self, history: &mut Vec<ChatMessage>, request: &mut ChatRequest) {
        self.0.append(history, request);
    }

    fn undo_append(&mut self, history: &mut Vec<ChatMessage>, request: &mut ChatRequest) {
        self.0.undo_append(history, request);
    }

    async fn record_effect_possible(&mut self) {
        self.0.record_effect_possible().await;
    }

    async fn record_rejected(&mut self, error_class: &'static str) {
        self.0.record_rejected(error_class).await;
    }
}
