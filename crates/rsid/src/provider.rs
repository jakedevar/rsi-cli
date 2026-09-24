//! Provider abstraction layer for Rsi session communication.
//!
//! Defines `ProviderSession` trait that abstracts over unidirectional (CLI stdout)
//! and bidirectional (app-server JSON-RPC) provider communication modes.

use crate::claude::StreamEvent;
use crate::error::Result;
use crate::model_control::ModelExecutionCapability;
use crate::model_control::call_control::ModelCallSettlement;
use rsi_common::agent_coordination::MessageAttemptFenceV1;

/// A daemon-private, one-use native message turn (P2-01, C-P2-07).
///
/// It owns the already admitted invocation settlement/permit AND the one-use
/// model execution capability for exactly one message attempt, bound to that
/// attempt's immutable [`MessageAttemptFenceV1`]. This follows the same
/// ownership pattern as `AdmittedHttpRequest`: because the wrapper owns both
/// values, the turn can be dispatched at most once.
///
/// It is deliberately NOT `Clone`, NOT `Serialize`, and NOT a wire DTO. The
/// combination means neither a provider nor a streaming-fallback controller can
/// admit a second model invocation for the same attempt — the fallback fence
/// C-P2-07 requires.
///
/// P2-01 landed this contract; P2-05b is the production caller that dispatches
/// it, through [`ProviderSession::start_admitted_message_turn`] from
/// `session::agent_message_delivery`. The `dead_code` allow that stood here
/// until that wiring existed is therefore gone.
pub struct NativeMessageTurnRequest {
    fence: MessageAttemptFenceV1,
    settlement: ModelCallSettlement,
    execution: ModelExecutionCapability,
}

/// Hand-written so the wrapper stays diagnosable without requiring `Debug` on
/// the settlement, and so no provider payload can ever reach a log line.
impl std::fmt::Debug for NativeMessageTurnRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeMessageTurnRequest")
            .field("message_id", &self.fence.message_id)
            .field("attempt_number", &self.fence.attempt_number)
            .field(
                "delivery_model_invocation_id",
                &self.fence.delivery_model_invocation_id,
            )
            .finish_non_exhaustive()
    }
}

impl NativeMessageTurnRequest {
    /// Bind an admitted settlement and execution capability to one attempt
    /// fence.
    ///
    /// # Errors
    ///
    /// Returns a bounded safe class when the settlement does not name the
    /// exact model invocation the fence was issued for, so a capability
    /// admitted for a different invocation can never dispatch this attempt.
    pub(crate) fn new(
        fence: MessageAttemptFenceV1,
        settlement: ModelCallSettlement,
        execution: ModelExecutionCapability,
    ) -> std::result::Result<Self, &'static str> {
        if settlement.invocation_id() != Some(fence.delivery_model_invocation_id) {
            return Err("agent_message_native_turn_invocation_fence_mismatch");
        }
        Ok(Self {
            fence,
            settlement,
            execution,
        })
    }

    #[must_use]
    pub(crate) const fn fence(&self) -> &MessageAttemptFenceV1 {
        &self.fence
    }

    /// Consume the wrapper at the exact provider dispatch boundary, yielding
    /// the fence, the settlement identity the caller must settle, and the
    /// one-use execution capability.
    #[must_use]
    pub(crate) fn into_parts(
        self,
    ) -> (
        MessageAttemptFenceV1,
        ModelCallSettlement,
        ModelExecutionCapability,
    ) {
        (self.fence, self.settlement, self.execution)
    }
}

/// What one [`ProviderSession::start_admitted_message_turn`] call actually did
/// to the provider (P2-05b).
///
/// # Why this is not `Result<()>`, and not `Result<BoundaryAdmissionV1>`
///
/// Two separate rules force this shape, and both are load-bearing.
///
/// **1. Settlement ownership.** After admission, three parties could settle the
/// same invocation: an explicit `fail`, [`ModelCallSettlement`]'s `Drop`, and
/// the provider's own call control. The plan's rule is that every NON-dispatch
/// exit settles *explicitly* with a pre-effect error class, and that `Drop` is
/// the safety net rather than the plan. A `Result<()>` return cannot express
/// that, because it gives the caller no way to get the settlement back after
/// the provider declined to install it — the settlement would silently drop and
/// reclassify a provably clean pre-effect abort as `model_call_task_exited`. So
/// every non-dispatch arm HANDS THE SETTLEMENT BACK, unconsumed, and the caller
/// is obliged to settle it.
///
/// **2. No no-effect proof may be inferred from a generic error.** The plan is
/// explicit (`:2578-2580`): *"Generic `Err`, lease expiry, missing output,
/// process death, or timeout is never proof of no effect."* If `unsupported`
/// and `rejected_before_effect` were both reported as some `Err` variant, the
/// caller would have to infer "nothing happened" from an error value, which is
/// exactly the forbidden inference. Here they are typed `Ok` values carrying
/// the returned settlement as their evidence, and `Err` keeps ONE meaning for
/// every implementation: a daemon-internal fault of unknown effect, which the
/// caller must treat conservatively as effect-possible.
///
/// The caller — not the provider — builds the `BoundaryAdmissionV1`, because
/// the coherent `provider_kind`/`capability_kind` pair comes from the arbiter's
/// grant and the attempt fence, neither of which a provider can see.
pub enum AdmittedMessageTurnOutcome {
    /// The provider consumed the one-use capability at its writer boundary and
    /// installed the settlement into its own turn slot. Effect is possible from
    /// this point on and the attempt can never be retried or requeued.
    ///
    /// `native_turn_id` stays `None` until a genuine provider response supplies
    /// it; turn-ID correlation is a separate unit and the attempt deliberately
    /// rests in `correlation_pending` until then.
    Dispatched { native_turn_id: Option<String> },
    /// Durable, provider-specific proof that nothing carrying this message
    /// crossed the effect threshold. The settlement comes back unconsumed.
    RejectedBeforeEffect {
        settlement: ModelCallSettlement,
        /// `&'static str` on purpose: an error class is a bounded closed
        /// vocabulary, so a provider payload cannot be formatted into one.
        error_class: &'static str,
    },
    /// Fail-closed pre-dispatch default: this provider has no admitted-message
    /// path and made no provider call whatsoever. The settlement comes back
    /// unconsumed.
    Unsupported { settlement: ModelCallSettlement },
}

/// Decision type for structured approval responses.
#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    /// Approve for this single request.
    Approve,
    /// Approve for the remainder of the session.
    ApproveForSession,
    /// Deny the request.
    Deny,
}

/// Configuration for starting a new turn on an existing session.
#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub input: String,
    pub working_dir: Option<std::path::PathBuf>,
}

/// Opaque turn identifier returned by start_turn().
#[derive(Debug, Clone)]
pub struct TurnId(pub String);

/// Trait abstracting provider communication.
/// Implemented by both unidirectional (CLI stdout) and bidirectional (app-server) providers.
#[async_trait::async_trait]
pub trait ProviderSession: Send {
    /// Receive the next event from the provider stream.
    /// Returns None when the stream is exhausted (process exited / turn completed).
    async fn next_event(&mut self) -> Option<StreamEvent>;

    /// Send an approval response back to the provider.
    /// The method and original params bind the response format and offered choices.
    /// Default: unsupported transport; never acknowledge a response that was not sent.
    async fn send_approval(
        &mut self,
        _request_id: &serde_json::Value,
        _method: &str,
        _params: &serde_json::Value,
        _decision: ApprovalDecision,
    ) -> Result<()> {
        Err(crate::error::DaemonError::InvalidParam(
            "provider_approval_transport_unsupported".into(),
        ))
    }

    /// Exact bidirectional Codex AppServer capability. Other providers have no
    /// approval writer; their generic send_approval default grants no authority.
    fn app_server_approval_writer(&self) -> Option<crate::codex_app_server::AppServerWriter> {
        None
    }

    /// Current native turn invocation, when different from the launch invocation.
    fn app_server_approval_invocation(&self) -> Option<uuid::Uuid> {
        None
    }

    /// Start a new turn on the same session (app-server protocols only).
    /// Default: error (unidirectional providers require a new subprocess).
    async fn start_turn(&mut self, _config: &TurnConfig) -> Result<TurnId> {
        Err(crate::error::DaemonError::Process(
            "start_turn requires app-server protocol".to_string(),
        ))
    }

    /// Dispatch ONE externally admitted agent-message turn (P2-05b, C-P2-08).
    ///
    /// This sits ALONGSIDE [`Self::start_turn`], which stays exactly as it was
    /// for the ordinary synthetic continuation. The two differ in who admits:
    /// `start_turn` self-admits through the provider's own `turn_control`,
    /// which is precisely what C-P2-08 exists to replace for message delivery,
    /// while this path *installs* an invocation the monitor already admitted
    /// and consumes a capability it was already granted. An implementation of
    /// this method must therefore never call `turn_control.admit`.
    ///
    /// `turn` is taken **by value** because [`NativeMessageTurnRequest`] owns
    /// the non-`Clone` [`ModelExecutionCapability`]; that ownership is the
    /// C-P2-07 fence making a second dispatch of one attempt unrepresentable
    /// rather than merely unlikely. Taking `&mut` would need interior
    /// mutability and would destroy it.
    ///
    /// # Errors
    ///
    /// `Err` means a daemon-internal fault whose effect on the provider is
    /// UNKNOWN. It is never proof that no effect occurred — callers must treat
    /// it conservatively as effect-possible. Every outcome the provider can
    /// actually prove is an `Ok` variant of [`AdmittedMessageTurnOutcome`].
    ///
    /// The default is fail-closed and pre-dispatch: it makes no provider call
    /// and returns the settlement unconsumed. Every provider except
    /// `CodexAppServer` is a `terminal_one_turn` boundary that the plan routes
    /// differently, and `supports_multi_turn` is `false` for all of them, so
    /// this default is the correct answer rather than a stub.
    async fn start_admitted_message_turn(
        &mut self,
        turn: NativeMessageTurnRequest,
        _config: &TurnConfig,
    ) -> Result<AdmittedMessageTurnOutcome> {
        let (_fence, settlement, _execution) = turn.into_parts();
        Ok(AdmittedMessageTurnOutcome::Unsupported { settlement })
    }

    /// Execute a dynamic tool call from the provider and return the result.
    /// Default: error (no tool support).
    async fn send_tool_result(&mut self, _call_id: i64, _result: serde_json::Value) -> Result<()> {
        Err(crate::error::DaemonError::Process(
            "send_tool_result requires app-server protocol".to_string(),
        ))
    }

    /// Whether this provider supports multi-turn continuation on the same process.
    fn supports_multi_turn(&self) -> bool {
        false
    }

    /// Whether this provider supports structured approval responses.
    fn supports_approvals(&self) -> bool {
        false
    }
}

/// Wrapper that adapts an `mpsc::Receiver<StreamEvent>` into a `ProviderSession`.
/// Used by all existing CLI-based providers (Claude, Codex CLI, etc.).
pub struct CliProviderSession {
    event_rx: tokio::sync::mpsc::Receiver<StreamEvent>,
}

impl CliProviderSession {
    pub fn new(event_rx: tokio::sync::mpsc::Receiver<StreamEvent>) -> Self {
        Self { event_rx }
    }
}

#[async_trait::async_trait]
impl ProviderSession for CliProviderSession {
    async fn next_event(&mut self) -> Option<StreamEvent> {
        self.event_rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::StreamEvent;
    use serde_json::json;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_cli_provider_session_wraps_and_forwards() {
        let (tx, rx) = mpsc::channel(4);
        let mut session = CliProviderSession::new(rx);

        let event = StreamEvent {
            event_type: "message".to_string(),
            data: json!({"content": "hello"}),
        };
        tx.send(event).await.unwrap();
        drop(tx);

        let received = session.next_event().await;
        assert!(received.is_some());
        let received = received.unwrap();
        assert_eq!(received.event_type, "message");

        // Stream exhausted
        let done = session.next_event().await;
        assert!(done.is_none());
    }

    #[tokio::test]
    async fn test_send_approval_default_refuses_unsupported_transport() {
        let (_tx, rx) = mpsc::channel::<StreamEvent>(1);
        let mut session = CliProviderSession::new(rx);
        let result = session
            .send_approval(
                &serde_json::json!(1),
                "item/commandExecution/requestApproval",
                &serde_json::json!({}),
                ApprovalDecision::Approve,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_start_turn_default_returns_err() {
        let (_tx, rx) = mpsc::channel::<StreamEvent>(1);
        let mut session = CliProviderSession::new(rx);
        let config = TurnConfig {
            input: "test".to_string(),
            working_dir: None,
        };
        let result = session.start_turn(&config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_supports_multi_turn_default_false() {
        let (_tx, rx) = mpsc::channel::<StreamEvent>(1);
        let session = CliProviderSession::new(rx);
        assert!(!session.supports_multi_turn());
    }

    #[tokio::test]
    async fn test_supports_approvals_default_false() {
        let (_tx, rx) = mpsc::channel::<StreamEvent>(1);
        let session = CliProviderSession::new(rx);
        assert!(!session.supports_approvals());
    }
}
