//! Interfaces the harness turn loop calls out through.
//!
//! The loop (`agent_loop`) owns the algorithm: compaction, request building, retry,
//! tool batching, repeat-failure recovery and completion-gate sequencing. Everything that
//! needs daemon authority sits behind these traits and stays in `rsid`:
//!
//! - [`ModelPort`] / [`Admission`]: provider selection, credentials, model-call admission
//!   and settlement, egress, and the execution capability that gates a send;
//! - [`LoopTools`]: the tool registry with its policy, process custody and MCP clients,
//!   and the completion-gate runner;
//! - [`MailBoundary`] / [`MailDelivery`]: agent-mail claim and delivery bookkeeping.
//!
//! No trait here carries a credential, a path to a secret or a network address.

use crate::compaction::SummaryTransport;
use crate::errors::ProviderError;
use crate::stream::StreamEvent;
use crate::tools::ToolExecutionMode;
use crate::types::{
    ChatMessage, ChatRequest, ChatResponse, HarnessToolSpec, StreamChunk, TokenUsage, ToolResult,
};
use async_trait::async_trait;
use std::any::Any;
use std::path::Path;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// What the loop needs to know about why a port call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaultKind {
    /// Model-call policy refused the admission (the reason is policy text).
    PolicyDenied(String),
    /// The streaming attempt failed and a separately admitted blocking call is requested.
    StreamFallbackRequired(String),
    /// The event receiver is gone.
    ChannelClosed,
    /// A plain process failure.
    Process(String),
    /// Anything else the daemon reported.
    Other,
}

/// A port failure. `origin` carries the daemon's own error value so the daemon can hand
/// the very same error back to its callers; the leaf never looks inside it.
pub struct LoopFault {
    kind: FaultKind,
    message: String,
    provider: Option<ProviderError>,
    origin: Option<Box<dyn Any + Send>>,
}

impl LoopFault {
    #[must_use]
    pub fn new(kind: FaultKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            provider: None,
            origin: None,
        }
    }

    #[must_use]
    pub fn channel_closed() -> Self {
        Self::new(FaultKind::ChannelClosed, "channel closed")
    }

    #[must_use]
    pub fn process(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::new(FaultKind::Process(message.clone()), message)
    }

    /// A classified provider failure with no daemon origin.
    #[must_use]
    pub fn provider(error: ProviderError) -> Self {
        let mut fault = Self::new(
            FaultKind::Other,
            format!("provider_error:{}", error.detail_code),
        );
        fault.provider = Some(error);
        fault
    }

    /// Attach the classified provider failure the daemon recovered from its error.
    #[must_use]
    pub fn with_provider(mut self, provider: Option<ProviderError>) -> Self {
        self.provider = provider;
        self
    }

    /// Attach the daemon's own error value.
    #[must_use]
    pub fn with_origin(mut self, origin: Box<dyn Any + Send>) -> Self {
        self.origin = Some(origin);
        self
    }

    #[must_use]
    pub fn kind(&self) -> &FaultKind {
        &self.kind
    }

    #[must_use]
    pub fn provider_error(&self) -> Option<&ProviderError> {
        self.provider.as_ref()
    }

    #[must_use]
    pub fn policy_denied_reason(&self) -> Option<&str> {
        match &self.kind {
            FaultKind::PolicyDenied(reason) => Some(reason),
            _ => None,
        }
    }

    #[must_use]
    pub fn stream_fallback_reason(&self) -> Option<&str> {
        match &self.kind {
            FaultKind::StreamFallbackRequired(reason) => Some(reason),
            _ => None,
        }
    }

    /// Take the daemon's own error value, if the daemon attached one.
    pub fn take_origin(&mut self) -> Option<Box<dyn Any + Send>> {
        self.origin.take()
    }
}

impl std::fmt::Display for LoopFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::fmt::Debug for LoopFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopFault")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

impl std::error::Error for LoopFault {}

/// Hosted (provider-run) tools a route offers for one model.
#[derive(Debug, Clone, Default)]
pub struct HostedCapabilities {
    pub names: Vec<String>,
    pub specs: Vec<HarnessToolSpec>,
}

/// One admitted model call: the settlement and the execution authority it carries.
#[async_trait]
pub trait Admission: Send {
    /// The invocation this admission recorded, when it recorded one.
    fn invocation_id(&self) -> Option<Uuid>;

    /// Send `request` as a blocking call. The execution authority is spent by the first
    /// send; a second send on the same admission fails.
    async fn chat(&mut self, request: &ChatRequest) -> Result<ChatResponse, LoopFault>;

    /// Send `request` as a streaming call, relaying chunks through `chunks`.
    async fn stream_chat(
        &mut self,
        request: &ChatRequest,
        chunks: mpsc::Sender<StreamChunk>,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse, LoopFault>;

    /// Settle the call as failed with `error_class`.
    async fn fail(self: Box<Self>, error_class: &str) -> Result<(), LoopFault>;

    /// Settle the call as completed with the response usage.
    async fn complete(self: Box<Self>, usage: &TokenUsage) -> Result<(), LoopFault>;
}

/// The provider and its model-call control, as the loop sees them.
#[async_trait]
pub trait ModelPort: SummaryTransport {
    fn provider_name(&self) -> String;
    fn supports_native_tools(&self) -> bool;
    fn supports_streaming(&self) -> bool;
    fn accepts_tool_choice_none(&self) -> bool;
    fn default_max_output_tokens(&self) -> Option<u32>;
    fn hosted_capabilities(&self, model: &str) -> HostedCapabilities;

    /// Admit one primary model call. `retry_of` is the failed invocation being retried.
    async fn admit_primary(
        &self,
        dedup_hint: &str,
        fingerprint_hint: &str,
        retry_of: Option<Uuid>,
    ) -> Result<Box<dyn Admission>, LoopFault>;

    /// Fail the initial admission before execution authority is claimed.
    async fn fail_pending(&self, error_class: &str) -> Result<(), LoopFault>;
}

/// A worktree content fingerprint for completion-gate change detection. Two unbounded
/// fingerprints are never equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateDigest {
    value: Option<String>,
}

impl GateDigest {
    /// A bounded digest rendered as its event value (`sha256:<hex>`).
    #[must_use]
    pub fn bounded(value: impl Into<String>) -> Self {
        Self {
            value: Some(value.into()),
        }
    }

    /// The worktree was too large to digest.
    #[must_use]
    pub fn unbounded() -> Self {
        Self { value: None }
    }

    #[must_use]
    pub fn event_value(&self) -> Option<String> {
        self.value.clone()
    }

    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        matches!((&self.value, &other.value), (Some(left), Some(right)) if left == right)
    }
}

/// How one completion gate ended.
#[derive(Debug, Clone)]
pub struct GateOutcome {
    pub success: bool,
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
}

/// The tool registry and completion-gate runner, as the loop sees them.
#[async_trait]
pub trait LoopTools: Send + Sync {
    /// Hold the process-custody turn for the whole run.
    async fn process_turn_guard(&self) -> Box<dyn Any + Send + Sync>;
    async fn take_process_notices(&self) -> Vec<String>;
    fn set_context_usage(&self, used_tokens: u64, token_limit: u64);
    async fn take_compaction_focus(&self) -> Option<String>;
    fn specs(&self) -> Vec<HarnessToolSpec>;
    fn filter_hosted_specs(&self, specs: Vec<HarnessToolSpec>) -> Vec<HarnessToolSpec>;
    fn context_editing_enabled(&self) -> bool;
    /// The launch's output-token ceiling for a length-stop recovery call (#4028);
    /// `None` is the loop's own default.
    fn recovery_max_output_tokens(&self) -> Option<u32> {
        None
    }
    fn execution_mode(&self, name: &str) -> Option<ToolExecutionMode>;
    /// Whether the session policy admits a provider-run call; `Err` is the denial text.
    fn admit_hosted_call(&self, name: &str) -> Result<(), String>;
    fn record_hosted_result_bytes(&self, bytes: usize);
    fn unknown_tool_message(&self, name: &str) -> String;
    fn parameters_json(&self, name: &str) -> Option<String>;
    async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        working_dir: &Path,
        cancel: &CancellationToken,
        events: mpsc::Sender<StreamEvent>,
    ) -> ToolResult;
    async fn execute_completion_gate(
        &self,
        gate: &rsi_common::completion_gates::CompletionGate,
        working_dir: &Path,
        cancel: &CancellationToken,
    ) -> GateOutcome;
    fn worktree_digest(&self, working_dir: &Path) -> Result<GateDigest, String>;
}

/// Claims mid-turn agent mail for a model invocation.
#[async_trait]
pub trait MailBoundary: Send + Sync {
    async fn claim(
        &self,
        model_invocation_id: Uuid,
    ) -> Result<Option<Box<dyn MailDelivery>>, String>;
}

/// One claimed mail delivery: appended to the request, then settled.
#[async_trait]
pub trait MailDelivery: Send {
    fn append(&mut self, history: &mut Vec<ChatMessage>, request: &mut ChatRequest);

    fn undo_append(&mut self, history: &mut Vec<ChatMessage>, request: &mut ChatRequest);

    async fn record_effect_possible(&mut self);

    async fn record_rejected(&mut self, error_class: &'static str);
}
