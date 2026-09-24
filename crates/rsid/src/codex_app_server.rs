//! Codex app-server integration for bidirectional JSON-RPC communication.
//!
//! Implements `CodexAppServerSession` which wraps a `codex app-server` subprocess
//! and communicates via bidirectional stdio JSON-RPC 2.0.

use crate::app_server_control::{
    AppServerControlPlane, IngressFrameClass, IngressOverflowAction, ProviderLifecycleKind,
    QuarantineSealReason, classify_ingress_frame,
};
use crate::claude::{LaunchConfig, StreamEvent};
use crate::error::{DaemonError, Result};
use crate::model_control::call_control::{
    ModelCallControl, ModelCallKind, ModelCallSettlement, ModelCallUsage,
};
use crate::model_control::{
    AdmissionPermit, AppServerDispatchOutcome, InvocationCompletion, ModelExecutionCapability,
    complete_invocation,
};
use crate::process_control::{
    BoundedLineError, BoundedLines, PROVIDER_MAX_LINE_BYTES, PROVIDER_MAX_STDERR_BYTES,
    ProcessContainment, configure_tokio_process_group,
};
use crate::provider::{
    AdmittedMessageTurnOutcome, ApprovalDecision, NativeMessageTurnRequest, ProviderSession,
    TurnConfig, TurnId,
};
use crate::store::Store;
use rsi_common::model_control::ModelUsageConfidence;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc};

/// Supported approval methods from the installed codex-cli 0.153.4 schemas.
/// Permissions, user input, elicitation and legacy approvals have different
/// response contracts and must stay unresolved until separately implemented.
pub(crate) const APPROVAL_METHODS: &[&str] = &[
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
];

fn is_operator_request(method: &str) -> bool {
    method.ends_with("/requestApproval")
        || matches!(
            method,
            "execCommandApproval"
                | "applyPatchApproval"
                | "item/tool/requestUserInput"
                | "mcpServer/elicitation/request"
        )
}

/// Method-bound response construction shared by the direct ProviderSession and
/// detached writer paths. Request IDs remain JSON strings or signed integers;
/// no coercion, missing-ID fallback, legacy enum guess, or persistent grant is
/// introduced by an operator's single-request approve/deny answer.
pub(crate) fn approval_response(
    request_id: &Value,
    method: &str,
    params: &Value,
    decision: ApprovalDecision,
) -> Result<Value> {
    if !APPROVAL_METHODS.contains(&method) {
        return Err(DaemonError::InvalidParam(
            "unsupported_appserver_approval_method".into(),
        ));
    }
    if !(request_id.is_i64() || request_id.is_string()) {
        return Err(DaemonError::InvalidParam(
            "invalid_appserver_approval_request_id".into(),
        ));
    }
    if !["threadId", "turnId", "itemId"]
        .iter()
        .all(|key| params[*key].is_string())
        || !params["startedAtMs"].is_i64()
    {
        return Err(DaemonError::InvalidParam(
            "incomplete_appserver_approval_params".into(),
        ));
    }
    let decision = match (method, decision) {
        (
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval",
            ApprovalDecision::Approve,
        ) => "accept",
        (
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval",
            ApprovalDecision::Deny,
        ) => "decline",
        (
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval",
            ApprovalDecision::ApproveForSession,
        ) => "acceptForSession",
        _ => {
            return Err(DaemonError::InvalidParam(
                "unsupported_appserver_approval_decision".into(),
            ));
        }
    };
    if let Some(available) = params.get("availableDecisions").filter(|v| !v.is_null()) {
        if !available
            .as_array()
            .is_some_and(|values| values.iter().any(|v| v.as_str() == Some(decision)))
        {
            return Err(DaemonError::InvalidParam(
                "appserver_approval_decision_not_available".into(),
            ));
        }
    }
    Ok(json!({"jsonrpc":"2.0", "id":request_id, "result":{"decision":decision}}))
}

/// `pub(crate)` ONLY so the frozen Group A module
/// (`crate::session::issue21_phase2_tests`) can name the router's mailbox
/// element types. It must never widen to `pub`: `codex_app_server` is a
/// `pub mod` (see `lib.rs`), so `pub` here would export a provider-execution
/// detail out of the crate that deliberately seals that authority.
#[derive(Debug)]
pub(crate) enum AppServerResponse {
    Result(Value),
    Error(Value),
}

fn app_server_response_error(stage: &str, error: &Value) -> DaemonError {
    DaemonError::Process(format!(
        "CodexAppServer {stage} JSON-RPC error: {}",
        serde_json::to_string(error).unwrap_or_else(|_| error.to_string())
    ))
}

fn thread_id_from_result(result: &Value) -> Option<String> {
    result
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .or_else(|| result.get("threadId"))
        .or_else(|| result.get("thread_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

async fn settle_thread_start_response(
    turn_control: &Arc<dyn ModelCallControl>,
    current_turn_call: &mut Option<ModelCallSettlement>,
    response: AppServerResponse,
) -> Result<String> {
    let failure = match response {
        AppServerResponse::Result(result) => {
            if let Some(thread_id) = thread_id_from_result(&result) {
                return Ok(thread_id);
            }
            (
                "thread_start_missing_thread_id",
                DaemonError::Process(
                    "CodexAppServer thread/start response contained no non-empty thread ID"
                        .to_string(),
                ),
            )
        }
        AppServerResponse::Error(error) => (
            "thread_start_json_rpc_error",
            app_server_response_error("thread/start", &error),
        ),
    };

    if let Some(call) = current_turn_call.take() {
        turn_control.fail(call, failure.0).await?;
    }
    Err(failure.1)
}

fn app_server_notification_to_stream_event(method: &str, params: Value) -> Option<StreamEvent> {
    match method {
        "turn/completed" => {
            let turn = params.get("turn").cloned().unwrap_or_else(|| json!({}));
            let status = turn
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("completed");
            if status == "completed" {
                let usage = params
                    .get("usage")
                    .or_else(|| turn.get("usage"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                Some(StreamEvent {
                    event_type: "result".to_string(),
                    data: json!({
                        "subtype": "turn_completed",
                        "provider_event_type": method,
                        "thread_id": params.get("threadId").cloned().unwrap_or(Value::Null),
                        "turn_id": turn.get("id").cloned().unwrap_or(Value::Null),
                        "usage": usage,
                    }),
                })
            } else if matches!(status, "failed" | "interrupted") {
                let error = turn
                    .get("error")
                    .and_then(|error| error.get("message").or(Some(error)))
                    .and_then(Value::as_str)
                    .unwrap_or(status);
                Some(StreamEvent {
                    event_type: "process_error".to_string(),
                    data: json!({
                        "error": error,
                        "error_class": if status == "interrupted" { "interrupted" } else { "turn_failed" },
                        "provider_event_type": method,
                        "thread_id": params.get("threadId").cloned().unwrap_or(Value::Null),
                        "turn_id": turn.get("id").cloned().unwrap_or(Value::Null),
                        "turn_status": status,
                    }),
                })
            } else {
                None
            }
        }
        "error"
            if !params
                .get("willRetry")
                .and_then(Value::as_bool)
                .unwrap_or(false) =>
        {
            let error = params
                .get("error")
                .and_then(|error| error.get("message").or(Some(error)))
                .and_then(Value::as_str)
                .unwrap_or("CodexAppServer terminal error");
            Some(StreamEvent {
                event_type: "process_error".to_string(),
                data: json!({
                    "error": error,
                    "error_class": "app_server_terminal_error",
                    "provider_event_type": method,
                    "thread_id": params.get("threadId").cloned().unwrap_or(Value::Null),
                    "turn_id": params.get("turnId").cloned().unwrap_or(Value::Null),
                }),
            })
        }
        _ => None,
    }
}

/// One notification frame handed from the stdout reader to the notification
/// forwarder, carrying the overflow action classified from the ORIGINAL frame.
///
/// The classification travels WITH the payload because this boundary discards
/// the frame's `id`, and `id` presence is exactly what separates a provider
/// request (never droppable) from an ordinary notification (droppable).
///
/// `pub(crate)` for the same reason as [`AppServerResponse`], and under the
/// same prohibition on widening to `pub`.
pub(crate) type AppServerNotification = (String, Value, IngressOverflowAction);

/// What the ingress path must do after handling one frame (C-P2-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IngressOutcome {
    /// The frame was delivered, or was structurally proven ordinary and
    /// dropped on a full mailbox.
    Continue,
    /// A non-suppressible frame could not be delivered. Custody can never be
    /// established from a discarded response, provider request, lifecycle
    /// fact, or ambiguous frame, so ingress fails closed and stops.
    TerminateIngress,
}

/// Offer one already-classified frame to a bounded mailbox WITHOUT EVER
/// AWAITING IT (C-P2-15).
///
/// This is the single chokepoint that makes the stdout path non-blocking. It
/// distinguishes three outcomes that the previous `send(..).await` conflated:
///
/// - `Ok` — delivered.
/// - `Closed` — the consumer is gone. This is NOT overflow and must never
///   seal: it is exactly the pre-existing silent no-op (the handshake's
///   `response_rx` is dropped once `launch` returns, so post-handshake
///   responses have always been discarded here).
/// - `Full` — genuine overflow, where C-P2-15's policy applies.
fn offer_without_blocking<T>(
    tx: &mpsc::Sender<T>,
    item: T,
    class: &IngressFrameClass<'_>,
    plane: &AppServerControlPlane,
) -> IngressOutcome {
    offer_without_blocking_classified(tx, item, class.as_str(), class.overflow_action(), plane)
}

/// [`offer_without_blocking`] for callers that already carry the CLASSIFIED
/// overflow action rather than a live [`IngressFrameClass`].
///
/// `IngressOverflowAction` is `Copy` and borrows nothing, so a classification
/// taken from the original frame can be carried across a channel boundary and
/// applied here verbatim. That matters because re-deriving a class from a
/// lossy projection of the frame (a `method` whose `id` has been dropped, say)
/// can silently demote a provider request to droppable ordinary evidence, which
/// C-P2-15 permits only for structurally proven non-message/non-response/
/// non-request evidence.
fn offer_without_blocking_classified<T>(
    tx: &mpsc::Sender<T>,
    item: T,
    class_label: &str,
    action: IngressOverflowAction,
    plane: &AppServerControlPlane,
) -> IngressOutcome {
    match tx.try_send(item) {
        Ok(()) => IngressOutcome::Continue,
        Err(mpsc::error::TrySendError::Closed(_)) => IngressOutcome::Continue,
        Err(mpsc::error::TrySendError::Full(_)) => match action {
            IngressOverflowAction::DropWithoutSeal => {
                tracing::debug!(
                    frame_class = class_label,
                    "CodexAppServer ingress: dropped structurally proven ordinary \
                     suppressible evidence on a full mailbox"
                );
                IngressOutcome::Continue
            }
            IngressOverflowAction::LatchLifecycle | IngressOverflowAction::SealOrTerminate => {
                // The fact must outlive the bounded queue. Latching is
                // synchronous and cannot fail for capacity, so the seal is
                // recorded even though the mailbox refused the frame.
                //
                // C-P2-13: this is a SEAL, not a death. The provider is alive
                // and the reader has not reached EOF, so overflow must produce
                // `sealed_live_uncertain` (custody RETAINED) and must never
                // travel on a `settles_custody()` kind such as `ReaderEof`.
                // Doing so would release custody without confirmed death, raise
                // a daemon-wide false `provider_death_observed`, and — because
                // `coalesce_latch` keeps the strongest fact and can never
                // downgrade — permanently overwrite genuine `TurnCompleted` /
                // `TurnFailed` latches with an unrecoverable false one.
                let sealed =
                    plane.latch_overflow_seal_all(QuarantineSealReason::IngressMailboxOverflow);
                tracing::warn!(
                    frame_class = class_label,
                    sealed_attempts = sealed,
                    "CodexAppServer ingress: a non-suppressible frame overflowed its \
                     mailbox; sealing pre-ack evidence and failing closed rather than \
                     blocking the stdout task"
                );
                IngressOutcome::TerminateIngress
            }
        },
    }
}

async fn forward_app_server_notifications(
    mut notification_rx: mpsc::Receiver<AppServerNotification>,
    event_tx: mpsc::Sender<StreamEvent>,
    plane: Arc<AppServerControlPlane>,
) {
    while let Some((method, params, action)) = notification_rx.recv().await {
        // C-P2-15: the overflow action is the one classified from the ORIGINAL
        // frame by `classify_ingress_frame`, carried across this boundary
        // rather than re-derived here. This channel drops the frame's `id`, so
        // re-deriving would re-classify any method-bearing frame that DID carry
        // an `id` — a provider request — as droppable ordinary evidence.
        if let Some(event) = app_server_notification_to_stream_event(&method, params)
            && offer_without_blocking_classified(&event_tx, event, &method, action, &plane)
                == IngressOutcome::TerminateIngress
        {
            break;
        }
    }
}

/// Route one inbound frame.
///
/// This function is deliberately SYNCHRONOUS. C-P2-15 freezes the rule that
/// the stdout task may await only stdout reads; making the router `fn` rather
/// than `async fn` turns that rule into a compile-time property instead of a
/// convention, exactly as `app_server_control.rs` does for the plane itself.
///
/// `pub(crate)` ONLY so the frozen Group A module
/// (`crate::session::issue21_phase2_tests`) can drive this exact production
/// entry point rather than a mock. Never widen to `pub` — see
/// [`AppServerResponse`].
pub(crate) fn route_app_server_message(
    msg: Value,
    plane: &AppServerControlPlane,
    response_tx: &mpsc::Sender<(i64, AppServerResponse)>,
    notification_tx: &mpsc::Sender<AppServerNotification>,
    event_tx: &mpsc::Sender<StreamEvent>,
    thread_id: &mut Option<String>,
) -> IngressOutcome {
    let class = classify_ingress_frame(&msg);

    if let Some(id) = msg.get("id").and_then(Value::as_i64) {
        if let Some(result) = msg.get("result") {
            return offer_without_blocking(
                response_tx,
                (id, AppServerResponse::Result(result.clone())),
                &class,
                plane,
            );
        }
        if let Some(error) = msg.get("error") {
            return offer_without_blocking(
                response_tx,
                (id, AppServerResponse::Error(error.clone())),
                &class,
                plane,
            );
        }
    }

    if let Some(method) = msg.get("method").and_then(Value::as_str) {
        let request_id = msg.get("id").and_then(Value::as_i64).unwrap_or(0);
        let params = msg.get("params").cloned().unwrap_or_default();
        if method == "serverRequest/resolved" {
            // Same FIFO as publication: a separate notification forwarder can
            // reorder closure behind a reused request ID. This fact is never
            // ordinary suppressible evidence, including malformed frames.
            return offer_without_blocking_classified(
                event_tx,
                StreamEvent {
                    event_type: "approval_resolved".into(),
                    data: if msg.get("id").is_none()
                        && msg.get("result").is_none()
                        && msg.get("error").is_none()
                    {
                        params
                    } else {
                        json!({"invalid_notification_shape":true})
                    },
                },
                "provider_request_resolution",
                IngressOverflowAction::SealOrTerminate,
                plane,
            );
        }
        if is_operator_request(method) {
            let description = params
                .get("reason")
                .or_else(|| params.get("description"))
                .or_else(|| params.get("command"))
                .or_else(|| params.get("path"))
                .or_else(|| params.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return offer_without_blocking(
                event_tx,
                StreamEvent {
                    event_type: "approval_request".to_string(),
                    data: json!({
                        "request_id": msg.get("id").cloned().unwrap_or(Value::Null),
                        "method": method,
                        "description": description,
                        "params": params,
                    }),
                },
                &class,
                plane,
            );
        } else if method == "item/tool/call" {
            let tool_name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let tool_args = params.get("arguments").cloned().unwrap_or_default();
            return offer_without_blocking(
                event_tx,
                StreamEvent {
                    event_type: "tool_call".to_string(),
                    data: json!({
                        "call_id": request_id,
                        "name": tool_name,
                        "arguments": tool_args,
                    }),
                },
                &class,
                plane,
            );
        } else if method == "thread/tokenUsage/updated" {
            if let Some(event) = map_token_usage_notification(&params) {
                return offer_without_blocking(event_tx, event, &class, plane);
            }
        } else {
            let action = class.overflow_action();
            return offer_without_blocking(
                notification_tx,
                (method.to_string(), params, action),
                &class,
                plane,
            );
        }
        return IngressOutcome::Continue;
    }

    if let Some(event) = map_app_server_event_to_stream_event(&msg, thread_id) {
        return offer_without_blocking(event_tx, event, &class, plane);
    }
    IngressOutcome::Continue
}

/// Terminate the provider process behind ingress.
///
/// C-P2-15 permits exactly two responses to a non-suppressible frame that
/// cannot be delivered: a **durable** seal, or **terminating the provider**.
/// The durable half requires the control worker, which is not built yet, so
/// ingress takes the second branch explicitly.
///
/// `SIGKILL`, not the `SIGINT` used by [`CodexAppServerProcess::interrupt`]:
/// this path is reached precisely when nobody is draining the provider's
/// evidence, so the provider may already be blocked writing into a full stdout
/// pipe, where a catchable signal can be ignored indefinitely. Killing it makes
/// the death observable through the existing paths — `try_wait` on the retained
/// `Child`, and stderr EOF, which releases the last `event_tx` clone so
/// `event_rx` finally closes and the turn settles.
///
/// Returns `true` when the signal was delivered.
fn terminate_app_server_provider(child_pid: Option<u32>, reason: &str) -> bool {
    let Some(pid) = child_pid else {
        tracing::error!(
            reason,
            "CodexAppServer ingress: cannot terminate the provider, no PID was captured; \
             the reader is stopping with the process possibly still running"
        );
        return false;
    };
    let pgid = nix::unistd::Pid::from_raw(pid as i32);
    let signal_result = match nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL) {
        Ok(()) => Ok(()),
        Err(group_error) => {
            tracing::warn!(
                reason,
                pid,
                error = %group_error,
                "CodexAppServer ingress: process-group signal failed; trying direct child"
            );
            nix::sys::signal::kill(pgid, nix::sys::signal::Signal::SIGKILL)
        }
    };
    match signal_result {
        Ok(()) => {
            tracing::warn!(
                reason,
                pid,
                "CodexAppServer ingress failed closed; terminated the provider rather than \
                 abandoning the stdout reader on a live process"
            );
            true
        }
        Err(errno) => {
            tracing::error!(
                reason,
                pid,
                error = %errno,
                "CodexAppServer ingress: failed to terminate the provider"
            );
            false
        }
    }
}

/// Read provider stdout to EOF, routing each frame synchronously.
///
/// C-P2-15: this task awaits ONLY `lines.next_line()`. Routing is a synchronous
/// call, so no bounded mailbox can ever wedge it. Extracted from `launch` so the
/// fail-closed path can be exercised against a real process in tests.
async fn run_app_server_stdout_reader<R>(
    stdout: R,
    plane: Arc<AppServerControlPlane>,
    response_tx: mpsc::Sender<(i64, AppServerResponse)>,
    notification_tx: mpsc::Sender<AppServerNotification>,
    event_tx: mpsc::Sender<StreamEvent>,
    child_pid: Option<u32>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BoundedLines::new(stdout, PROVIDER_MAX_LINE_BYTES);
    let mut thread_id = None;
    let mut failed_closed = false;
    let mut observed_eof = false;

    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                observed_eof = true;
                break;
            }
            Err(error) => {
                let _ = plane.latch_provider_death(ProviderLifecycleKind::OverflowSeal);
                let error_class = if matches!(error, BoundedLineError::Exceeded { .. }) {
                    "provider_output_overflow"
                } else {
                    "provider_output_read_error"
                };
                let _ = event_tx.try_send(StreamEvent {
                    event_type: "process_error".to_string(),
                    data: json!({
                        "error": error.to_string(),
                        "source": "stdout",
                        "terminal": true,
                        "error_class": error_class,
                    }),
                });
                let _ = terminate_app_server_provider(child_pid, error_class);
                break;
            }
        };
        if failed_closed {
            // Ingress has failed closed and the provider has been terminated.
            // Keep draining stdout to its genuine EOF so the reader is never
            // abandoned while the pipe still has a writer, but route nothing
            // further: custody can never be established from this point.
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<Value>(&line) {
            Ok(msg) => {
                if route_app_server_message(
                    msg,
                    &plane,
                    &response_tx,
                    &notification_tx,
                    &event_tx,
                    &mut thread_id,
                ) == IngressOutcome::TerminateIngress
                {
                    // A bare `break` here would satisfy NEITHER branch of
                    // C-P2-15: the seal is in-memory only (not durable) and the
                    // child keeps running. That wedges the session — no further
                    // stdout is consumed, so the turn never settles, and the
                    // stderr task holds an `event_tx` clone forever so the
                    // monitor never observes EOF. Terminate the provider
                    // instead, then drain to real EOF.
                    if terminate_app_server_provider(child_pid, "ingress_failed_closed") {
                        failed_closed = true;
                    } else {
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(line = %line, error = %e, "Failed to parse app-server message");
            }
        }
    }

    if observed_eof {
        // C-P2-15: EOF is a lifecycle fact and must never live solely in a bounded
        // evidence queue. Latching is synchronous and cannot fail for capacity, so
        // it is recorded even when every mailbox is full. A bounded-reader error is
        // not EOF: its overflow seal remains until process death is confirmed.
        let latched = plane.latch_provider_death(ProviderLifecycleKind::ReaderEof);
        tracing::debug!(
            latched_attempts = latched,
            "CodexAppServer stdout reader finished; reader EOF latched on the control plane"
        );
    }
}

/// Wraps a running Codex app-server process (without the session handle).
pub struct CodexAppServerProcess {
    child: Child,
}

impl CodexAppServerProcess {
    /// Send SIGINT to gracefully interrupt the session.
    pub fn interrupt(&self) -> Result<()> {
        if let Some(pid) = self.child.id() {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGINT,
            )
            .map_err(|e| DaemonError::Process(format!("Failed to send SIGINT: {}", e)))?;
        }
        Ok(())
    }

    /// Force kill the process.
    pub async fn kill(&mut self) -> Result<()> {
        self.child.kill().await?;
        Ok(())
    }

    /// Non-blocking check if the process has exited.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }
}

/// Writer half of the app-server session, stored separately for RPC access.
/// Wraps the write channel to the writer task so approval responses and
/// other control messages can be sent without owning the full session.
#[derive(Clone)]
pub struct AppServerWriter {
    write_tx: mpsc::Sender<Vec<u8>>,
    next_id: Arc<AtomicI64>,
}

pub(crate) struct PreparedApprovalResponse {
    permit: mpsc::OwnedPermit<Vec<u8>>,
    bytes: Vec<u8>,
}
impl PreparedApprovalResponse {
    /// Only channel acceptance is established. This is not a provider receipt.
    pub(crate) fn enqueue(self) {
        self.permit.send(self.bytes);
    }
}

impl AppServerWriter {
    /// Reserving channel space has no provider effect. The caller can commit
    /// its durable write intent and then enqueue synchronously under its fences.
    pub(crate) async fn prepare_approval(
        &self,
        request_id: &Value,
        method: &str,
        params: &Value,
        decision: ApprovalDecision,
    ) -> Result<PreparedApprovalResponse> {
        let response = approval_response(request_id, method, params, decision)?;
        let bytes = format!("{}\n", serde_json::to_string(&response)?).into_bytes();
        let permit = self
            .write_tx
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| DaemonError::ChannelClosed)?;
        Ok(PreparedApprovalResponse { permit, bytes })
    }

    /// Send an approval response back to the provider.
    pub async fn send_approval(
        &self,
        request_id: &Value,
        method: &str,
        params: &Value,
        decision: ApprovalDecision,
    ) -> Result<()> {
        self.prepare_approval(request_id, method, params, decision)
            .await?
            .enqueue();
        Ok(())
    }

    /// Send a tool result back to the provider.
    pub async fn send_tool_result(&self, call_id: i64, result: Value) -> Result<()> {
        let response = json!({
            "jsonrpc": "2.0",
            "id": call_id,
            "result": result
        });
        let bytes = format!("{}\n", serde_json::to_string(&response)?).into_bytes();
        self.write_tx
            .send(bytes)
            .await
            .map_err(|_| DaemonError::ChannelClosed)?;
        Ok(())
    }

    /// Allocate a new request ID.
    pub fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }
}

/// The one place an AppServer execution capability is consumed, reporting
/// WHERE the write stopped (P2-05b).
///
/// **This function's name, path, and its single direct
/// `execution.send_codex_app_server(..)` call are a frozen contract**, not a
/// style choice: `registry::EXECUTION_BOUNDARIES`'s `appserver_model_request`
/// row names this exact item in this exact file and requires exactly one direct
/// capability consumption inside it. P2-05b therefore widened the return type
/// in place rather than delegating to a `_classified` helper — moving the call
/// into a helper would drop this item's direct-use count to zero and fail the
/// real-tree validator test.
///
/// The agent-message delivery path must distinguish a refused enqueue (proved
/// no effect, requeueable) from an accepted one (effect possible, never
/// retryable), and the plan forbids recovering that distinction from a generic
/// `Err`. Callers that only ever wanted the old `Result<()>` shape collapse
/// `RejectedBeforeEnqueue` back to `DaemonError::ChannelClosed` at their own
/// call site, which keeps their behaviour byte-identical to before this slice.
async fn send_admitted_app_server_request(
    write_tx: &mpsc::Sender<Vec<u8>>,
    next_id: &Arc<AtomicI64>,
    method: &'static str,
    params: Value,
    execution: ModelExecutionCapability,
) -> Result<(i64, AppServerDispatchOutcome)> {
    let id = next_id.fetch_add(1, Ordering::SeqCst);
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let bytes = format!("{}\n", serde_json::to_string(&request)?).into_bytes();
    let outcome = execution
        .send_codex_app_server(write_tx, bytes, method)
        .await?;
    Ok((id, outcome))
}

/// Collapse a dispatch outcome back to the pre-P2-05b `Result<i64>` shape.
///
/// The ordinary `thread/start` and `start_turn` paths have no way to act on the
/// refused-enqueue distinction and never did: before this slice a refused
/// enqueue surfaced as `DaemonError::ChannelClosed`, so it still does. Keeping
/// that collapse in one named place makes it obvious that the ordinary paths
/// are unchanged rather than quietly re-classified.
fn require_enqueued(dispatch: (i64, AppServerDispatchOutcome)) -> Result<i64> {
    match dispatch {
        (id, AppServerDispatchOutcome::EnqueuedWithoutReceipt) => Ok(id),
        (_, AppServerDispatchOutcome::RejectedBeforeEnqueue) => Err(DaemonError::ChannelClosed),
    }
}

/// Active session handle for a `codex app-server` process.
/// Implements `ProviderSession` for the monitor loop.
pub struct CodexAppServerSession {
    /// Thread ID from `thread/start` response.
    thread_id: String,
    /// Working directory for the session.
    working_dir: PathBuf,
    /// Send serialized JSON-RPC messages to the writer task.
    write_tx: mpsc::Sender<Vec<u8>>,
    /// Receive normalized StreamEvents from the reader task.
    event_rx: mpsc::Receiver<StreamEvent>,
    /// Shared outbound request ID counter.
    next_id: Arc<AtomicI64>,
    turn_control: Arc<dyn ModelCallControl>,
    current_turn_call: Option<ModelCallSettlement>,
}

impl CodexAppServerSession {
    /// Controlled transport using the real admitted writer and reader session.
    /// Tests inject frames through `route_app_server_message`, just as stdout
    /// ingress does, and inspect bytes accepted by the real writer channel.
    #[cfg(test)]
    pub(crate) async fn approval_test_transport(
        turn_control: Arc<dyn ModelCallControl>,
    ) -> Result<(Self, mpsc::Sender<StreamEvent>, mpsc::Receiver<Vec<u8>>)> {
        let (write_tx, write_rx) = mpsc::channel(16);
        let (event_tx, event_rx) = mpsc::channel(16);
        let next_id = Arc::new(AtomicI64::new(1));
        let (settlement, execution) = turn_control
            .admit(
                ModelCallKind::Primary,
                "approval-transport",
                "controlled input",
                None,
            )
            .await?
            .into_parts();
        require_enqueued(
            send_admitted_app_server_request(
                &write_tx,
                &next_id,
                "thread/start",
                json!({"input":"controlled input"}),
                execution,
            )
            .await?,
        )?;
        Ok((
            Self {
                thread_id: "approval-transport-thread".into(),
                working_dir: PathBuf::from("/var/tmp"),
                write_tx,
                event_rx,
                next_id,
                turn_control,
                current_turn_call: Some(settlement),
            },
            event_tx,
            write_rx,
        ))
    }

    async fn send_response(&mut self, id: i64, result: Value) -> Result<()> {
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        let bytes = format!("{}\n", serde_json::to_string(&response)?).into_bytes();
        self.write_tx
            .send(bytes)
            .await
            .map_err(|_| DaemonError::ChannelClosed)?;
        Ok(())
    }

    /// Split into a writer half (for RPC access) and keep the reader half in place.
    pub fn writer(&self) -> AppServerWriter {
        AppServerWriter {
            write_tx: self.write_tx.clone(),
            next_id: Arc::clone(&self.next_id),
        }
    }

    async fn settle_current_turn_success(&mut self, event: &StreamEvent) -> Result<()> {
        let Some(call) = self.current_turn_call.take() else {
            return Ok(());
        };
        let usage = event
            .data
            .get("usage")
            .cloned()
            .unwrap_or_else(|| json!({}));
        self.turn_control
            .complete(
                call,
                ModelCallUsage {
                    input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
                    output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
                    cache_creation_tokens: usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64()),
                    cache_read_tokens: usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64()),
                    ..ModelCallUsage::default()
                },
            )
            .await
    }

    async fn settle_current_turn_failure(&mut self, error_class: &str) -> Result<()> {
        let Some(call) = self.current_turn_call.take() else {
            return Ok(());
        };
        self.turn_control.fail(call, error_class).await
    }
}

#[async_trait::async_trait]
impl ProviderSession for CodexAppServerSession {
    async fn next_event(&mut self) -> Option<StreamEvent> {
        match self.event_rx.recv().await {
            Some(event) => {
                if event.event_type == "result"
                    && event.data.get("subtype").and_then(|v| v.as_str()) == Some("turn_completed")
                {
                    if let Err(error) = self.settle_current_turn_success(&event).await {
                        return Some(settlement_error_event("turn completion", error));
                    }
                } else if event.event_type == "process_error" {
                    let error_class = event
                        .data
                        .get("error_class")
                        .and_then(Value::as_str)
                        .unwrap_or("turn_failed");
                    if let Err(error) = self.settle_current_turn_failure(error_class).await {
                        return Some(settlement_error_event("turn failure", error));
                    }
                }
                Some(event)
            }
            None => match self.settle_current_turn_failure("stream_closed").await {
                Ok(()) => None,
                Err(error) => Some(settlement_error_event("stream close", error)),
            },
        }
    }

    fn app_server_approval_writer(&self) -> Option<AppServerWriter> {
        Some(self.writer())
    }

    fn app_server_approval_invocation(&self) -> Option<uuid::Uuid> {
        self.current_turn_call
            .as_ref()
            .and_then(ModelCallSettlement::invocation_id)
    }

    async fn send_approval(
        &mut self,
        request_id: &Value,
        method: &str,
        params: &Value,
        decision: ApprovalDecision,
    ) -> Result<()> {
        self.writer()
            .send_approval(request_id, method, params, decision)
            .await
    }

    async fn start_turn(&mut self, config: &TurnConfig) -> Result<TurnId> {
        if self.current_turn_call.is_some() {
            return Err(DaemonError::PolicyDenied(
                "CodexAppServer turn denied: prior turn is still unsettled".to_string(),
            ));
        }
        let admitted_call = self
            .turn_control
            .admit(ModelCallKind::Primary, &self.thread_id, &config.input, None)
            .await?;
        let (settlement, execution) = admitted_call.into_parts();
        let cwd = config
            .working_dir
            .as_ref()
            .unwrap_or(&self.working_dir)
            .display()
            .to_string();

        let params = json!({
            "threadId": self.thread_id,
            "input": config.input,
            "cwd": cwd,
        });

        if let Err(error) = send_admitted_app_server_request(
            &self.write_tx,
            &self.next_id,
            "turn/start",
            params,
            execution,
        )
        .await
        .and_then(require_enqueued)
        {
            self.turn_control
                .fail(settlement, "turn_start_send_failed")
                .await?;
            return Err(error);
        }
        self.current_turn_call = Some(settlement);

        // The TurnId is the thread_id since app-server uses thread-scoped turns
        Ok(TurnId(self.thread_id.clone()))
    }

    /// Install an EXTERNALLY admitted message turn (P2-05b, C-P2-08).
    ///
    /// This mirrors [`Self::start_turn`] step for step with one deliberate
    /// difference: it **installs** rather than **admits**. It must never call
    /// `self.turn_control.admit` — the monitor already admitted this invocation
    /// and already holds the durable `model_invocations` row that the attempt's
    /// fence is bound to. Self-admitting here would create a second invocation
    /// for one attempt, which is exactly what C-P2-08 exists to prevent.
    ///
    /// The economy of this design is the last line of the success arm: the
    /// settlement lands in the SAME `current_turn_call` slot the ordinary path
    /// uses, so it settles through the existing `settle_current_turn_success` /
    /// `settle_current_turn_failure` machinery with zero new settlement code.
    async fn start_admitted_message_turn(
        &mut self,
        turn: NativeMessageTurnRequest,
        config: &TurnConfig,
    ) -> Result<AdmittedMessageTurnOutcome> {
        let (_fence, settlement, execution) = turn.into_parts();

        // Same refusal as the ordinary path, and it is PROVED pre-effect: no
        // capability has been consumed and no byte has been built, let alone
        // queued. An active turn keeps the message queued for a later boundary
        // rather than interrupting the provider — delivery is idle-only.
        if self.current_turn_call.is_some() {
            return Ok(AdmittedMessageTurnOutcome::RejectedBeforeEffect {
                settlement,
                error_class: "agent_message_app_server_turn_busy",
            });
        }

        let cwd = config
            .working_dir
            .as_ref()
            .unwrap_or(&self.working_dir)
            .display()
            .to_string();
        let params = json!({
            "threadId": self.thread_id,
            "input": config.input,
            "cwd": cwd,
        });

        match send_admitted_app_server_request(
            &self.write_tx,
            &self.next_id,
            "turn/start",
            params,
            execution,
        )
        .await
        {
            // The writer handed the bytes back unsent. Nothing was queued, so
            // nothing could have been written: this is the one AppServer
            // outcome the frozen matrix accepts as proof of no effect, and the
            // idle turn state is restored by never having been changed.
            Ok((_, AppServerDispatchOutcome::RejectedBeforeEnqueue)) => {
                Ok(AdmittedMessageTurnOutcome::RejectedBeforeEffect {
                    settlement,
                    error_class: "agent_message_app_server_rejected_before_enqueue",
                })
            }
            // Enqueue is NOT completion. The writer task may already have
            // written these bytes, so from here the attempt is effect-possible
            // and can never be retried, requeued, or replaced by a synthetic
            // continuation. `native_turn_id` stays None: the genuine
            // `result.turn.id` needs the response waiter, so the attempt rests
            // in `correlation_pending` for a later exact correlation.
            Ok((_, AppServerDispatchOutcome::EnqueuedWithoutReceipt)) => {
                self.current_turn_call = Some(settlement);
                Ok(AdmittedMessageTurnOutcome::Dispatched {
                    native_turn_id: None,
                })
            }
            // Dropping `settlement` here is the ONE place this method relies on
            // the `Drop` net, and it is deliberately conservative rather than
            // clever. Every `Err` this call can actually produce today —
            // serialization, the route check, the method check — is raised
            // strictly before any byte reaches the writer queue, so it *could*
            // be reported as proved-no-effect. It is not, because that would
            // encode a claim about a callee's internal ordering that the type
            // system does not enforce: if a future change ever raises an `Err`
            // after the enqueue, a `RejectedBeforeEffect` here would silently
            // authorize a second delivery of a message the provider already
            // received. Unknown-effect is the only answer that stays correct
            // under that change.
            Err(error) => Err(error),
        }
    }

    async fn send_tool_result(&mut self, call_id: i64, result: Value) -> Result<()> {
        self.send_response(call_id, result).await
    }

    fn supports_multi_turn(&self) -> bool {
        true
    }

    fn supports_approvals(&self) -> bool {
        true
    }
}

/// Client for launching `codex app-server` sessions.
pub struct CodexAppServerClient {
    binary_path: PathBuf,
}

/// Internal launch checkpoints; these are never protocol or caller fields.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppServerLaunchGate {
    BeforeSpawn,
    BeforeThreadAdmission,
    BeforeThreadSend,
}

impl CodexAppServerClient {
    pub fn new() -> Result<Self> {
        let binary_path = which::which("codex").map_err(|_| DaemonError::CodexBinaryNotFound)?;
        Ok(Self { binary_path })
    }

    pub fn is_available() -> bool {
        which::which("codex").is_ok()
    }

    pub(crate) async fn new_after_admission(
        store: &Arc<Mutex<Store>>,
        event_bus: &Arc<crate::bus::EventBus>,
        permit: &AdmissionPermit,
    ) -> Result<Self> {
        Self::new_after_admission_with(store, event_bus, permit, Self::new).await
    }

    /// Test-only injected launcher used to exercise the real deferred launch
    /// and JSON-RPC handshake without locating or invoking a live provider.
    #[cfg(test)]
    pub(crate) async fn new_after_admission_with_test_binary(
        store: &Arc<Mutex<Store>>,
        event_bus: &Arc<crate::bus::EventBus>,
        permit: &AdmissionPermit,
        binary_path: PathBuf,
    ) -> Result<Self> {
        Self::new_after_admission_with(store, event_bus, permit, || Ok(Self { binary_path })).await
    }

    async fn new_after_admission_with<F>(
        store: &Arc<Mutex<Store>>,
        event_bus: &Arc<crate::bus::EventBus>,
        permit: &AdmissionPermit,
        resolve: F,
    ) -> Result<Self>
    where
        F: FnOnce() -> Result<Self>,
    {
        match resolve() {
            Ok(client) => Ok(client),
            Err(error) => {
                let settlement = complete_invocation(
                    store,
                    permit,
                    InvocationCompletion {
                        error_class: Some(
                            "codex_app_server_client_construction_failed".to_string(),
                        ),
                        confidence: Some(ModelUsageConfidence::Unavailable),
                        ..InvocationCompletion::default()
                    },
                    event_bus,
                )
                .await;
                match settlement {
                    Ok(()) => Err(error),
                    Err(settlement_error) => Err(DaemonError::Store(format!(
                        "CodexAppServer client construction failed with {error}; \
                         additionally failed to settle invocation {}: {settlement_error}",
                        permit.invocation_id()
                    ))),
                }
            }
        }
    }

    /// Launch a `codex app-server` session and perform the initialize/thread_start handshake.
    ///
    /// Returns `(CodexAppServerProcess, CodexAppServerSession)` where:
    /// - `CodexAppServerProcess` wraps the Child for interrupt/kill
    /// - `CodexAppServerSession` implements `ProviderSession` for the monitor loop
    pub(crate) async fn launch(
        &self,
        config: &LaunchConfig,
        model_invocation_id: uuid::Uuid,
        tool_specs: &[crate::tool_registry::ToolSpec],
        turn_control: Arc<dyn ModelCallControl>,
        app_server_control: Arc<AppServerControlPlane>,
    ) -> Result<(CodexAppServerProcess, CodexAppServerSession)> {
        self.launch_with_gate(
            config,
            model_invocation_id,
            tool_specs,
            &turn_control,
            app_server_control,
            |_| async { Ok(()) },
        )
        .await
    }

    /// Daemon-only revalidation at the process and productive-request boundaries.
    /// This hook is not serialized and grants no authority of its own.
    pub(crate) async fn launch_with_gate<F, Fut>(
        &self,
        config: &LaunchConfig,
        model_invocation_id: uuid::Uuid,
        tool_specs: &[crate::tool_registry::ToolSpec],
        turn_control: &Arc<dyn ModelCallControl>,
        app_server_control: Arc<AppServerControlPlane>,
        gate: F,
    ) -> Result<(CodexAppServerProcess, CodexAppServerSession)>
    where
        F: Fn(AppServerLaunchGate) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let working_dir = config
            .working_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("/tmp"));

        let mut cmd =
            build_app_server_command(&self.binary_path, config, model_invocation_id, &working_dir)?;

        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // Handshake failures return before a CodexAppServerProcess can own the
        // child. Ensure those pre-establishment exits cannot orphan a provider.
        cmd.kill_on_drop(true);
        configure_tokio_process_group(&mut cmd, ProcessContainment::Group)?;

        // MODEL_CALL_ADMISSION: mandatory turn_control admits thread/start before spawn.
        gate(AppServerLaunchGate::BeforeSpawn).await?;
        let mut child = cmd.spawn()?;
        // Captured before any handle is taken: the reader task needs it to
        // terminate the provider when ingress fails closed (C-P2-15).
        let child_pid = child.id();

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| DaemonError::Process("Failed to capture stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DaemonError::Process("Failed to capture stdout".to_string()))?;
        let stderr = child.stderr.take();

        // Channel for sending serialized messages to the writer task
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(32);

        // Channel for normalized StreamEvents from the reader task to the monitor
        let (event_tx, event_rx) = mpsc::channel::<StreamEvent>(100);

        // Channel for JSON-RPC responses (used by the handshake phase)
        let (response_tx, mut response_rx) = mpsc::channel::<(i64, AppServerResponse)>(16);
        // Channel for JSON-RPC notifications
        let (notification_tx, notification_rx) = mpsc::channel::<AppServerNotification>(16);

        // Shared ID counter
        let next_id = Arc::new(AtomicI64::new(1));
        let next_id_clone = Arc::clone(&next_id);

        // Writer task: owns stdin, serializes messages
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(bytes) = write_rx.recv().await {
                if stdin.write_all(&bytes).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        tokio::spawn(forward_app_server_notifications(
            notification_rx,
            event_tx.clone(),
            Arc::clone(&app_server_control),
        ));

        // Reader task: reads stdout, routes responses vs notifications vs events.
        // `child_pid` lets a fail-closed ingress terminate the provider rather
        // than abandon the reader on a live process (C-P2-15).
        tokio::spawn(run_app_server_stdout_reader(
            stdout,
            Arc::clone(&app_server_control),
            response_tx,
            notification_tx,
            event_tx.clone(),
            child_pid,
        ));

        // Stderr capture
        if let Some(stderr) = stderr {
            let err_tx = event_tx.clone();
            let stderr_plane = Arc::clone(&app_server_control);
            tokio::spawn(async move {
                let mut lines = BoundedLines::new(stderr, PROVIDER_MAX_LINE_BYTES);
                let mut stderr_lines = Vec::new();
                let mut stderr_bytes = 0_usize;
                loop {
                    let line = match lines.next_line().await {
                        Ok(Some(line)) => line,
                        Ok(None) => break,
                        Err(error) => {
                            let _ = stderr_plane
                                .latch_provider_death(ProviderLifecycleKind::OverflowSeal);
                            let error_class = if matches!(error, BoundedLineError::Exceeded { .. })
                            {
                                "provider_output_overflow"
                            } else {
                                "provider_output_read_error"
                            };
                            let _ = err_tx.try_send(StreamEvent {
                                event_type: "process_error".to_string(),
                                data: json!({
                                    "error": error.to_string(),
                                    "source": "stderr",
                                    "terminal": true,
                                    "error_class": error_class,
                                }),
                            });
                            let _ = terminate_app_server_provider(child_pid, error_class);
                            return;
                        }
                    };
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Some(reason) = crate::codex::codex_stderr_suppression_reason(trimmed) {
                        tracing::debug!(
                            line = %trimmed,
                            reason,
                            "Codex app-server: suppressed non-fatal stderr message"
                        );
                        continue;
                    }
                    tracing::warn!(line = %line, "Codex app-server stderr");
                    let separator = usize::from(!stderr_lines.is_empty());
                    let next_bytes = stderr_bytes
                        .saturating_add(separator)
                        .saturating_add(line.len());
                    if next_bytes > PROVIDER_MAX_STDERR_BYTES {
                        let _ =
                            stderr_plane.latch_provider_death(ProviderLifecycleKind::OverflowSeal);
                        let _ = err_tx.try_send(StreamEvent {
                            event_type: "process_error".to_string(),
                            data: json!({
                                "error": format!(
                                    "provider stderr exceeded {}-byte bound",
                                    PROVIDER_MAX_STDERR_BYTES
                                ),
                                "source": "stderr",
                                "terminal": true,
                                "error_class": "provider_output_overflow",
                            }),
                        });
                        let _ =
                            terminate_app_server_provider(child_pid, "provider_output_overflow");
                        return;
                    }
                    stderr_bytes = next_bytes;
                    stderr_lines.push(line);
                }
                if !stderr_lines.is_empty() {
                    // A terminal stderr summary is lifecycle evidence, not
                    // suppressible traffic, and this is still a reader path.
                    let _ = offer_without_blocking(
                        &err_tx,
                        StreamEvent {
                            event_type: "process_error".to_string(),
                            data: json!({"error": stderr_lines.join("\n"), "source": "stderr"}),
                        },
                        &IngressFrameClass::Lifecycle { method: "stderr" },
                        &stderr_plane,
                    );
                }
            });
        }

        // --- Handshake phase ---

        // Helper: send a request and wait for its response
        let send_and_wait = |write_tx: &mpsc::Sender<Vec<u8>>,
                             next_id: &Arc<AtomicI64>,
                             method: &str,
                             params: Value|
         -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(i64, Value)>> + Send + '_>,
        > {
            let id = next_id.fetch_add(1, Ordering::SeqCst);
            let request = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            });
            let bytes = format!("{}\n", serde_json::to_string(&request).unwrap()).into_bytes();
            let write_tx = write_tx.clone();
            Box::pin(async move {
                write_tx
                    .send(bytes)
                    .await
                    .map_err(|_| DaemonError::ChannelClosed)?;
                Ok((id, Value::Null))
            })
        };

        // 1. Send initialize request
        let (init_id, _) = send_and_wait(
            &write_tx,
            &next_id_clone,
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "rsi",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )
        .await?;

        // Wait for initialize response
        let init_response = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some((rid, val)) = response_rx.recv().await {
                    if rid == init_id {
                        return Ok::<AppServerResponse, DaemonError>(val);
                    }
                }
            }
        })
        .await
        .map_err(|_| {
            DaemonError::Process("Timeout waiting for initialize response".to_string())
        })??;

        let init_response = match init_response {
            AppServerResponse::Result(result) => result,
            AppServerResponse::Error(error) => {
                let response_error = app_server_response_error("initialize", &error);
                turn_control
                    .fail_pending("initialize_json_rpc_error")
                    .await?;
                return Err(response_error);
            }
        };
        tracing::debug!(response = ?init_response, "app-server initialize response");

        // 2. Send initialized notification
        let initialized_notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        });
        let bytes = format!("{}\n", serde_json::to_string(&initialized_notif)?).into_bytes();
        write_tx
            .send(bytes)
            .await
            .map_err(|_| DaemonError::ChannelClosed)?;

        gate(AppServerLaunchGate::BeforeThreadAdmission).await?;
        // 3. Send thread/start request with tools.
        let thread_start_params = build_thread_start_params(config, &working_dir, tool_specs);
        let admitted_call = turn_control
            .admit(ModelCallKind::Primary, "thread_start", &config.query, None)
            .await?;
        let (settlement, execution) = admitted_call.into_parts();
        let mut current_turn_call = Some(settlement);
        if let Err(error) = gate(AppServerLaunchGate::BeforeThreadSend).await {
            turn_control
                .fail(
                    current_turn_call.take().expect("admitted initial call"),
                    "provider_launch_gate_changed",
                )
                .await?;
            return Err(error);
        }

        let thread_start_send = send_admitted_app_server_request(
            &write_tx,
            &next_id_clone,
            "thread/start",
            thread_start_params,
            execution,
        )
        .await
        .and_then(require_enqueued);
        let thread_start_id = match thread_start_send {
            Ok(request_id) => request_id,
            Err(error) => {
                turn_control
                    .fail(
                        current_turn_call
                            .take()
                            .expect("thread/start permit is present"),
                        "thread_start_send_failed",
                    )
                    .await?;
                return Err(error);
            }
        };

        // Wait for thread/start response to get thread_id
        let thread_start_response =
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    if let Some((rid, val)) = response_rx.recv().await {
                        if rid == thread_start_id {
                            return Ok::<AppServerResponse, DaemonError>(val);
                        }
                    }
                }
            })
            .await;
        let thread_start_response = match thread_start_response {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                turn_control
                    .fail(
                        current_turn_call
                            .take()
                            .expect("thread/start permit is present"),
                        "thread_start_failed",
                    )
                    .await?;
                return Err(error);
            }
            Err(_) => {
                turn_control
                    .fail(
                        current_turn_call
                            .take()
                            .expect("thread/start permit is present"),
                        "thread_start_timeout",
                    )
                    .await?;
                return Err(DaemonError::Process(
                    "Timeout waiting for thread/start response".to_string(),
                ));
            }
        };

        let thread_id = settle_thread_start_response(
            &turn_control,
            &mut current_turn_call,
            thread_start_response,
        )
        .await?;

        tracing::info!(thread_id = %thread_id, "Codex app-server thread started");

        // Emit a synthetic system init event (like CLI thread.started)
        let _ = event_tx
            .send(StreamEvent {
                event_type: "system".to_string(),
                data: json!({
                    "subtype": "init",
                    "session_id": thread_id,
                    "provider_event_type": "thread/start",
                }),
            })
            .await;

        let session = CodexAppServerSession {
            thread_id,
            working_dir,
            write_tx,
            event_rx,
            next_id: next_id_clone,
            turn_control: Arc::clone(turn_control),
            current_turn_call,
        };

        let process = CodexAppServerProcess { child };

        Ok((process, session))
    }
}

fn build_app_server_command(
    binary_path: &Path,
    config: &LaunchConfig,
    model_invocation_id: uuid::Uuid,
    working_dir: &Path,
) -> Result<Command> {
    let mut cmd = Command::new(binary_path);
    cmd.arg("app-server");
    if let Some(effort) = crate::codex::codex_reasoning_effort(config.effort.as_deref()) {
        cmd.args(["-c", &format!("model_reasoning_effort=\"{}\"", effort)]);
    }
    cmd.current_dir(working_dir);

    crate::claude::stamp_execution_environment(&mut cmd, config, model_invocation_id)?;
    // AppServer carried the daemon ownership namespace before Slice 8 even on
    // ordinary launches; preserve that provider-specific baseline.
    cmd.env(
        rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE,
        rsi_common::identity::process_ownership_namespace(),
    );
    Ok(cmd)
}

fn settlement_error_event(stage: &str, error: DaemonError) -> StreamEvent {
    StreamEvent {
        event_type: "process_error".to_string(),
        data: json!({
            "error": format!("CodexAppServer model-call settlement failed after {stage}: {error}"),
            "source": "model_control",
        }),
    }
}

fn build_thread_start_params(
    config: &LaunchConfig,
    working_dir: &Path,
    tool_specs: &[crate::tool_registry::ToolSpec],
) -> Value {
    let dynamic_tools: Vec<Value> = tool_specs
        .iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "description": spec.description,
                "input_schema": spec.parameters,
            })
        })
        .collect();

    let mut params = serde_json::Map::new();
    params.insert("input".to_string(), json!(config.query));
    params.insert("cwd".to_string(), json!(working_dir.display().to_string()));
    params.insert("dynamicTools".to_string(), json!(dynamic_tools));
    if let Some(model) = &config.model {
        params.insert("model".to_string(), json!(model));
    }
    if let Some(system_prompt) = &config.system_prompt {
        params.insert("developerInstructions".to_string(), json!(system_prompt));
    }
    Value::Object(params)
}

/// Map app-server NDJSON-style events to StreamEvents.
/// Reuses the same normalization logic as CLI mode where possible.
fn map_app_server_event_to_stream_event(
    value: &Value,
    thread_id: &mut Option<String>,
) -> Option<StreamEvent> {
    // Delegate to the existing CLI mapper for compatible events
    crate::codex::map_app_server_compatible_event(value, thread_id)
}

/// Normalize `thread/tokenUsage/updated` into the shared Codex token-count
/// event shape. The app-server calls the aggregate value `total` and the live
/// context value `last`; only `last.totalTokens` is meaningful for context
/// fill, matching Codex's native TUI calculation.
fn map_token_usage_notification(params: &Value) -> Option<StreamEvent> {
    let token_usage = params.get("tokenUsage")?;
    let last = token_usage.get("last")?;
    let total_tokens = last.get("totalTokens").and_then(|value| value.as_u64())?;
    if total_tokens == 0 {
        return None;
    }

    Some(StreamEvent {
        event_type: "codex_token_count".to_string(),
        data: json!({
            "session_id": params.get("threadId").and_then(|value| value.as_str()),
            "info": {
                "last_token_usage": {
                    "total_tokens": total_tokens,
                    "output_tokens": last.get("outputTokens").and_then(|value| value.as_u64()).unwrap_or(0),
                },
                "model_context_window": token_usage.get("modelContextWindow").and_then(|value| value.as_u64()),
            },
            "provider_event_type": "thread/tokenUsage/updated",
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_server_control::{
        AttemptKey, AttemptRegistrationOutcome, ProviderLifecycleSignal,
    };
    use crate::model_control::call_control::{
        ModelCallControl, NoopModelCallControl, StoreBackedModelCallControl,
    };
    use crate::model_control::{AdmissionDecision, admit_invocation};
    use crate::store::Store;
    use crate::tool_registry::ToolSpec;
    use rsi_common::agent_coordination::{CorrelationStateV1, MessageAttemptFenceV1};
    use rsi_common::types::SessionKind;
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    /// A fixed attempt fence, so ingress tests can register a real attempt and
    /// assert against the coalescing latch rather than a daemon-wide boolean.
    fn ingress_test_fence(attempt_number: u32) -> MessageAttemptFenceV1 {
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

    async fn root_permit(
        store: &Arc<Mutex<Store>>,
        session_id: uuid::Uuid,
    ) -> crate::model_control::AdmissionPermit {
        let request = crate::model_control::ModelAdmissionRequest {
            purpose: rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
            provider: Some("CodexAppServer".to_string()),
            model: Some("gpt-5.4".to_string()),
            backend: Some("CodexAppServer".to_string()),
            effort: Some("high".to_string()),
            trigger: "test_codex_app_server".to_string(),
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
                Some("CodexAppServer"),
                Some("CodexAppServer"),
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

    fn turn_control(
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
            "CodexAppServer",
            Some("gpt-5.4".to_string()),
            "CodexAppServer",
            Some("high".to_string()),
            "test_codex_app_server",
            permit,
            rsi_common::model_control::ModelInvocationPurpose::SessionCodexAppServerTurn,
            None,
            crate::model_control::registry::RuntimeExecutionRoute::CodexAppServer,
        ))
    }

    fn launch_config_for_prompt_test(query: &str, system_prompt: Option<&str>) -> LaunchConfig {
        LaunchConfig {
            query: query.to_string(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            working_dir: None,
            provider: None,
            model: None,
            configured_context_window: None,
            max_turns: None,
            system_prompt: system_prompt.map(str::to_string),
            resume_session_id: None,
            session_kind: Some(SessionKind::Task),
            project_id: None,
            rsi_session_id: None,
            rsi_socket: None,
            rsi_session_token: None,
            continued_from: None,
            openai_base_url: None,
            openai_api_key: None,
            conversation_history: None,
            workflow_id: None,
            workflow_id_override: None,
            max_retries: None,
            group_id: None,
            parent_id: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: None,
            model_invocation_request_fingerprint: None,
            skip_project_model_default: false,
            model_invocation_purpose:
                rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
            sandbox: None,
            cargo_target_dir: None,
            execution_scratch: None,
            is_eval: false,
            skip_context_pipeline: false,
            capability_class: None,
            tags: vec![],
            topology_node_id: None,
            topology_iteration: 0,
            closure_selector: None,
        }
    }

    #[test]
    fn test_initialize_request_serialization() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "rsi",
                    "version": "0.1.0",
                }
            }
        });
        let serialized = serde_json::to_string(&request).unwrap();
        let parsed: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["method"], "initialize");
    }

    #[test]
    fn test_thread_start_request_serialization() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "thread/start",
            "params": {
                "input": "test query",
                "cwd": "/tmp",
                "dynamicTools": [],
            }
        });
        let serialized = serde_json::to_string(&request).unwrap();
        let parsed: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["method"], "thread/start");
        assert_eq!(parsed["params"]["input"], "test query");
    }

    #[test]
    fn thread_start_params_keep_input_as_query_body_and_send_developer_instructions_separately() {
        let config = launch_config_for_prompt_test(
            "Implement only the child task body.",
            Some("RSI worker preamble and contract"),
        );
        let params = build_thread_start_params(&config, Path::new("/tmp/work"), &[]);

        assert_eq!(params["input"], "Implement only the child task body.");
        assert_eq!(
            params["developerInstructions"],
            "RSI worker preamble and contract"
        );
        assert_eq!(params["cwd"], "/tmp/work");
        assert_eq!(params["dynamicTools"], json!([]));
        assert!(params.get("developer_instructions").is_none());

        let input = params["input"].as_str().unwrap();
        assert!(!input.contains("RSI worker preamble"));
        assert!(!input.contains("<docregblock>"));
        assert!(!input.contains("/spawn_child"));
    }

    #[test]
    fn ordinary_app_server_review_launch_gets_current_invocation_without_authority_token() {
        let session_id = uuid::Uuid::new_v4();
        let invocation_id = uuid::Uuid::new_v4();
        let mut config = launch_config_for_prompt_test("independent review", None);
        config.rsi_session_id = Some(session_id);
        config.closure_selector = None;
        config.rsi_session_token = None;
        let command = build_app_server_command(
            Path::new("/usr/bin/codex"),
            &config,
            invocation_id,
            Path::new("/tmp"),
        )
        .unwrap();
        let env = command
            .as_std()
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            env.get(rsi_common::identity::ENV_SESSION_ID),
            Some(&session_id.to_string())
        );
        assert_eq!(
            env.get(rsi_common::identity::ENV_MODEL_INVOCATION_ID),
            Some(&invocation_id.to_string())
        );
        assert!(!env.contains_key(rsi_common::identity::ENV_SESSION_TOKEN));
    }

    #[test]
    fn app_server_command_stamps_authenticated_execution_scratch() {
        use crate::sandbox::execution_scratch::SandboxExecutionScratch;

        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"))
            .join("slice8-app-server-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::Builder::new()
            .prefix("scratch-")
            .tempdir_in(base)
            .unwrap();
        let scratch = SandboxExecutionScratch::prepare_for_test(root.path()).unwrap();
        let session_id = uuid::Uuid::new_v4();
        let invocation_id = uuid::Uuid::new_v4();
        let mut config = launch_config_for_prompt_test("scratch", None);
        config.rsi_session_id = Some(session_id);
        config.execution_scratch = Some(scratch.clone());
        let command = build_app_server_command(
            Path::new("/usr/bin/codex"),
            &config,
            invocation_id,
            root.path(),
        )
        .unwrap();
        let env = command
            .as_std()
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            env.get(rsi_common::identity::ENV_CARGO_TARGET_DIR),
            Some(&scratch.target().display().to_string())
        );
        assert_eq!(
            env.get(rsi_common::identity::ENV_TMPDIR),
            Some(&scratch.temp().display().to_string())
        );
        assert_eq!(
            env.get(rsi_common::identity::ENV_SESSION_ID),
            Some(&session_id.to_string())
        );
        assert_eq!(
            env.get(rsi_common::identity::ENV_MODEL_INVOCATION_ID),
            Some(&invocation_id.to_string())
        );
        assert_eq!(
            env.get(rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE),
            Some(&rsi_common::identity::process_ownership_namespace())
        );
    }

    #[test]
    fn ordinary_app_server_environment_preserves_pre_slice8_ownership_only() {
        let root = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"));
        let config = launch_config_for_prompt_test("ordinary", None);
        let command = build_app_server_command(
            Path::new("/usr/bin/codex"),
            &config,
            uuid::Uuid::new_v4(),
            &root,
        )
        .unwrap();
        let env = command
            .as_std()
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert!(!env.contains_key(rsi_common::identity::ENV_CARGO_TARGET_DIR));
        assert!(!env.contains_key(rsi_common::identity::ENV_TMPDIR));
        assert_eq!(
            env.get(rsi_common::identity::ENV_PROCESS_OWNERSHIP_NAMESPACE),
            Some(&rsi_common::identity::process_ownership_namespace())
        );
    }

    #[test]
    fn thread_start_params_omit_developer_instructions_when_no_system_prompt() {
        let config = launch_config_for_prompt_test("plain query", None);
        let params = build_thread_start_params(&config, Path::new("/tmp/work"), &[]);

        assert_eq!(params["input"], "plain query");
        assert!(params.get("developerInstructions").is_none());
    }

    #[test]
    fn thread_start_params_preserve_dynamic_tool_shape() {
        let config = launch_config_for_prompt_test("query", Some("contract"));
        let specs = vec![ToolSpec {
            name: "rsi_memory_search".to_string(),
            description: "Search RSI memory".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                }
            }),
        }];

        let params = build_thread_start_params(&config, Path::new("/tmp/work"), &specs);
        assert_eq!(params["dynamicTools"][0]["name"], "rsi_memory_search");
        assert_eq!(
            params["dynamicTools"][0]["input_schema"],
            specs[0].parameters.clone()
        );
    }

    #[test]
    fn thread_start_params_preserve_manager_tool_schemas() {
        use rsi_common::agent_control_schema::AgentControlVerbV1;

        let config = launch_config_for_prompt_test("manager request", Some("contract"));
        let specs: Vec<_> = [
            AgentControlVerbV1::ManagerProgress,
            AgentControlVerbV1::ManagerInbox,
            AgentControlVerbV1::ManagerSend,
            AgentControlVerbV1::ManagerReply,
            AgentControlVerbV1::ManagerPrepareControl,
            AgentControlVerbV1::ManagerCommitPreparedControl,
            AgentControlVerbV1::ManagerGetAction,
        ]
        .into_iter()
        .map(|verb| {
            let descriptor = verb.descriptor();
            ToolSpec {
                name: descriptor.native_tool.unwrap().name().to_string(),
                description: descriptor.description.to_string(),
                parameters: descriptor.parameters(),
            }
        })
        .collect();
        let params = build_thread_start_params(&config, Path::new("/tmp/work"), &specs);
        let dynamic_tools = params["dynamicTools"].as_array().unwrap();
        assert_eq!(dynamic_tools.len(), 7);
        for (tool, spec) in dynamic_tools.iter().zip(&specs) {
            assert_eq!(tool["name"], spec.name);
            assert_eq!(tool["description"], spec.description);
            assert_eq!(tool["input_schema"], spec.parameters);
            assert_eq!(tool["input_schema"]["additionalProperties"], false);
        }
    }

    #[test]
    fn test_turn_start_request_serialization() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": {
                "threadId": "thread-abc",
                "input": "continue",
                "cwd": "/home/user",
            }
        });
        let serialized = serde_json::to_string(&request).unwrap();
        let parsed: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["method"], "turn/start");
        assert_eq!(parsed["params"]["threadId"], "thread-abc");
    }

    #[test]
    fn test_thread_start_response_parses_thread_id() {
        let result = json!({
            "thread": {
                "id": "thread-xyz-123",
            },
        });
        assert_eq!(
            thread_id_from_result(&result).as_deref(),
            Some("thread-xyz-123")
        );
    }

    #[tokio::test]
    async fn thread_start_json_rpc_error_and_empty_thread_id_settle_the_admission() {
        for (response_envelope, expected_error_class) in [
            (
                json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "error": {
                        "code": -32000,
                        "message": "deterministic rejection",
                    },
                }),
                "thread_start_json_rpc_error",
            ),
            (
                json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "result": {"thread": {"id": "   "}},
                }),
                "thread_start_missing_thread_id",
            ),
        ] {
            let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
            let session_id = uuid::Uuid::new_v4();
            let permit = root_permit(&store, session_id).await;
            let invocation_id = permit.invocation_id();
            let control = turn_control(&store, session_id, permit);
            let admitted = control
                .admit(ModelCallKind::Primary, "thread_start", "query", None)
                .await
                .expect("initial admission");
            let (settlement, execution) = admitted.into_parts();
            drop(execution);
            let mut current_turn_call = Some(settlement);
            let test_plane = Arc::new(AppServerControlPlane::new());
            let (response_tx, mut response_rx) = mpsc::channel::<(i64, AppServerResponse)>(1);
            let (notification_tx, _notification_rx) = mpsc::channel::<AppServerNotification>(1);
            let (event_tx, _event_rx) = mpsc::channel::<StreamEvent>(1);
            let mut routed_thread_id = None;

            route_app_server_message(
                response_envelope,
                &test_plane,
                &response_tx,
                &notification_tx,
                &event_tx,
                &mut routed_thread_id,
            );
            let (response_id, response) = response_rx.recv().await.expect("routed response");
            assert_eq!(response_id, 7);
            settle_thread_start_response(&control, &mut current_turn_call, response)
                .await
                .expect_err("invalid thread/start response must fail");
            assert!(current_turn_call.is_none());

            let guard = store.lock().await;
            let row: (String, Option<String>) = guard
                .conn
                .query_row(
                    "SELECT status, error_class FROM model_invocations WHERE id = ?1",
                    rusqlite::params![invocation_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("invocation row");
            assert_eq!(row.0, "failed");
            assert_eq!(row.1.as_deref(), Some(expected_error_class));
        }
    }

    #[test]
    fn terminal_error_notification_normalizes_but_retrying_error_remains_control_only() {
        let terminal = app_server_notification_to_stream_event(
            "error",
            json!({
                "error": {"message": "terminal failure"},
                "threadId": "thread-real",
                "turnId": "turn-real",
                "willRetry": false,
            }),
        )
        .expect("terminal error event");
        assert_eq!(terminal.event_type, "process_error");
        assert_eq!(terminal.data["error_class"], "app_server_terminal_error");

        assert!(
            app_server_notification_to_stream_event(
                "error",
                json!({
                    "error": {"message": "retrying"},
                    "threadId": "thread-real",
                    "turnId": "turn-real",
                    "willRetry": true,
                }),
            )
            .is_none(),
            "retrying provider errors do not settle the active turn"
        );
    }

    #[tokio::test]
    async fn post_admission_missing_binary_settles_before_client_ownership_is_released() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(crate::bus::EventBus::new(8));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let invocation_id = permit.invocation_id();

        let result = CodexAppServerClient::new_after_admission_with(&store, &bus, &permit, || {
            Err(DaemonError::CodexBinaryNotFound)
        })
        .await;
        assert!(matches!(result, Err(DaemonError::CodexBinaryNotFound)));

        let guard = store.lock().await;
        let row: (String, Option<String>) = guard
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations WHERE id = ?1",
                rusqlite::params![invocation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("invocation row");
        assert_eq!(row.0, "failed");
        assert_eq!(
            row.1.as_deref(),
            Some("codex_app_server_client_construction_failed")
        );
    }

    #[test]
    fn test_approval_request_normalization_all_methods() {
        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
        ] {
            for id in [json!(42), json!("42")] {
                let (responses, _) = mpsc::channel(1);
                let (notifications, _) = mpsc::channel(1);
                let (events, mut event_rx) = mpsc::channel(1);
                let params = approval_protocol_tests::params();
                approval_protocol_tests::assert_request(method, &id, &params);
                route_app_server_message(
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
                    &AppServerControlPlane::default(),
                    &responses,
                    &notifications,
                    &events,
                    &mut None,
                );
                let event = event_rx.try_recv().unwrap();
                assert_eq!(event.event_type, "approval_request");
                assert_eq!(event.data["request_id"], id);
                assert_eq!(event.data["method"], method);
                assert_eq!(event.data["params"], params);
            }
        }
    }

    #[test]
    fn test_tool_call_event_normalization() {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "item/tool/call",
            "params": {
                "name": "rsi_memory_search",
                "arguments": {
                    "query": "context rotation",
                    "max_results": 5
                }
            }
        });

        let req_id = msg.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let params = msg.get("params").cloned().unwrap_or_default();
        let tool_name = params
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let event = StreamEvent {
            event_type: "tool_call".to_string(),
            data: json!({
                "call_id": req_id,
                "name": tool_name,
                "arguments": params.get("arguments").cloned().unwrap_or_default(),
            }),
        };

        assert_eq!(event.event_type, "tool_call");
        assert_eq!(event.data["call_id"], 99);
        assert_eq!(event.data["name"], "rsi_memory_search");
    }

    #[tokio::test]
    async fn test_send_approval_serializes_correct_jsonrpc() {
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(4);
        let (_, event_rx) = mpsc::channel::<StreamEvent>(4);
        let next_id = Arc::new(AtomicI64::new(1));

        let mut session = CodexAppServerSession {
            thread_id: "thread-test".to_string(),
            working_dir: PathBuf::from("/tmp"),
            write_tx,
            event_rx,
            next_id,
            turn_control: Arc::new(NoopModelCallControl),
            current_turn_call: None,
        };

        session
            .send_approval(
                &json!(42),
                "item/commandExecution/requestApproval",
                &approval_protocol_tests::params(),
                ApprovalDecision::Approve,
            )
            .await
            .unwrap();

        let bytes = write_rx.recv().await.unwrap();
        let msg: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(msg["jsonrpc"], "2.0");
        assert_eq!(msg["id"], 42);
        approval_protocol_tests::assert_response(
            "item/commandExecution/requestApproval",
            &json!(42),
            &msg,
        );
        assert_eq!(msg["result"], json!({"decision":"accept"}));
    }

    #[tokio::test]
    async fn test_send_approval_deny() {
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(4);
        let (_, event_rx) = mpsc::channel::<StreamEvent>(4);
        let next_id = Arc::new(AtomicI64::new(1));

        let mut session = CodexAppServerSession {
            thread_id: "thread-test".to_string(),
            working_dir: PathBuf::from("/tmp"),
            write_tx,
            event_rx,
            next_id,
            turn_control: Arc::new(NoopModelCallControl),
            current_turn_call: None,
        };

        session
            .send_approval(
                &json!(7),
                "item/fileChange/requestApproval",
                &approval_protocol_tests::params(),
                ApprovalDecision::Deny,
            )
            .await
            .unwrap();

        let bytes = write_rx.recv().await.unwrap();
        let msg: Value = serde_json::from_slice(&bytes).unwrap();
        approval_protocol_tests::assert_response(
            "item/fileChange/requestApproval",
            &json!(7),
            &msg,
        );
        assert_eq!(msg["result"], json!({"decision":"decline"}));
    }

    #[tokio::test]
    async fn test_start_turn_serializes_correct_request() {
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(4);
        let (_, event_rx) = mpsc::channel::<StreamEvent>(4);
        let next_id = Arc::new(AtomicI64::new(1));

        let mut session = CodexAppServerSession {
            thread_id: "thread-abc".to_string(),
            working_dir: PathBuf::from("/home/user"),
            write_tx,
            event_rx,
            next_id,
            turn_control: Arc::new(NoopModelCallControl),
            current_turn_call: None,
        };

        let config = TurnConfig {
            input: "continue working".to_string(),
            working_dir: None,
        };

        let turn_id = session.start_turn(&config).await.unwrap();
        assert_eq!(turn_id.0, "thread-abc");

        let bytes = write_rx.recv().await.unwrap();
        let msg: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(msg["method"], "turn/start");
        assert_eq!(msg["params"]["threadId"], "thread-abc");
        assert_eq!(msg["params"]["input"], "continue working");
    }

    #[tokio::test]
    async fn test_start_turn_denial_prevents_second_send() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        {
            let guard = store.lock().await;
            guard
                .conn
                .execute(
                    "INSERT INTO model_budget_policies (
                        policy_key, scope_kind, scope_id, max_calls, updated_at
                     ) VALUES (?1, 'session', ?2, 1, datetime('now'))",
                    rusqlite::params![
                        format!("session-max-calls:{session_id}"),
                        session_id.to_string()
                    ],
                )
                .expect("policy insert");
        }
        let permit = root_permit(&store, session_id).await;
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(4);
        let (_, event_rx) = mpsc::channel::<StreamEvent>(4);
        let mut session = CodexAppServerSession {
            thread_id: "thread-abc".to_string(),
            working_dir: PathBuf::from("/home/user"),
            write_tx,
            event_rx,
            next_id: Arc::new(AtomicI64::new(1)),
            turn_control: turn_control(&store, session_id, permit),
            current_turn_call: None,
        };
        let config = TurnConfig {
            input: "continue working".to_string(),
            working_dir: None,
        };

        session.start_turn(&config).await.expect("first turn");
        write_rx.recv().await.expect("first send");

        let error = session
            .start_turn(&config)
            .await
            .expect_err("second turn denied");
        assert!(error.to_string().contains("prior turn is still unsettled"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), write_rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_turn_completed_settles_current_turn_exactly_once() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let invocation_id = permit.invocation_id();
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(4);
        let (event_tx, event_rx) = mpsc::channel::<StreamEvent>(4);
        let mut session = CodexAppServerSession {
            thread_id: "thread-abc".to_string(),
            working_dir: PathBuf::from("/home/user"),
            write_tx,
            event_rx,
            next_id: Arc::new(AtomicI64::new(1)),
            turn_control: turn_control(&store, session_id, permit),
            current_turn_call: None,
        };

        session
            .start_turn(&TurnConfig {
                input: "continue working".to_string(),
                working_dir: None,
            })
            .await
            .expect("turn starts");
        write_rx.recv().await.expect("turn request");

        event_tx
            .send(StreamEvent {
                event_type: "result".to_string(),
                data: json!({
                    "subtype": "turn_completed",
                    "usage": {
                        "input_tokens": 34,
                        "output_tokens": 8,
                    },
                }),
            })
            .await
            .expect("send event");
        let event = session.next_event().await.expect("completed event");
        assert_eq!(event.event_type, "result");
        drop(event_tx);
        assert!(session.next_event().await.is_none());

        let guard = store.lock().await;
        let settled = guard
            .conn
            .query_row(
                "SELECT status, input_tokens, output_tokens, error_class
                 FROM model_invocations WHERE id = ?1",
                rusqlite::params![invocation_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .expect("settled row");
        assert_eq!(settled.0, "completed");
        assert_eq!(settled.1, Some(34));
        assert_eq!(settled.2, Some(8));
        assert_eq!(settled.3, None);
    }

    #[tokio::test]
    async fn completed_turn_requires_a_new_admission_for_the_next_send() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let root_invocation_id = permit.invocation_id();
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(4);
        let (event_tx, event_rx) = mpsc::channel::<StreamEvent>(4);
        let mut session = CodexAppServerSession {
            thread_id: "thread-abc".to_string(),
            working_dir: PathBuf::from("/home/user"),
            write_tx,
            event_rx,
            next_id: Arc::new(AtomicI64::new(1)),
            turn_control: turn_control(&store, session_id, permit),
            current_turn_call: None,
        };
        let config = TurnConfig {
            input: "continue working".to_string(),
            working_dir: None,
        };

        session.start_turn(&config).await.expect("first turn");
        write_rx.recv().await.expect("first admitted send");
        event_tx
            .send(StreamEvent {
                event_type: "result".to_string(),
                data: json!({"subtype": "turn_completed", "usage": {}}),
            })
            .await
            .expect("first completion");
        session.next_event().await.expect("first completion event");

        session.start_turn(&config).await.expect("second turn");
        write_rx
            .recv()
            .await
            .expect("second separately admitted send");
        event_tx
            .send(StreamEvent {
                event_type: "result".to_string(),
                data: json!({"subtype": "turn_completed", "usage": {}}),
            })
            .await
            .expect("second completion");
        session.next_event().await.expect("second completion event");

        let guard = store.lock().await;
        let rows: Vec<(String, Option<String>)> = {
            let mut stmt = guard
                .conn
                .prepare(
                    "SELECT id, parent_invocation_id
                     FROM model_invocations
                     WHERE session_id = ?1 AND admission_status = 'admitted'
                     ORDER BY created_at",
                )
                .expect("prepare invocation query");
            stmt.query_map(rusqlite::params![session_id.to_string()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("query invocation rows")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect invocation rows")
        };
        assert_eq!(rows.len(), 2);
        let root_invocation_id = root_invocation_id.to_string();
        assert_eq!(rows[0].0, root_invocation_id);
        assert_eq!(
            rows[1].1.as_deref(),
            Some(root_invocation_id.as_str()),
            "the repeated visible turn has its own child lineage"
        );
    }

    #[tokio::test]
    async fn real_completion_notification_settles_initial_thread_start_and_allows_followup() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let root_invocation_id = permit.invocation_id();
        let control = turn_control(&store, session_id, permit);
        let admitted = control
            .admit(
                ModelCallKind::Primary,
                "thread_start",
                "initial query",
                None,
            )
            .await
            .expect("initial thread/start admission");
        let (settlement, execution) = admitted.into_parts();
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(4);
        let next_id = Arc::new(AtomicI64::new(1));
        send_admitted_app_server_request(
            &write_tx,
            &next_id,
            "thread/start",
            json!({"input": "initial query", "cwd": "/tmp", "dynamicTools": []}),
            execution,
        )
        .await
        .and_then(require_enqueued)
        .expect("initial thread/start send");
        let initial_request: Value =
            serde_json::from_slice(&write_rx.recv().await.expect("initial request"))
                .expect("initial JSON-RPC request");
        assert_eq!(initial_request["method"], "thread/start");

        let (event_tx, event_rx) = mpsc::channel::<StreamEvent>(4);
        let (response_tx, _response_rx) = mpsc::channel::<(i64, AppServerResponse)>(4);
        let (notification_tx, notification_rx) = mpsc::channel::<AppServerNotification>(4);
        let test_plane = Arc::new(AppServerControlPlane::new());
        tokio::spawn(forward_app_server_notifications(
            notification_rx,
            event_tx.clone(),
            Arc::clone(&test_plane),
        ));
        let mut session = CodexAppServerSession {
            thread_id: "thread-real".to_string(),
            working_dir: PathBuf::from("/tmp"),
            write_tx,
            event_rx,
            next_id,
            turn_control: Arc::clone(&control),
            current_turn_call: Some(settlement),
        };
        let mut routed_thread_id = None;

        route_app_server_message(
            json!({
                "jsonrpc": "2.0",
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-real",
                    "turn": {
                        "id": "turn-initial",
                        "status": "completed",
                        "items": [],
                    }
                }
            }),
            &test_plane,
            &response_tx,
            &notification_tx,
            &event_tx,
            &mut routed_thread_id,
        );
        let completion = session.next_event().await.expect("completion event");
        assert_eq!(completion.event_type, "result");
        assert_eq!(completion.data["provider_event_type"], "turn/completed");

        session
            .start_turn(&TurnConfig {
                input: "legal followup".to_string(),
                working_dir: None,
            })
            .await
            .expect("first followup turn is admitted");
        let followup_request: Value =
            serde_json::from_slice(&write_rx.recv().await.expect("followup request"))
                .expect("followup JSON-RPC request");
        assert_eq!(followup_request["method"], "turn/start");

        let guard = store.lock().await;
        let rows: Vec<(String, Option<String>, String)> = {
            let mut statement = guard
                .conn
                .prepare(
                    "SELECT id, parent_invocation_id, status
                     FROM model_invocations
                     WHERE session_id = ?1
                     ORDER BY created_at",
                )
                .expect("prepare invocation query");
            statement
                .query_map(rusqlite::params![session_id.to_string()], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .expect("query rows")
                .collect::<std::result::Result<Vec<_>, _>>()
                .expect("collect rows")
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, root_invocation_id.to_string());
        assert_eq!(rows[0].2, "completed");
        assert_eq!(
            rows[1].1.as_deref(),
            Some(root_invocation_id.to_string().as_str())
        );
        assert_eq!(rows[1].2, "running");
    }

    #[tokio::test]
    async fn denied_followup_turn_performs_zero_additional_sends() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        {
            let guard = store.lock().await;
            guard
                .conn
                .execute(
                    "INSERT INTO model_budget_policies (
                        policy_key, scope_kind, scope_id, max_calls, updated_at
                     ) VALUES (?1, 'session', ?2, 1, datetime('now'))",
                    rusqlite::params![
                        format!("session-max-calls:{session_id}"),
                        session_id.to_string()
                    ],
                )
                .expect("policy insert");
        }
        let permit = root_permit(&store, session_id).await;
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(4);
        let (event_tx, event_rx) = mpsc::channel::<StreamEvent>(4);
        let mut session = CodexAppServerSession {
            thread_id: "thread-abc".to_string(),
            working_dir: PathBuf::from("/home/user"),
            write_tx,
            event_rx,
            next_id: Arc::new(AtomicI64::new(1)),
            turn_control: turn_control(&store, session_id, permit),
            current_turn_call: None,
        };
        let config = TurnConfig {
            input: "continue working".to_string(),
            working_dir: None,
        };

        session.start_turn(&config).await.expect("first turn");
        write_rx.recv().await.expect("first admitted send");
        event_tx
            .send(StreamEvent {
                event_type: "result".to_string(),
                data: json!({"subtype": "turn_completed", "usage": {}}),
            })
            .await
            .expect("completion");
        session.next_event().await.expect("completion event");

        let error = session
            .start_turn(&config)
            .await
            .expect_err("followup admission denied");
        assert!(matches!(error, DaemonError::PolicyDenied(_)));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), write_rx.recv())
                .await
                .is_err(),
            "denial reaches no AppServer send"
        );
    }

    #[tokio::test]
    async fn dropping_session_settles_the_in_flight_turn() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let invocation_id = permit.invocation_id();
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(4);
        let (_, event_rx) = mpsc::channel::<StreamEvent>(4);
        let mut session = CodexAppServerSession {
            thread_id: "thread-abc".to_string(),
            working_dir: PathBuf::from("/home/user"),
            write_tx,
            event_rx,
            next_id: Arc::new(AtomicI64::new(1)),
            turn_control: turn_control(&store, session_id, permit),
            current_turn_call: None,
        };

        session
            .start_turn(&TurnConfig {
                input: "continue working".to_string(),
                working_dir: None,
            })
            .await
            .expect("turn starts");
        write_rx.recv().await.expect("turn request");
        drop(session);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let row = {
                    let guard = store.lock().await;
                    guard
                        .conn
                        .query_row(
                            "SELECT status, error_class
                             FROM model_invocations WHERE id = ?1",
                            rusqlite::params![invocation_id.to_string()],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                        )
                        .expect("invocation row")
                };
                if row.0 != "running" {
                    assert_eq!(row.0, "failed");
                    assert_eq!(row.1.as_deref(), Some("model_call_task_exited"));
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop settlement completes");
    }

    #[tokio::test]
    async fn turn_start_send_failure_settles_the_admitted_permit() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let invocation_id = permit.invocation_id();
        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(1);
        drop(write_rx);
        let (_, event_rx) = mpsc::channel::<StreamEvent>(1);
        let mut session = CodexAppServerSession {
            thread_id: "thread-abc".to_string(),
            working_dir: PathBuf::from("/home/user"),
            write_tx,
            event_rx,
            next_id: Arc::new(AtomicI64::new(1)),
            turn_control: turn_control(&store, session_id, permit),
            current_turn_call: None,
        };

        session
            .start_turn(&TurnConfig {
                input: "continue working".to_string(),
                working_dir: None,
            })
            .await
            .expect_err("closed writer rejects turn/start");

        let guard = store.lock().await;
        let settled = guard
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations WHERE id = ?1",
                rusqlite::params![invocation_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .expect("settled row");
        assert_eq!(settled.0, "failed");
        assert_eq!(settled.1.as_deref(), Some("turn_start_send_failed"));
    }

    #[tokio::test]
    async fn test_process_error_settles_current_turn_once() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let permit = root_permit(&store, session_id).await;
        let invocation_id = permit.invocation_id();
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(4);
        let (event_tx, event_rx) = mpsc::channel::<StreamEvent>(4);
        let (response_tx, _response_rx) = mpsc::channel::<(i64, AppServerResponse)>(4);
        let (notification_tx, notification_rx) = mpsc::channel::<AppServerNotification>(4);
        let test_plane = Arc::new(AppServerControlPlane::new());
        tokio::spawn(forward_app_server_notifications(
            notification_rx,
            event_tx.clone(),
            Arc::clone(&test_plane),
        ));
        let mut session = CodexAppServerSession {
            thread_id: "thread-abc".to_string(),
            working_dir: PathBuf::from("/home/user"),
            write_tx,
            event_rx,
            next_id: Arc::new(AtomicI64::new(1)),
            turn_control: turn_control(&store, session_id, permit),
            current_turn_call: None,
        };

        session
            .start_turn(&TurnConfig {
                input: "continue working".to_string(),
                working_dir: None,
            })
            .await
            .expect("turn starts");
        write_rx.recv().await.expect("turn request");

        let mut routed_thread_id = None;
        route_app_server_message(
            json!({
                "jsonrpc": "2.0",
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-abc",
                    "turn": {
                        "id": "turn-failed",
                        "status": "failed",
                        "items": [],
                        "error": {"message": "boom"},
                    }
                }
            }),
            &test_plane,
            &response_tx,
            &notification_tx,
            &event_tx,
            &mut routed_thread_id,
        );
        let event = session.next_event().await.expect("error event");
        assert_eq!(event.event_type, "process_error");
        drop(notification_tx);
        drop(event_tx);
        assert!(session.next_event().await.is_none());

        let guard = store.lock().await;
        let settled = guard
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations WHERE id = ?1",
                rusqlite::params![invocation_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .expect("settled row");
        assert_eq!(settled.0, "failed");
        assert_eq!(settled.1.as_deref(), Some("turn_failed"));
    }

    #[test]
    fn test_supports_multi_turn_true() {
        let (_, event_rx) = mpsc::channel::<StreamEvent>(1);
        let (write_tx, _) = mpsc::channel::<Vec<u8>>(1);
        let session = CodexAppServerSession {
            thread_id: "t".to_string(),
            working_dir: PathBuf::from("/tmp"),
            write_tx,
            event_rx,
            next_id: Arc::new(AtomicI64::new(1)),
            turn_control: Arc::new(NoopModelCallControl),
            current_turn_call: None,
        };
        assert!(session.supports_multi_turn());
    }

    #[test]
    fn test_supports_approvals_true() {
        let (_, event_rx) = mpsc::channel::<StreamEvent>(1);
        let (write_tx, _) = mpsc::channel::<Vec<u8>>(1);
        let session = CodexAppServerSession {
            thread_id: "t".to_string(),
            working_dir: PathBuf::from("/tmp"),
            write_tx,
            event_rx,
            next_id: Arc::new(AtomicI64::new(1)),
            turn_control: Arc::new(NoopModelCallControl),
            current_turn_call: None,
        };
        assert!(session.supports_approvals());
    }

    #[test]
    fn token_usage_notification_normalizes_current_window_usage() {
        let event = map_token_usage_notification(&json!({
            "threadId": "thread-123",
            "turnId": "turn-123",
            "tokenUsage": {
                "total": { "totalTokens": 686719 },
                "last": { "totalTokens": 130492, "outputTokens": 640 },
                "modelContextWindow": 258400,
            }
        }))
        .expect("valid current-window notification");

        assert_eq!(event.event_type, "codex_token_count");
        let usage = crate::monitor::extract_codex_context_usage(&event).unwrap();
        assert_eq!(usage.context_tokens, 130_492);
        assert_eq!(usage.output_tokens, 640);
        assert_eq!(usage.context_window, Some(258_400));
    }

    /// A mailbox with no consumer left is CLOSED, not overflowing. That is the
    /// pre-existing post-handshake state of `response_rx`, so treating it as
    /// overflow would terminate every healthy live session.
    #[tokio::test]
    async fn a_closed_mailbox_is_not_overflow_and_never_terminates_ingress() {
        let plane = Arc::new(AppServerControlPlane::new());
        let (response_tx, response_rx) = mpsc::channel::<(i64, AppServerResponse)>(1);
        let (notification_tx, notification_rx) = mpsc::channel::<AppServerNotification>(1);
        let (event_tx, event_rx) = mpsc::channel::<StreamEvent>(1);
        // Exactly what happens once `launch` returns.
        drop(response_rx);
        drop(notification_rx);
        drop(event_rx);

        let mut thread_id = None;
        for frame in [
            json!({"jsonrpc": "2.0", "id": 7, "result": {}}),
            json!({"jsonrpc": "2.0", "method": "turn/completed", "params": {}}),
            json!({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {}}),
        ] {
            assert_eq!(
                route_app_server_message(
                    frame.clone(),
                    &plane,
                    &response_tx,
                    &notification_tx,
                    &event_tx,
                    &mut thread_id,
                ),
                IngressOutcome::Continue,
                "{frame} reached a closed mailbox, which is not overflow"
            );
        }
        assert!(
            !plane.provider_death_observed(),
            "a closed mailbox must never be mistaken for a terminal provider fact"
        );
    }

    /// PARTIAL backpressure: a mailbox that fills and then DRAINS.
    ///
    /// Every other overflow test saturates a mailbox permanently, so none of
    /// them can tell a transient backlog from a fatal one. Ordinary live turns
    /// back up the 100-slot `event_tx` routinely (a busy monitor under DB
    /// pressure), and the router must recover rather than stay poisoned.
    #[tokio::test]
    async fn a_mailbox_that_fills_then_drains_resumes_delivering_evidence() {
        let plane = Arc::new(AppServerControlPlane::new());
        // A registered attempt, so a seal would actually be observable rather
        // than silently latching nothing.
        let fence = ingress_test_fence(1);
        plane.register_attempt(
            &fence,
            CorrelationStateV1::CorrelationPending,
            std::time::Instant::now(),
        );

        let (response_tx, _response_rx) = mpsc::channel::<(i64, AppServerResponse)>(4);
        let (notification_tx, _notification_rx) = mpsc::channel::<AppServerNotification>(4);
        let (event_tx, mut event_rx) = mpsc::channel::<StreamEvent>(1);

        let mut thread_id = None;
        let saturate = |event_tx: &mpsc::Sender<StreamEvent>| {
            event_tx
                .try_send(StreamEvent {
                    event_type: "prime".to_string(),
                    data: json!({}),
                })
                .expect("saturate the event mailbox");
        };

        // --- ordinary suppressible evidence -------------------------------
        saturate(&event_tx);
        let token_usage = json!({"jsonrpc": "2.0", "method": "thread/tokenUsage/updated",
                                 "params": {"threadId": "t1",
                                            "tokenUsage": {"last": {"totalTokens": 128,
                                                                    "outputTokens": 64},
                                                           "modelContextWindow": 258_400}}});
        assert_eq!(
            route_app_server_message(
                token_usage.clone(),
                &plane,
                &response_tx,
                &notification_tx,
                &event_tx,
                &mut thread_id,
            ),
            IngressOutcome::Continue,
            "ordinary evidence drops on a full mailbox"
        );
        assert_eq!(
            plane.dirty_latch_count(),
            0,
            "dropping suppressible evidence must not seal the attempt"
        );
        assert!(!plane.provider_death_observed());

        // The backlog clears, exactly as a monitor catching up would clear it.
        let _ = event_rx.recv().await.expect("drain the primed slot");
        assert_eq!(
            route_app_server_message(
                token_usage,
                &plane,
                &response_tx,
                &notification_tx,
                &event_tx,
                &mut thread_id,
            ),
            IngressOutcome::Continue
        );
        let recovered = event_rx
            .try_recv()
            .expect("a drained mailbox delivers again; the router is not poisoned");
        assert_eq!(
            recovered.event_type, "codex_token_count",
            "the frame after recovery is really delivered, not merely 'not an error'"
        );

        // --- non-suppressible evidence ------------------------------------
        // A provider request is the ordinary-path trigger: `item/tool/call`
        // and the approval methods carry both `method` and `id`, so they
        // classify ProviderRequest -> SealOrTerminate.
        saturate(&event_tx);
        let approval = json!({"jsonrpc": "2.0", "id": 9,
                              "method": "item/commandExecution/requestApproval",
                              "params": {"command": "ls"}});
        assert_eq!(
            route_app_server_message(
                approval.clone(),
                &plane,
                &response_tx,
                &notification_tx,
                &event_tx,
                &mut thread_id,
            ),
            IngressOutcome::TerminateIngress,
            "an undeliverable provider request fails closed"
        );
        let drained = plane.drain_dirty_latches(std::time::Instant::now());
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0].kind,
            ProviderLifecycleKind::OverflowSeal,
            "transient backpressure seals; it does not observe death"
        );
        assert!(!plane.provider_death_observed());

        // Once the backlog clears the same request is deliverable. The seal is
        // a per-offer fact about one refused frame, not a latched router mode.
        let _ = event_rx.recv().await.expect("drain the primed slot");
        assert_eq!(
            route_app_server_message(
                approval,
                &plane,
                &response_tx,
                &notification_tx,
                &event_tx,
                &mut thread_id,
            ),
            IngressOutcome::Continue,
            "a drained mailbox accepts the provider request"
        );
        let delivered = event_rx
            .try_recv()
            .expect("the approval request is really delivered after recovery");
        assert_eq!(delivered.event_type, "approval_request");
    }

    /// C-P2-15 permits a durable seal OR terminating the provider. The durable
    /// half needs the unbuilt control worker, so ingress must take the second
    /// branch: a bare `break` abandons the reader while the child keeps
    /// running, which wedges the session permanently.
    ///
    /// Driven through the real production reader against a REAL child process.
    #[tokio::test]
    async fn ingress_failing_closed_terminates_the_provider_instead_of_abandoning_it() {
        use std::os::unix::process::ExitStatusExt;

        // A real, live process standing in for the app-server.
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn a stand-in provider process");
        let child_pid = child.id();
        assert!(child_pid.is_some(), "the stand-in provider must have a PID");

        let plane = Arc::new(AppServerControlPlane::new());
        let (response_tx, _response_rx) = mpsc::channel::<(i64, AppServerResponse)>(4);
        let (notification_tx, _notification_rx) = mpsc::channel::<AppServerNotification>(4);
        // Capacity-1 and saturated: the next provider request cannot land.
        let (event_tx, _event_rx) = mpsc::channel::<StreamEvent>(1);
        event_tx
            .try_send(StreamEvent {
                event_type: "prime".to_string(),
                data: json!({}),
            })
            .expect("saturate the event mailbox");

        // One provider-request frame on stdout, which must fail closed.
        let stdout = std::io::Cursor::new(
            b"{\"jsonrpc\":\"2.0\",\"id\":9,\
              \"method\":\"item/commandExecution/requestApproval\",\
              \"params\":{\"command\":\"ls\"}}\n"
                .to_vec(),
        );

        run_app_server_stdout_reader(
            stdout,
            Arc::clone(&plane),
            response_tx,
            notification_tx,
            event_tx,
            child_pid,
        )
        .await;

        // The provider is genuinely dead, so `try_wait` on the retained Child
        // and stderr EOF both fire, and the turn can settle. Before this fix
        // the reader simply exited and this wait would hang forever.
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("the provider must be terminated, not left running")
            .expect("wait on the stand-in provider");
        assert_eq!(
            status.signal(),
            Some(nix::sys::signal::Signal::SIGKILL as i32),
            "ingress terminates the provider with SIGKILL, which a provider blocked \
             writing into a full stdout pipe cannot ignore"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // `start_admitted_message_turn` — the ONLY real production implementation
    // of the new trait method (H21-P2-R5-003).
    //
    // R5 found this method had ZERO tests: every test that reaches a
    // `Dispatched` outcome does so through `FakeProvider` in
    // `agent_message_delivery`, so the real busy-turn refusal, the real
    // `RejectedBeforeEnqueue → RejectedBeforeEffect` mapping, and the real
    // `current_turn_call` install were all unverified. These two tests drive
    // the REAL `CodexAppServerSession` against a REAL `Store`, a REAL admitted
    // permit and a REAL execution capability — no fake provider anywhere.
    // ══════════════════════════════════════════════════════════════════════

    /// Build a real admitted delivery call bound to its own attempt fence,
    /// exactly as `deliver_at_idle_boundary` does before it dispatches.
    async fn admitted_delivery_turn(
        store: &Arc<Mutex<Store>>,
        session_id: uuid::Uuid,
        attempt_number: u32,
    ) -> (NativeMessageTurnRequest, uuid::Uuid) {
        // Built here rather than through `root_permit` so this mirrors what
        // `deliver_at_idle_boundary::build_admission_request` actually admits:
        // purpose `SessionCodexAppServerTurn` (which is what resolves to the
        // `appserver_model_request` boundary, so the route claim below is a real
        // policy decision and not a nil-invocation test bypass), and a dedup key
        // unique per ATTEMPT — a later attempt at the same message is a
        // genuinely new paid call and must admit its own row.
        let purpose = rsi_common::model_control::ModelInvocationPurpose::SessionCodexAppServerTurn;
        let request = crate::model_control::ModelAdmissionRequest {
            purpose,
            provider: Some("CodexAppServer".to_string()),
            model: Some("gpt-5.4".to_string()),
            backend: Some("CodexAppServer".to_string()),
            effort: Some("high".to_string()),
            trigger: "agent_message_delivery".to_string(),
            owner: rsi_common::model_control::InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            dedup_key: Some(format!(
                "agent_message_delivery:{session_id}:{attempt_number}"
            )),
            request_fingerprint: Some(format!("sha256:delivery:{attempt_number}")),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                purpose,
                Some("CodexAppServer"),
                Some("CodexAppServer"),
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
        let permit = match admit_invocation(store, request, &bus)
            .await
            .expect("admit the delivery invocation")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { .. } => {
                panic!("each attempt must admit its own invocation row")
            }
        };
        let event_bus = Arc::new(crate::bus::EventBus::new(8));
        let settlements = crate::model_control::call_control::ModelCallSettlementWorker::new(
            Arc::clone(store),
            Arc::clone(&event_bus),
        )
        .expect("settlement worker")
        .handle()
        .expect("settlement producer");
        let call = crate::model_control::call_control::AdmittedModelCall::real(
            permit,
            crate::model_control::registry::RuntimeExecutionRoute::CodexAppServer,
            settlements,
        )
        .expect("claim a real AppServer execution capability");
        let invocation_id = call
            .invocation_id()
            .expect("a real delivery settlement retains its permit");
        let (settlement, execution) = call.into_parts();
        let mut fence = ingress_test_fence(attempt_number);
        // The fence must name the exact invocation the settlement carries, or
        // `NativeMessageTurnRequest::new` refuses to bind it.
        fence.delivery_model_invocation_id = invocation_id;
        let turn = NativeMessageTurnRequest::new(fence, settlement, execution)
            .expect("the fence names this settlement's own invocation");
        (turn, invocation_id)
    }

    /// A busy AppServer session refuses an admitted message turn BEFORE any
    /// effect, and hands the settlement back for the caller to settle.
    ///
    /// Delivery is idle-only: an active turn keeps the message queued for a
    /// later boundary rather than interrupting the provider. The refusal is
    /// PROVED pre-effect here rather than asserted — no byte reaches the writer
    /// channel, which is checked directly.
    #[tokio::test]
    async fn an_admitted_message_turn_is_refused_before_any_effect_while_a_turn_is_live() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(4);
        let (_, event_rx) = mpsc::channel::<StreamEvent>(4);

        // The slot is occupied by a genuine settlement, not by a placeholder.
        let (occupying_turn, occupying_invocation) =
            admitted_delivery_turn(&store, session_id, 1).await;
        let (_, occupying_settlement, _) = occupying_turn.into_parts();

        let mut session = CodexAppServerSession {
            thread_id: "thread-abc".to_string(),
            working_dir: PathBuf::from("/home/user"),
            write_tx,
            event_rx,
            next_id: Arc::new(AtomicI64::new(1)),
            turn_control: Arc::new(NoopModelCallControl),
            current_turn_call: Some(occupying_settlement),
        };

        let (turn, invocation_id) = admitted_delivery_turn(&store, session_id, 2).await;
        let config = TurnConfig {
            input: "delivered agent message".to_string(),
            working_dir: None,
        };

        match session
            .start_admitted_message_turn(turn, &config)
            .await
            .expect("a busy refusal is not an error")
        {
            AdmittedMessageTurnOutcome::RejectedBeforeEffect {
                settlement,
                error_class,
            } => {
                assert_eq!(error_class, "agent_message_app_server_turn_busy");
                assert_eq!(
                    settlement.invocation_id(),
                    Some(invocation_id),
                    "the refusal must hand back THIS attempt's settlement, unconsumed, \
                     so the caller can settle it explicitly with a pre-effect class"
                );
            }
            // Matched explicitly rather than with a `{other:?}` catch-all:
            // `AdmittedMessageTurnOutcome` deliberately has no `Debug`, because
            // it owns a settlement and this crate keeps provider payloads out of
            // formatting impls. An exhaustive match costs nothing here.
            AdmittedMessageTurnOutcome::Dispatched { .. } => panic!(
                "a busy session must refuse before any effect; dispatching here would \
                 interrupt the turn that is already running"
            ),
            AdmittedMessageTurnOutcome::Unsupported { .. } => panic!(
                "`Unsupported` is the fail-closed TRAIT DEFAULT for providers that cannot \
                 accept an externally admitted turn; the real CodexAppServerSession \
                 overrides it and must never report it"
            ),
        }

        // PROVED pre-effect: nothing was queued, so nothing could have been
        // written. This is what makes the refusal requeueable.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), write_rx.recv())
                .await
                .is_err(),
            "a refused-because-busy turn must not enqueue a single byte"
        );
        // The live turn's own slot is undisturbed.
        assert_eq!(
            session
                .current_turn_call
                .as_ref()
                .and_then(crate::model_control::call_control::ModelCallSettlement::invocation_id),
            Some(occupying_invocation),
            "refusing a delivery must not evict the turn that is actually running"
        );
    }

    /// A writer that hands the bytes back unsent maps to `RejectedBeforeEffect`,
    /// not to `Dispatched` and not to a generic `Err`.
    ///
    /// This is the entire point of widening the return type. The plan forbids
    /// inferring absence of effect from a generic `Err`, so the ONE AppServer
    /// outcome that is genuine proof of no effect must arrive as its own typed
    /// value. `RejectedBeforeEnqueue` is produced when `Sender::send` returns
    /// the value unsent — positive evidence the bytes were never queued — which
    /// this test drives by dropping the writer's receiving end.
    #[tokio::test]
    async fn a_refused_enqueue_maps_to_rejected_before_effect_rather_than_dispatched() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let session_id = uuid::Uuid::new_v4();
        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(4);
        let (_, event_rx) = mpsc::channel::<StreamEvent>(4);
        // Closing the writer's receiving end is what makes the real
        // `ModelExecutionCapability::send_codex_app_server` hand the bytes back
        // unsent and report `RejectedBeforeEnqueue`.
        drop(write_rx);

        let mut session = CodexAppServerSession {
            thread_id: "thread-abc".to_string(),
            working_dir: PathBuf::from("/home/user"),
            write_tx,
            event_rx,
            next_id: Arc::new(AtomicI64::new(1)),
            turn_control: Arc::new(NoopModelCallControl),
            current_turn_call: None,
        };

        let (turn, invocation_id) = admitted_delivery_turn(&store, session_id, 1).await;
        let config = TurnConfig {
            input: "delivered agent message".to_string(),
            working_dir: None,
        };

        match session
            .start_admitted_message_turn(turn, &config)
            .await
            .expect("a refused enqueue is a typed outcome, NEVER a generic Err")
        {
            AdmittedMessageTurnOutcome::RejectedBeforeEffect {
                settlement,
                error_class,
            } => {
                assert_eq!(
                    error_class,
                    "agent_message_app_server_rejected_before_enqueue"
                );
                assert_eq!(
                    settlement.invocation_id(),
                    Some(invocation_id),
                    "the settlement must come back unconsumed on the one outcome the \
                     frozen matrix accepts as proof of no effect"
                );
            }
            // Exhaustive rather than a `{other:?}` catch-all — see the note in
            // the sibling test; this type has no `Debug` by design.
            AdmittedMessageTurnOutcome::Dispatched { .. } => panic!(
                "a writer that refused the enqueue PROVES no effect and must map to \
                 RejectedBeforeEffect; reporting Dispatched here would mark a message \
                 effect-possible that provably never reached the provider, stranding it \
                 forever as unretryable"
            ),
            AdmittedMessageTurnOutcome::Unsupported { .. } => panic!(
                "`Unsupported` is the fail-closed TRAIT DEFAULT; the real \
                 CodexAppServerSession overrides it and must never report it"
            ),
        }

        // The idle turn state is restored by never having been changed. If this
        // regressed, a proved-no-effect attempt would leave the session looking
        // busy and wedge every later delivery for it.
        assert!(
            session.current_turn_call.is_none(),
            "a refused enqueue must not install a current turn"
        );
    }
}

#[cfg(test)]
pub(crate) mod approval_protocol_tests;
