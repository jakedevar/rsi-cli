use thiserror::Error;

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Session not found: {0}")]
    SessionNotFound(uuid::Uuid),

    #[error("Session already exists: {0}")]
    SessionExists(uuid::Uuid),

    #[error("Claude binary not found in PATH")]
    ClaudeBinaryNotFound,

    #[error("Codex binary not found in PATH")]
    CodexBinaryNotFound,

    #[error("agy binary not found in PATH")]
    AgyBinaryNotFound,

    #[error("OpenAI-compatible API error: {0}")]
    OpenAiApiError(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("{message}")]
    StructuredRpc {
        rpc_code: i32,
        message: String,
        data: serde_json::Value,
    },

    #[error("Process error: {0}")]
    Process(String),

    /// Startup could not prove and terminate the complete exact-stamped
    /// provider cohort. Continuing would let later best-effort reconciliation
    /// erase the durable candidate set while a provider process remains live.
    #[error("Startup provider inventory error: {0}")]
    StartupProviderInventory(String),

    /// A streaming provider attempt failed in a way that permits the caller to
    /// request a separately admitted non-streaming fallback.
    #[error("Provider stream fallback required: {0}")]
    StreamFallbackRequired(String),

    #[error("Channel closed")]
    ChannelClosed,

    /// Cancellation was observed, but the owned provider child reported a
    /// cleanup failure only after it was reaped and both output pipes drained.
    #[error("Cancellation cleanup error: {0}")]
    CancellationCleanup(String),

    #[error("Invalid parameter: {0}")]
    InvalidParam(String),

    #[error("Policy denied: {0}")]
    PolicyDenied(String),

    /// Final descriptor-relative scratch validation failed before a provider
    /// command could be dispatched.  Launch settlement must distinguish this
    /// from a generic provider spawn failure without parsing diagnostic text.
    #[error("Execution scratch unavailable: {0}")]
    ExecutionScratchUnavailable(String),

    #[error("Store error: {0}")]
    Store(String),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, DaemonError>;

impl DaemonError {
    /// A reclaim intent temporarily holds custody authority without changing
    /// the session's status. Callers with durable retry ownership must keep
    /// their work armed when this exact typed refusal reaches them.
    pub(crate) fn is_reclaim_prepared(&self) -> bool {
        matches!(
            self,
            Self::StructuredRpc { data, .. }
                if data.get("kind").and_then(serde_json::Value::as_str)
                    == Some("sandbox_custody")
                    && data.pointer("/error/code").and_then(serde_json::Value::as_str)
                        == Some("reclaim_prepared")
        )
    }
}

/// Construct the bounded error envelope used only by additive operator Issue
/// workspace RPCs. It never includes request values, paths, tokens, or caller
/// topology.
pub(crate) fn issue_workspace_error(
    code: rsi_common::issue_workspace::IssueWorkspaceErrorCodeV1,
    issue_id: Option<uuid::Uuid>,
    expected_row_version: Option<i64>,
    actual_row_version: Option<i64>,
) -> DaemonError {
    use rsi_common::issue_workspace::{IssueWorkspaceErrorCodeV1 as Code, IssueWorkspaceErrorV1};

    let (retryable, next_action) = match code {
        Code::InvalidRequest => (false, "correct the request fields and retry"),
        Code::NotFoundInProject => (false, "refresh the project Issue list"),
        Code::StaleVersion => (true, "refresh the Issue and rebase the draft"),
        Code::IdempotencyConflict => (
            false,
            "retry the original request or use a new idempotency_key",
        ),
        Code::NoSemanticChange => (false, "change at least one Issue field"),
        Code::InvalidTransition => (false, "choose a valid transition from the current status"),
        Code::NotTerminal => (false, "close or cancel the Issue before archiving"),
        Code::NotArchived => (false, "refresh the Issue archive state"),
        Code::DependencyCycle => (false, "choose a dependency that does not create a cycle"),
        Code::DependencyEndpoint => (false, "choose Issue endpoints in the current project"),
        Code::AccessDenied => (false, "use the local operator connection"),
    };
    let envelope = IssueWorkspaceErrorV1 {
        code,
        issue_id,
        expected_row_version,
        actual_row_version,
        retryable,
        next_action: next_action.to_string(),
    };
    DaemonError::StructuredRpc {
        rpc_code: -32602,
        message: "issue_workspace_error".to_string(),
        data: serde_json::to_value(envelope).unwrap_or(serde_json::Value::Null),
    }
}

/// Stable code for a session-attributed read of a session outside the
/// caller's read scope (#241).
pub(crate) const AGENT_READ_SCOPE_DENIED: &str = "agent_read_scope_denied";

/// Typed refusal for an out-of-scope attributed read. The envelope is a
/// constant: it names no target, foreign session, project or topology, so a
/// denial is indistinguishable from reading a session that does not exist.
pub(crate) fn agent_read_scope_denied() -> DaemonError {
    DaemonError::StructuredRpc {
        rpc_code: -32602,
        message: AGENT_READ_SCOPE_DENIED.to_string(),
        data: serde_json::json!({
            "code": AGENT_READ_SCOPE_DENIED,
            "next_action": "read only your own session and rotation lineage, sessions you control, your own Epic/Group, or sessions in your appointed manager scope; use the Agent* verbs for coordination",
        }),
    }
}

/// The single safe-envelope construction point for guarded V97 Issue control.
/// Store paths supply only the stable code and (for in-scope stale CAS) version
/// witnesses; SQLite/topology details never cross this boundary.
pub(crate) fn agent_issue_error(
    code: rsi_common::rpc::AgentIssueErrorCodeV1,
    expected_row_version: Option<i64>,
    actual_row_version: Option<i64>,
) -> DaemonError {
    let envelope = agent_issue_error_envelope(code, expected_row_version, actual_row_version);
    agent_issue_error_from_envelope(envelope)
}

/// Construct the sole additive malformed-request envelope from bounded typed
/// validation evidence. Callers cannot supply diagnostic text or raw paths.
pub(crate) fn agent_issue_invalid_request(
    validation: rsi_common::rpc::AgentIssueValidationV1,
) -> DaemonError {
    let mut envelope = agent_issue_error_envelope(
        rsi_common::rpc::AgentIssueErrorCodeV1::InvalidRequest,
        None,
        None,
    );
    envelope.validation = Some(validation);
    agent_issue_error_from_envelope(envelope)
}

fn agent_issue_error_from_envelope(envelope: rsi_common::rpc::AgentIssueErrorV1) -> DaemonError {
    DaemonError::StructuredRpc {
        rpc_code: -32602,
        message: format!("agent_issue_{}", envelope.code.as_str()),
        data: serde_json::to_value(envelope).unwrap_or(serde_json::Value::Null),
    }
}

pub(crate) fn agent_issue_error_envelope(
    code: rsi_common::rpc::AgentIssueErrorCodeV1,
    expected_row_version: Option<i64>,
    actual_row_version: Option<i64>,
) -> rsi_common::rpc::AgentIssueErrorV1 {
    use rsi_common::rpc::{AgentIssueErrorCodeV1, AgentIssueErrorV1};

    let next_action = match code {
        AgentIssueErrorCodeV1::InvalidRequest => "correct the request fields and retry",
        AgentIssueErrorCodeV1::AuthorityDenied => {
            "use the current lead of the Issue project's owning Epic"
        }
        AgentIssueErrorCodeV1::NotFoundInScope => {
            "verify the Issue id from the current owning Epic project"
        }
        AgentIssueErrorCodeV1::IdempotencyConflict => {
            "retry the original semantic request or choose a new idempotency_key"
        }
        AgentIssueErrorCodeV1::StaleVersion => "refresh the Issue and retry with its row_version",
        AgentIssueErrorCodeV1::NoSemanticChange => "make a semantic change before retrying",
        AgentIssueErrorCodeV1::InvalidTransition => {
            "use a lifecycle transition allowed from the current Issue status"
        }
        AgentIssueErrorCodeV1::Archived => "restore the Issue before updating it",
        AgentIssueErrorCodeV1::NotArchived => "archive the terminal Issue before restoring it",
        AgentIssueErrorCodeV1::StorageFailure => "retry later; the Issue mutation did not commit",
    };
    AgentIssueErrorV1 {
        code,
        expected_row_version,
        actual_row_version,
        validation: None,
        next_action: next_action.to_string(),
    }
}

/// Collapse every internal failure entering an Issue agent surface into the
/// closed safe envelope. Existing typed Issue envelopes retain their precise
/// code/version witnesses; no Store, SQLite, topology, token, or path detail is
/// copied into the returned value.
pub(crate) fn normalize_agent_issue_error(error: DaemonError) -> DaemonError {
    use rsi_common::rpc::{AgentIssueErrorCodeV1, AgentIssueErrorV1};

    if let DaemonError::StructuredRpc { data, .. } = &error
        && let Ok(envelope) = serde_json::from_value::<AgentIssueErrorV1>(data.clone())
    {
        let mut normalized = agent_issue_error_envelope(
            envelope.code,
            envelope.expected_row_version,
            envelope.actual_row_version,
        );
        normalized.validation = envelope.validation;
        return agent_issue_error_from_envelope(normalized);
    }
    let code = match error {
        DaemonError::InvalidParam(_) | DaemonError::Rpc(_) | DaemonError::Json(_) => {
            AgentIssueErrorCodeV1::InvalidRequest
        }
        DaemonError::PolicyDenied(_) | DaemonError::SessionNotFound(_) => {
            AgentIssueErrorCodeV1::AuthorityDenied
        }
        _ => AgentIssueErrorCodeV1::StorageFailure,
    };
    agent_issue_error(code, None, None)
}

pub(crate) fn agent_issue_error_json(error: DaemonError) -> String {
    let normalized = normalize_agent_issue_error(error);
    let DaemonError::StructuredRpc { data, .. } = normalized else {
        unreachable!("Issue normalization always returns StructuredRpc")
    };
    serde_json::to_string(&data).unwrap_or_else(|_| {
        r#"{"code":"storage_failure","next_action":"retry later; the Issue mutation did not commit"}"#.to_string()
    })
}

#[cfg(test)]
mod agent_issue_error_tests {
    use super::*;
    use rsi_common::rpc::{
        AgentIssueErrorCodeV1, AgentIssueErrorV1, AgentIssueValidationClassV1,
        AgentIssueValidationFieldV1, AgentIssueValidationV1,
    };

    #[test]
    fn normalization_rebuilds_canonical_next_action_and_preserves_typed_evidence() {
        let validation = AgentIssueValidationV1 {
            class: AgentIssueValidationClassV1::MissingField,
            field: Some(AgentIssueValidationFieldV1::IssueId),
        };
        let supplied = AgentIssueErrorV1 {
            code: AgentIssueErrorCodeV1::InvalidRequest,
            expected_row_version: Some(4),
            actual_row_version: Some(5),
            validation: Some(validation.clone()),
            next_action: "untrusted internal detail /tmp/private".to_string(),
        };

        let normalized = normalize_agent_issue_error(DaemonError::StructuredRpc {
            rpc_code: 123,
            message: "untrusted message".to_string(),
            data: serde_json::to_value(supplied).unwrap(),
        });
        let DaemonError::StructuredRpc {
            rpc_code,
            message,
            data,
        } = normalized
        else {
            panic!("Issue normalization must return StructuredRpc");
        };
        let envelope: AgentIssueErrorV1 = serde_json::from_value(data.clone()).unwrap();

        assert_eq!(rpc_code, -32602);
        assert_eq!(message, "agent_issue_invalid_request");
        assert_eq!(envelope.code, AgentIssueErrorCodeV1::InvalidRequest);
        assert_eq!(envelope.expected_row_version, Some(4));
        assert_eq!(envelope.actual_row_version, Some(5));
        assert_eq!(envelope.validation, Some(validation));
        assert_eq!(envelope.next_action, "correct the request fields and retry");
        assert!(!data.to_string().contains("/tmp/private"));
        assert!(!data.to_string().contains("untrusted internal detail"));
    }
}

/// Typed sandbox-custody mapping that does not widen unrelated daemon error
/// matches before the later RPC/status wiring continuation.
pub(crate) fn sandbox_custody_error(
    error: rsi_common::types::SandboxCustodyErrorV1,
) -> DaemonError {
    let rpc_code = match error.code {
        rsi_common::types::SandboxCustodyErrorCodeV1::TupleIncomplete
        | rsi_common::types::SandboxCustodyErrorCodeV1::HistoricalPurged
        | rsi_common::types::SandboxCustodyErrorCodeV1::HistoricalTransferred
        | rsi_common::types::SandboxCustodyErrorCodeV1::CleanupFailed
        | rsi_common::types::SandboxCustodyErrorCodeV1::RootOutsideBase
        | rsi_common::types::SandboxCustodyErrorCodeV1::SourceWorktreeDirty => -32602,
        _ => -32603,
    };
    let message = format!("sandbox_custody:{}", error.code.as_str());
    let transient_custody_gate = matches!(
        error.code,
        rsi_common::types::SandboxCustodyErrorCodeV1::ReclaimPrepared
            | rsi_common::types::SandboxCustodyErrorCodeV1::RootBusy
    );
    let mut data = serde_json::json!({ "kind": "sandbox_custody", "error": error });
    if transient_custody_gate {
        data["retry_after_ms"] = serde_json::json!(1_000);
    }
    DaemonError::StructuredRpc {
        rpc_code,
        message,
        data,
    }
}

pub(crate) fn is_reclaim_prepared_error(error: &DaemonError) -> bool {
    matches!(
        error,
        DaemonError::StructuredRpc { data, .. }
            if data.get("kind").and_then(serde_json::Value::as_str) == Some("sandbox_custody")
                && data.pointer("/error/code").and_then(serde_json::Value::as_str)
                    == Some("reclaim_prepared")
    )
}

pub(crate) fn is_retryable_custody_wake_error(error: &DaemonError) -> bool {
    matches!(
        error,
        DaemonError::StructuredRpc { data, .. }
            if data.get("kind").and_then(serde_json::Value::as_str) == Some("sandbox_custody")
                && matches!(
                    data.pointer("/error/code").and_then(serde_json::Value::as_str),
                    Some("reclaim_prepared" | "root_busy")
                )
    )
}

#[cfg(test)]
mod reclaim_prepared_tests {
    use super::*;
    use rsi_common::types::{
        SandboxCustodyErrorCodeV1, SandboxCustodyErrorV1, SandboxCustodyRecoveryV1,
        SandboxCustodyTransitionV1,
    };

    #[test]
    fn v1_reclaim_prepared_rpc_keeps_code_and_adds_bounded_retry_hint() {
        let error = sandbox_custody_error(SandboxCustodyErrorV1 {
            version: 1,
            code: SandboxCustodyErrorCodeV1::ReclaimPrepared,
            session_id: None,
            transition: SandboxCustodyTransitionV1::Continue,
            retryable: true,
            recovery: SandboxCustodyRecoveryV1::RetryAfterReconcile,
        });
        let DaemonError::StructuredRpc { data, .. } = &error else {
            panic!("custody refusal must be a structured RPC error");
        };
        assert!(is_reclaim_prepared_error(&error));
        assert_eq!(
            data.pointer("/error/code"),
            Some(&serde_json::json!("reclaim_prepared"))
        );
        assert_eq!(data.get("retry_after_ms"), Some(&serde_json::json!(1_000)));
    }

    #[test]
    fn root_busy_is_distinct_from_prepared_and_has_a_bounded_retry_hint() {
        let error = sandbox_custody_error(SandboxCustodyErrorV1 {
            version: 1,
            code: SandboxCustodyErrorCodeV1::RootBusy,
            session_id: None,
            transition: SandboxCustodyTransitionV1::ResumeWake,
            retryable: true,
            recovery: SandboxCustodyRecoveryV1::RetryAfterReconcile,
        });
        let DaemonError::StructuredRpc { data, .. } = &error else {
            panic!("custody refusal must be a structured RPC error");
        };
        assert!(is_retryable_custody_wake_error(&error));
        assert!(!is_reclaim_prepared_error(&error));
        assert_eq!(
            data.pointer("/error/code"),
            Some(&serde_json::json!("root_busy"))
        );
        assert_eq!(data.get("retry_after_ms"), Some(&serde_json::json!(1_000)));
    }
}

pub(crate) fn agent_progress_cohort_too_large(cohort_size: usize) -> DaemonError {
    use rsi_common::agent_coordination::{AGENT_PROGRESS_MAX_COHORT, AgentCoordinationErrorCodeV1};

    let cohort_size = u32::try_from(cohort_size).unwrap_or(u32::MAX);
    let max_cohort_size = u32::try_from(AGENT_PROGRESS_MAX_COHORT).unwrap_or(u32::MAX);
    let data = serde_json::json!({
        "code": AgentCoordinationErrorCodeV1::CohortTooLarge,
        "cohort_size": cohort_size,
        "max_cohort_size": max_cohort_size,
        "next_action": "subdivide the orchestration topology and request at most 256 child session_ids",
    });
    DaemonError::StructuredRpc {
        rpc_code: -32602,
        message: format!(
            "agent_progress_cohort_too_large:{cohort_size}>{AGENT_PROGRESS_MAX_COHORT}"
        ),
        data,
    }
}

/// The single construction point for every P2-03 `AgentSendMessage` failure.
///
/// Every messaging rejection — authority, idempotency conflict, and both
/// pending caps — is a `StructuredRpc` carrying the frozen
/// [`AgentMessageErrorV1`] envelope in `data`, so a caller reads one closed
/// `code` instead of parsing prose. `message` deliberately keeps the stable
/// `agent_message_*` class prefix (plus its detail suffix) that the Store half
/// already emits, so `Display`-based assertions and log greps remain exact.
///
/// `next_action` is derived from `code` here rather than supplied by callers:
/// a per-call-site string would let the same closed code advertise two
/// different remediations.
pub(crate) fn agent_message_error(
    code: rsi_common::agent_coordination::AgentMessageErrorCodeV1,
    detail: Option<String>,
    message_id: Option<uuid::Uuid>,
    observed: Option<u32>,
    limit: Option<u32>,
) -> DaemonError {
    use rsi_common::agent_coordination::{AgentMessageErrorCodeV1, AgentMessageErrorV1};

    let next_action = match code {
        AgentMessageErrorCodeV1::TargetNotAuthorized => {
            "send only to your own reserved or direct child, or to a child of an Epic you lead"
        }
        AgentMessageErrorCodeV1::IdempotencyConflict => {
            "retry with the original target, message, and expiry, or send under a new idempotency key"
        }
        AgentMessageErrorCodeV1::TargetQueueFull => {
            "wait for the target to drain its pending mail before sending again"
        }
        AgentMessageErrorCodeV1::OwnerQueueFull => {
            "wait for your outstanding mail to settle before sending again"
        }
        AgentMessageErrorCodeV1::PayloadTooLarge => {
            "reduce the message to at most 16384 bytes and resend under the same idempotency key"
        }
        AgentMessageErrorCodeV1::TargetUnknown => {
            "spawn the child first, then send to the returned child_session_id"
        }
        AgentMessageErrorCodeV1::ProviderUnsupported => {
            "this target's provider cannot accept delivered mail; use a different child"
        }
        AgentMessageErrorCodeV1::TargetTerminal => {
            "continue the child or target its current live successor before sending"
        }
    };
    let envelope = AgentMessageErrorV1 {
        code,
        message_id,
        observed,
        limit,
        next_action: next_action.to_string(),
    };
    let message = match detail {
        Some(detail) => format!("{}:{detail}", code.as_str()),
        None => code.as_str().to_string(),
    };
    DaemonError::StructuredRpc {
        rpc_code: -32602,
        message,
        data: serde_json::to_value(&envelope).unwrap_or(serde_json::Value::Null),
    }
}

/// Build the stable typed `AgentArchiveChild` error envelope.
///
/// `observed` is carried ONLY for
/// [`AgentArchiveErrorCodeV1::StaleArchive`]; `detail` only for the classes
/// that define a bounded refusal detail. Every other refusal is flat and
/// discloses no cursor, payload or topology.
///
/// [`AgentArchiveErrorCodeV1::StaleArchive`]: rsi_common::agent_coordination::AgentArchiveErrorCodeV1::StaleArchive
pub(crate) fn agent_archive_error(
    code: rsi_common::agent_coordination::AgentArchiveErrorCodeV1,
    detail: Option<rsi_common::agent_coordination::AgentArchiveRefusalDetailV1>,
    observed: Option<rsi_common::agent_coordination::AgentContinuationCursorV1>,
) -> DaemonError {
    use rsi_common::agent_coordination::{AgentArchiveErrorCodeV1, AgentArchiveErrorV1};

    let next_action = match code {
        AgentArchiveErrorCodeV1::InvalidRequest => {
            "correct the request fields using the AgentArchiveChild schema before retrying"
        }
        AgentArchiveErrorCodeV1::SelfArchiveDenied => {
            "target a child; a session cannot archive itself"
        }
        AgentArchiveErrorCodeV1::TargetUnknown => {
            "read AgentGetProgress and archive a child_session_id it reports"
        }
        AgentArchiveErrorCodeV1::TargetNotAuthorized => {
            "archive only a child of an Epic you currently lead; ask that lead otherwise"
        }
        AgentArchiveErrorCodeV1::StaleArchive => {
            "adopt the observed cursor, re-decide whether the archive still applies, then retry"
        }
        AgentArchiveErrorCodeV1::TargetNotTerminal => {
            "wait for the child to finish, or halt it, before archiving"
        }
        AgentArchiveErrorCodeV1::TargetNotLeaf => {
            "archive a leaf child; containers are operator-managed"
        }
        AgentArchiveErrorCodeV1::TargetIsLead => {
            "the target leads a container; lead replacement belongs to the manager"
        }
        AgentArchiveErrorCodeV1::RecoveryOwnerHeld => {
            "a human, operator or recovery owner holds the child; leave it for that owner"
        }
        AgentArchiveErrorCodeV1::LiveContinuation => {
            "a continuation of the child is live or pending; let it settle before archiving"
        }
        AgentArchiveErrorCodeV1::ReviewSourceSealed => {
            "the child authors review work without a verdict; wait for the verdict before archiving"
        }
        AgentArchiveErrorCodeV1::ArchiveFailed => {
            "inspect the child with AgentGetProgress before retrying the archive"
        }
    };
    let envelope = AgentArchiveErrorV1 {
        code,
        detail,
        observed: matches!(code, AgentArchiveErrorCodeV1::StaleArchive)
            .then_some(observed)
            .flatten(),
        next_action: next_action.to_string(),
    };
    let message = detail.map_or_else(
        || code.as_str().to_string(),
        |detail| format!("{}:{}", code.as_str(), detail.as_str()),
    );
    DaemonError::StructuredRpc {
        rpc_code: -32602,
        message,
        data: serde_json::to_value(&envelope).unwrap_or(serde_json::Value::Null),
    }
}

/// `AgentArchiveChild` `invalid_request` carrying the bounded validation class
/// in the message only.
pub(crate) fn agent_archive_invalid_request(class: &'static str) -> DaemonError {
    match agent_archive_error(
        rsi_common::agent_coordination::AgentArchiveErrorCodeV1::InvalidRequest,
        None,
        None,
    ) {
        DaemonError::StructuredRpc {
            rpc_code,
            message,
            data,
        } => DaemonError::StructuredRpc {
            rpc_code,
            message: format!("{message}:{class}"),
            data,
        },
        other => other,
    }
}

/// Build the stable typed `AgentContinueChild` error envelope.
///
/// `observed` is supplied ONLY for
/// [`AgentContinueErrorCodeV1::StaleContinuation`], where it is the version
/// witness the caller must adopt before retrying. Every other class is a flat
/// refusal that leaks no cursor, no payload, and no topology.
pub(crate) fn agent_continue_error(
    code: rsi_common::agent_coordination::AgentContinueErrorCodeV1,
    detail: Option<String>,
    observed: Option<rsi_common::agent_coordination::AgentContinuationCursorV1>,
) -> DaemonError {
    agent_continue_error_with_receipt(code, detail, observed, None)
}

pub(crate) fn agent_continue_error_with_receipt(
    code: rsi_common::agent_coordination::AgentContinueErrorCodeV1,
    detail: Option<String>,
    observed: Option<rsi_common::agent_coordination::AgentContinuationCursorV1>,
    receipt: Option<serde_json::Value>,
) -> DaemonError {
    use rsi_common::agent_coordination::{AgentContinueErrorCodeV1, AgentContinueErrorV1};

    let next_action = match code {
        AgentContinueErrorCodeV1::InvalidRequest => {
            "correct the request fields using the AgentContinueChild schema before retrying"
        }
        AgentContinueErrorCodeV1::TargetNotAuthorized => {
            "continue only your own direct child, or a child of an Epic you lead"
        }
        AgentContinueErrorCodeV1::TargetUnknown => {
            "spawn the child first, then continue the returned child_session_id"
        }
        AgentContinueErrorCodeV1::SelfContinuationDenied => {
            "target a child; use AgentScheduleWake with mode resume to continue yourself"
        }
        AgentContinueErrorCodeV1::StaleContinuation => {
            "adopt the observed cursor, re-decide whether the continuation still applies, then retry"
        }
        AgentContinueErrorCodeV1::ProviderUnsupported => {
            "this target's provider cannot continue under its own session id; spawn a successor instead"
        }
        AgentContinueErrorCodeV1::ResumeUnavailableTaskUnresolved => {
            "restore a durable task prompt before retrying"
        }
        AgentContinueErrorCodeV1::IdempotencyConflict => {
            "retry with the original request or use a new idempotency_key"
        }
        AgentContinueErrorCodeV1::RelaunchAbandoned => "retry with a new idempotency_key",
        AgentContinueErrorCodeV1::RelaunchInProgress => {
            "wait for the open relaunch request to settle before making a new decision"
        }
        AgentContinueErrorCodeV1::ContinuationFailed => {
            "inspect the target with AgentGetProgress before retrying the continuation"
        }
    };
    let envelope = AgentContinueErrorV1 {
        code,
        observed: matches!(code, AgentContinueErrorCodeV1::StaleContinuation)
            .then_some(observed)
            .flatten(),
        receipt,
        next_action: next_action.to_string(),
    };
    let message = match detail {
        Some(detail) => format!("{}:{detail}", code.as_str()),
        None => code.as_str().to_string(),
    };
    DaemonError::StructuredRpc {
        rpc_code: -32602,
        message,
        data: serde_json::to_value(&envelope).unwrap_or(serde_json::Value::Null),
    }
}
