//! One daemon-global AppServer control plane (C-P2-15).
//!
//! This module owns the daemon-private half of P2-01. It replaces the rejected
//! per-session ingress registries with exactly one `AppServerControlPlane` per
//! daemon, constructed in `main.rs` and threaded through Session
//! lifecycle/launch into every `CodexAppServerSession`. Per-session registries
//! or priority queues are forbidden.
//!
//! # Why the state lock is a `std::sync::Mutex`
//!
//! C-P2-15 freezes a hard rule: the AppServer stdout task may await ONLY
//! stdout reads. Routing a frame must complete an exact response oneshot,
//! mutate a coalescing latch, or `try_send` structurally proven ordinary
//! suppressible evidence — it must never await `response_tx`,
//! `notification_tx`, `event_tx`, monitor progress, Store I/O, or quarantine
//! capacity. Every control-plane operation here is therefore synchronous and
//! non-blocking, guarded by a `std::sync::Mutex` that is never held across an
//! `.await`. Using `tokio::sync::Mutex` would reintroduce exactly the awaiting
//! ingress path this correction exists to remove.
//!
//! # Ownership
//!
//! The plane owns:
//! - exact response waiters keyed by canonical [`JsonRpcId`],
//! - active message-attempt/turn registrations,
//! - daemon-wide quarantine counters and per-attempt overflow/timeout/seal
//!   latches,
//! - coalescing lifecycle/death latches,
//! - writer-operation receipts,
//! - running effect-task join handles.
//!
//! # Temporary allow
//!
//! P2-01 lands these contracts; P2-04 (claim/CAS, dispatcher) and P2-05
//! (provider effect thresholds, frame reader) are the production callers that
//! consume them, and `codex_app_server.rs` is not rewired until then. Every
//! item below is exercised by this module's own tests. This allow MUST be
//! removed once P2-05 wires the plane into `CodexAppServerSession`; anything
//! still unused at that point is genuinely dead and should be deleted.
//! Precedent: `program_run_control.rs` and `session/harness/sse.rs`.
#![allow(dead_code)]
// Every item here is deliberately `pub(crate)` rather than `pub`: it documents
// that these are daemon-private control contracts no external crate may name.
// Same precedent and rationale as `program_run_control.rs`.
#![allow(clippy::redundant_pub_crate)]

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rsi_common::agent_coordination::{
    AGENT_MESSAGE_MAX_ENUM_BYTES, AGENT_MESSAGE_MAX_ERROR_CLASS_BYTES,
    AGENT_MESSAGE_MAX_METHOD_BYTES, AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES,
    AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS, AGENT_MESSAGE_QUARANTINE_MAX_BYTES_GLOBAL,
    AGENT_MESSAGE_QUARANTINE_MAX_BYTES_PER_ATTEMPT, AGENT_MESSAGE_QUARANTINE_MAX_EVENTS_GLOBAL,
    AGENT_MESSAGE_QUARANTINE_MAX_EVENTS_PER_ATTEMPT, AGENT_MESSAGE_QUARANTINE_TIMEOUT_MS,
    APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK, APP_SERVER_CONTROL_MAX_MS_PER_TICK,
    APP_SERVER_MAX_JSONRPC_STRING_ID_BYTES, CorrelationStateV1, MessageAttemptFenceV1,
    ProviderRequestHandlerKindV1,
};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// JSON-RPC identity
// ---------------------------------------------------------------------------

/// Exact JSON-RPC request/response identity (C-P2-15).
///
/// A JSON-RPC ID is either an `i64` number or a string of at most 256 raw
/// UTF-8 bytes. It persists reversibly as `n:<decimal>` or
/// `s:<canonical JSON string>` so a numeric ID and a string ID can never
/// alias, and replies reconstruct the original JSON type.
///
/// Null, floats, booleans, containers, out-of-range numbers, oversized
/// strings, and the forbidden `n:0` alias all fail closed. There is no
/// active-turn fallback: an unexpected response routes only by exact
/// registered ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum JsonRpcId {
    Number(i64),
    String(String),
}

/// Closed reasons a JSON-RPC ID fails closed. Each is a bounded safe class,
/// never provider payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonRpcIdError {
    /// `null`, a float, a boolean, an array, or an object.
    UnsupportedJsonType,
    /// A JSON number outside exact `i64` range, or not an integer.
    NumberOutOfRange,
    /// The daemon must never synthesize `0` as a stand-in for an absent ID.
    ForbiddenZeroAlias,
    /// A string ID longer than 256 raw UTF-8 bytes.
    StringTooLong,
    /// A canonical form with a missing/unknown prefix or a non-canonical body.
    NotCanonical,
}

impl JsonRpcIdError {
    /// A stable, bounded safe class suitable for durable evidence.
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedJsonType => "jsonrpc_id_unsupported_json_type",
            Self::NumberOutOfRange => "jsonrpc_id_number_out_of_range",
            Self::ForbiddenZeroAlias => "jsonrpc_id_forbidden_zero_alias",
            Self::StringTooLong => "jsonrpc_id_string_too_long",
            Self::NotCanonical => "jsonrpc_id_not_canonical",
        }
    }
}

impl JsonRpcId {
    /// Build an exact numeric ID, rejecting the forbidden `n:0` alias.
    ///
    /// # Errors
    ///
    /// Returns [`JsonRpcIdError::ForbiddenZeroAlias`] for `0`.
    pub(crate) const fn number(value: i64) -> Result<Self, JsonRpcIdError> {
        if value == 0 {
            return Err(JsonRpcIdError::ForbiddenZeroAlias);
        }
        Ok(Self::Number(value))
    }

    /// Build an exact string ID bounded at 256 raw UTF-8 bytes.
    ///
    /// # Errors
    ///
    /// Returns [`JsonRpcIdError::StringTooLong`] past the frozen ceiling.
    pub(crate) fn string(value: impl Into<String>) -> Result<Self, JsonRpcIdError> {
        let value = value.into();
        if value.len() > APP_SERVER_MAX_JSONRPC_STRING_ID_BYTES {
            return Err(JsonRpcIdError::StringTooLong);
        }
        Ok(Self::String(value))
    }

    /// Classify a raw JSON `id` field, failing closed on every non-exact form.
    ///
    /// # Errors
    ///
    /// Returns a bounded safe class for null, float, boolean, container,
    /// out-of-range, zero-alias, and oversized-string IDs.
    pub(crate) fn from_json(value: &Value) -> Result<Self, JsonRpcIdError> {
        match value {
            Value::Number(number) => {
                let exact = number.as_i64().ok_or(JsonRpcIdError::NumberOutOfRange)?;
                Self::number(exact)
            }
            Value::String(text) => Self::string(text.clone()),
            Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => {
                Err(JsonRpcIdError::UnsupportedJsonType)
            }
        }
    }

    /// The exact reversible persistence form.
    ///
    /// This is the ONLY producer of values stored in a V81 `*_request_id`
    /// column, and every value it produces satisfies the
    /// `rsi_jsonrpc_id_is_canonical` SQL function registered in
    /// `Store::register_sql_functions`.
    #[must_use]
    pub(crate) fn to_canonical(&self) -> String {
        match self {
            Self::Number(value) => format!("n:{value}"),
            // `serde_json` string encoding is the canonical escape spelling the
            // SQL checker re-derives, so one logical ID has exactly one
            // persisted representation.
            Self::String(value) => {
                let encoded = Value::String(value.clone()).to_string();
                format!("s:{encoded}")
            }
        }
    }

    /// Recover the exact ID from its persisted canonical form.
    ///
    /// # Errors
    ///
    /// Returns [`JsonRpcIdError::NotCanonical`] for any value the SQL
    /// canonical-form checker would also reject.
    pub(crate) fn from_canonical(value: &str) -> Result<Self, JsonRpcIdError> {
        if let Some(decimal) = value.strip_prefix("n:") {
            let parsed = decimal
                .parse::<i64>()
                .map_err(|_| JsonRpcIdError::NotCanonical)?;
            // Round-trip equality rejects `+7`, `007`, and `-0` in one check.
            if parsed.to_string() != decimal {
                return Err(JsonRpcIdError::NotCanonical);
            }
            return Self::number(parsed).map_err(|_| JsonRpcIdError::NotCanonical);
        }
        if let Some(encoded) = value.strip_prefix("s:") {
            let Ok(Value::String(decoded)) = serde_json::from_str::<Value>(encoded) else {
                return Err(JsonRpcIdError::NotCanonical);
            };
            if decoded.len() > APP_SERVER_MAX_JSONRPC_STRING_ID_BYTES {
                return Err(JsonRpcIdError::NotCanonical);
            }
            // Reject a non-canonical escape spelling.
            if Value::String(decoded.clone()).to_string().as_str() != encoded {
                return Err(JsonRpcIdError::NotCanonical);
            }
            return Ok(Self::String(decoded));
        }
        Err(JsonRpcIdError::NotCanonical)
    }

    /// Reconstruct the original JSON type for an outbound reply. No public API
    /// accepts a lossy integer substitute.
    #[must_use]
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Number(value) => Value::Number((*value).into()),
            Self::String(value) => Value::String(value.clone()),
        }
    }
}

/// A correlated JSON-RPC response delivered to an exact registered waiter.
#[derive(Debug, Clone)]
pub(crate) enum JsonRpcResponse {
    Result(Value),
    Error(Value),
}

// ---------------------------------------------------------------------------
// Writer contracts
// ---------------------------------------------------------------------------

/// Closed writer outcome (C-P2-15).
///
/// `WriteFlushed` exists ONLY after both `write_all()` and `flush()` succeed.
/// Enqueue alone is never success: an enqueue without a receipt is
/// effect-possible uncertainty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriterOperationResult {
    RejectedBeforeEnqueue,
    WriteFlushed,
    WriteFailed,
    WriterClosed,
}

impl WriterOperationResult {
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RejectedBeforeEnqueue => "rejected_before_enqueue",
            Self::WriteFlushed => "write_flushed",
            Self::WriteFailed => "write_failed",
            Self::WriterClosed => "writer_closed",
        }
    }

    /// Only a flushed write proves the bytes reached the provider. Every other
    /// outcome leaves a started provider-reply permit unsettled until exact
    /// death/EOF join evidence arrives.
    #[must_use]
    pub(crate) const fn proves_delivery(self) -> bool {
        matches!(self, Self::WriteFlushed)
    }

    /// A refusal that provably produced no bytes on the wire.
    #[must_use]
    pub(crate) const fn proves_no_effect(self) -> bool {
        matches!(self, Self::RejectedBeforeEnqueue)
    }
}

/// One writer operation. Replaces the rejected bare `Vec<u8>` enqueue so every
/// provider write carries an identity and a completion channel.
#[derive(Debug)]
pub(crate) struct WriterCommand {
    pub(crate) operation_id: Uuid,
    pub(crate) bytes: Vec<u8>,
    pub(crate) completion: Option<oneshot::Sender<WriterOperationResult>>,
}

impl WriterCommand {
    #[must_use]
    pub(crate) fn new(
        operation_id: Uuid,
        bytes: Vec<u8>,
    ) -> (Self, oneshot::Receiver<WriterOperationResult>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                operation_id,
                bytes,
                completion: Some(tx),
            },
            rx,
        )
    }
}

/// A durable-facing writer receipt. The control plane retains this
/// independently of the caller, so the result reaches the latch even if the
/// caller oneshot is dropped (C-P2-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WriterReceipt {
    pub(crate) operation_id: Uuid,
    pub(crate) result: WriterOperationResult,
}

// ---------------------------------------------------------------------------
// Non-clone provider-request capabilities
// ---------------------------------------------------------------------------

/// A one-use authority to START a provider-request handler (tool execution or
/// approval presentation).
///
/// Deliberately NOT `Clone` and with private fields: the only way to spend it
/// is [`AuthorizedProviderRequestHandler::consume_for_started_cas`], which
/// takes `self` by value and requires the matching durable capability ID. A
/// replayed evidence read returns no capability at all.
#[derive(Debug)]
pub(crate) struct AuthorizedProviderRequestHandler {
    capability_id: Uuid,
    request_row_id: Uuid,
    kind: ProviderRequestHandlerKindV1,
    gate_generation: i64,
}

impl AuthorizedProviderRequestHandler {
    #[must_use]
    pub(crate) const fn new(
        capability_id: Uuid,
        request_row_id: Uuid,
        kind: ProviderRequestHandlerKindV1,
        gate_generation: i64,
    ) -> Self {
        Self {
            capability_id,
            request_row_id,
            kind,
            gate_generation,
        }
    }

    #[must_use]
    pub(crate) const fn capability_id(&self) -> Uuid {
        self.capability_id
    }

    #[must_use]
    pub(crate) const fn request_row_id(&self) -> Uuid {
        self.request_row_id
    }

    #[must_use]
    pub(crate) const fn kind(&self) -> ProviderRequestHandlerKindV1 {
        self.kind
    }

    #[must_use]
    pub(crate) const fn gate_generation(&self) -> i64 {
        self.gate_generation
    }

    /// Spend this capability at its matching durable `handler_started` CAS.
    ///
    /// # Errors
    ///
    /// Returns the capability back when the durable row names a different
    /// capability ID, so a mismatched CAS consumes nothing.
    pub(crate) fn consume_for_started_cas(
        self,
        durable_capability_id: Uuid,
    ) -> Result<ConsumedHandlerCapability, Self> {
        if self.capability_id != durable_capability_id {
            return Err(self);
        }
        Ok(ConsumedHandlerCapability {
            capability_id: self.capability_id,
            request_row_id: self.request_row_id,
            kind: self.kind,
            gate_generation: self.gate_generation,
        })
    }
}

/// Proof that exactly one handler capability was spent.
#[derive(Debug)]
pub(crate) struct ConsumedHandlerCapability {
    pub(crate) capability_id: Uuid,
    pub(crate) request_row_id: Uuid,
    pub(crate) kind: ProviderRequestHandlerKindV1,
    pub(crate) gate_generation: i64,
}

/// A one-use authority to START a provider reply (the JSON-RPC result/error).
///
/// A tool execution and its reply are explicitly TWO effects (C-P2-11), so
/// this capability is distinct from [`AuthorizedProviderRequestHandler`] and
/// cannot be produced from it. Handler uncertainty can never authorize a
/// reply.
#[derive(Debug)]
pub(crate) struct AuthorizedProviderRequestReply {
    capability_id: Uuid,
    request_row_id: Uuid,
    provider_request_id: JsonRpcId,
    gate_generation: i64,
}

impl AuthorizedProviderRequestReply {
    #[must_use]
    pub(crate) const fn new(
        capability_id: Uuid,
        request_row_id: Uuid,
        provider_request_id: JsonRpcId,
        gate_generation: i64,
    ) -> Self {
        Self {
            capability_id,
            request_row_id,
            provider_request_id,
            gate_generation,
        }
    }

    #[must_use]
    pub(crate) const fn capability_id(&self) -> Uuid {
        self.capability_id
    }

    #[must_use]
    pub(crate) const fn request_row_id(&self) -> Uuid {
        self.request_row_id
    }

    #[must_use]
    pub(crate) const fn provider_request_id(&self) -> &JsonRpcId {
        &self.provider_request_id
    }

    #[must_use]
    pub(crate) const fn gate_generation(&self) -> i64 {
        self.gate_generation
    }

    /// Spend this capability at its matching durable `reply_started` CAS.
    ///
    /// # Errors
    ///
    /// Returns the capability back on a capability-ID mismatch.
    pub(crate) fn consume_for_started_cas(
        self,
        durable_capability_id: Uuid,
    ) -> Result<ConsumedReplyCapability, Self> {
        if self.capability_id != durable_capability_id {
            return Err(self);
        }
        Ok(ConsumedReplyCapability {
            capability_id: self.capability_id,
            request_row_id: self.request_row_id,
            provider_request_id: self.provider_request_id,
            gate_generation: self.gate_generation,
        })
    }
}

/// Proof that exactly one reply capability was spent.
#[derive(Debug)]
pub(crate) struct ConsumedReplyCapability {
    pub(crate) capability_id: Uuid,
    pub(crate) request_row_id: Uuid,
    pub(crate) provider_request_id: JsonRpcId,
    pub(crate) gate_generation: i64,
}

// ---------------------------------------------------------------------------
// Fenced provider request
// ---------------------------------------------------------------------------

/// Closed reasons a fenced provider request fails construction. Each is a
/// bounded safe class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FencedRequestError {
    /// A thread ID, process ID, transcript token, timestamp, message ID, or
    /// `continuation:*` value was offered as a turn ID substitute.
    NotAGenuineTurnId,
    /// The genuine turn ID exceeded its frozen ceiling or was empty.
    TurnIdOutOfBounds,
    /// The bounded request digest was empty or oversized.
    DigestOutOfBounds,
    /// The bounded provider method name was empty or oversized.
    MethodOutOfBounds,
}

impl FencedRequestError {
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NotAGenuineTurnId => "fenced_request_not_a_genuine_turn_id",
            Self::TurnIdOutOfBounds => "fenced_request_turn_id_out_of_bounds",
            Self::DigestOutOfBounds => "fenced_request_digest_out_of_bounds",
            Self::MethodOutOfBounds => "fenced_request_method_out_of_bounds",
        }
    }
}

/// A provider-originated request that belongs to a message turn, carrying its
/// complete fence (C-P2-11).
///
/// Deliberately NOT `Clone`: no conversion to the ordinary event channel may
/// erase the fence, and a cloned fence would let a stale copy authorize a
/// second effect. There is no constructor that accepts a thread ID as a turn
/// substitute.
#[derive(Debug)]
pub(crate) struct FencedAppServerProviderRequest {
    fence: MessageAttemptFenceV1,
    turn_start_request_id: JsonRpcId,
    provider_request_id: JsonRpcId,
    provider_request_kind: ProviderRequestHandlerKindV1,
    provider_request_method: String,
    provider_request_digest: String,
    provider_turn_id: Option<String>,
}

impl FencedAppServerProviderRequest {
    /// Build a fenced request.
    ///
    /// `provider_turn_id` is populated ONLY from a genuine provider response or
    /// notification. A `continuation:*` value is rejected outright, matching
    /// the shared `BoundaryAdmissionV1` rule.
    ///
    /// # Errors
    ///
    /// Returns a bounded safe class when any scalar violates its frozen
    /// ceiling or when a non-turn token is offered as a turn ID.
    pub(crate) fn new(
        fence: MessageAttemptFenceV1,
        turn_start_request_id: JsonRpcId,
        provider_request_id: JsonRpcId,
        provider_request_kind: ProviderRequestHandlerKindV1,
        provider_request_method: impl Into<String>,
        provider_request_digest: impl Into<String>,
        provider_turn_id: Option<String>,
    ) -> Result<Self, FencedRequestError> {
        let provider_request_method = provider_request_method.into();
        if provider_request_method.is_empty()
            || provider_request_method.len() > AGENT_MESSAGE_MAX_METHOD_BYTES
        {
            return Err(FencedRequestError::MethodOutOfBounds);
        }
        let provider_request_digest = provider_request_digest.into();
        if provider_request_digest.is_empty()
            || provider_request_digest.len() > AGENT_MESSAGE_MAX_ERROR_CLASS_BYTES
        {
            return Err(FencedRequestError::DigestOutOfBounds);
        }
        if let Some(turn) = provider_turn_id.as_deref() {
            if turn.is_empty() || turn.len() > AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES {
                return Err(FencedRequestError::TurnIdOutOfBounds);
            }
            if turn.starts_with("continuation:") {
                return Err(FencedRequestError::NotAGenuineTurnId);
            }
        }
        Ok(Self {
            fence,
            turn_start_request_id,
            provider_request_id,
            provider_request_kind,
            provider_request_method,
            provider_request_digest,
            provider_turn_id,
        })
    }

    #[must_use]
    pub(crate) const fn fence(&self) -> &MessageAttemptFenceV1 {
        &self.fence
    }

    #[must_use]
    pub(crate) const fn turn_start_request_id(&self) -> &JsonRpcId {
        &self.turn_start_request_id
    }

    #[must_use]
    pub(crate) const fn provider_request_id(&self) -> &JsonRpcId {
        &self.provider_request_id
    }

    #[must_use]
    pub(crate) const fn provider_request_kind(&self) -> ProviderRequestHandlerKindV1 {
        self.provider_request_kind
    }

    #[must_use]
    pub(crate) fn provider_request_method(&self) -> &str {
        &self.provider_request_method
    }

    #[must_use]
    pub(crate) fn provider_request_digest(&self) -> &str {
        &self.provider_request_digest
    }

    #[must_use]
    pub(crate) fn provider_turn_id(&self) -> Option<&str> {
        self.provider_turn_id.as_deref()
    }

    /// The exact attempt this request belongs to.
    #[must_use]
    pub(crate) const fn attempt_key(&self) -> AttemptKey {
        AttemptKey {
            message_id: self.fence.message_id,
            attempt_number: self.fence.attempt_number,
        }
    }
}

// ---------------------------------------------------------------------------
// Provider lifecycle signals
// ---------------------------------------------------------------------------

/// Closed provider lifecycle facts carried on the priority control lane
/// (C-P2-13).
///
/// Deliberately absent: any variant meaning "the `turn/start` response is
/// missing or timed out". That is admission CORRELATION uncertainty, tracked by
/// `CorrelationStateV1::CorrelationPending`, and is never a terminal lifecycle
/// signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderLifecycleKind {
    /// Quarantine capacity overflow sealed pre-ack evidence.
    OverflowSeal,
    /// Quarantine timeout sealed pre-ack evidence.
    TimeoutSeal,
    /// An exact correlated terminal turn result.
    TurnCompleted,
    /// An exact correlated terminal turn error.
    TurnFailed,
    /// Reader EOF on provider stdout.
    ReaderEof,
    /// Observed, confirmed process death.
    ConfirmedProcessDeath,
}

impl ProviderLifecycleKind {
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::OverflowSeal => "overflow_seal",
            Self::TimeoutSeal => "timeout_seal",
            Self::TurnCompleted => "turn_completed",
            Self::TurnFailed => "turn_failed",
            Self::ReaderEof => "reader_eof",
            Self::ConfirmedProcessDeath => "confirmed_process_death",
        }
    }

    /// Coalescing strength. A latch keeps the strongest terminal fact
    /// observed: confirmed death and EOF outrank a correlated terminal result,
    /// which outranks a pre-ack seal (C-P2-15).
    #[must_use]
    const fn strength(self) -> u8 {
        match self {
            Self::OverflowSeal | Self::TimeoutSeal => 1,
            Self::TurnCompleted | Self::TurnFailed => 2,
            Self::ReaderEof => 3,
            Self::ConfirmedProcessDeath => 4,
        }
    }

    /// Only confirmed death or reader EOF settle and release retained custody
    /// (C-P2-12/C-P2-13).
    #[must_use]
    pub(crate) const fn settles_custody(self) -> bool {
        matches!(self, Self::ReaderEof | Self::ConfirmedProcessDeath)
    }
}

/// Optional usage evidence. It can never block lifecycle settlement
/// (C-P2-13), so it is a plain bounded scalar bag, not a precondition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ProviderLifecycleUsage {
    pub(crate) input_tokens: Option<i64>,
    pub(crate) output_tokens: Option<i64>,
}

/// A minimal priority-lane lifecycle signal. It contains only bounded terminal
/// identity/outcome scalars and NEVER conversation or request payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderLifecycleSignal {
    pub(crate) kind: ProviderLifecycleKind,
    pub(crate) message_id: Uuid,
    pub(crate) attempt_number: u32,
    pub(crate) model_invocation_id: Uuid,
    pub(crate) native_turn_id: Option<String>,
    pub(crate) terminal_status: Option<String>,
    pub(crate) error_class: Option<String>,
    pub(crate) usage: Option<ProviderLifecycleUsage>,
}

/// Closed reasons a lifecycle signal fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleSignalError {
    TurnIdOutOfBounds,
    NotAGenuineTurnId,
    StatusOutOfBounds,
    ErrorClassOutOfBounds,
}

impl LifecycleSignalError {
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::TurnIdOutOfBounds => "lifecycle_signal_turn_id_out_of_bounds",
            Self::NotAGenuineTurnId => "lifecycle_signal_not_a_genuine_turn_id",
            Self::StatusOutOfBounds => "lifecycle_signal_status_out_of_bounds",
            Self::ErrorClassOutOfBounds => "lifecycle_signal_error_class_out_of_bounds",
        }
    }
}

impl ProviderLifecycleSignal {
    /// Validate every bounded scalar against its frozen ceiling.
    ///
    /// # Errors
    ///
    /// Returns a bounded safe class when a scalar is empty, oversized, or is a
    /// non-turn token offered as a turn ID.
    pub(crate) fn validate(&self) -> Result<(), LifecycleSignalError> {
        if let Some(turn) = self.native_turn_id.as_deref() {
            if turn.is_empty() || turn.len() > AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES {
                return Err(LifecycleSignalError::TurnIdOutOfBounds);
            }
            if turn.starts_with("continuation:") {
                return Err(LifecycleSignalError::NotAGenuineTurnId);
            }
        }
        if let Some(status) = self.terminal_status.as_deref()
            && (status.is_empty() || status.len() > AGENT_MESSAGE_MAX_ENUM_BYTES)
        {
            return Err(LifecycleSignalError::StatusOutOfBounds);
        }
        if let Some(class) = self.error_class.as_deref()
            && (class.is_empty() || class.len() > AGENT_MESSAGE_MAX_ERROR_CLASS_BYTES)
        {
            return Err(LifecycleSignalError::ErrorClassOutOfBounds);
        }
        Ok(())
    }

    #[must_use]
    pub(crate) const fn attempt_key(&self) -> AttemptKey {
        AttemptKey {
            message_id: self.message_id,
            attempt_number: self.attempt_number,
        }
    }
}

// ---------------------------------------------------------------------------
// Control-plane state
// ---------------------------------------------------------------------------

/// Exact identity of one message delivery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct AttemptKey {
    pub(crate) message_id: Uuid,
    pub(crate) attempt_number: u32,
}

/// Outcome of registering an attempt with the daemon-global plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptRegistrationOutcome {
    Registered,
    /// Exact re-registration of the same attempt is idempotent.
    AlreadyRegistered,
    /// The frozen global 64-attempt cap is exhausted. Refusal occurs BEFORE
    /// writer admission (C-P2-15).
    RegistryFull,
}

/// Why an attempt's pre-ack evidence must seal as `sealed_live_uncertain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuarantineSealReason {
    PerAttemptEvents,
    PerAttemptBytes,
    GlobalEvents,
    GlobalBytes,
    Timeout,
    /// A bounded stdout ingress mailbox refused a non-suppressible frame.
    ///
    /// Distinct from the quarantine capacity reasons above: nothing about the
    /// quarantine budget was exhausted, so recording one of those would state a
    /// false cause. The provider is alive and the reader has not reached EOF,
    /// which is why this maps to the non-settling `OverflowSeal`.
    IngressMailboxOverflow,
}

impl QuarantineSealReason {
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PerAttemptEvents => "quarantine_per_attempt_events",
            Self::PerAttemptBytes => "quarantine_per_attempt_bytes",
            Self::GlobalEvents => "quarantine_global_events",
            Self::GlobalBytes => "quarantine_global_bytes",
            Self::Timeout => "quarantine_timeout",
            Self::IngressMailboxOverflow => "ingress_mailbox_overflow",
        }
    }

    /// The corresponding lifecycle kind for the coalescing latch.
    #[must_use]
    pub(crate) const fn lifecycle_kind(self) -> ProviderLifecycleKind {
        match self {
            Self::Timeout => ProviderLifecycleKind::TimeoutSeal,
            _ => ProviderLifecycleKind::OverflowSeal,
        }
    }
}

/// Result of offering one quarantined pre-ack evidence frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuarantineAdmission {
    Accepted {
        retained_events: usize,
        retained_bytes: usize,
    },
    /// Capacity or timeout exhausted: the attempt seals once and permanently.
    SealRequired(QuarantineSealReason),
    /// The attempt is not registered, or is already sealed. Evidence is
    /// dropped without effect and without a second seal.
    Rejected,
}

/// Whether the plane will admit a new provider write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriterAdmission {
    Admitted,
    /// Startup keyset reconciliation has not completed. New writer admission
    /// is disabled until every durable live attempt is registered (C-P2-15).
    ReconciliationPending,
    /// The frozen global attempt cap is exhausted.
    RegistryFull,
}

/// One durable live attempt replayed at startup reconciliation.
#[derive(Debug, Clone)]
pub(crate) struct DurableAttemptRegistration {
    pub(crate) fence: MessageAttemptFenceV1,
    pub(crate) correlation: CorrelationStateV1,
    pub(crate) provider_turn_id: Option<String>,
}

/// Outcome of startup keyset reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReconcileOutcome {
    pub(crate) registered: usize,
    /// Attempts whose in-memory evidence was lost across the restart. Each
    /// seals exactly once; a durable registration still consumes the global
    /// cap until settlement, so restart cannot reset capacity or admit a
    /// duplicate turn.
    pub(crate) sealed_for_lost_evidence: Vec<AttemptKey>,
    pub(crate) refused_registry_full: Vec<AttemptKey>,
}

/// A dirty control latch awaiting its exact Store CAS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirtyLatch {
    pub(crate) key: AttemptKey,
    pub(crate) kind: ProviderLifecycleKind,
    pub(crate) seal_reason: Option<QuarantineSealReason>,
    pub(crate) signal: Option<ProviderLifecycleSignal>,
}

/// One bounded, resumable control-worker sweep over the dirty latches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirtyLatchSweep {
    /// The dirty latches this pass claimed, capped by the frozen budget.
    pub(crate) latches: Vec<DirtyLatch>,
    /// The last key examined. Feed this back on the next tick to resume.
    pub(crate) next_cursor: Option<AttemptKey>,
    /// How many keys the pass examined, dirty or not.
    pub(crate) examined: usize,
    /// Dirty latches still outstanding after this pass, for observability. A
    /// nonzero value means the next tick has work; it is never a busy-loop
    /// signal, because the worker is driven by a fixed 100 ms interval.
    pub(crate) remaining_dirty: usize,
}

#[derive(Debug)]
struct LifecycleLatch {
    kind: ProviderLifecycleKind,
    seal_reason: Option<QuarantineSealReason>,
    signal: Option<ProviderLifecycleSignal>,
    /// Stays dirty until the exact Store CAS commits, or until a stronger
    /// terminal fact replaces it (C-P2-15).
    dirty: bool,
}

#[derive(Debug)]
struct AttemptRegistration {
    fence: MessageAttemptFenceV1,
    correlation: CorrelationStateV1,
    provider_turn_id: Option<String>,
    quarantined_events: usize,
    quarantined_bytes: usize,
    sealed: bool,
    registered_at: Instant,
}

struct ControlPlaneState {
    /// Exact response waiters. Routing completes one of these by exact
    /// registered canonical ID; there is no active-turn fallback.
    response_waiters: HashMap<String, oneshot::Sender<JsonRpcResponse>>,
    attempts: HashMap<AttemptKey, AttemptRegistration>,
    lifecycle_latches: HashMap<AttemptKey, LifecycleLatch>,
    writer_receipts: HashMap<Uuid, WriterReceipt>,
    effect_tasks: HashMap<Uuid, JoinHandle<()>>,
    global_quarantined_events: usize,
    global_quarantined_bytes: usize,
    /// Disabled until startup keyset reconciliation completes.
    writer_admission_enabled: bool,
    provider_death_observed: bool,
}

/// Exactly one per daemon (C-P2-15). Constructed in `main.rs` and passed
/// through Session lifecycle/launch into every `CodexAppServerSession`.
pub(crate) struct AppServerControlPlane {
    state: Mutex<ControlPlaneState>,
}

impl fmt::Debug for AppServerControlPlane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Ok(state) = self.state.lock() else {
            return f
                .debug_struct("AppServerControlPlane")
                .field("state", &"poisoned")
                .finish();
        };
        f.debug_struct("AppServerControlPlane")
            .field("response_waiters", &state.response_waiters.len())
            .field("attempts", &state.attempts.len())
            .field("lifecycle_latches", &state.lifecycle_latches.len())
            .field("writer_receipts", &state.writer_receipts.len())
            .field("effect_tasks", &state.effect_tasks.len())
            .field(
                "global_quarantined_events",
                &state.global_quarantined_events,
            )
            .field("global_quarantined_bytes", &state.global_quarantined_bytes)
            .field("writer_admission_enabled", &state.writer_admission_enabled)
            .field("provider_death_observed", &state.provider_death_observed)
            .finish()
    }
}

impl Default for AppServerControlPlane {
    fn default() -> Self {
        Self::new()
    }
}

impl AppServerControlPlane {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(ControlPlaneState {
                response_waiters: HashMap::new(),
                attempts: HashMap::new(),
                lifecycle_latches: HashMap::new(),
                writer_receipts: HashMap::new(),
                effect_tasks: HashMap::new(),
                global_quarantined_events: 0,
                global_quarantined_bytes: 0,
                writer_admission_enabled: false,
                provider_death_observed: false,
            }),
        }
    }

    /// Lock the state, recovering a poisoned mutex rather than panicking. A
    /// panic here would take down ingress for every session in the daemon.
    fn lock(&self) -> std::sync::MutexGuard<'_, ControlPlaneState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            self.state.clear_poison();
            poisoned.into_inner()
        })
    }

    // -- response correlation ------------------------------------------------

    /// Register an exact response waiter. Returns `None` when the ID is
    /// already registered: a duplicate/conflicting ID fails closed rather than
    /// silently replacing the live waiter.
    #[must_use]
    pub(crate) fn register_response_waiter(
        &self,
        id: &JsonRpcId,
    ) -> Option<oneshot::Receiver<JsonRpcResponse>> {
        let canonical = id.to_canonical();
        let mut state = self.lock();
        match state.response_waiters.entry(canonical) {
            Entry::Occupied(_) => None,
            Entry::Vacant(slot) => {
                let (tx, rx) = oneshot::channel();
                slot.insert(tx);
                Some(rx)
            }
        }
    }

    /// Complete an exact registered waiter. Returns `false` when no waiter is
    /// registered for that exact ID — an unexpected response routes ONLY by
    /// exact registered ID and never falls back to an active turn.
    ///
    /// This never awaits: `oneshot::Sender::send` is synchronous.
    pub(crate) fn complete_response(&self, id: &JsonRpcId, response: JsonRpcResponse) -> bool {
        let canonical = id.to_canonical();
        let sender = {
            let mut state = self.lock();
            state.response_waiters.remove(&canonical)
        };
        sender.is_some_and(|tx| tx.send(response).is_ok())
    }

    /// Drop a waiter whose caller gave up (for example on admission timeout).
    pub(crate) fn cancel_response_waiter(&self, id: &JsonRpcId) -> bool {
        let canonical = id.to_canonical();
        let mut state = self.lock();
        state.response_waiters.remove(&canonical).is_some()
    }

    #[must_use]
    pub(crate) fn registered_waiter_count(&self) -> usize {
        self.lock().response_waiters.len()
    }

    // -- attempt registration ------------------------------------------------

    /// Register one live message attempt against the frozen global 64-attempt
    /// cap.
    pub(crate) fn register_attempt(
        &self,
        fence: &MessageAttemptFenceV1,
        correlation: CorrelationStateV1,
        now: Instant,
    ) -> AttemptRegistrationOutcome {
        let key = AttemptKey {
            message_id: fence.message_id,
            attempt_number: fence.attempt_number,
        };
        let mut state = self.lock();
        if state.attempts.contains_key(&key) {
            return AttemptRegistrationOutcome::AlreadyRegistered;
        }
        if state.attempts.len() >= AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS {
            return AttemptRegistrationOutcome::RegistryFull;
        }
        state.attempts.insert(
            key,
            AttemptRegistration {
                fence: fence.clone(),
                correlation,
                provider_turn_id: None,
                quarantined_events: 0,
                quarantined_bytes: 0,
                sealed: false,
                registered_at: now,
            },
        );
        AttemptRegistrationOutcome::Registered
    }

    /// Release a settled attempt and return its retained quarantine budget to
    /// the global pool.
    pub(crate) fn release_attempt(&self, key: AttemptKey) -> bool {
        let mut state = self.lock();
        let Some(registration) = state.attempts.remove(&key) else {
            return false;
        };
        state.global_quarantined_events = state
            .global_quarantined_events
            .saturating_sub(registration.quarantined_events);
        state.global_quarantined_bytes = state
            .global_quarantined_bytes
            .saturating_sub(registration.quarantined_bytes);
        state.lifecycle_latches.remove(&key);
        true
    }

    #[must_use]
    pub(crate) fn registered_attempt_count(&self) -> usize {
        self.lock().attempts.len()
    }

    #[must_use]
    pub(crate) fn attempt_correlation(&self, key: AttemptKey) -> Option<CorrelationStateV1> {
        self.lock().attempts.get(&key).map(|row| row.correlation)
    }

    /// Fill the genuine provider turn ID exactly once, and only while the
    /// durable class is still `correlation_pending` (C-P2-13). After
    /// `sealed_live_uncertain` a late response can never fill, ack, or reopen
    /// evidence.
    ///
    /// Returns `false` when the attempt is unknown, already sealed, already
    /// filled, or when the offered value is not a genuine turn ID.
    pub(crate) fn fill_genuine_turn_id(&self, key: AttemptKey, turn_id: &str) -> bool {
        if turn_id.is_empty()
            || turn_id.len() > AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES
            || turn_id.starts_with("continuation:")
        {
            return false;
        }
        let mut state = self.lock();
        let Some(registration) = state.attempts.get_mut(&key) else {
            return false;
        };
        if registration.sealed
            || registration.correlation != CorrelationStateV1::CorrelationPending
            || registration.provider_turn_id.is_some()
        {
            return false;
        }
        registration.provider_turn_id = Some(turn_id.to_string());
        true
    }

    #[must_use]
    pub(crate) fn genuine_turn_id(&self, key: AttemptKey) -> Option<String> {
        self.lock()
            .attempts
            .get(&key)
            .and_then(|row| row.provider_turn_id.clone())
    }

    // -- quarantine accounting ----------------------------------------------

    /// Offer one pre-ack evidence frame to the per-attempt and daemon-global
    /// quarantine budget.
    ///
    /// Frozen limits: 8 events / 64 KiB per attempt, and 64 attempts /
    /// 256 events / 2 MiB globally. Size arithmetic is checked. Crossing any
    /// limit, or exceeding the 5-second timeout, seals the attempt exactly
    /// once as `sealed_live_uncertain`.
    pub(crate) fn offer_quarantined_evidence(
        &self,
        key: AttemptKey,
        bytes: usize,
        now: Instant,
    ) -> QuarantineAdmission {
        let mut state = self.lock();
        let global_events = state.global_quarantined_events;
        let global_bytes = state.global_quarantined_bytes;
        let Some(registration) = state.attempts.get_mut(&key) else {
            return QuarantineAdmission::Rejected;
        };
        if registration.sealed {
            return QuarantineAdmission::Rejected;
        }

        let timeout = Duration::from_millis(AGENT_MESSAGE_QUARANTINE_TIMEOUT_MS);
        if now.saturating_duration_since(registration.registered_at) >= timeout {
            registration.sealed = true;
            let reason = QuarantineSealReason::Timeout;
            Self::mark_seal_latch(&mut state, key, reason);
            return QuarantineAdmission::SealRequired(reason);
        }

        let next_events = registration.quarantined_events.saturating_add(1);
        let Some(next_bytes) = registration.quarantined_bytes.checked_add(bytes) else {
            registration.sealed = true;
            let reason = QuarantineSealReason::PerAttemptBytes;
            Self::mark_seal_latch(&mut state, key, reason);
            return QuarantineAdmission::SealRequired(reason);
        };

        let reason = if next_events > AGENT_MESSAGE_QUARANTINE_MAX_EVENTS_PER_ATTEMPT {
            Some(QuarantineSealReason::PerAttemptEvents)
        } else if next_bytes > AGENT_MESSAGE_QUARANTINE_MAX_BYTES_PER_ATTEMPT {
            Some(QuarantineSealReason::PerAttemptBytes)
        } else if global_events.saturating_add(1) > AGENT_MESSAGE_QUARANTINE_MAX_EVENTS_GLOBAL {
            Some(QuarantineSealReason::GlobalEvents)
        } else if global_bytes.saturating_add(bytes) > AGENT_MESSAGE_QUARANTINE_MAX_BYTES_GLOBAL {
            Some(QuarantineSealReason::GlobalBytes)
        } else {
            None
        };

        if let Some(reason) = reason {
            registration.sealed = true;
            Self::mark_seal_latch(&mut state, key, reason);
            return QuarantineAdmission::SealRequired(reason);
        }

        registration.quarantined_events = next_events;
        registration.quarantined_bytes = next_bytes;
        state.global_quarantined_events = global_events.saturating_add(1);
        state.global_quarantined_bytes = global_bytes.saturating_add(bytes);
        QuarantineAdmission::Accepted {
            retained_events: next_events,
            retained_bytes: next_bytes,
        }
    }

    /// Seal an attempt for quarantine timeout without offering a frame. The
    /// control worker calls this on its tick.
    pub(crate) fn seal_timed_out_attempts(&self, now: Instant) -> Vec<AttemptKey> {
        let timeout = Duration::from_millis(AGENT_MESSAGE_QUARANTINE_TIMEOUT_MS);
        let mut sealed = Vec::new();
        let mut state = self.lock();
        let expired: Vec<AttemptKey> = state
            .attempts
            .iter()
            .filter(|(_, row)| {
                !row.sealed && now.saturating_duration_since(row.registered_at) >= timeout
            })
            .map(|(key, _)| *key)
            .collect();
        for key in expired {
            if let Some(row) = state.attempts.get_mut(&key) {
                row.sealed = true;
            }
            Self::mark_seal_latch(&mut state, key, QuarantineSealReason::Timeout);
            sealed.push(key);
        }
        sealed
    }

    #[must_use]
    pub(crate) fn is_sealed(&self, key: AttemptKey) -> bool {
        self.lock().attempts.get(&key).is_some_and(|row| row.sealed)
    }

    #[must_use]
    pub(crate) fn global_quarantine_usage(&self) -> (usize, usize) {
        let state = self.lock();
        (
            state.global_quarantined_events,
            state.global_quarantined_bytes,
        )
    }

    fn mark_seal_latch(
        state: &mut ControlPlaneState,
        key: AttemptKey,
        reason: QuarantineSealReason,
    ) {
        Self::coalesce_latch(state, key, reason.lifecycle_kind(), Some(reason), None);
    }

    // -- lifecycle / death latches ------------------------------------------

    /// Record a coalescing, idempotent lifecycle fact. Never blocks, never
    /// awaits, and never fails for capacity: the priority lane is a control
    /// latch, not a bounded channel that can fill (C-P2-13).
    pub(crate) fn latch_lifecycle(&self, signal: ProviderLifecycleSignal) {
        let key = signal.attempt_key();
        let kind = signal.kind;
        let mut state = self.lock();
        if kind.settles_custody() {
            state.provider_death_observed = true;
        }
        Self::coalesce_latch(&mut state, key, kind, None, Some(signal));
    }

    /// Record a NON-SETTLING seal for every registered attempt at once.
    ///
    /// This is the overflow counterpart of [`Self::latch_provider_death`] and
    /// deliberately NOT that function. A full bounded mailbox is backpressure,
    /// not death: the provider is alive and the reader has not reached EOF.
    /// C-P2-13 requires this event to produce `sealed_live_uncertain`, which
    /// RETAINS the invocation, `current_turn_call`, monitor and arbiter, so it
    /// must never travel on a `settles_custody()` kind and must never set
    /// `provider_death_observed`.
    ///
    /// Two properties make the daemon-wide fan-out safe here, where it is not
    /// safe for a death latch:
    ///
    /// 1. `OverflowSeal` carries the weakest coalescing strength, so this can
    ///    only create a latch where none existed. It can never overwrite or
    ///    downgrade a genuine `TurnCompleted` / `TurnFailed` / `ReaderEof`.
    /// 2. It retains custody rather than releasing it, so an over-broad seal
    ///    errs toward reconciliation, never toward a false terminal fact.
    ///
    /// The fan-out IS an over-approximation: ingress overflows one session's
    /// mailbox but seals every registered attempt, because the stdout reader
    /// has no attempt key of its own. Narrowing this to the exact attempt is
    /// the dispatcher's job (P2-04) and is deliberately not attempted here.
    ///
    /// Returns the number of attempts sealed.
    pub(crate) fn latch_overflow_seal_all(&self, reason: QuarantineSealReason) -> usize {
        debug_assert!(!reason.lifecycle_kind().settles_custody());
        let mut state = self.lock();
        let keys: Vec<AttemptKey> = state.attempts.keys().copied().collect();
        for key in &keys {
            Self::mark_seal_latch(&mut state, *key, reason);
        }
        keys.len()
    }

    /// Record provider death/EOF for every registered attempt at once. Death
    /// is a daemon-wide fact, not a per-attempt one.
    pub(crate) fn latch_provider_death(&self, kind: ProviderLifecycleKind) -> usize {
        debug_assert!(kind.settles_custody());
        let mut state = self.lock();
        state.provider_death_observed = true;
        let keys: Vec<AttemptKey> = state.attempts.keys().copied().collect();
        for key in &keys {
            Self::coalesce_latch(&mut state, *key, kind, None, None);
        }
        keys.len()
    }

    fn coalesce_latch(
        state: &mut ControlPlaneState,
        key: AttemptKey,
        kind: ProviderLifecycleKind,
        seal_reason: Option<QuarantineSealReason>,
        signal: Option<ProviderLifecycleSignal>,
    ) {
        match state.lifecycle_latches.entry(key) {
            Entry::Occupied(mut slot) => {
                let existing = slot.get_mut();
                // Keep the strongest terminal fact. A weaker later fact never
                // downgrades a stronger one, and an equal one is idempotent.
                if kind.strength() > existing.kind.strength() {
                    existing.kind = kind;
                    existing.seal_reason = seal_reason;
                    existing.signal = signal;
                    existing.dirty = true;
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(LifecycleLatch {
                    kind,
                    seal_reason,
                    signal,
                    dirty: true,
                });
            }
        }
    }

    /// Drain up to the frozen control-worker budget: at most 32 dirty latches
    /// or 2 milliseconds per 100-millisecond tick (C-P2-15).
    ///
    /// Latches are RETURNED but stay dirty. They clear only when the exact
    /// Store CAS commits, via [`AppServerControlPlane::clear_latch`], or when
    /// a stronger terminal fact replaces them.
    ///
    /// This is the cursor-free form, equivalent to starting a fresh sweep. The
    /// control worker uses [`AppServerControlPlane::drain_dirty_latches_from`]
    /// so that a latch whose CAS keeps failing cannot starve the others.
    #[must_use]
    pub(crate) fn drain_dirty_latches(&self, started_at: Instant) -> Vec<DirtyLatch> {
        self.drain_dirty_latches_from(None, started_at).latches
    }

    /// One bounded, RESUMABLE pass over the dirty latches (C-P2-15).
    ///
    /// The scan is ordered by [`AttemptKey`] and starts strictly AFTER
    /// `cursor`, wrapping around at most once so a single pass visits each
    /// latch at most once. The returned `next_cursor` is the last key examined,
    /// which the worker feeds back on its next tick.
    ///
    /// Why a cursor is required rather than a plain scan: latches stay dirty
    /// until their exact Store CAS commits, so a latch whose CAS keeps failing
    /// stays in the candidate set forever. With an unordered, always-from-the-
    /// start scan and more than `APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK` dirty
    /// latches, the same prefix would be returned on every tick and every latch
    /// behind it would STARVE indefinitely. Resuming after the last key
    /// examined bounds the worst case to `ceil(n / 32)` ticks to reach any
    /// given latch.
    ///
    /// The scan is bounded twice over: by the frozen 32-latch count and by the
    /// frozen 2 ms wall-clock budget. The key collection it walks is itself
    /// bounded by the frozen 64-attempt registry cap, so there is no unbounded
    /// scan even before the budget applies.
    #[must_use]
    pub(crate) fn drain_dirty_latches_from(
        &self,
        cursor: Option<AttemptKey>,
        started_at: Instant,
    ) -> DirtyLatchSweep {
        let budget = Duration::from_millis(APP_SERVER_CONTROL_MAX_MS_PER_TICK);
        let mut sweep = DirtyLatchSweep {
            latches: Vec::new(),
            next_cursor: cursor,
            examined: 0,
            remaining_dirty: 0,
        };
        let state = self.lock();

        // Bounded by the frozen 64-attempt cap, so sorting is trivial work and
        // gives the cursor a stable, total order to resume within.
        let mut keys: Vec<AttemptKey> = state.lifecycle_latches.keys().copied().collect();
        keys.sort_unstable();
        if keys.is_empty() {
            sweep.next_cursor = None;
            return sweep;
        }

        // Resume strictly after the cursor, wrapping once.
        let start = match cursor {
            Some(cursor) => keys.partition_point(|key| *key <= cursor),
            None => 0,
        };
        let total = keys.len();

        for offset in 0..total {
            let key = keys[(start + offset) % total];
            if sweep.latches.len() >= APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK
                || Instant::now().saturating_duration_since(started_at) >= budget
            {
                break;
            }
            sweep.examined += 1;
            sweep.next_cursor = Some(key);
            let Some(latch) = state.lifecycle_latches.get(&key) else {
                continue;
            };
            if !latch.dirty {
                continue;
            }
            sweep.latches.push(DirtyLatch {
                key,
                kind: latch.kind,
                seal_reason: latch.seal_reason,
                signal: latch.signal.clone(),
            });
        }

        sweep.remaining_dirty = state
            .lifecycle_latches
            .values()
            .filter(|latch| latch.dirty)
            .count();
        sweep
    }

    /// Clear a latch after its exact Store CAS commits. Returns `false` when
    /// the latch was replaced by a stronger terminal fact in the meantime, in
    /// which case it deliberately stays dirty.
    pub(crate) fn clear_latch(&self, key: AttemptKey, committed: ProviderLifecycleKind) -> bool {
        let mut state = self.lock();
        let Some(latch) = state.lifecycle_latches.get_mut(&key) else {
            return false;
        };
        if latch.kind != committed {
            return false;
        }
        latch.dirty = false;
        true
    }

    #[must_use]
    pub(crate) fn dirty_latch_count(&self) -> usize {
        self.lock()
            .lifecycle_latches
            .values()
            .filter(|latch| latch.dirty)
            .count()
    }

    #[must_use]
    pub(crate) fn provider_death_observed(&self) -> bool {
        self.lock().provider_death_observed
    }

    // -- writer receipts -----------------------------------------------------

    /// Whether a new provider write may be admitted. Registry-full refusal and
    /// reconciliation gating both occur BEFORE writer admission (C-P2-15).
    #[must_use]
    pub(crate) fn writer_admission(&self) -> WriterAdmission {
        let state = self.lock();
        if !state.writer_admission_enabled {
            return WriterAdmission::ReconciliationPending;
        }
        if state.attempts.len() >= AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS {
            return WriterAdmission::RegistryFull;
        }
        WriterAdmission::Admitted
    }

    /// Retain a writer receipt independently of the caller. The result reaches
    /// this latch even if the caller oneshot was dropped, so a started
    /// provider-reply permit can still be settled by exact flush evidence.
    pub(crate) fn record_writer_receipt(&self, receipt: WriterReceipt) {
        let mut state = self.lock();
        state.writer_receipts.insert(receipt.operation_id, receipt);
    }

    /// Take the retained receipt for one writer operation.
    #[must_use]
    pub(crate) fn take_writer_receipt(&self, operation_id: Uuid) -> Option<WriterReceipt> {
        let mut state = self.lock();
        state.writer_receipts.remove(&operation_id)
    }

    #[must_use]
    pub(crate) fn writer_receipt_count(&self) -> usize {
        self.lock().writer_receipts.len()
    }

    // -- effect task join handles -------------------------------------------

    /// Retain a running effect task's join handle, keyed by the permit's
    /// immutable `external_join_id`. The plane holds this independently of the
    /// monitor (C-P2-14).
    pub(crate) fn register_effect_task(&self, external_join_id: Uuid, handle: JoinHandle<()>) {
        let mut state = self.lock();
        state.effect_tasks.insert(external_join_id, handle);
    }

    /// Take a retained join handle so the caller can await exact join
    /// evidence. A permit whose handle is absent after restart is
    /// `uncertain_unjoined` and must NOT be inferred dead.
    #[must_use]
    pub(crate) fn take_effect_task(&self, external_join_id: Uuid) -> Option<JoinHandle<()>> {
        let mut state = self.lock();
        state.effect_tasks.remove(&external_join_id)
    }

    #[must_use]
    pub(crate) fn effect_task_count(&self) -> usize {
        self.lock().effect_tasks.len()
    }

    // -- startup reconciliation ---------------------------------------------

    /// Register every durable live `AppServer` attempt BEFORE enabling new
    /// writer admission (C-P2-15).
    ///
    /// Lost in-memory evidence seals that attempt once. Durable registrations
    /// still consume the global 64-attempt cap until settlement, so restart
    /// cannot reset capacity or admit a duplicate turn.
    pub(crate) fn reconcile_startup(
        &self,
        durable: &[DurableAttemptRegistration],
        now: Instant,
    ) -> ReconcileOutcome {
        let mut outcome = ReconcileOutcome {
            registered: 0,
            sealed_for_lost_evidence: Vec::new(),
            refused_registry_full: Vec::new(),
        };
        {
            let mut state = self.lock();
            for row in durable {
                let key = AttemptKey {
                    message_id: row.fence.message_id,
                    attempt_number: row.fence.attempt_number,
                };
                if state.attempts.contains_key(&key) {
                    continue;
                }
                if state.attempts.len() >= AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS {
                    outcome.refused_registry_full.push(key);
                    continue;
                }
                // In-memory quarantine evidence did not survive the restart, so
                // a still-unacknowledged attempt seals exactly once.
                //
                // An attempt that is DURABLY `sealed_live_uncertain` was already
                // sealed by the control worker before the restart. It is
                // registered as sealed but is NOT re-sealed and NOT reported as
                // lost evidence: the seal is already in force, and counting it
                // again would double-settle a fact that is durably recorded.
                let newly_sealed = row.correlation == CorrelationStateV1::CorrelationPending;
                let sealed =
                    newly_sealed || row.correlation == CorrelationStateV1::SealedLiveUncertain;
                state.attempts.insert(
                    key,
                    AttemptRegistration {
                        fence: row.fence.clone(),
                        correlation: row.correlation,
                        provider_turn_id: row.provider_turn_id.clone(),
                        quarantined_events: 0,
                        quarantined_bytes: 0,
                        sealed,
                        registered_at: now,
                    },
                );
                outcome.registered += 1;
                if newly_sealed {
                    Self::mark_seal_latch(&mut state, key, QuarantineSealReason::Timeout);
                    outcome.sealed_for_lost_evidence.push(key);
                }
            }
            state.writer_admission_enabled = true;
        }
        outcome
    }
}

// ---------------------------------------------------------------------------
// Ingress frame classification (C-P2-15)
// ---------------------------------------------------------------------------

/// The frozen set of notification methods that carry lifecycle/terminal
/// authority for an AppServer thread.
///
/// Membership is decided by an EXACT match on the top-level `method` string.
/// C-P2-15 forbids substring classification: a terminal-looking substring or a
/// nested key inside `params` is ordinary payload and never lifecycle
/// authority. `turn/completed` is included even though it also carries the
/// success case, because its `turn.status` decides success/failure/interrupt
/// and losing it strands the turn's settlement.
pub(crate) const APP_SERVER_LIFECYCLE_NOTIFICATION_METHODS: &[&str] = &["turn/completed", "error"];

/// Why a frame could not be structurally proven to belong to a suppressible
/// class. Every variant fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IngressAmbiguity {
    /// The frame is not a JSON object, so it has no addressable shape.
    NotAnObject,
    /// A response body carried an `id` that is not an exact `JsonRpcId`.
    UnusableResponseId,
    /// A request carried an `id` that is not an exact `JsonRpcId`.
    UnusableRequestId,
    /// A single frame carried both `result` and `error`.
    ConflictingResponseBody,
    /// A frame carried a response body and a `method`, so it is neither a
    /// well-formed response nor a well-formed request.
    ResponseCarriesMethod,
    /// No `method` and no response body: this is the CLI-compatible event
    /// shape, which carries agent message content and is never suppressible.
    UnknownFrameShape,
}

impl IngressAmbiguity {
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NotAnObject => "not_an_object",
            Self::UnusableResponseId => "unusable_response_id",
            Self::UnusableRequestId => "unusable_request_id",
            Self::ConflictingResponseBody => "conflicting_response_body",
            Self::ResponseCarriesMethod => "response_carries_method",
            Self::UnknownFrameShape => "unknown_frame_shape",
        }
    }
}

/// Structural class of exactly one inbound AppServer stdout frame (C-P2-15).
///
/// Classification is performed BEFORE any delivery attempt so that the stdout
/// task already knows, at the moment a mailbox reports full, whether the frame
/// may be dropped. It borrows the method out of the frame, so classifying is
/// allocation-free on the hot path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IngressFrameClass<'a> {
    /// Exact `id` plus exactly one of `result`/`error`. Delivery completes a
    /// registered response waiter, never a bounded channel.
    Response { id: JsonRpcId, is_error: bool },
    /// Exact `id` plus a `method`: the provider is blocked awaiting a reply.
    ProviderRequest { id: JsonRpcId, method: &'a str },
    /// A `method` in [`APP_SERVER_LIFECYCLE_NOTIFICATION_METHODS`] and no
    /// `id`. Terminality must outlive any bounded queue.
    Lifecycle { method: &'a str },
    /// A `method` outside the lifecycle set and no `id`. Structurally proven
    /// non-response, non-request and nonterminal.
    OrdinarySuppressible { method: &'a str },
    /// Anything not structurally proven to be one of the above.
    Ambiguous(IngressAmbiguity),
}

/// What the stdout task must do when the destination mailbox for a frame
/// reports FULL.
///
/// This is the overflow policy only. A mailbox that reports CLOSED has no
/// consumer left and is a different fact entirely — it is not overflow and
/// must not seal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IngressOverflowAction {
    /// The only class C-P2-15 permits to drop with no seal: structurally
    /// proven ordinary non-message, non-response, non-request, nonterminal
    /// evidence.
    DropWithoutSeal,
    /// The fact must outlive the bounded queue, so it is recorded on the
    /// plane's coalescing lifecycle latch instead of being dropped.
    LatchLifecycle,
    /// Response, provider-request, or ambiguous overflow. Custody can never be
    /// established from a frame that was silently discarded, so this fails
    /// closed: seal the attempt if one is registered, otherwise terminate the
    /// provider.
    SealOrTerminate,
}

impl IngressFrameClass<'_> {
    /// The C-P2-15 overflow policy for this class.
    #[must_use]
    pub(crate) const fn overflow_action(&self) -> IngressOverflowAction {
        match self {
            Self::OrdinarySuppressible { .. } => IngressOverflowAction::DropWithoutSeal,
            Self::Lifecycle { .. } => IngressOverflowAction::LatchLifecycle,
            Self::Response { .. } | Self::ProviderRequest { .. } | Self::Ambiguous(_) => {
                IngressOverflowAction::SealOrTerminate
            }
        }
    }

    /// A stable label for tracing. Never includes payload bytes.
    #[must_use]
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Response { .. } => "response",
            Self::ProviderRequest { .. } => "provider_request",
            Self::Lifecycle { .. } => "lifecycle",
            Self::OrdinarySuppressible { .. } => "ordinary_suppressible",
            Self::Ambiguous(_) => "ambiguous",
        }
    }
}

/// Classify one inbound AppServer frame structurally, without allocating and
/// without inspecting payload bytes (C-P2-15).
///
/// The decision uses only exact top-level keys — `id`, `result`, `error`,
/// `method` — and an exact `method` match against the frozen lifecycle set.
/// Every shape that is not positively proven suppressible resolves to
/// [`IngressFrameClass::Ambiguous`], which fails closed.
#[must_use]
pub(crate) fn classify_ingress_frame(frame: &Value) -> IngressFrameClass<'_> {
    let Some(object) = frame.as_object() else {
        return IngressFrameClass::Ambiguous(IngressAmbiguity::NotAnObject);
    };

    let method = object.get("method").and_then(Value::as_str);
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    let id = object.get("id");

    // A response body is `result` XOR `error`, and never carries a method.
    if has_result || has_error {
        if has_result && has_error {
            return IngressFrameClass::Ambiguous(IngressAmbiguity::ConflictingResponseBody);
        }
        if method.is_some() {
            return IngressFrameClass::Ambiguous(IngressAmbiguity::ResponseCarriesMethod);
        }
        let Some(id) = id else {
            return IngressFrameClass::Ambiguous(IngressAmbiguity::UnusableResponseId);
        };
        return match JsonRpcId::from_json(id) {
            Ok(id) => IngressFrameClass::Response {
                id,
                is_error: has_error,
            },
            Err(_) => IngressFrameClass::Ambiguous(IngressAmbiguity::UnusableResponseId),
        };
    }

    if let Some(method) = method {
        // `method` + `id` is a provider request: the provider blocks until it
        // is answered, so it can never be suppressed.
        if let Some(id) = id {
            return match JsonRpcId::from_json(id) {
                Ok(id) => IngressFrameClass::ProviderRequest { id, method },
                Err(_) => IngressFrameClass::Ambiguous(IngressAmbiguity::UnusableRequestId),
            };
        }
        return if APP_SERVER_LIFECYCLE_NOTIFICATION_METHODS.contains(&method) {
            IngressFrameClass::Lifecycle { method }
        } else {
            IngressFrameClass::OrdinarySuppressible { method }
        };
    }

    // No method and no response body: the CLI-compatible event shape, which
    // carries agent message content. Never suppressible.
    IngressFrameClass::Ambiguous(IngressAmbiguity::UnknownFrameShape)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::is_canonical_jsonrpc_id;
    use rsi_common::agent_coordination::AGENT_MESSAGE_MAX_CAPABILITY_ID_BYTES;
    use std::collections::BTreeSet;

    fn fence(attempt_number: u32) -> MessageAttemptFenceV1 {
        MessageAttemptFenceV1 {
            message_id: Uuid::from_u128(0x1111_1111),
            attempt_number,
            claim_token: Uuid::from_u128(0x2222_2222),
            delivery_boot_id: Uuid::from_u128(0x3333_3333),
            delivery_session_id: Uuid::from_u128(0x4444_4444),
            delivery_session_generation: 1,
            delivery_model_invocation_id: Uuid::from_u128(0x5555_5555),
        }
    }

    fn fence_for(message: u128, attempt_number: u32) -> MessageAttemptFenceV1 {
        MessageAttemptFenceV1 {
            message_id: Uuid::from_u128(message),
            ..fence(attempt_number)
        }
    }

    // -- JsonRpcId ----------------------------------------------------------

    #[test]
    fn jsonrpc_id_canonical_form_matches_the_sql_checker_and_round_trips() {
        let cases = vec![
            JsonRpcId::Number(1),
            JsonRpcId::Number(-1),
            JsonRpcId::Number(i64::MAX),
            JsonRpcId::Number(i64::MIN),
            JsonRpcId::String(String::from("abc")),
            JsonRpcId::String(String::new()),
            JsonRpcId::String(String::from("with \"quote\" and \\ backslash")),
            JsonRpcId::String(String::from("newline\nand\ttab")),
            JsonRpcId::String("x".repeat(APP_SERVER_MAX_JSONRPC_STRING_ID_BYTES)),
        ];
        for id in cases {
            let canonical = id.to_canonical();
            assert!(
                is_canonical_jsonrpc_id(&canonical),
                "the SQL CHECK must accept every canonical form the daemon produces: {canonical}"
            );
            assert_eq!(
                JsonRpcId::from_canonical(&canonical),
                Ok(id.clone()),
                "canonical persistence must be exactly reversible"
            );
        }
    }

    #[test]
    fn jsonrpc_id_number_and_string_never_alias() {
        let numeric = JsonRpcId::Number(7).to_canonical();
        let textual = JsonRpcId::String(String::from("7")).to_canonical();
        assert_ne!(numeric, textual);
        assert_eq!(numeric, "n:7");
        assert_eq!(textual, "s:\"7\"");
    }

    #[test]
    fn jsonrpc_id_rejects_the_forbidden_zero_alias() {
        assert_eq!(
            JsonRpcId::number(0),
            Err(JsonRpcIdError::ForbiddenZeroAlias)
        );
        assert_eq!(
            JsonRpcId::from_json(&Value::Number(0.into())),
            Err(JsonRpcIdError::ForbiddenZeroAlias)
        );
        assert_eq!(
            JsonRpcId::from_canonical("n:0"),
            Err(JsonRpcIdError::NotCanonical)
        );
        assert!(!is_canonical_jsonrpc_id("n:0"));
    }

    #[test]
    fn jsonrpc_id_from_json_fails_closed_on_every_non_exact_type() {
        assert_eq!(
            JsonRpcId::from_json(&Value::Null),
            Err(JsonRpcIdError::UnsupportedJsonType)
        );
        assert_eq!(
            JsonRpcId::from_json(&Value::Bool(true)),
            Err(JsonRpcIdError::UnsupportedJsonType)
        );
        assert_eq!(
            JsonRpcId::from_json(&Value::Array(vec![])),
            Err(JsonRpcIdError::UnsupportedJsonType)
        );
        assert_eq!(
            JsonRpcId::from_json(&Value::Object(serde_json::Map::new())),
            Err(JsonRpcIdError::UnsupportedJsonType)
        );
        let float: Value = serde_json::from_str("1.5").expect("float literal");
        assert_eq!(
            JsonRpcId::from_json(&float),
            Err(JsonRpcIdError::NumberOutOfRange)
        );
        let too_big: Value =
            serde_json::from_str("9223372036854775808").expect("out-of-range literal");
        assert_eq!(
            JsonRpcId::from_json(&too_big),
            Err(JsonRpcIdError::NumberOutOfRange)
        );
    }

    #[test]
    fn jsonrpc_id_rejects_oversized_strings_at_the_frozen_ceiling() {
        let at_limit = "x".repeat(APP_SERVER_MAX_JSONRPC_STRING_ID_BYTES);
        assert!(JsonRpcId::string(at_limit).is_ok());
        let over_limit = "x".repeat(APP_SERVER_MAX_JSONRPC_STRING_ID_BYTES + 1);
        assert_eq!(
            JsonRpcId::string(over_limit),
            Err(JsonRpcIdError::StringTooLong)
        );
    }

    #[test]
    fn jsonrpc_id_from_canonical_rejects_non_canonical_spellings() {
        for bad in [
            "n:+7",
            "n:007",
            "n:-0",
            "n:",
            "n:abc",
            "7",
            "s:7",
            "s:'a'",
            "s:\"a",
            "x:1",
            "s:\"\\u0041\"",
        ] {
            assert_eq!(
                JsonRpcId::from_canonical(bad),
                Err(JsonRpcIdError::NotCanonical),
                "must reject non-canonical form {bad}"
            );
            assert!(
                !is_canonical_jsonrpc_id(bad),
                "the SQL checker must agree about {bad}"
            );
        }
    }

    #[test]
    fn jsonrpc_id_reconstructs_the_original_json_type() {
        assert_eq!(JsonRpcId::Number(7).to_json(), Value::Number(7.into()));
        assert_eq!(
            JsonRpcId::String(String::from("7")).to_json(),
            Value::String(String::from("7"))
        );
    }

    // -- writer -------------------------------------------------------------

    #[test]
    fn writer_result_treats_only_a_flushed_write_as_delivery_proof() {
        assert!(WriterOperationResult::WriteFlushed.proves_delivery());
        for result in [
            WriterOperationResult::RejectedBeforeEnqueue,
            WriterOperationResult::WriteFailed,
            WriterOperationResult::WriterClosed,
        ] {
            assert!(
                !result.proves_delivery(),
                "{} must never prove delivery",
                result.as_str()
            );
        }
        assert!(WriterOperationResult::RejectedBeforeEnqueue.proves_no_effect());
        assert!(!WriterOperationResult::WriteFailed.proves_no_effect());
    }

    #[test]
    fn writer_receipt_reaches_the_plane_even_when_the_caller_oneshot_is_dropped() {
        let plane = AppServerControlPlane::new();
        let operation_id = Uuid::from_u128(0xABCD);
        let (command, receiver) = WriterCommand::new(operation_id, b"{}\n".to_vec());
        drop(receiver);
        assert!(command.completion.is_some());
        plane.record_writer_receipt(WriterReceipt {
            operation_id,
            result: WriterOperationResult::WriteFlushed,
        });
        let receipt = plane.take_writer_receipt(operation_id);
        assert_eq!(
            receipt.map(|r| r.result),
            Some(WriterOperationResult::WriteFlushed)
        );
        assert_eq!(plane.writer_receipt_count(), 0);
    }

    // -- capabilities --------------------------------------------------------

    #[test]
    fn handler_capability_is_consumed_only_by_its_matching_durable_cas() {
        let capability_id = Uuid::from_u128(1);
        let handler = AuthorizedProviderRequestHandler::new(
            capability_id,
            Uuid::from_u128(2),
            ProviderRequestHandlerKindV1::ToolExecution,
            5,
        );
        let returned = handler
            .consume_for_started_cas(Uuid::from_u128(999))
            .expect_err("a mismatched CAS must consume nothing");
        assert_eq!(returned.capability_id(), capability_id);
        let consumed = returned
            .consume_for_started_cas(capability_id)
            .expect("the matching CAS spends the capability");
        assert_eq!(consumed.capability_id, capability_id);
        assert_eq!(consumed.gate_generation, 5);
    }

    #[test]
    fn reply_capability_is_consumed_only_by_its_matching_durable_cas() {
        let capability_id = Uuid::from_u128(3);
        let reply = AuthorizedProviderRequestReply::new(
            capability_id,
            Uuid::from_u128(4),
            JsonRpcId::Number(11),
            7,
        );
        let returned = reply
            .consume_for_started_cas(Uuid::from_u128(888))
            .expect_err("a mismatched CAS must consume nothing");
        let consumed = returned
            .consume_for_started_cas(capability_id)
            .expect("the matching CAS spends the capability");
        assert_eq!(consumed.provider_request_id, JsonRpcId::Number(11));
    }

    #[test]
    fn capability_ids_fit_the_frozen_persistence_ceiling() {
        assert_eq!(
            Uuid::from_u128(1).to_string().len(),
            AGENT_MESSAGE_MAX_CAPABILITY_ID_BYTES
        );
    }

    // -- fenced request ------------------------------------------------------

    #[test]
    fn fenced_request_never_accepts_a_continuation_token_as_a_turn_id() {
        let error = FencedAppServerProviderRequest::new(
            fence(1),
            JsonRpcId::Number(1),
            JsonRpcId::Number(2),
            ProviderRequestHandlerKindV1::ToolExecution,
            "item/tool/call",
            "sha256:abc",
            Some(String::from("continuation:thread-7")),
        )
        .expect_err("a continuation token is never a genuine turn ID");
        assert_eq!(error, FencedRequestError::NotAGenuineTurnId);
    }

    #[test]
    fn fenced_request_bounds_every_scalar() {
        let base = |method: &str, digest: &str, turn: Option<String>| {
            FencedAppServerProviderRequest::new(
                fence(1),
                JsonRpcId::Number(1),
                JsonRpcId::Number(2),
                ProviderRequestHandlerKindV1::ApprovalPresentation,
                method,
                digest,
                turn,
            )
        };
        assert_eq!(
            base("", "sha256:abc", None).expect_err("empty method"),
            FencedRequestError::MethodOutOfBounds
        );
        assert_eq!(
            base(&"m".repeat(AGENT_MESSAGE_MAX_METHOD_BYTES + 1), "d", None)
                .expect_err("oversized method"),
            FencedRequestError::MethodOutOfBounds
        );
        assert_eq!(
            base("item/x", "", None).expect_err("empty digest"),
            FencedRequestError::DigestOutOfBounds
        );
        assert_eq!(
            base(
                "item/x",
                "d",
                Some("t".repeat(AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES + 1))
            )
            .expect_err("oversized turn"),
            FencedRequestError::TurnIdOutOfBounds
        );
        let ok = base("item/x", "sha256:abc", Some(String::from("turn-1")))
            .expect("a bounded genuine turn is accepted");
        assert_eq!(ok.provider_turn_id(), Some("turn-1"));
        assert_eq!(ok.attempt_key().attempt_number, 1);
    }

    // -- lifecycle signals ---------------------------------------------------

    #[test]
    fn lifecycle_signal_bounds_scalars_and_rejects_non_turn_tokens() {
        let signal = |turn: Option<&str>, status: Option<&str>, class: Option<&str>| {
            ProviderLifecycleSignal {
                kind: ProviderLifecycleKind::TurnCompleted,
                message_id: Uuid::from_u128(1),
                attempt_number: 1,
                model_invocation_id: Uuid::from_u128(2),
                native_turn_id: turn.map(str::to_string),
                terminal_status: status.map(str::to_string),
                error_class: class.map(str::to_string),
                usage: None,
            }
        };
        assert!(
            signal(Some("turn-1"), Some("completed"), None)
                .validate()
                .is_ok()
        );
        assert_eq!(
            signal(Some("continuation:x"), None, None)
                .validate()
                .expect_err("continuation token"),
            LifecycleSignalError::NotAGenuineTurnId
        );
        assert_eq!(
            signal(Some(""), None, None)
                .validate()
                .expect_err("empty turn"),
            LifecycleSignalError::TurnIdOutOfBounds
        );
        assert_eq!(
            signal(
                None,
                Some(&"s".repeat(AGENT_MESSAGE_MAX_ENUM_BYTES + 1)),
                None
            )
            .validate()
            .expect_err("oversized status"),
            LifecycleSignalError::StatusOutOfBounds
        );
        assert_eq!(
            signal(
                None,
                None,
                Some(&"e".repeat(AGENT_MESSAGE_MAX_ERROR_CLASS_BYTES + 1))
            )
            .validate()
            .expect_err("oversized error class"),
            LifecycleSignalError::ErrorClassOutOfBounds
        );
    }

    #[test]
    fn usage_evidence_can_never_block_lifecycle_settlement() {
        let mut signal = ProviderLifecycleSignal {
            kind: ProviderLifecycleKind::TurnCompleted,
            message_id: Uuid::from_u128(1),
            attempt_number: 1,
            model_invocation_id: Uuid::from_u128(2),
            native_turn_id: Some(String::from("turn-1")),
            terminal_status: Some(String::from("completed")),
            error_class: None,
            usage: None,
        };
        assert!(signal.validate().is_ok());
        signal.usage = Some(ProviderLifecycleUsage {
            input_tokens: Some(10),
            output_tokens: None,
        });
        assert!(
            signal.validate().is_ok(),
            "absent or partial usage is optional evidence, never a settlement precondition"
        );
    }

    #[test]
    fn only_eof_and_confirmed_death_settle_retained_custody() {
        assert!(ProviderLifecycleKind::ReaderEof.settles_custody());
        assert!(ProviderLifecycleKind::ConfirmedProcessDeath.settles_custody());
        for kind in [
            ProviderLifecycleKind::OverflowSeal,
            ProviderLifecycleKind::TimeoutSeal,
            ProviderLifecycleKind::TurnCompleted,
            ProviderLifecycleKind::TurnFailed,
        ] {
            assert!(
                !kind.settles_custody(),
                "{} must not release custody on its own",
                kind.as_str()
            );
        }
    }

    // -- response correlation ------------------------------------------------

    #[test]
    fn responses_route_only_by_exact_registered_id_with_no_fallback() {
        let plane = AppServerControlPlane::new();
        let registered = JsonRpcId::Number(41);
        let mut rx = plane
            .register_response_waiter(&registered)
            .expect("first registration succeeds");

        // A different ID must not complete the registered waiter.
        assert!(
            !plane.complete_response(&JsonRpcId::Number(42), JsonRpcResponse::Result(json_ok()))
        );
        assert!(rx.try_recv().is_err());

        // A string ID that stringifies the same must not alias the numeric one.
        assert!(!plane.complete_response(
            &JsonRpcId::String(String::from("41")),
            JsonRpcResponse::Result(json_ok())
        ));
        assert!(rx.try_recv().is_err());

        assert!(plane.complete_response(&registered, JsonRpcResponse::Result(json_ok())));
        assert!(matches!(rx.try_recv(), Ok(JsonRpcResponse::Result(_))));
        assert_eq!(plane.registered_waiter_count(), 0);
    }

    #[test]
    fn duplicate_response_registration_fails_closed() {
        let plane = AppServerControlPlane::new();
        let id = JsonRpcId::Number(5);
        let _first = plane
            .register_response_waiter(&id)
            .expect("first registration succeeds");
        assert!(
            plane.register_response_waiter(&id).is_none(),
            "a duplicate ID must fail closed rather than replace the live waiter"
        );
        assert!(plane.cancel_response_waiter(&id));
        assert_eq!(plane.registered_waiter_count(), 0);
    }

    fn json_ok() -> Value {
        serde_json::json!({"ok": true})
    }

    // -- registration and capacity ------------------------------------------

    #[test]
    fn attempt_registry_enforces_the_frozen_global_cap() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        for index in 0..AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS {
            let outcome = plane.register_attempt(
                &fence_for(index as u128 + 1, 1),
                CorrelationStateV1::CorrelationPending,
                now,
            );
            assert_eq!(outcome, AttemptRegistrationOutcome::Registered);
        }
        assert_eq!(
            plane.registered_attempt_count(),
            AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS
        );
        assert_eq!(
            plane.register_attempt(
                &fence_for(9999, 1),
                CorrelationStateV1::CorrelationPending,
                now
            ),
            AttemptRegistrationOutcome::RegistryFull,
            "the 65th attempt must be refused"
        );
    }

    #[test]
    fn exact_reregistration_is_idempotent() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let f = fence(1);
        assert_eq!(
            plane.register_attempt(&f, CorrelationStateV1::CorrelationPending, now),
            AttemptRegistrationOutcome::Registered
        );
        assert_eq!(
            plane.register_attempt(&f, CorrelationStateV1::CorrelationPending, now),
            AttemptRegistrationOutcome::AlreadyRegistered
        );
        assert_eq!(plane.registered_attempt_count(), 1);
    }

    // -- quarantine ----------------------------------------------------------

    #[test]
    fn quarantine_seals_at_the_frozen_per_attempt_event_ceiling() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let key = AttemptKey {
            message_id: fence(1).message_id,
            attempt_number: 1,
        };
        plane.register_attempt(&fence(1), CorrelationStateV1::CorrelationPending, now);
        for index in 0..AGENT_MESSAGE_QUARANTINE_MAX_EVENTS_PER_ATTEMPT {
            assert!(
                matches!(
                    plane.offer_quarantined_evidence(key, 16, now),
                    QuarantineAdmission::Accepted { .. }
                ),
                "event {index} must fit inside the frozen per-attempt budget"
            );
        }
        assert_eq!(
            plane.offer_quarantined_evidence(key, 16, now),
            QuarantineAdmission::SealRequired(QuarantineSealReason::PerAttemptEvents)
        );
        assert!(plane.is_sealed(key));
    }

    #[test]
    fn quarantine_seals_at_the_frozen_per_attempt_byte_ceiling() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let key = AttemptKey {
            message_id: fence(1).message_id,
            attempt_number: 1,
        };
        plane.register_attempt(&fence(1), CorrelationStateV1::CorrelationPending, now);
        assert_eq!(
            plane.offer_quarantined_evidence(
                key,
                AGENT_MESSAGE_QUARANTINE_MAX_BYTES_PER_ATTEMPT + 1,
                now
            ),
            QuarantineAdmission::SealRequired(QuarantineSealReason::PerAttemptBytes)
        );
        assert!(plane.is_sealed(key));
    }

    #[test]
    fn a_sealed_attempt_never_seals_twice_and_admits_no_further_evidence() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let key = AttemptKey {
            message_id: fence(1).message_id,
            attempt_number: 1,
        };
        plane.register_attempt(&fence(1), CorrelationStateV1::CorrelationPending, now);
        assert_eq!(
            plane.offer_quarantined_evidence(
                key,
                AGENT_MESSAGE_QUARANTINE_MAX_BYTES_PER_ATTEMPT + 1,
                now
            ),
            QuarantineAdmission::SealRequired(QuarantineSealReason::PerAttemptBytes)
        );
        assert_eq!(
            plane.offer_quarantined_evidence(key, 1, now),
            QuarantineAdmission::Rejected,
            "post-seal evidence is dropped without a second seal"
        );
    }

    #[test]
    fn quarantine_seals_on_the_frozen_five_second_timeout() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let key = AttemptKey {
            message_id: fence(1).message_id,
            attempt_number: 1,
        };
        plane.register_attempt(&fence(1), CorrelationStateV1::CorrelationPending, now);
        let later = now + Duration::from_millis(AGENT_MESSAGE_QUARANTINE_TIMEOUT_MS);
        assert_eq!(
            plane.offer_quarantined_evidence(key, 1, later),
            QuarantineAdmission::SealRequired(QuarantineSealReason::Timeout)
        );
        assert!(plane.is_sealed(key));
    }

    #[test]
    fn control_worker_sweep_seals_every_timed_out_attempt_exactly_once() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        plane.register_attempt(
            &fence_for(1, 1),
            CorrelationStateV1::CorrelationPending,
            now,
        );
        plane.register_attempt(
            &fence_for(2, 1),
            CorrelationStateV1::CorrelationPending,
            now,
        );
        let later = now + Duration::from_millis(AGENT_MESSAGE_QUARANTINE_TIMEOUT_MS + 1);
        let mut sealed = plane.seal_timed_out_attempts(later);
        sealed.sort_unstable();
        assert_eq!(sealed.len(), 2);
        assert!(
            plane.seal_timed_out_attempts(later).is_empty(),
            "an already-sealed attempt must not seal again"
        );
    }

    #[test]
    fn releasing_an_attempt_returns_its_budget_to_the_global_pool() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let key = AttemptKey {
            message_id: fence(1).message_id,
            attempt_number: 1,
        };
        plane.register_attempt(&fence(1), CorrelationStateV1::CorrelationPending, now);
        plane.offer_quarantined_evidence(key, 100, now);
        assert_eq!(plane.global_quarantine_usage(), (1, 100));
        assert!(plane.release_attempt(key));
        assert_eq!(plane.global_quarantine_usage(), (0, 0));
        assert_eq!(plane.registered_attempt_count(), 0);
    }

    // -- correlation custody -------------------------------------------------

    #[test]
    fn a_genuine_turn_id_fills_once_and_only_while_correlation_is_pending() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let key = AttemptKey {
            message_id: fence(1).message_id,
            attempt_number: 1,
        };
        plane.register_attempt(&fence(1), CorrelationStateV1::CorrelationPending, now);
        assert!(plane.fill_genuine_turn_id(key, "turn-1"));
        assert_eq!(plane.genuine_turn_id(key).as_deref(), Some("turn-1"));
        assert!(
            !plane.fill_genuine_turn_id(key, "turn-2"),
            "the real turn ID fills exactly once"
        );
    }

    #[test]
    fn a_sealed_attempt_rejects_a_late_turn_fill() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let key = AttemptKey {
            message_id: fence(1).message_id,
            attempt_number: 1,
        };
        plane.register_attempt(&fence(1), CorrelationStateV1::CorrelationPending, now);
        plane.offer_quarantined_evidence(
            key,
            AGENT_MESSAGE_QUARANTINE_MAX_BYTES_PER_ATTEMPT + 1,
            now,
        );
        assert!(
            !plane.fill_genuine_turn_id(key, "turn-1"),
            "after sealed_live_uncertain a late response can never fill or ack"
        );
    }

    #[test]
    fn a_continuation_token_is_never_accepted_as_a_turn_fill() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let key = AttemptKey {
            message_id: fence(1).message_id,
            attempt_number: 1,
        };
        plane.register_attempt(&fence(1), CorrelationStateV1::CorrelationPending, now);
        assert!(!plane.fill_genuine_turn_id(key, "continuation:thread-1"));
        assert!(!plane.fill_genuine_turn_id(key, ""));
        assert!(plane.genuine_turn_id(key).is_none());
    }

    // -- latches -------------------------------------------------------------

    #[test]
    fn lifecycle_latches_coalesce_and_never_downgrade_a_stronger_fact() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let key = AttemptKey {
            message_id: fence(1).message_id,
            attempt_number: 1,
        };
        plane.register_attempt(&fence(1), CorrelationStateV1::CorrelationPending, now);

        plane.latch_lifecycle(ProviderLifecycleSignal {
            kind: ProviderLifecycleKind::TurnCompleted,
            message_id: key.message_id,
            attempt_number: key.attempt_number,
            model_invocation_id: Uuid::from_u128(2),
            native_turn_id: None,
            terminal_status: None,
            error_class: None,
            usage: None,
        });
        assert_eq!(plane.dirty_latch_count(), 1);

        // A weaker seal must not downgrade the correlated terminal fact.
        plane.latch_lifecycle(ProviderLifecycleSignal {
            kind: ProviderLifecycleKind::OverflowSeal,
            message_id: key.message_id,
            attempt_number: key.attempt_number,
            model_invocation_id: Uuid::from_u128(2),
            native_turn_id: None,
            terminal_status: None,
            error_class: None,
            usage: None,
        });
        let drained = plane.drain_dirty_latches(Instant::now());
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].kind, ProviderLifecycleKind::TurnCompleted);

        // Confirmed death is stronger and replaces it.
        assert_eq!(
            plane.latch_provider_death(ProviderLifecycleKind::ConfirmedProcessDeath),
            1
        );
        let drained = plane.drain_dirty_latches(Instant::now());
        assert_eq!(
            drained[0].kind,
            ProviderLifecycleKind::ConfirmedProcessDeath
        );
        assert!(plane.provider_death_observed());
    }

    #[test]
    fn a_latch_stays_dirty_until_its_exact_store_cas_commits() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let key = AttemptKey {
            message_id: fence(1).message_id,
            attempt_number: 1,
        };
        plane.register_attempt(&fence(1), CorrelationStateV1::CorrelationPending, now);
        plane.latch_lifecycle(ProviderLifecycleSignal {
            kind: ProviderLifecycleKind::TurnFailed,
            message_id: key.message_id,
            attempt_number: key.attempt_number,
            model_invocation_id: Uuid::from_u128(2),
            native_turn_id: None,
            terminal_status: None,
            error_class: Some(String::from("provider_error")),
            usage: None,
        });
        assert_eq!(plane.dirty_latch_count(), 1);

        // Draining alone does NOT clear the latch.
        let _ = plane.drain_dirty_latches(Instant::now());
        assert_eq!(plane.dirty_latch_count(), 1);

        // A mismatched commit must not clear it either.
        assert!(!plane.clear_latch(key, ProviderLifecycleKind::TurnCompleted));
        assert_eq!(plane.dirty_latch_count(), 1);

        assert!(plane.clear_latch(key, ProviderLifecycleKind::TurnFailed));
        assert_eq!(plane.dirty_latch_count(), 0);
    }

    /// C-P2-15: the resumable cursor is what stops a stuck prefix from
    /// starving every latch behind it.
    ///
    /// More than one tick's worth of latches are made dirty and NONE of them
    /// are ever cleared — modelling a Store CAS that keeps failing. A drain
    /// that always restarted from the beginning would return the same prefix
    /// forever and the suffix would never be seen at all. Resuming after the
    /// last key examined must reach EVERY latch within a finite number of
    /// ticks.
    #[test]
    fn the_resumable_cursor_reaches_every_latch_and_never_starves_a_suffix() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let total = APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK + 10;
        let mut all = BTreeSet::new();
        for index in 0..total {
            let f = fence_for(index as u128 + 1, 1);
            plane.register_attempt(&f, CorrelationStateV1::CorrelationPending, now);
            all.insert(AttemptKey {
                message_id: f.message_id,
                attempt_number: 1,
            });
        }
        assert_eq!(
            plane.latch_overflow_seal_all(QuarantineSealReason::IngressMailboxOverflow),
            total
        );
        assert_eq!(plane.dirty_latch_count(), total);

        // Never clear anything: every latch stays a candidate on every tick.
        let mut seen = BTreeSet::new();
        let mut cursor = None;
        let expected_ticks = total.div_ceil(APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK);
        for _ in 0..expected_ticks {
            let sweep = plane.drain_dirty_latches_from(cursor, Instant::now());
            assert!(
                sweep.latches.len() <= APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK,
                "a tick must never exceed the frozen {APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK}-latch budget, got {}",
                sweep.latches.len()
            );
            for latch in &sweep.latches {
                seen.insert(latch.key);
            }
            cursor = sweep.next_cursor;
        }

        assert_eq!(
            seen, all,
            "ceil({total} / {APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK}) ticks must reach EVERY \
             latch; a starved suffix means the cursor did not resume"
        );
        assert_eq!(
            plane.dirty_latch_count(),
            total,
            "draining alone must never clear a latch"
        );
    }

    /// The cursor wraps, so a sweep that starts mid-registry still terminates
    /// and still visits the keys before its start position on a later tick.
    #[test]
    fn the_resumable_cursor_wraps_instead_of_running_off_the_end() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        for index in 0..4u128 {
            let f = fence_for(index + 1, 1);
            plane.register_attempt(&f, CorrelationStateV1::CorrelationPending, now);
        }
        assert_eq!(
            plane.latch_overflow_seal_all(QuarantineSealReason::IngressMailboxOverflow),
            4
        );

        // Start after the LAST key: the next pass must wrap to the first.
        let last = plane
            .drain_dirty_latches_from(None, Instant::now())
            .latches
            .iter()
            .map(|latch| latch.key)
            .max()
            .expect("four latches exist");
        let wrapped = plane.drain_dirty_latches_from(Some(last), Instant::now());
        assert_eq!(
            wrapped.latches.len(),
            4,
            "a wrapped sweep must still see every dirty latch, not run off the end"
        );
        assert!(
            wrapped.latches.iter().any(|latch| latch.key < last),
            "wrapping must reach keys ordered BEFORE the cursor"
        );
    }

    /// An empty registry must cost nothing and reset the cursor, so an idle
    /// daemon neither scans nor busy-loops.
    #[test]
    fn an_empty_latch_set_yields_an_empty_sweep_and_resets_the_cursor() {
        let plane = AppServerControlPlane::new();
        let stale = AttemptKey {
            message_id: Uuid::from_u128(9),
            attempt_number: 3,
        };
        let sweep = plane.drain_dirty_latches_from(Some(stale), Instant::now());
        assert!(sweep.latches.is_empty());
        assert_eq!(sweep.examined, 0);
        assert_eq!(sweep.remaining_dirty, 0);
        assert_eq!(
            sweep.next_cursor, None,
            "an empty registry must reset the cursor rather than pin it to a vanished key"
        );
    }

    #[test]
    fn the_control_worker_drain_respects_the_frozen_latch_budget() {
        let plane = AppServerControlPlane::new();
        let now = Instant::now();
        let total = APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK + 10;
        for index in 0..total {
            let f = fence_for(index as u128 + 1, 1);
            plane.register_attempt(&f, CorrelationStateV1::CorrelationPending, now);
            plane.latch_lifecycle(ProviderLifecycleSignal {
                kind: ProviderLifecycleKind::TurnCompleted,
                message_id: f.message_id,
                attempt_number: 1,
                model_invocation_id: Uuid::from_u128(2),
                native_turn_id: None,
                terminal_status: None,
                error_class: None,
                usage: None,
            });
        }
        let drained = plane.drain_dirty_latches(Instant::now());
        assert!(
            drained.len() <= APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK,
            "a tick must process at most {APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK} latches, got {}",
            drained.len()
        );
    }

    // -- writer admission and reconciliation ---------------------------------

    #[test]
    fn writer_admission_is_disabled_until_startup_reconciliation_completes() {
        let plane = AppServerControlPlane::new();
        assert_eq!(
            plane.writer_admission(),
            WriterAdmission::ReconciliationPending
        );
        let outcome = plane.reconcile_startup(&[], Instant::now());
        assert_eq!(outcome.registered, 0);
        assert_eq!(plane.writer_admission(), WriterAdmission::Admitted);
    }

    #[test]
    fn reconciliation_registers_durable_attempts_and_seals_lost_evidence_once() {
        let plane = AppServerControlPlane::new();
        let durable = vec![
            DurableAttemptRegistration {
                fence: fence_for(1, 1),
                correlation: CorrelationStateV1::CorrelationPending,
                provider_turn_id: None,
            },
            DurableAttemptRegistration {
                fence: fence_for(2, 1),
                correlation: CorrelationStateV1::SealedLiveUncertain,
                provider_turn_id: Some(String::from("turn-9")),
            },
        ];
        let outcome = plane.reconcile_startup(&durable, Instant::now());
        assert_eq!(outcome.registered, 2);
        assert_eq!(
            outcome.sealed_for_lost_evidence.len(),
            1,
            "only the still-pending attempt loses in-memory evidence and seals"
        );
        assert_eq!(plane.registered_attempt_count(), 2);
    }

    #[test]
    fn restart_cannot_reset_capacity_or_admit_a_duplicate_turn() {
        let plane = AppServerControlPlane::new();
        let durable: Vec<DurableAttemptRegistration> = (0..AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS)
            .map(|index| DurableAttemptRegistration {
                fence: fence_for(index as u128 + 1, 1),
                correlation: CorrelationStateV1::SealedLiveUncertain,
                provider_turn_id: None,
            })
            .collect();
        let outcome = plane.reconcile_startup(&durable, Instant::now());
        assert_eq!(outcome.registered, AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS);
        assert_eq!(
            plane.writer_admission(),
            WriterAdmission::RegistryFull,
            "durable registrations still consume the global cap until settlement"
        );
    }

    #[test]
    fn reconciliation_refuses_beyond_the_global_cap_rather_than_overcommitting() {
        let plane = AppServerControlPlane::new();
        let durable: Vec<DurableAttemptRegistration> = (0..AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS
            + 3)
            .map(|index| DurableAttemptRegistration {
                fence: fence_for(index as u128 + 1, 1),
                correlation: CorrelationStateV1::SealedLiveUncertain,
                provider_turn_id: None,
            })
            .collect();
        let outcome = plane.reconcile_startup(&durable, Instant::now());
        assert_eq!(outcome.registered, AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS);
        assert_eq!(outcome.refused_registry_full.len(), 3);
    }

    // -- effect tasks --------------------------------------------------------

    #[tokio::test]
    async fn effect_task_join_handles_are_retained_independently_of_the_monitor() {
        let plane = AppServerControlPlane::new();
        let join_id = Uuid::from_u128(0x77);
        let handle = tokio::spawn(async {});
        plane.register_effect_task(join_id, handle);
        assert_eq!(plane.effect_task_count(), 1);
        let taken = plane
            .take_effect_task(join_id)
            .expect("the retained handle is available for exact join evidence");
        taken.await.expect("the effect task joins cleanly");
        assert_eq!(plane.effect_task_count(), 0);
        assert!(
            plane.take_effect_task(join_id).is_none(),
            "a handle is taken at most once"
        );
    }

    // -- ingress classification (C-P2-15) ------------------------------------

    #[test]
    fn only_structurally_proven_ordinary_notifications_may_drop_without_a_seal() {
        // The ONE droppable class: a notification with no `id` whose method is
        // outside the frozen lifecycle set.
        let ordinary = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "thread/tokenUsage/updated",
            "params": {"tokenUsage": {"last": {"totalTokens": 5}}},
        });
        let class = classify_ingress_frame(&ordinary);
        assert_eq!(
            class,
            IngressFrameClass::OrdinarySuppressible {
                method: "thread/tokenUsage/updated"
            }
        );
        assert_eq!(
            class.overflow_action(),
            IngressOverflowAction::DropWithoutSeal
        );

        // Every other shape MUST NOT drop without a seal. Each of these is a
        // frame that really appears on the AppServer stdout stream.
        let non_droppable = [
            // a response to a handshake request
            serde_json::json!({"jsonrpc": "2.0", "id": 7, "result": {"thread": {"id": "t"}}}),
            // an error response
            serde_json::json!({"jsonrpc": "2.0", "id": 7, "error": {"message": "boom"}}),
            // a provider request the provider is blocked on
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "item/commandExecution/requestApproval",
                "params": {"command": "rm -rf /"},
            }),
            // a tool call the provider is blocked on
            serde_json::json!({"jsonrpc": "2.0", "id": 10, "method": "item/tool/call"}),
            // lifecycle terminality
            serde_json::json!({"jsonrpc": "2.0", "method": "turn/completed", "params": {}}),
            serde_json::json!({"jsonrpc": "2.0", "method": "error", "params": {}}),
            // the CLI-compatible event shape, which carries agent output
            serde_json::json!({"type": "assistant", "message": {"content": "secret"}}),
            // not an object at all
            serde_json::json!([1, 2, 3]),
        ];
        for frame in &non_droppable {
            let class = classify_ingress_frame(frame);
            assert_ne!(
                class.overflow_action(),
                IngressOverflowAction::DropWithoutSeal,
                "frame {frame} was classified {} and would have been dropped \
                 with no seal, which C-P2-15 forbids",
                class.as_str()
            );
        }
    }

    #[test]
    fn a_terminal_looking_substring_or_nested_key_is_never_lifecycle_authority() {
        // C-P2-15: lifecycle authority comes from an EXACT top-level method
        // match, never from a substring or a nested key. Each of these frames
        // contains terminal-looking text that a substring classifier would
        // wrongly promote.
        let disguised = [
            // "error" as a SUBSTRING of a longer method name
            serde_json::json!({"method": "item/errorRecovered", "params": {}}),
            serde_json::json!({"method": "thread/error/cleared", "params": {}}),
            // "turn/completed" nested inside params, not the method
            serde_json::json!({
                "method": "item/agentMessage/delta",
                "params": {"text": "turn/completed", "method": "error"},
            }),
            // a nested `error` KEY under params, not a top-level response body
            serde_json::json!({
                "method": "item/agentMessage/delta",
                "params": {"error": {"message": "not terminal"}},
            }),
        ];
        for frame in &disguised {
            let class = classify_ingress_frame(frame);
            let method = frame
                .get("method")
                .and_then(Value::as_str)
                .expect("fixture has a method");
            assert_eq!(
                class,
                IngressFrameClass::OrdinarySuppressible { method },
                "{frame} must stay ordinary payload, never lifecycle authority"
            );
        }

        // And the exact matches ARE lifecycle.
        for method in APP_SERVER_LIFECYCLE_NOTIFICATION_METHODS {
            let frame = serde_json::json!({"method": method, "params": {}});
            assert_eq!(
                classify_ingress_frame(&frame),
                IngressFrameClass::Lifecycle { method },
                "{method} is frozen lifecycle authority"
            );
            assert_eq!(
                classify_ingress_frame(&frame).overflow_action(),
                IngressOverflowAction::LatchLifecycle,
            );
        }
    }

    #[test]
    fn malformed_and_conflicting_frame_identity_fails_closed_as_ambiguous() {
        let cases = [
            (
                serde_json::json!({"id": 1, "result": {}, "error": {}}),
                IngressAmbiguity::ConflictingResponseBody,
            ),
            (
                serde_json::json!({"id": 1, "result": {}, "method": "turn/completed"}),
                IngressAmbiguity::ResponseCarriesMethod,
            ),
            // `null`, float, and bool IDs are not exact JsonRpcIds.
            (
                serde_json::json!({"id": null, "result": {}}),
                IngressAmbiguity::UnusableResponseId,
            ),
            (
                serde_json::json!({"id": 1.5, "result": {}}),
                IngressAmbiguity::UnusableResponseId,
            ),
            (
                serde_json::json!({"id": true, "method": "item/tool/call"}),
                IngressAmbiguity::UnusableRequestId,
            ),
            (
                serde_json::json!({"result": {}}),
                IngressAmbiguity::UnusableResponseId,
            ),
            (
                serde_json::json!({"jsonrpc": "2.0"}),
                IngressAmbiguity::UnknownFrameShape,
            ),
            (
                serde_json::json!("bare string"),
                IngressAmbiguity::NotAnObject,
            ),
        ];
        for (frame, expected) in &cases {
            assert_eq!(
                classify_ingress_frame(frame),
                IngressFrameClass::Ambiguous(*expected),
                "{frame} must fail closed"
            );
            assert_eq!(
                classify_ingress_frame(frame).overflow_action(),
                IngressOverflowAction::SealOrTerminate,
                "{frame} must never drop silently"
            );
        }
    }

    #[test]
    fn a_string_id_response_routes_by_exact_identity_and_never_aliases_a_number() {
        let numeric = serde_json::json!({"id": 4, "result": {}});
        let stringy = serde_json::json!({"id": "4", "result": {}});
        let IngressFrameClass::Response { id: n_id, .. } = classify_ingress_frame(&numeric) else {
            panic!("numeric id is a response");
        };
        let IngressFrameClass::Response { id: s_id, .. } = classify_ingress_frame(&stringy) else {
            panic!("string id is a response");
        };
        assert_ne!(
            n_id, s_id,
            "a numeric ID and a string ID must never alias each other"
        );
        assert_eq!(n_id, JsonRpcId::number(4).expect("4 is a valid id"));
        assert_eq!(s_id, JsonRpcId::string("4").expect("\"4\" is a valid id"));

        // is_error distinguishes the two response bodies.
        let err = serde_json::json!({"id": 4, "error": {"message": "x"}});
        assert_eq!(
            classify_ingress_frame(&err),
            IngressFrameClass::Response {
                id: JsonRpcId::number(4).expect("4 is a valid id"),
                is_error: true,
            }
        );
    }
}
