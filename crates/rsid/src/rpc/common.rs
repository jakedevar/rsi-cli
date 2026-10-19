use super::*;

pub(super) fn structured_rpc_error(
    rpc_code: i32,
    data_code: impl Into<String>,
    message: impl Into<String>,
    resource_type: Option<&str>,
    resource_id: Option<String>,
    details: Option<serde_json::Value>,
) -> DaemonError {
    let data = RpcErrorData {
        code: data_code.into(),
        resource_type: resource_type.map(str::to_string),
        resource_id,
        retryable: false,
        details,
    };
    DaemonError::StructuredRpc {
        rpc_code,
        message: message.into(),
        data: serde_json::to_value(data).unwrap_or_else(|_| serde_json::json!({})),
    }
}

pub(super) fn structured_invalid_params(
    data_code: impl Into<String>,
    message: impl Into<String>,
    resource_type: Option<&str>,
    resource_id: Option<String>,
    details: Option<serde_json::Value>,
) -> DaemonError {
    structured_rpc_error(
        INVALID_PARAMS,
        data_code,
        message,
        resource_type,
        resource_id,
        details,
    )
}

/// P1.12: standalone validator extracted from `RpcServer::validate_parent_id`
/// so integration tests can exercise it against a bare `SessionManager`
/// without constructing the full RPC server. Same behavior — `None` returns
/// `Ok(())`; otherwise validates that the parent is a visible container.
pub async fn validate_parent_id_against(
    manager: &SessionManager,
    parent_id: Option<Uuid>,
) -> Result<()> {
    let Some(pid) = parent_id else {
        return Ok(());
    };
    let Some(parent) = manager.get_session(pid).await else {
        return Err(DaemonError::InvalidParam(format!(
            "parent_id session {pid} not found"
        )));
    };
    if !rsi_common::types::is_container_kind(parent.session_kind) {
        return Err(DaemonError::InvalidParam(format!(
            "parent_id session {pid} is a leaf kind ({:?}); must be Group or Epic",
            parent.session_kind
        )));
    }
    use rsi_common::types::SessionStatus::*;
    if matches!(parent.status, Archived | Deleted) {
        return Err(DaemonError::InvalidParam(format!(
            "parent_id session {pid} is not attachable in state {:?}",
            parent.status
        )));
    }
    Ok(())
}

pub(crate) fn serialize_agent_issue_result<T: serde::Serialize>(
    result: Result<T>,
) -> Result<serde_json::Value> {
    let value = result.map_err(crate::error::normalize_agent_issue_error)?;
    serde_json::to_value(value).map_err(|_| {
        crate::error::agent_issue_error(
            rsi_common::rpc::AgentIssueErrorCodeV1::StorageFailure,
            None,
            None,
        )
    })
}

/// #565: the typed envelope of an `Io` error that is scratch-storage pressure
/// (`EDQUOT`/`ENOSPC`). The RPC error carries it as `data`, so the operator
/// reads a disk/quota condition with its `code` and `next_action`.
pub(super) fn scratch_pressure_data(error: &std::io::Error) -> Option<serde_json::Value> {
    rsi_common::scratch_pressure::ScratchPressureV1::from_io(error, None)
        .and_then(|envelope| serde_json::to_value(envelope).ok())
}

/// The RPC error message: a scratch-pressure `Io` error leads with its stable
/// code so a log line or `rsi-rpc` stderr names the disk condition.
pub(super) fn rpc_error_message(error: &DaemonError) -> String {
    match error {
        DaemonError::Io(io) => match rsi_common::scratch_pressure::classify(io) {
            Some(kind) => format!("{}: {error}", kind.code()),
            None => error.to_string(),
        },
        _ => error.to_string(),
    }
}

/// Admission precedes handlers and parameter decoding, but retains the guarded
/// Issue and topology families' closed authority envelopes without exposing
/// transport details.
pub(super) fn transport_admission_error(method: &str, error: DaemonError) -> RpcError {
    let error = if matches!(
        method,
        "AgentListIssues"
            | "AgentGetIssue"
            | "AgentUpdateIssue"
            | "AgentUpdateIssueStatus"
            | "AgentArchiveIssue"
            | "AgentRestoreIssue"
            | "AgentListIssueEvents"
    ) {
        crate::error::agent_issue_error(
            rsi_common::rpc::AgentIssueErrorCodeV1::AuthorityDenied,
            None,
            None,
        )
    } else if AgentControlVerbV1::from_method_name(method).is_some_and(|verb| {
        crate::session::topology_agent_verbs::TOPOLOGY_AGENT_VERBS.contains(&verb)
    }) {
        crate::session::topology_agent_verbs::unattributed()
    } else {
        error
    };
    match error {
        DaemonError::StructuredRpc {
            rpc_code,
            message,
            data,
        } => RpcError {
            code: rpc_code,
            message,
            data: Some(data),
        },
        error => RpcError {
            code: INVALID_PARAMS,
            message: error.to_string(),
            data: None,
        },
    }
}

impl RpcServer {
    /// P1.12: validate that `parent_id` (if provided) points at a visible
    /// container session (Group or Epic). Rejects:
    ///   - missing sessions
    ///   - leaf-kind sessions (Standard, TaskRabbit, Bug, Story, Task, etc.)
    ///   - hidden containers (Archived / Deleted)
    /// `None` short-circuits to `Ok(())` (legacy backward-compat path).
    pub(super) async fn validate_parent_id(&self, parent_id: Option<Uuid>) -> Result<()> {
        validate_parent_id_against(&self.session_manager, parent_id).await
    }
}

#[cfg(test)]
mod scratch_pressure_tests {
    use super::*;

    /// #565: a refused scratch write surfaces as a typed disk/quota condition,
    /// never as a parameter error, without needing a real exhausted quota.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn refused_scratch_write_is_typed_disk_pressure_not_invalid_params() {
        let quota = DaemonError::Io(std::io::Error::from(std::io::ErrorKind::QuotaExceeded));
        let data = match &quota {
            DaemonError::Io(io) => scratch_pressure_data(io).expect("typed data"),
            _ => unreachable!(),
        };
        assert_eq!(data["code"], "scratch_disk_quota_exceeded");
        assert_eq!(data["kind"], "disk_pressure");
        assert!(
            data["next_action"]
                .as_str()
                .unwrap()
                .contains("scratch-quota")
        );
        assert!(rpc_error_message(&quota).starts_with("scratch_disk_quota_exceeded: "));

        let full = DaemonError::Io(std::io::Error::from(std::io::ErrorKind::StorageFull));
        assert!(rpc_error_message(&full).starts_with("scratch_disk_full: "));

        let other = DaemonError::Io(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(matches!(&other, DaemonError::Io(io) if scratch_pressure_data(io).is_none()));
        assert!(!rpc_error_message(&other).starts_with("scratch_"));
    }
}
