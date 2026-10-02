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
