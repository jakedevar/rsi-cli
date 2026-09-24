pub mod call_control;
pub mod registry;
pub mod retry;
pub mod validator;

use crate::bus::{DaemonEvent, EventBus};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use rsi_common::model_control::{
    AdmissionStatus, InvocationOwner, ModelCircuitStatus, ModelControlMode, ModelInvocationPurpose,
    ModelInvocationStatus, ModelTier, ModelUsageConfidence,
};
use rusqlite::OptionalExtension;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelControlSignalChange {
    Snapshot,
    ModeChanged {
        previous_mode: ModelControlMode,
        current_mode: ModelControlMode,
    },
    CircuitChanged {
        scope_kind: rsi_common::model_control::BudgetScopeKind,
        scope_id: Option<String>,
        state: String,
    },
    CancellationRequested {
        invocation_id: Uuid,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelControlSignal {
    pub mode: ModelControlMode,
    pub updated_at: Option<String>,
    pub circuits: Vec<ModelCircuitStatus>,
    pub fault: Option<String>,
    pub change: ModelControlSignalChange,
}

#[derive(Clone)]
pub struct ModelControlRuntime {
    inner: Arc<ModelControlRuntimeInner>,
}

struct ModelControlRuntimeInner {
    next_generation: AtomicU64,
    cancellations: std::sync::Mutex<HashMap<Uuid, RegisteredCancellation>>,
    signal_tx: watch::Sender<ModelControlSignal>,
}

struct RegisteredCancellation {
    generation: u64,
    label: String,
    cancel: Arc<dyn Fn() + Send + Sync>,
    requested: bool,
}

pub struct CancellationRegistration {
    runtime: ModelControlRuntime,
    invocation_id: Uuid,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancellationAttempt {
    pub request_started: bool,
    pub already_requested: bool,
    pub mechanism: String,
}

impl ModelControlRuntime {
    pub fn new(signal: ModelControlSignal) -> Self {
        let (signal_tx, _) = watch::channel(signal);
        Self {
            inner: Arc::new(ModelControlRuntimeInner {
                next_generation: AtomicU64::new(1),
                cancellations: std::sync::Mutex::new(HashMap::new()),
                signal_tx,
            }),
        }
    }

    pub fn default_normal() -> Self {
        Self::new(ModelControlSignal {
            mode: ModelControlMode::Normal,
            updated_at: None,
            circuits: Vec::new(),
            fault: None,
            change: ModelControlSignalChange::Snapshot,
        })
    }

    /// Construct the one daemon-wide control runtime from durable state.
    ///
    /// A read failure is itself a safety event: admitting paid work after a
    /// corrupt or unavailable policy row is worse than temporarily stopping
    /// all model work.
    pub fn from_store_fail_closed(store: &Store) -> Self {
        match (
            store.current_model_control_mode_with_updated_at(),
            store.list_model_circuits(),
        ) {
            (Ok((mode, updated_at)), Ok(circuits)) => Self::new(ModelControlSignal {
                mode,
                updated_at,
                circuits,
                fault: None,
                change: ModelControlSignalChange::Snapshot,
            }),
            (mode, circuits) => {
                let mode_error = mode.err().map(|error| error.to_string());
                let circuits_error = circuits.err().map(|error| error.to_string());
                let fault = format!(
                    "durable model-control state could not be loaded; mode_error={}; circuits_error={}",
                    mode_error.as_deref().unwrap_or("none"),
                    circuits_error.as_deref().unwrap_or("none")
                );
                tracing::error!(
                    mode_error = ?mode_error,
                    circuits_error = ?circuits_error,
                    "failed to load durable model-control state; starting fail closed"
                );
                Self::new(ModelControlSignal {
                    mode: ModelControlMode::StopAll,
                    updated_at: None,
                    circuits: Vec::new(),
                    fault: Some(fault),
                    change: ModelControlSignalChange::Snapshot,
                })
            }
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<ModelControlSignal> {
        self.inner.signal_tx.subscribe()
    }

    pub fn publish_signal(&self, signal: ModelControlSignal) {
        let _ = self.inner.signal_tx.send(signal);
    }

    pub fn snapshot(&self) -> ModelControlSignal {
        self.inner.signal_tx.borrow().clone()
    }

    pub fn register_cancellation(
        &self,
        invocation_id: Uuid,
        label: impl Into<String>,
        cancel: Arc<dyn Fn() + Send + Sync>,
    ) -> CancellationRegistration {
        let generation = self.inner.next_generation.fetch_add(1, Ordering::Relaxed);
        self.inner
            .cancellations
            .lock()
            .expect("model control runtime cancellations lock")
            .insert(
                invocation_id,
                RegisteredCancellation {
                    generation,
                    label: label.into(),
                    cancel,
                    requested: false,
                },
            );
        CancellationRegistration {
            runtime: self.clone(),
            invocation_id,
            generation,
        }
    }

    pub fn cancel_invocation(&self, invocation_id: Uuid) -> CancellationAttempt {
        let mut guard = self
            .inner
            .cancellations
            .lock()
            .expect("model control runtime cancellations lock");
        let Some(entry) = guard.get_mut(&invocation_id) else {
            return CancellationAttempt {
                request_started: false,
                already_requested: false,
                mechanism: "none".to_string(),
            };
        };
        if entry.requested {
            return CancellationAttempt {
                request_started: false,
                already_requested: true,
                mechanism: entry.label.clone(),
            };
        }
        entry.requested = true;
        let cancel = Arc::clone(&entry.cancel);
        let mechanism = entry.label.clone();
        drop(guard);
        (cancel)();
        CancellationAttempt {
            request_started: true,
            already_requested: false,
            mechanism,
        }
    }

    fn unregister(&self, invocation_id: Uuid, generation: u64) {
        let mut guard = self
            .inner
            .cancellations
            .lock()
            .expect("model control runtime cancellations lock");
        if guard
            .get(&invocation_id)
            .is_some_and(|entry| entry.generation == generation)
        {
            guard.remove(&invocation_id);
        }
    }
}

impl Drop for CancellationRegistration {
    fn drop(&mut self) {
        self.runtime.unregister(self.invocation_id, self.generation);
    }
}

#[derive(Debug, Clone)]
pub struct ExpectedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub reasoning_tokens: u64,
    pub embedding_input_count: u64,
    pub wall_time_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ModelAdmissionRequest {
    pub purpose: ModelInvocationPurpose,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub backend: Option<String>,
    pub effort: Option<String>,
    pub trigger: String,
    pub owner: InvocationOwner,
    pub dedup_key: Option<String>,
    pub request_fingerprint: Option<String>,
    pub parent_invocation_id: Option<Uuid>,
    pub retry_of_invocation_id: Option<Uuid>,
    pub expected_usage: Option<ExpectedUsage>,
    pub baseline_input_tokens: u64,
    pub baseline_output_tokens: u64,
    pub baseline_cache_creation_tokens: u64,
    pub baseline_cache_read_tokens: u64,
    pub baseline_reasoning_tokens: u64,
    pub baseline_embedding_input_count: u64,
    pub baseline_wall_time_ms: u64,
}

#[derive(Debug, Clone)]
pub struct AdmissionPermit {
    invocation_id: Uuid,
    pub purpose: ModelInvocationPurpose,
    pub model_tier: ModelTier,
    execution_claimed: Arc<AtomicBool>,
}

/// A purpose-bound, one-use authority to start a model-bearing CLI child.
///
/// This is intentionally distinct from [`AdmissionPermit`]: the permit stays
/// with the caller as settlement identity after this capability is consumed at
/// the immediate process boundary. The private constructor and shared claim
/// bit ensure cloned settlement permits cannot authorize another child.
#[derive(Debug)]
pub(crate) struct CliExecutionCapability {
    invocation_id: Uuid,
    purpose: ModelInvocationPurpose,
    route: registry::RuntimeExecutionRoute,
}

/// A purpose-bound, one-use authority for one HTTP request or one direct
/// CodexAppServer model request.
///
/// The type is deliberately not `Clone`. Its fields are private, and the only
/// production constructor is `AdmissionPermit::claim_model_execution`.
#[derive(Debug)]
pub(crate) struct ModelExecutionCapability {
    invocation_id: Uuid,
    purpose: ModelInvocationPurpose,
    route: registry::RuntimeExecutionRoute,
}

/// A command wrapper that retains execution authority until the exact
/// `tokio::process::Command::spawn` boundary.
pub(crate) struct AdmittedCliCommand {
    command: tokio::process::Command,
    execution: CliExecutionCapability,
    expected_route: registry::RuntimeExecutionRoute,
}

/// A request wrapper that retains execution authority until the exact
/// `reqwest::RequestBuilder::send` boundary.
pub(crate) struct AdmittedHttpRequest {
    request: reqwest::RequestBuilder,
    execution: ModelExecutionCapability,
    expected_route: registry::RuntimeExecutionRoute,
}

/// Opaque, one-use authority passed through the public embedding-provider
/// interface. Provider implementations can consume it at one HTTP boundary,
/// but cannot construct, clone, inspect, or retain the underlying capability.
#[derive(Debug)]
pub struct AdmittedEmbeddingExecution {
    execution: ModelExecutionCapability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingInvocation {
    pub invocation_id: Uuid,
    pub admission_status: AdmissionStatus,
    pub status: String,
    pub request_fingerprint: Option<String>,
}

#[derive(Debug, Clone)]
pub enum AdmissionDecision {
    Admitted(AdmissionPermit),
    Duplicate { invocation_id: Uuid },
}

#[derive(Debug, Clone)]
pub(crate) enum CapacityAdmissionDecision {
    Admitted(AdmissionPermit),
    CapacityDuplicate {
        invocation_id: Uuid,
        phase: crate::store::capacity_recovery::CapacityDeliveryPhase,
    },
}

#[derive(Debug, Clone, Copy)]
enum AdmissionStoreChannel {
    Generic,
    ScheduledCapacity,
}

enum ProcessedAdmission {
    Admitted(AdmissionPermit),
    Duplicate(Uuid),
    CapacityDuplicate(crate::store::capacity_recovery::CapacityDeliveryReceipt),
}

enum StoreChannelAdmissionOutcome {
    Admitted(Uuid),
    Duplicate(Uuid),
    CapacityDuplicate(crate::store::capacity_recovery::CapacityDeliveryReceipt),
    Denied {
        invocation_id: Uuid,
        inserted: bool,
        reason: String,
    },
}

impl AdmissionPermit {
    pub fn invocation_id(&self) -> Uuid {
        self.invocation_id
    }

    /// Claim the single model-bearing CLI execution authorized by this
    /// admission. The returned capability is not cloneable and must be moved
    /// into the process-launching function.
    pub(crate) fn claim_cli_execution(
        &self,
        route: registry::RuntimeExecutionRoute,
    ) -> Result<CliExecutionCapability> {
        self.ensure_runtime_route(route)?;
        self.claim_execution_once("CLI")?;

        Ok(CliExecutionCapability {
            invocation_id: self.invocation_id,
            purpose: self.purpose,
            route,
        })
    }

    /// Claim the single direct model execution authorized by this admission.
    ///
    /// HTTP/AppServer capabilities share the same claim bit as CLI authority,
    /// so a cloned settlement permit cannot cross execution families or start
    /// a second visible provider attempt.
    pub(crate) fn claim_model_execution(
        &self,
        route: registry::RuntimeExecutionRoute,
    ) -> Result<ModelExecutionCapability> {
        self.ensure_runtime_route(route)?;
        self.claim_execution_once("model")?;

        Ok(ModelExecutionCapability {
            invocation_id: self.invocation_id,
            purpose: self.purpose,
            route,
        })
    }

    pub(crate) fn claim_embedding_execution(
        &self,
        route: registry::RuntimeExecutionRoute,
    ) -> Result<AdmittedEmbeddingExecution> {
        Ok(AdmittedEmbeddingExecution {
            execution: self.claim_model_execution(route)?,
        })
    }

    fn ensure_runtime_route(&self, route: registry::RuntimeExecutionRoute) -> Result<()> {
        if registry::purpose_allows_runtime_route(self.purpose, route) {
            return Ok(());
        }
        Err(DaemonError::PolicyDenied(format!(
            "model invocation {} ({}) cannot authorize runtime route {:?}",
            self.invocation_id, self.purpose, route,
        )))
    }

    fn claim_execution_once(&self, execution_kind: &str) -> Result<()> {
        if self.execution_claimed.swap(true, Ordering::AcqRel) {
            return Err(DaemonError::PolicyDenied(format!(
                "model invocation {} already consumed its model execution capability; cannot authorize {execution_kind}",
                self.invocation_id,
            )));
        }
        Ok(())
    }
}

impl CliExecutionCapability {
    pub(crate) fn invocation_id(&self) -> Uuid {
        self.invocation_id
    }

    pub(crate) fn bind_command(
        self,
        expected_route: registry::RuntimeExecutionRoute,
        command: tokio::process::Command,
    ) -> AdmittedCliCommand {
        AdmittedCliCommand {
            command,
            execution: self,
            expected_route,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(route: registry::RuntimeExecutionRoute) -> Self {
        Self {
            invocation_id: Uuid::nil(),
            purpose: ModelInvocationPurpose::SessionLaunchFresh,
            route,
        }
    }
}

/// Where one AppServer write actually stopped (P2-05b).
///
/// Deliberately only two variants: with today's receipt-free `write_tx` these
/// are the only two states this seam can *prove*. Adding a `WriteFlushed`
/// variant here would be a claim the code cannot support, so flush evidence
/// stays with the receipt-bearing writer work rather than being faked here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppServerDispatchOutcome {
    /// The bytes entered the bounded writer queue. There is no flush receipt on
    /// this path, and enqueue without a receipt is effect-possible: the writer
    /// task may already have written them.
    EnqueuedWithoutReceipt,
    /// The writer refused the bytes and handed them back unsent, so nothing
    /// was queued and nothing could have been written. This is the ONE
    /// AppServer outcome that proves no effect.
    RejectedBeforeEnqueue,
}

impl ModelExecutionCapability {
    /// Bind this capability to one HTTP request. The returned wrapper owns
    /// both values and can therefore send the request at most once.
    pub(crate) fn bind_http(
        self,
        expected_route: registry::RuntimeExecutionRoute,
        request: reqwest::RequestBuilder,
    ) -> AdmittedHttpRequest {
        AdmittedHttpRequest {
            request,
            execution: self,
            expected_route,
        }
    }

    /// Consume this capability at the direct AppServer model-request boundary,
    /// reporting WHERE the operation stopped instead of collapsing every
    /// outcome into one `Result<()>` (P2-05b).
    ///
    /// The checks, their order, and the enqueue are byte-for-byte what this
    /// function did before P2-05b; only the *reporting* changed, from
    /// `Result<()>` to `Result<AppServerDispatchOutcome>`.
    ///
    /// **The name is load-bearing and must not change.** The frozen
    /// capability-consumption validator resolves
    /// `CapabilityConsumption::SendCodexAppServer` by matching this exact method
    /// name on this exact receiver ident inside the single contracted item
    /// `send_admitted_app_server_request`
    /// (`registry::EXECUTION_BOUNDARIES`'s `appserver_model_request` row;
    /// `validator.rs`'s `send_codex_app_server` arm). Renaming it, or moving the
    /// call out of that item into a helper, makes the boundary's direct-use
    /// count zero and fails the real-tree validator test. That is why P2-05b
    /// changed the return type in place rather than introducing a parallel
    /// `_classified` entry point.
    ///
    /// The agent-message delivery path needs this distinction and may not
    /// recover it from a generic `Err`, because the plan forbids inferring
    /// absence of effect from a generic error. Here the distinction is
    /// structural and local: **every `Err` this function returns, and the
    /// `RejectedBeforeEnqueue` outcome, are produced strictly BEFORE any byte
    /// reaches the writer queue.** The route check and the method check both
    /// return before the send, and `tokio`'s `Sender::send` hands the value
    /// back in its error, which is positive evidence the bytes were never
    /// queued and therefore never written.
    ///
    /// A successful enqueue is deliberately NOT reported as delivery.
    /// `write_tx` is a bounded channel to a writer task and this path carries
    /// no flush receipt, so the strongest true statement is
    /// enqueue-without-receipt — which the frozen provider matrix classifies as
    /// effect-possible, not as success.
    pub(crate) async fn send_codex_app_server(
        self,
        write_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
        bytes: Vec<u8>,
        method: &'static str,
    ) -> Result<AppServerDispatchOutcome> {
        self.ensure_route(registry::RuntimeExecutionRoute::CodexAppServer)?;
        if !matches!(method, "thread/start" | "turn/start") {
            return Err(DaemonError::InvalidParam(format!(
                "model execution capability cannot authorize AppServer method {method}"
            )));
        }
        match write_tx.send(bytes).await {
            Ok(()) => Ok(AppServerDispatchOutcome::EnqueuedWithoutReceipt),
            Err(_returned_unsent) => Ok(AppServerDispatchOutcome::RejectedBeforeEnqueue),
        }
    }

    fn ensure_route(&self, expected: registry::RuntimeExecutionRoute) -> Result<()> {
        #[cfg(test)]
        if self.invocation_id.is_nil() {
            return Ok(());
        }
        if self.route != expected {
            return Err(DaemonError::PolicyDenied(format!(
                "model invocation {} ({}) was admitted for {:?}, not {:?}",
                self.invocation_id, self.purpose, self.route, expected,
            )));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_test(route: registry::RuntimeExecutionRoute) -> Self {
        Self {
            invocation_id: Uuid::nil(),
            purpose: ModelInvocationPurpose::SessionLaunchFresh,
            route,
        }
    }
}

impl AdmittedCliCommand {
    /// Consume the wrapper and its capability at one model-bearing child spawn.
    pub(crate) fn spawn(mut self) -> Result<tokio::process::Child> {
        if self.execution.route != self.expected_route {
            return Err(DaemonError::PolicyDenied(format!(
                "model invocation {} ({}) was admitted for {:?}, not {:?}",
                self.execution.invocation_id,
                self.execution.purpose,
                self.execution.route,
                self.expected_route,
            )));
        }
        // The durable invocation row is written before this one-use execution
        // capability exists. Stamp every admitted CLI at the final spawn seam
        // so MemoryCli and any future CLI route remain recoverable even when a
        // daemon crash lands before a Session row is persisted.
        self.command.env(
            rsi_common::identity::ENV_MODEL_INVOCATION_ID,
            self.execution.invocation_id.to_string(),
        );
        self.command.env(
            rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
            rsi_common::identity::process_ownership_namespace(),
        );
        self.command.spawn().map_err(DaemonError::Io)
    }
}

impl AdmittedHttpRequest {
    /// Consume the wrapper and its capability at the one raw HTTP send.
    pub(crate) async fn send(self, provider_label: &'static str) -> Result<reqwest::Response> {
        self.execution.ensure_route(self.expected_route)?;
        self.request
            .send()
            .await
            .map_err(|error| DaemonError::Process(format!("{provider_label} HTTP error: {error}")))
    }
}

impl AdmittedEmbeddingExecution {
    pub(crate) fn bind_http(
        self,
        expected_route: registry::RuntimeExecutionRoute,
        request: reqwest::RequestBuilder,
    ) -> AdmittedHttpRequest {
        self.execution.bind_http(expected_route, request)
    }

    #[cfg(test)]
    pub(crate) fn for_test(route: registry::RuntimeExecutionRoute) -> Self {
        Self {
            execution: ModelExecutionCapability::for_test(route),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct InvocationCompletion {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub embedding_input_count: Option<u64>,
    pub wall_time_ms: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
    pub error_class: Option<String>,
    pub confidence: Option<ModelUsageConfidence>,
}

pub fn completion_with_wall_time(
    started_at: Instant,
    error_class: Option<String>,
    confidence: ModelUsageConfidence,
) -> InvocationCompletion {
    InvocationCompletion {
        wall_time_ms: Some(started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
        error_class,
        confidence: Some(confidence),
        ..InvocationCompletion::default()
    }
}

pub fn classify_error_class(error: &DaemonError) -> String {
    match error {
        DaemonError::ChannelClosed | DaemonError::CancellationCleanup(_) => "cancelled".to_string(),
        DaemonError::InvalidParam(_) => "input".to_string(),
        DaemonError::OpenAiApiError(_) => "api".to_string(),
        DaemonError::PolicyDenied(_) => "policy_denied".to_string(),
        DaemonError::ExecutionScratchUnavailable(_) => "execution_scratch_unavailable".to_string(),
        DaemonError::StreamFallbackRequired(_) => "stream_fallback_required".to_string(),
        DaemonError::Process(_) => "process".to_string(),
        DaemonError::StartupProviderInventory(_) => "startup_provider_inventory".to_string(),
        DaemonError::Rpc(_) | DaemonError::StructuredRpc { .. } => "rpc".to_string(),
        DaemonError::Store(_) | DaemonError::Database(_) => "store".to_string(),
        DaemonError::Io(_) => "io".to_string(),
        DaemonError::SessionNotFound(_) => "session_not_found".to_string(),
        DaemonError::SessionExists(_) => "session_exists".to_string(),
        DaemonError::ClaudeBinaryNotFound => "claude_binary_missing".to_string(),
        DaemonError::CodexBinaryNotFound => "codex_binary_missing".to_string(),
        DaemonError::AgyBinaryNotFound => "agy_binary_missing".to_string(),
        DaemonError::Json(_) => "json".to_string(),
    }
}

async fn admit_invocation_through_store_channel(
    store: &Arc<Mutex<Store>>,
    request: ModelAdmissionRequest,
    event_bus: &Arc<EventBus>,
    channel: AdmissionStoreChannel,
    launch_origin: Option<&crate::store::manager_resources::ManagerResourceLaunchOrigin>,
) -> Result<ProcessedAdmission> {
    let registry = registry::lookup(request.purpose).ok_or_else(|| {
        DaemonError::InvalidParam(format!(
            "unregistered model invocation purpose: {}",
            request.purpose
        ))
    })?;
    if registry.paid_risk.is_non_invocation() {
        return Err(DaemonError::InvalidParam(format!(
            "purpose {} is documented as non-invocation and cannot acquire an admission permit",
            request.purpose
        )));
    }

    let model_tier = classify_model_tier(
        request.provider.as_deref(),
        request.backend.as_deref(),
        request.model.as_deref(),
    );
    if request.expected_usage.is_none() {
        return Err(DaemonError::InvalidParam(format!(
            "model admission request missing expected_usage for {}",
            request.purpose
        )));
    }
    let invocation_id =
        if let Some(claim) = launch_origin.and_then(|o| o.manager_succession_claim()) {
            claim.reservation.model_invocation_id
        } else if request.purpose == ModelInvocationPurpose::AgentReserveSuccessor {
            let dedup_key = request.dedup_key.as_deref().ok_or_else(|| {
                DaemonError::InvalidParam(
                    "agent successor admission requires daemon-authored dedup identity".into(),
                )
            })?;
            let mut parts = dedup_key.split(':');
            let prefix = (parts.next(), parts.next(), parts.next());
            let reservation = parts.next();
            let invocation = parts.next();
            if prefix != (Some("agent"), Some("reserve"), Some("successor"))
                || parts.next().is_some()
                || reservation
                    .and_then(|value| Uuid::parse_str(value).ok())
                    .is_none()
            {
                return Err(DaemonError::InvalidParam(
                    "agent successor admission dedup identity is malformed".into(),
                ));
            }
            invocation
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or_else(|| {
                    DaemonError::InvalidParam(
                        "agent successor admission invocation identity is malformed".into(),
                    )
                })?
        } else {
            Uuid::new_v4()
        };
    let guard = store.lock().await;
    let outcome = match channel {
        AdmissionStoreChannel::Generic => {
            match guard.admit_model_invocation_with_launch_origin(
                invocation_id,
                *registry,
                model_tier,
                &request,
                launch_origin,
            )? {
                crate::store::StoreAdmissionOutcome::Admitted(id) => {
                    StoreChannelAdmissionOutcome::Admitted(id)
                }
                crate::store::StoreAdmissionOutcome::Duplicate(id) => {
                    StoreChannelAdmissionOutcome::Duplicate(id)
                }
                crate::store::StoreAdmissionOutcome::Denied {
                    invocation_id,
                    inserted,
                    reason,
                } => StoreChannelAdmissionOutcome::Denied {
                    invocation_id,
                    inserted,
                    reason,
                },
            }
        }
        AdmissionStoreChannel::ScheduledCapacity => {
            match guard.admit_scheduled_capacity_model_invocation(
                invocation_id,
                *registry,
                model_tier,
                &request,
            )? {
                crate::store::CapacityStoreAdmissionOutcome::Admitted(id) => {
                    StoreChannelAdmissionOutcome::Admitted(id)
                }
                crate::store::CapacityStoreAdmissionOutcome::Duplicate(receipt) => {
                    StoreChannelAdmissionOutcome::CapacityDuplicate(receipt)
                }
                crate::store::CapacityStoreAdmissionOutcome::Denied {
                    invocation_id,
                    inserted,
                    reason,
                } => StoreChannelAdmissionOutcome::Denied {
                    invocation_id,
                    inserted,
                    reason,
                },
            }
        }
    };
    Ok(match outcome {
        StoreChannelAdmissionOutcome::Admitted(admission_id) => {
            event_bus.publish(DaemonEvent::ModelInvocationAdmitted {
                invocation_id: admission_id,
                purpose: request.purpose,
                session_id: request.owner.session_id,
            });
            publish_budget_alerts(&guard, event_bus, admission_id);
            ProcessedAdmission::Admitted(AdmissionPermit {
                invocation_id: admission_id,
                purpose: request.purpose,
                model_tier,
                execution_claimed: Arc::new(AtomicBool::new(false)),
            })
        }
        StoreChannelAdmissionOutcome::Duplicate(invocation_id) => {
            ProcessedAdmission::Duplicate(invocation_id)
        }
        StoreChannelAdmissionOutcome::CapacityDuplicate(receipt) => {
            ProcessedAdmission::CapacityDuplicate(receipt)
        }
        StoreChannelAdmissionOutcome::Denied {
            invocation_id,
            inserted,
            reason,
        } => {
            if inserted {
                event_bus.publish(DaemonEvent::ModelInvocationDenied {
                    invocation_id,
                    purpose: request.purpose,
                    reason: reason.clone(),
                    session_id: request.owner.session_id,
                });
            }
            return Err(DaemonError::PolicyDenied(reason));
        }
    })
}

pub async fn admit_invocation(
    store: &Arc<Mutex<Store>>,
    request: ModelAdmissionRequest,
    event_bus: &Arc<EventBus>,
) -> Result<AdmissionDecision> {
    admit_launch_invocation(store, request, event_bus, None).await
}

/// Internal launch context only; no RPC/agent DTO can supply this witness.
pub(crate) async fn admit_launch_invocation(
    store: &Arc<Mutex<Store>>,
    request: ModelAdmissionRequest,
    event_bus: &Arc<EventBus>,
    launch_origin: Option<&crate::store::manager_resources::ManagerResourceLaunchOrigin>,
) -> Result<AdmissionDecision> {
    match admit_invocation_through_store_channel(
        store,
        request,
        event_bus,
        AdmissionStoreChannel::Generic,
        launch_origin,
    )
    .await?
    {
        ProcessedAdmission::Admitted(permit) => Ok(AdmissionDecision::Admitted(permit)),
        ProcessedAdmission::Duplicate(invocation_id) => {
            Ok(AdmissionDecision::Duplicate { invocation_id })
        }
        ProcessedAdmission::CapacityDuplicate(_) => Err(DaemonError::Store(
            "generic_admission_returned_capacity_duplicate".into(),
        )),
    }
}

pub(crate) async fn admit_capacity_invocation(
    store: &Arc<Mutex<Store>>,
    request: ModelAdmissionRequest,
    event_bus: &Arc<EventBus>,
) -> Result<CapacityAdmissionDecision> {
    match admit_invocation_through_store_channel(
        store,
        request,
        event_bus,
        AdmissionStoreChannel::ScheduledCapacity,
        None,
    )
    .await?
    {
        ProcessedAdmission::Admitted(permit) => Ok(CapacityAdmissionDecision::Admitted(permit)),
        ProcessedAdmission::CapacityDuplicate(receipt) => {
            Ok(CapacityAdmissionDecision::CapacityDuplicate {
                invocation_id: receipt.invocation_id,
                phase: receipt.phase,
            })
        }
        ProcessedAdmission::Duplicate(_) => Err(DaemonError::Store(
            "capacity_admission_duplicate_without_receipt".into(),
        )),
    }
}

/// Reconstitute the one-use execution capability for a reserved Closure
/// source launch that crashed after model admission but before its direct
/// session/custody transaction committed.
///
/// This is deliberately narrower than generic duplicate admission recovery.
/// Direct launches persist the session before any provider boundary, so the
/// absence of that exact reserved session is durable proof that this admitted
/// invocation never acquired provider execution. Once the session exists,
/// replay adopts that row instead and this function refuses to mint authority.
pub(crate) async fn resume_unexecuted_closure_launch_admission(
    store: &Arc<Mutex<Store>>,
    request: &ModelAdmissionRequest,
    invocation_id: Uuid,
) -> Result<AdmissionPermit> {
    if request.purpose != ModelInvocationPurpose::SessionLaunchFresh {
        return Err(DaemonError::PolicyDenied(
            "only a reserved Closure fresh launch can resume model admission".into(),
        ));
    }
    let session_id = request.owner.session_id.ok_or_else(|| {
        DaemonError::PolicyDenied("Closure launch admission has no reserved session owner".into())
    })?;
    let guard = store.lock().await;
    if guard.get_session(session_id)?.is_some() {
        return Err(DaemonError::PolicyDenied(
            "Closure launch admission cannot resume after its session became durable".into(),
        ));
    }
    let record = guard
        .load_model_invocation_record(invocation_id)?
        .ok_or_else(|| DaemonError::Store("duplicate Closure invocation disappeared".into()))?;
    if record.admission_status != AdmissionStatus::Admitted
        || record.status != ModelInvocationStatus::Running
        || record.purpose != request.purpose
        || record.owner != request.owner
        || record.dedup_key != request.dedup_key
        || record.request_fingerprint != request.request_fingerprint
        || record.provider != request.provider
        || record.model != request.model
        || record.backend != request.backend
        || record.effort != request.effort
    {
        return Err(DaemonError::PolicyDenied(
            "duplicate Closure launch admission does not match its reserved request".into(),
        ));
    }
    let model_tier = record.model_tier.ok_or_else(|| {
        DaemonError::Store("admitted Closure invocation omitted its model tier".into())
    })?;
    Ok(AdmissionPermit {
        invocation_id,
        purpose: request.purpose,
        model_tier,
        execution_claimed: Arc::new(AtomicBool::new(false)),
    })
}

/// Recover the stable admission for a durable successor reservation after a
/// daemon restart. Successor launch commits the exact candidate row and its
/// invocation binding before token mint or provider dispatch, so a missing
/// candidate row is durable proof that this invocation never crossed the
/// provider-effect boundary. Once the row exists, replay adopts or terminally
/// settles that incarnation and this function refuses to mint authority.
pub(crate) async fn resume_unexecuted_agent_successor_admission(
    store: &Arc<Mutex<Store>>,
    request: &ModelAdmissionRequest,
    invocation_id: Uuid,
) -> Result<AdmissionPermit> {
    if request.purpose != ModelInvocationPurpose::AgentReserveSuccessor {
        return Err(DaemonError::PolicyDenied(
            "only an agent successor can resume successor admission".into(),
        ));
    }
    let session_id = request.owner.session_id.ok_or_else(|| {
        DaemonError::PolicyDenied("successor admission has no candidate owner".into())
    })?;
    let guard = store.lock().await;
    if guard.get_session(session_id)?.is_some() {
        return Err(DaemonError::PolicyDenied(
            "successor admission cannot resume after its candidate became durable".into(),
        ));
    }
    let record = guard
        .load_model_invocation_record(invocation_id)?
        .ok_or_else(|| DaemonError::Store("duplicate successor invocation disappeared".into()))?;
    if record.admission_status != AdmissionStatus::Admitted
        || record.status != ModelInvocationStatus::Running
        || record.purpose != request.purpose
        || record.owner != request.owner
        || record.dedup_key != request.dedup_key
        || record.request_fingerprint != request.request_fingerprint
        || record.provider != request.provider
        || record.model != request.model
        || record.backend != request.backend
        || record.effort != request.effort
    {
        return Err(DaemonError::PolicyDenied(
            "duplicate successor admission does not match its frozen request".into(),
        ));
    }
    let model_tier = record.model_tier.ok_or_else(|| {
        DaemonError::Store("admitted successor invocation omitted its model tier".into())
    })?;
    Ok(AdmissionPermit {
        invocation_id,
        purpose: request.purpose,
        model_tier,
        execution_claimed: Arc::new(AtomicBool::new(false)),
    })
}

pub(crate) async fn resume_unexecuted_capacity_delivery_admission(
    store: &Arc<Mutex<Store>>,
    request: &ModelAdmissionRequest,
    invocation_id: Uuid,
) -> Result<AdmissionPermit> {
    if request.purpose != ModelInvocationPurpose::SessionContinueResume {
        return Err(DaemonError::PolicyDenied(
            "only a capacity scheduled Resume can recover admitted execution authority".into(),
        ));
    }
    let guard = store.lock().await;
    guard.validate_unexecuted_capacity_delivery_admission(request, invocation_id)?;
    let record = guard
        .load_model_invocation_record(invocation_id)?
        .ok_or_else(|| DaemonError::Store("duplicate capacity invocation disappeared".into()))?;
    let mismatches = [
        (
            record.admission_status != AdmissionStatus::Admitted,
            "admission_status",
        ),
        (record.status != ModelInvocationStatus::Running, "status"),
        (record.purpose != request.purpose, "purpose"),
        (record.owner != request.owner, "owner"),
        (record.dedup_key != request.dedup_key, "dedup_key"),
        (
            record.request_fingerprint != request.request_fingerprint,
            "request_fingerprint",
        ),
        (record.provider != request.provider, "provider"),
        (record.model != request.model, "model"),
        (record.backend != request.backend, "backend"),
        (record.effort != request.effort, "effort"),
    ]
    .into_iter()
    .filter_map(|(mismatched, field)| mismatched.then_some(field))
    .collect::<Vec<_>>();
    if !mismatches.is_empty() {
        return Err(DaemonError::PolicyDenied(format!(
            "duplicate capacity delivery admission does not match its reserved request: {}",
            mismatches.join(",")
        )));
    }
    let model_tier = record.model_tier.ok_or_else(|| {
        DaemonError::Store("admitted capacity invocation omitted its model tier".into())
    })?;
    Ok(AdmissionPermit {
        invocation_id,
        purpose: request.purpose,
        model_tier,
        execution_claimed: Arc::new(AtomicBool::new(false)),
    })
}

pub async fn deny_invocation(
    store: &Arc<Mutex<Store>>,
    request: &ModelAdmissionRequest,
    reason: &str,
    event_bus: &Arc<EventBus>,
) -> Result<()> {
    let registry = registry::lookup(request.purpose).ok_or_else(|| {
        DaemonError::InvalidParam(format!(
            "unregistered model invocation purpose: {}",
            request.purpose
        ))
    })?;
    let model_tier = classify_model_tier(
        request.provider.as_deref(),
        request.backend.as_deref(),
        request.model.as_deref(),
    );
    let policy_snapshot = json!({
        "reason": reason,
    });
    let invocation_id = Uuid::new_v4();
    let guard = store.lock().await;
    guard.insert_model_invocation_denied(
        invocation_id,
        registry.kind,
        registry.foreground,
        registry.paid_risk,
        model_tier,
        request,
        &policy_snapshot,
        reason,
    )?;
    event_bus.publish(DaemonEvent::ModelInvocationDenied {
        invocation_id,
        purpose: request.purpose,
        reason: reason.to_string(),
        session_id: request.owner.session_id,
    });
    Ok(())
}

pub async fn complete_invocation(
    store: &Arc<Mutex<Store>>,
    permit: &AdmissionPermit,
    completion: InvocationCompletion,
    event_bus: &Arc<EventBus>,
) -> Result<()> {
    complete_invocation_by_id(store, permit.invocation_id, completion, event_bus).await
}

pub async fn complete_invocation_by_id(
    store: &Arc<Mutex<Store>>,
    invocation_id: Uuid,
    completion: InvocationCompletion,
    event_bus: &Arc<EventBus>,
) -> Result<()> {
    let guard = store.lock().await;
    complete_invocation_locked(&guard, invocation_id, &completion, event_bus)
}

fn complete_invocation_locked(
    store: &Store,
    invocation_id: Uuid,
    completion: &InvocationCompletion,
    event_bus: &Arc<EventBus>,
) -> Result<()> {
    match store.complete_model_invocation(invocation_id, completion)? {
        crate::store::StoreCompletionOutcome::Missing
        | crate::store::StoreCompletionOutcome::NoChange => {}
        crate::store::StoreCompletionOutcome::Transitioned {
            record,
            circuit_transition,
        } => {
            publish_budget_alerts(store, event_bus, invocation_id);
            if let Some(circuit) = circuit_transition {
                publish_circuit_transition(
                    event_bus,
                    &circuit,
                    store
                        .current_model_control_mode()
                        .unwrap_or(ModelControlMode::Normal),
                );
            }
            match record.status {
                rsi_common::model_control::ModelInvocationStatus::Cancelled => {
                    event_bus.publish(DaemonEvent::ModelInvocationCancelled {
                        invocation_id,
                        session_id: record.owner.session_id,
                        reason: record
                            .cancellation_reason
                            .clone()
                            .unwrap_or_else(|| "cancelled".to_string()),
                    });
                }
                _ => {
                    event_bus.publish(DaemonEvent::ModelInvocationCompleted {
                        invocation_id,
                        error_class: completion.error_class.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

pub async fn request_invocation_cancellation(
    store: &Arc<Mutex<Store>>,
    runtime: &ModelControlRuntime,
    event_bus: &Arc<EventBus>,
    invocation_id: Uuid,
    reason: &str,
    mechanism: &str,
) -> Result<crate::store::StoreCancellationOutcome> {
    let guard = store.lock().await;
    let outcome = guard.request_model_invocation_cancellation(invocation_id, reason, mechanism)?;
    if let crate::store::StoreCancellationOutcome::Requested(record) = &outcome {
        event_bus.publish(DaemonEvent::ModelInvocationCancellationRequested {
            invocation_id,
            session_id: record.owner.session_id,
            reason: record
                .cancellation_reason
                .clone()
                .unwrap_or_else(|| reason.to_string()),
            mechanism: record
                .cancellation_mechanism
                .clone()
                .unwrap_or_else(|| mechanism.to_string()),
        });
        runtime.publish_signal(ModelControlSignal {
            mode: guard
                .current_model_control_mode()
                .unwrap_or(ModelControlMode::Normal),
            updated_at: None,
            circuits: guard.list_model_circuits().unwrap_or_default(),
            fault: runtime.snapshot().fault,
            change: ModelControlSignalChange::CancellationRequested { invocation_id },
        });
    }
    Ok(outcome)
}

pub async fn apply_policy_transition(
    store: &Arc<Mutex<Store>>,
    runtime: &ModelControlRuntime,
    event_bus: &Arc<EventBus>,
    mode: ModelControlMode,
    replace_policies: bool,
    policies: &[rsi_common::model_control::ModelBudgetPolicy],
    circuit_updates: &[rsi_common::rpc::ModelCircuitUpdate],
) -> Result<crate::store::ModelControlPolicyTransition> {
    let guard = store.lock().await;
    let transition = guard.apply_model_control_policy_transition(
        mode,
        replace_policies,
        policies,
        circuit_updates,
    )?;
    if transition.mode_changed {
        event_bus.publish(DaemonEvent::ModelControlModeChanged {
            previous_mode: transition.previous_mode,
            current_mode: transition.current_mode,
            updated_at: transition.updated_at.clone(),
        });
        runtime.publish_signal(ModelControlSignal {
            mode: transition.current_mode,
            updated_at: Some(transition.updated_at.clone()),
            circuits: transition.circuits.clone(),
            fault: None,
            change: ModelControlSignalChange::ModeChanged {
                previous_mode: transition.previous_mode,
                current_mode: transition.current_mode,
            },
        });
    }
    for circuit in &transition.circuit_transitions {
        publish_circuit_transition(event_bus, circuit, transition.current_mode);
        runtime.publish_signal(ModelControlSignal {
            mode: transition.current_mode,
            updated_at: Some(transition.updated_at.clone()),
            circuits: transition.circuits.clone(),
            fault: None,
            change: ModelControlSignalChange::CircuitChanged {
                scope_kind: circuit.scope_kind,
                scope_id: circuit.scope_id.clone(),
                state: circuit.state.clone(),
            },
        });
    }
    Ok(transition)
}

pub async fn settle_result<T>(
    store: &Arc<Mutex<Store>>,
    permit: &AdmissionPermit,
    completion: InvocationCompletion,
    result: Result<T>,
    context: &str,
    event_bus: &Arc<EventBus>,
) -> Result<T> {
    match (
        result,
        complete_invocation(store, permit, completion, event_bus).await,
    ) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(settle_error)) => Err(DaemonError::Store(format!(
            "{context} completed but failed to settle model invocation {}: {settle_error}",
            permit.invocation_id()
        ))),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(settle_error)) => Err(DaemonError::Store(format!(
            "{context} failed with {error}; additionally failed to settle model invocation {}: {settle_error}",
            permit.invocation_id()
        ))),
    }
}

pub fn hash_request_fingerprint(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn publish_budget_alerts(store: &Store, event_bus: &Arc<EventBus>, invocation_id: Uuid) {
    match store.record_budget_alert_crossings(invocation_id) {
        Ok(alerts) => {
            for alert in alerts {
                event_bus.publish(DaemonEvent::ModelBudgetNearLimit {
                    invocation_id: alert.invocation_id,
                    metric: alert.metric,
                    remaining: alert.remaining,
                    limit: alert.limit,
                    scope_kind: alert.scope_kind,
                    scope_id: alert.scope_id,
                });
            }
        }
        Err(error) => {
            tracing::warn!(
                invocation_id = %invocation_id,
                error = %error,
                "Failed to publish model budget alerts"
            );
        }
    }
}

fn publish_circuit_transition(
    event_bus: &Arc<EventBus>,
    circuit: &ModelCircuitStatus,
    mode: ModelControlMode,
) {
    event_bus.publish(DaemonEvent::ModelControlCircuitChanged {
        mode,
        scope_kind: circuit.scope_kind,
        scope_id: circuit.scope_id.clone(),
        state: circuit.state.clone(),
        reason: circuit.reason.clone(),
        error_class: circuit.error_class.clone(),
        source: circuit.source.clone(),
        updated_at: circuit.updated_at.clone(),
    });
}

pub fn stable_dedup_key(scope: &str, parts: &[&str]) -> String {
    format!("{scope}:{}", hash_request_fingerprint(parts))
}

pub(crate) fn classify_model_tier(
    provider: Option<&str>,
    backend: Option<&str>,
    model: Option<&str>,
) -> ModelTier {
    let provider = provider.unwrap_or_default().to_ascii_lowercase();
    let backend = backend.unwrap_or_default().to_ascii_lowercase();
    let model = model.unwrap_or_default().to_ascii_lowercase();
    if provider == "local" || backend == "local" || backend == "ollama" {
        return ModelTier::Local;
    }
    if provider.is_empty()
        && backend.is_empty()
        && (model.starts_with("qwen") || model.starts_with("llama"))
    {
        return ModelTier::Local;
    }
    if model.starts_with("gpt-5")
        || model.starts_with("claude-opus")
        || model.starts_with("claude-sonnet")
        || model.starts_with("gemini-")
    {
        return ModelTier::Premium;
    }
    ModelTier::Standard
}

/// Builds the documented conservative reservation for a known registered purpose.
/// Callers must attach this estimate to their admission request explicitly.
pub fn explicit_expected_usage(
    purpose: ModelInvocationPurpose,
    provider: Option<&str>,
    backend: Option<&str>,
    model: Option<&str>,
) -> ExpectedUsage {
    default_usage_estimate(
        *registry::entry(purpose),
        classify_model_tier(provider, backend, model),
    )
}

fn default_usage_estimate(
    registry: registry::RegistryEntry,
    model_tier: ModelTier,
) -> ExpectedUsage {
    let (input_tokens, output_tokens, cache_creation_tokens, reasoning_tokens, wall_time_ms) =
        match (registry.foreground, model_tier) {
            (rsi_common::model_control::InvocationForeground::Background, ModelTier::Premium) => {
                (18_000, 4_000, 6_000, 1_500, 480_000)
            }
            (rsi_common::model_control::InvocationForeground::Background, ModelTier::Standard) => {
                (12_000, 3_000, 3_000, 1_000, 360_000)
            }
            (rsi_common::model_control::InvocationForeground::Background, ModelTier::Local) => {
                (8_000, 2_000, 1_000, 0, 240_000)
            }
            (rsi_common::model_control::InvocationForeground::Foreground, ModelTier::Premium) => {
                (24_000, 6_000, 8_000, 2_000, 600_000)
            }
            (rsi_common::model_control::InvocationForeground::Foreground, ModelTier::Standard) => {
                (16_000, 4_000, 4_000, 1_000, 420_000)
            }
            (rsi_common::model_control::InvocationForeground::Foreground, ModelTier::Local) => {
                (10_000, 3_000, 1_000, 0, 300_000)
            }
        };
    let embedding_input_count =
        if registry.kind == rsi_common::model_control::ModelInvocationKind::Embedding {
            256
        } else {
            0
        };
    let cache_read_tokens = if model_tier == ModelTier::Local
        || registry.kind == rsi_common::model_control::ModelInvocationKind::Embedding
        || registry.kind == rsi_common::model_control::ModelInvocationKind::Discovery
    {
        0
    } else {
        (input_tokens / 4).max(512)
    };
    ExpectedUsage {
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        reasoning_tokens,
        embedding_input_count,
        wall_time_ms,
    }
}

pub fn purpose_request(
    purpose: ModelInvocationPurpose,
    trigger: impl Into<String>,
    owner: InvocationOwner,
) -> ModelAdmissionRequest {
    ModelAdmissionRequest {
        purpose,
        provider: None,
        model: None,
        backend: None,
        effort: None,
        trigger: trigger.into(),
        owner,
        dedup_key: None,
        request_fingerprint: None,
        parent_invocation_id: None,
        retry_of_invocation_id: None,
        expected_usage: Some(explicit_expected_usage(purpose, None, None, None)),
        baseline_input_tokens: 0,
        baseline_output_tokens: 0,
        baseline_cache_creation_tokens: 0,
        baseline_cache_read_tokens: 0,
        baseline_reasoning_tokens: 0,
        baseline_embedding_input_count: 0,
        baseline_wall_time_ms: 0,
    }
}

pub fn lookup_existing_invocation_by_dedup(
    store: &Store,
    dedup_key: &str,
) -> Result<Option<ExistingInvocation>> {
    store
        .conn
        .query_row(
            "SELECT id, admission_status, status, request_fingerprint
             FROM model_invocations
             WHERE dedup_key = ?1",
            rusqlite::params![dedup_key],
            |row| {
                let id: String = row.get(0)?;
                let status: String = row.get(1)?;
                let lifecycle_status: String = row.get(2)?;
                let request_fingerprint: Option<String> = row.get(3)?;
                Ok((id, status, lifecycle_status, request_fingerprint))
            },
        )
        .optional()
        .map_err(Into::into)
        .and_then(|row| {
            row.map(|(id, status, lifecycle_status, request_fingerprint)| {
                let parsed = Uuid::parse_str(&id).map_err(|e| {
                    DaemonError::Store(format!("invalid model_invocations.id UUID: {e}"))
                })?;
                let status = match status.as_str() {
                    "admitted" => AdmissionStatus::Admitted,
                    "denied" => AdmissionStatus::Denied,
                    "duplicate" => AdmissionStatus::Duplicate,
                    other => {
                        return Err(DaemonError::Store(format!(
                            "invalid admission_status value in model_invocations: {other}"
                        )));
                    }
                };
                Ok(ExistingInvocation {
                    invocation_id: parsed,
                    admission_status: status,
                    status: lifecycle_status,
                    request_fingerprint,
                })
            })
            .transpose()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{DaemonEvent, EventBus};
    use crate::store::Store;
    use rsi_common::model_control::ModelInvocationPurpose;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::join;
    use tokio::net::TcpListener;
    use tokio::sync::broadcast::error::TryRecvError;
    use tokio::time::{Duration, timeout};

    fn session_launch_request(dedup_key: &str, session_id: Uuid) -> ModelAdmissionRequest {
        ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::SessionLaunchFresh,
            provider: Some("Codex".to_string()),
            model: Some("gpt-5.4".to_string()),
            backend: Some("Codex".to_string()),
            effort: Some("high".to_string()),
            trigger: "test".to_string(),
            owner: InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            dedup_key: Some(dedup_key.to_string()),
            request_fingerprint: Some(format!("fp:{dedup_key}")),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(explicit_expected_usage(
                ModelInvocationPurpose::SessionLaunchFresh,
                Some("Codex"),
                Some("Codex"),
                Some("gpt-5.4"),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        }
    }

    #[tokio::test]
    async fn reserved_closure_launch_resumes_only_pre_session_exact_admission() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let events = Arc::new(EventBus::new(8));
        let session_id = Uuid::new_v4();
        let request = session_launch_request("closure.source:reserved", session_id);
        let invocation_id = match admit_invocation(&store, request.clone(), &events)
            .await
            .expect("initial admission")
        {
            AdmissionDecision::Admitted(permit) => permit.invocation_id(),
            AdmissionDecision::Duplicate { .. } => panic!("first admission must be new"),
        };
        assert!(matches!(
            admit_invocation(&store, request.clone(), &events)
                .await
                .expect("duplicate admission"),
            AdmissionDecision::Duplicate { invocation_id: duplicate }
                if duplicate == invocation_id
        ));
        let resumed = resume_unexecuted_closure_launch_admission(&store, &request, invocation_id)
            .await
            .expect("pre-session crash recovery permit");
        assert_eq!(resumed.invocation_id(), invocation_id);

        let mut changed = request;
        changed.request_fingerprint = Some("sha256:changed".into());
        assert!(
            resume_unexecuted_closure_launch_admission(&store, &changed, invocation_id)
                .await
                .is_err(),
            "changed payload cannot reclaim the reserved invocation"
        );
    }

    #[test]
    fn runtime_loads_persisted_stop_all_before_services_start() {
        let store = Store::open_in_memory().expect("store");
        store
            .set_model_control_mode(ModelControlMode::StopAll)
            .expect("persist stop-all");
        let runtime = ModelControlRuntime::from_store_fail_closed(&store);
        let signal = runtime.snapshot();
        assert_eq!(signal.mode, ModelControlMode::StopAll);
        assert!(signal.fault.is_none());
    }

    #[test]
    fn runtime_fails_closed_when_durable_mode_is_corrupt() {
        let store = Store::open_in_memory().expect("store");
        store
            .set_daemon_setting("model_control_mode", "not-a-mode")
            .expect("write corrupt test setting");
        let runtime = ModelControlRuntime::from_store_fail_closed(&store);
        let signal = runtime.snapshot();
        assert_eq!(signal.mode, ModelControlMode::StopAll);
        assert!(
            signal
                .fault
                .as_deref()
                .is_some_and(|fault| fault.contains("not-a-mode"))
        );
    }

    #[tokio::test]
    async fn catalog_only_purpose_cannot_acquire_an_admission_permit() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let mut request = session_launch_request("catalog-only", Uuid::new_v4());
        request.purpose = ModelInvocationPurpose::ModelDiscoveryClaudeProbe;

        let error = admit_invocation(&store, request, &bus)
            .await
            .expect_err("catalog-only purpose must not acquire a permit");
        assert!(matches!(error, DaemonError::InvalidParam(_)));
        assert!(error.to_string().contains("non-invocation"));
    }

    #[tokio::test]
    async fn missing_expected_usage_is_denied_before_admission() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let mut request = session_launch_request("missing-usage", Uuid::new_v4());
        request.expected_usage = None;

        let error = admit_invocation(&store, request, &bus)
            .await
            .expect_err("missing usage must deny admission");
        assert!(matches!(error, DaemonError::InvalidParam(_)));
        assert!(error.to_string().contains("missing expected_usage"));
    }

    #[test]
    fn paid_cache_capable_reservations_include_cache_read_capacity() {
        let usage = explicit_expected_usage(
            ModelInvocationPurpose::SessionLaunchFresh,
            Some("Codex"),
            Some("Codex"),
            Some("gpt-5.4"),
        );
        assert!(usage.cache_read_tokens > 0);
    }

    #[tokio::test]
    async fn paid_background_is_denied_by_default() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::DreamConsolidation,
            provider: Some("Claude".to_string()),
            model: Some("claude-opus-4-1".to_string()),
            backend: Some("Claude".to_string()),
            effort: None,
            trigger: "test".to_string(),
            owner: InvocationOwner::default(),
            dedup_key: None,
            request_fingerprint: None,
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(explicit_expected_usage(
                ModelInvocationPurpose::DreamConsolidation,
                Some("Claude"),
                Some("Claude"),
                Some("claude-opus-4-1"),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };

        let error = admit_invocation(&store, request, &bus)
            .await
            .expect_err("must deny");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));
    }

    #[tokio::test]
    async fn local_background_is_admitted_in_normal_mode() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::DreamConsolidation,
            provider: Some("Local".to_string()),
            model: Some("qwen2.5".to_string()),
            backend: Some("Local".to_string()),
            effort: None,
            trigger: "test".to_string(),
            owner: InvocationOwner::default(),
            dedup_key: Some("dream-local".to_string()),
            request_fingerprint: None,
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(explicit_expected_usage(
                ModelInvocationPurpose::DreamConsolidation,
                Some("Local"),
                Some("Local"),
                Some("qwen2.5"),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };

        let decision = admit_invocation(&store, request, &bus)
            .await
            .expect("must admit");
        let AdmissionDecision::Admitted(permit) = decision else {
            panic!("unexpected duplicate admission");
        };
        assert_eq!(permit.model_tier, ModelTier::Local);
    }

    #[test]
    fn request_fingerprint_is_hashed() {
        let fingerprint =
            hash_request_fingerprint(&["purpose", "Codex", "gpt-5.4", "plain prompt"]);
        assert!(fingerprint.starts_with("sha256:"));
        assert!(!fingerprint.contains("plain prompt"));
    }

    #[tokio::test]
    async fn remote_model_with_colon_is_not_treated_as_local() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::SessionLaunchFresh,
            provider: Some("Harness".to_string()),
            model: Some("openrouter:anthropic/claude-sonnet-4".to_string()),
            backend: Some("Harness".to_string()),
            effort: None,
            trigger: "test".to_string(),
            owner: InvocationOwner::default(),
            dedup_key: Some("remote-colon".to_string()),
            request_fingerprint: None,
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(explicit_expected_usage(
                ModelInvocationPurpose::SessionLaunchFresh,
                Some("Harness"),
                Some("Harness"),
                Some("openrouter:anthropic/claude-sonnet-4"),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };

        let decision = admit_invocation(&store, request, &bus)
            .await
            .expect("admission succeeds");
        let AdmissionDecision::Admitted(permit) = decision else {
            panic!("unexpected duplicate admission");
        };
        assert_ne!(permit.model_tier, ModelTier::Local);
    }

    #[tokio::test]
    async fn concurrent_child_admissions_respect_explicit_tree_bounds() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let root_request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::SessionLaunchFresh,
            provider: Some("Codex".to_string()),
            model: Some("gpt-5.4".to_string()),
            backend: Some("Codex".to_string()),
            effort: Some("high".to_string()),
            trigger: "test".to_string(),
            owner: InvocationOwner {
                session_id: Some(Uuid::new_v4()),
                ..Default::default()
            },
            dedup_key: Some("root".to_string()),
            request_fingerprint: Some("sha256:root".to_string()),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(explicit_expected_usage(
                ModelInvocationPurpose::SessionLaunchFresh,
                Some("Codex"),
                Some("Codex"),
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
        let root_id = match admit_invocation(&store, root_request, &bus)
            .await
            .expect("root admission")
        {
            AdmissionDecision::Admitted(permit) => permit.invocation_id(),
            AdmissionDecision::Duplicate { .. } => panic!("unexpected duplicate"),
        };

        // Budgets are unbounded by default now — place an explicit tree-scope
        // cap so the concurrent race below still has a bound to contend for.
        // This is what proves the reservation is atomic (exactly one of two
        // simultaneous admissions wins the last slot), not that a hardcoded
        // default exists.
        store
            .lock()
            .await
            .update_model_budget_policies(
                &[rsi_common::model_control::ModelBudgetPolicy {
                    scope_kind: rsi_common::model_control::BudgetScopeKind::Tree,
                    scope_id: Some(root_id.to_string()),
                    purpose: None,
                    model_tier: None,
                    effort: None,
                    ceiling_model_tier: None,
                    ceiling_effort: None,
                    max_calls: Some(4),
                    max_total_tokens: None,
                    max_input_tokens: None,
                    max_output_tokens: None,
                    max_cache_creation_tokens: None,
                    max_cache_read_tokens: None,
                    max_reasoning_tokens: None,
                    max_embedding_inputs: None,
                    max_wall_time_ms: None,
                    max_concurrency: None,
                    max_retries: None,
                    max_calls_per_window: None,
                    rate_window_seconds: None,
                    alert_threshold_ratio: None,
                }],
                false,
            )
            .expect("tree cap persisted");

        for idx in 0..2 {
            let child_request = ModelAdmissionRequest {
                purpose: ModelInvocationPurpose::AgentSpawnChild,
                provider: Some("Codex".to_string()),
                model: Some("gpt-5.4".to_string()),
                backend: Some("Codex".to_string()),
                effort: Some("high".to_string()),
                trigger: "test".to_string(),
                owner: InvocationOwner {
                    session_id: Some(Uuid::new_v4()),
                    ..Default::default()
                },
                dedup_key: Some(format!("child-seq-{idx}")),
                request_fingerprint: Some(format!("sha256:child-seq-{idx}")),
                parent_invocation_id: Some(root_id),
                retry_of_invocation_id: None,
                expected_usage: Some(explicit_expected_usage(
                    ModelInvocationPurpose::AgentSpawnChild,
                    Some("Codex"),
                    Some("Codex"),
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
            let child_id = match admit_invocation(&store, child_request, &bus)
                .await
                .expect("sequential child admission")
            {
                AdmissionDecision::Admitted(permit) => permit.invocation_id(),
                AdmissionDecision::Duplicate { .. } => panic!("unexpected duplicate"),
            };
            complete_invocation_by_id(&store, child_id, InvocationCompletion::default(), &bus)
                .await
                .expect("sequential child completion");
        }

        let request_a = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::AgentSpawnChild,
            provider: Some("Codex".to_string()),
            model: Some("gpt-5.4".to_string()),
            backend: Some("Codex".to_string()),
            effort: Some("high".to_string()),
            trigger: "test".to_string(),
            owner: InvocationOwner {
                session_id: Some(Uuid::new_v4()),
                ..Default::default()
            },
            dedup_key: Some("child-race-a".to_string()),
            request_fingerprint: Some("sha256:child-race-a".to_string()),
            parent_invocation_id: Some(root_id),
            retry_of_invocation_id: None,
            expected_usage: Some(explicit_expected_usage(
                ModelInvocationPurpose::AgentSpawnChild,
                Some("Codex"),
                Some("Codex"),
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
        let request_b = ModelAdmissionRequest {
            dedup_key: Some("child-race-b".to_string()),
            request_fingerprint: Some("sha256:child-race-b".to_string()),
            owner: InvocationOwner {
                session_id: Some(Uuid::new_v4()),
                ..Default::default()
            },
            ..request_a.clone()
        };

        let (result_a, result_b) = join!(
            admit_invocation(&store, request_a, &bus),
            admit_invocation(&store, request_b, &bus)
        );

        let outcomes = [result_a, result_b];
        let admitted = outcomes
            .iter()
            .filter(|result| matches!(result, Ok(AdmissionDecision::Admitted(_))))
            .count();
        let denied = outcomes
            .iter()
            .filter(|result| matches!(result, Err(DaemonError::PolicyDenied(_))))
            .count();
        assert_eq!(admitted, 1);
        assert_eq!(denied, 1);

        let guard = store.lock().await;
        let row_count: i64 = guard
            .conn
            .query_row("SELECT COUNT(*) FROM model_invocations", [], |row| {
                row.get(0)
            })
            .expect("row count");
        assert_eq!(row_count, 5);
        let denied_count: i64 = guard
            .conn
            .query_row(
                "SELECT COUNT(*) FROM model_invocations WHERE admission_status = 'denied'",
                [],
                |row| row.get(0),
            )
            .expect("denied rows");
        assert_eq!(denied_count, 1);
    }

    #[test]
    fn runtime_cancellation_invokes_registered_callback_once() {
        let runtime = ModelControlRuntime::default_normal();
        let invocation_id = Uuid::new_v4();
        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = Arc::clone(&fired);
        let _registration = runtime.register_cancellation(
            invocation_id,
            "fake_background",
            Arc::new(move || {
                fired_clone.store(true, Ordering::SeqCst);
            }),
        );

        let attempt = runtime.cancel_invocation(invocation_id);
        assert!(attempt.request_started);
        assert!(!attempt.already_requested);
        assert_eq!(attempt.mechanism, "fake_background");
        assert!(fired.load(Ordering::SeqCst));

        let second = runtime.cancel_invocation(invocation_id);
        assert!(!second.request_started);
        assert!(second.already_requested);
        assert_eq!(second.mechanism, "fake_background");
    }

    #[test]
    fn runtime_publish_signal_notifies_subscribers() {
        let runtime = ModelControlRuntime::default_normal();
        let rx = runtime.subscribe();
        runtime.publish_signal(ModelControlSignal {
            mode: ModelControlMode::StopAll,
            updated_at: Some("2026-07-15T00:00:00Z".to_string()),
            circuits: Vec::new(),
            fault: None,
            change: ModelControlSignalChange::Snapshot,
        });
        let changed = rx.has_changed().expect("watch state");
        assert!(changed);
        let signal = rx.borrow().clone();
        assert_eq!(signal.mode, ModelControlMode::StopAll);
        assert_eq!(signal.updated_at.as_deref(), Some("2026-07-15T00:00:00Z"));
    }

    #[tokio::test]
    async fn duplicate_admission_and_completion_emit_once() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let mut rx = bus.subscribe();
        let session_id = Uuid::new_v4();
        let request = session_launch_request("emit-once", session_id);

        let decision = admit_invocation(&store, request.clone(), &bus)
            .await
            .expect("first admission");
        let invocation_id = match decision {
            AdmissionDecision::Admitted(permit) => permit.invocation_id(),
            other => panic!("unexpected first decision: {other:?}"),
        };
        match rx.recv().await.expect("admitted event").as_ref() {
            DaemonEvent::ModelInvocationAdmitted {
                invocation_id: event_id,
                ..
            } => assert_eq!(*event_id, invocation_id),
            other => panic!("unexpected first event: {other:?}"),
        }

        let duplicate = admit_invocation(&store, request, &bus)
            .await
            .expect("duplicate admission");
        assert!(matches!(duplicate, AdmissionDecision::Duplicate { .. }));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

        complete_invocation_by_id(&store, invocation_id, InvocationCompletion::default(), &bus)
            .await
            .expect("first completion");
        match rx.recv().await.expect("completed event").as_ref() {
            DaemonEvent::ModelInvocationCompleted {
                invocation_id: event_id,
                ..
            } => assert_eq!(*event_id, invocation_id),
            other => panic!("unexpected completion event: {other:?}"),
        }

        complete_invocation_by_id(&store, invocation_id, InvocationCompletion::default(), &bus)
            .await
            .expect("idempotent completion");
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        bus.unsubscribe();
    }

    #[tokio::test]
    async fn cancellation_request_emits_bus_event_and_signal_once() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let runtime = ModelControlRuntime::default_normal();
        let mut rx = bus.subscribe();
        let mut signal_rx = runtime.subscribe();
        let session_id = Uuid::new_v4();
        let request = session_launch_request("cancel-once", session_id);
        let decision = admit_invocation(&store, request, &bus)
            .await
            .expect("admission");
        let invocation_id = match decision {
            AdmissionDecision::Admitted(permit) => permit.invocation_id(),
            other => panic!("unexpected decision: {other:?}"),
        };
        let _ = rx.recv().await.expect("drain admitted event");
        let _ = signal_rx.borrow_and_update();

        let first = tokio::spawn({
            let store = Arc::clone(&store);
            let bus = Arc::clone(&bus);
            let runtime = runtime.clone();
            async move {
                request_invocation_cancellation(
                    &store,
                    &runtime,
                    &bus,
                    invocation_id,
                    "operator_cancelled",
                    "operator_request",
                )
                .await
            }
        });
        let second = tokio::spawn({
            let store = Arc::clone(&store);
            let bus = Arc::clone(&bus);
            let runtime = runtime.clone();
            async move {
                request_invocation_cancellation(
                    &store,
                    &runtime,
                    &bus,
                    invocation_id,
                    "operator_cancelled",
                    "operator_request",
                )
                .await
            }
        });
        let (first, second) = join!(first, second);
        let outcomes = [
            first
                .expect("first cancellation task")
                .expect("first cancellation"),
            second
                .expect("second cancellation task")
                .expect("second cancellation"),
        ];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(
                    outcome,
                    crate::store::StoreCancellationOutcome::Requested(_)
                ))
                .count(),
            1,
            "exactly one concurrent cancellation request transitions the row"
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(
                    outcome,
                    crate::store::StoreCancellationOutcome::NoChange(_)
                ))
                .count(),
            1,
            "the duplicate concurrent cancellation request is idempotent"
        );

        match rx.recv().await.expect("cancellation event").as_ref() {
            DaemonEvent::ModelInvocationCancellationRequested {
                invocation_id: event_id,
                session_id: event_session_id,
                reason,
                mechanism,
            } => {
                assert_eq!(*event_id, invocation_id);
                assert_eq!(*event_session_id, Some(session_id));
                assert_eq!(reason, "operator_cancelled");
                assert_eq!(mechanism, "operator_request");
            }
            other => panic!("unexpected cancellation event: {other:?}"),
        }

        signal_rx.changed().await.expect("cancellation signal");
        match signal_rx.borrow_and_update().change {
            ModelControlSignalChange::CancellationRequested {
                invocation_id: event_id,
            } => {
                assert_eq!(event_id, invocation_id);
            }
            ref other => panic!("unexpected signal change: {other:?}"),
        }

        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(
            timeout(Duration::from_millis(50), signal_rx.changed())
                .await
                .is_err()
        );
        bus.unsubscribe();
    }

    #[tokio::test]
    async fn concurrent_cloned_permits_claim_exactly_one_cli_execution() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let permit = match admit_invocation(
            &store,
            session_launch_request("concurrent-cli-claim", Uuid::new_v4()),
            &bus,
        )
        .await
        .expect("admission")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                panic!("unexpected duplicate admission: {invocation_id}")
            }
        };
        let left = permit.clone();
        let right = permit.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let (left_claimed, right_claimed) = std::thread::scope(|scope| {
            let left_barrier = Arc::clone(&barrier);
            let right_barrier = Arc::clone(&barrier);
            let left_task = scope.spawn(move || {
                left_barrier.wait();
                left.claim_cli_execution(registry::RuntimeExecutionRoute::ClaudeCli)
                    .is_ok()
            });
            let right_task = scope.spawn(move || {
                right_barrier.wait();
                right
                    .claim_cli_execution(registry::RuntimeExecutionRoute::ClaudeCli)
                    .is_ok()
            });
            (
                left_task.join().expect("left claim thread"),
                right_task.join().expect("right claim thread"),
            )
        });

        assert_eq!(
            left_claimed as usize + right_claimed as usize,
            1,
            "cloned permits share one atomic CLI execution claim"
        );
    }

    #[tokio::test]
    async fn one_admission_cannot_cross_or_repeat_execution_boundaries() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let mut request = purpose_request(
            ModelInvocationPurpose::DialecticQuery,
            "cross-boundary-claim",
            InvocationOwner {
                session_id: Some(Uuid::new_v4()),
                ..Default::default()
            },
        );
        request.provider = Some("OpenAI".to_string());
        request.backend = Some("openai_api".to_string());
        request.model = Some("gpt-5.4".to_string());
        request.dedup_key = Some("cross-boundary-claim".to_string());
        request.request_fingerprint = Some("fp:cross-boundary-claim".to_string());
        let permit = match admit_invocation(&store, request, &bus)
            .await
            .expect("admission")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                panic!("unexpected duplicate admission: {invocation_id}")
            }
        };
        let cloned_for_cli = permit.clone();
        let cloned_for_app_server = permit.clone();

        let http = permit
            .claim_model_execution(registry::RuntimeExecutionRoute::DialecticHttp)
            .expect("first model execution claim");
        assert_eq!(http.invocation_id, permit.invocation_id());
        assert_eq!(http.purpose, permit.purpose);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind deterministic fake HTTP transport");
        let address = listener.local_addr().expect("fake transport address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("one HTTP connection");
            let mut request = [0_u8; 4096];
            let bytes = socket.read(&mut request).await.expect("HTTP request");
            assert!(
                std::str::from_utf8(&request[..bytes])
                    .expect("UTF-8 request")
                    .starts_with("POST /model HTTP/1.1")
            );
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")
                .await
                .expect("HTTP response");
            1_usize
        });
        let response = http
            .bind_http(
                registry::RuntimeExecutionRoute::DialecticHttp,
                reqwest::Client::new().post(format!("http://{address}/model")),
            )
            .send("fake")
            .await
            .expect("admitted HTTP send");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(server.await.expect("fake HTTP transport"), 1);

        let cli_error = cloned_for_cli
            .claim_cli_execution(registry::RuntimeExecutionRoute::ClaudeCli)
            .expect_err("same admission cannot authorize a CLI fallback");
        assert!(matches!(cli_error, DaemonError::PolicyDenied(_)));

        let app_server_error = cloned_for_app_server
            .claim_model_execution(registry::RuntimeExecutionRoute::CodexAppServer)
            .expect_err("same admission cannot authorize an AppServer recovery");
        assert!(matches!(app_server_error, DaemonError::PolicyDenied(_)));
    }

    #[tokio::test]
    async fn purpose_rejects_an_unregistered_runtime_route_before_consumption() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let permit = match admit_invocation(
            &store,
            session_launch_request("wrong-purpose-route", Uuid::new_v4()),
            &bus,
        )
        .await
        .expect("admission")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                panic!("unexpected duplicate admission: {invocation_id}")
            }
        };

        let error = permit
            .claim_model_execution(registry::RuntimeExecutionRoute::DialecticHttp)
            .expect_err("session launch cannot authorize a Dialectic route");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));

        permit
            .claim_cli_execution(registry::RuntimeExecutionRoute::ClaudeCli)
            .expect("wrong-route rejection must not consume the one-use capability");
    }

    #[tokio::test]
    async fn claimed_cli_route_cannot_cross_to_another_registered_cli_sink() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let permit = match admit_invocation(
            &store,
            session_launch_request("exact-cli-route", Uuid::new_v4()),
            &bus,
        )
        .await
        .expect("admission")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                panic!("unexpected duplicate admission: {invocation_id}")
            }
        };
        let execution = permit
            .claim_cli_execution(registry::RuntimeExecutionRoute::ClaudeCli)
            .expect("Claude route");
        let command = tokio::process::Command::new("must-not-be-spawned");

        let error = execution
            .bind_command(registry::RuntimeExecutionRoute::CodexCli, command)
            .spawn()
            .expect_err("Claude authority cannot cross to the Codex sink");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn admitted_cli_spawn_centrally_stamps_invocation_and_daemon_namespace() {
        let route = registry::RuntimeExecutionRoute::ClaudeCli;
        let execution = CliExecutionCapability::for_test(route);
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("printf '%s\\n%s' \"$RSI_MODEL_INVOCATION_ID\" \"$RSI_PROCESS_OWNERSHIP_NAMESPACE\"")
            .stdout(std::process::Stdio::piped());

        let output = execution
            .bind_command(route, command)
            .spawn()
            .expect("spawn admitted test command")
            .wait_with_output()
            .await
            .expect("collect admitted command environment");

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).expect("UTF-8 command output"),
            format!(
                "{}\n{}",
                Uuid::nil(),
                rsi_common::identity::process_ownership_namespace()
            )
        );
    }
}
