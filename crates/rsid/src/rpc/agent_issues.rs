use super::*;

pub(super) fn decode_issue_workspace_params<T: DeserializeOwned>(
    request: &RpcRequest,
) -> Result<T> {
    serde_json::from_value(request.params.clone()).map_err(|_| {
        issue_workspace_error(IssueWorkspaceErrorCodeV1::InvalidRequest, None, None, None)
    })
}

pub(super) fn normalize_issue_workspace_failure(error: DaemonError) -> DaemonError {
    if matches!(error, DaemonError::StructuredRpc { .. }) {
        return error;
    }
    tracing::error!(error = %error, "Issue workspace RPC failed internally");
    DaemonError::Rpc("Issue workspace operation failed".to_string())
}

impl RpcServer {
    /// `AgentCreateIssue` is deliberately distinct from operator `CreateIssue`:
    /// caller attribution is resolved from the transport token and the strict
    /// request contains no creator or session identity.
    pub(super) async fn handle_agent_create_issue(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller_session_id = self.resolve_caller_session_id(request).await?;
        let params: AgentCreateIssueParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {e}")))?;
        let result = self
            .session_manager
            .agent_create_issue(caller_session_id, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_list_issues(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await.map_err(|_| {
            crate::error::agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::AuthorityDenied,
                None,
                None,
            )
        })?;
        let params = crate::agent_issue_validation::decode_list(&request.params)
            .map_err(crate::error::agent_issue_invalid_request)?;
        serialize_agent_issue_result(self.session_manager.agent_list_issues(caller, params).await)
    }

    pub(super) async fn handle_agent_get_issue(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await.map_err(|_| {
            crate::error::agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::AuthorityDenied,
                None,
                None,
            )
        })?;
        let params = crate::agent_issue_validation::decode_get(&request.params)
            .map_err(crate::error::agent_issue_invalid_request)?;
        serialize_agent_issue_result(self.session_manager.agent_get_issue(caller, params).await)
    }

    pub(super) async fn handle_agent_update_issue(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await.map_err(|_| {
            crate::error::agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::AuthorityDenied,
                None,
                None,
            )
        })?;
        let params = crate::agent_issue_validation::decode_update(&request.params)
            .map_err(crate::error::agent_issue_invalid_request)?;
        serialize_agent_issue_result(
            self.session_manager
                .agent_update_issue(caller, params)
                .await,
        )
    }

    pub(super) async fn handle_agent_update_issue_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await.map_err(|_| {
            crate::error::agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::AuthorityDenied,
                None,
                None,
            )
        })?;
        let params = crate::agent_issue_validation::decode_update_status(&request.params)
            .map_err(crate::error::agent_issue_invalid_request)?;
        serialize_agent_issue_result(
            self.session_manager
                .agent_update_issue_status(caller, params)
                .await,
        )
    }

    pub(super) async fn handle_agent_archive_issue(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await.map_err(|_| {
            crate::error::agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::AuthorityDenied,
                None,
                None,
            )
        })?;
        let params = crate::agent_issue_validation::decode_archive(&request.params)
            .map_err(crate::error::agent_issue_invalid_request)?;
        serialize_agent_issue_result(
            self.session_manager
                .agent_archive_issue(caller, params)
                .await,
        )
    }

    pub(super) async fn handle_agent_restore_issue(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await.map_err(|_| {
            crate::error::agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::AuthorityDenied,
                None,
                None,
            )
        })?;
        let params = crate::agent_issue_validation::decode_restore(&request.params)
            .map_err(crate::error::agent_issue_invalid_request)?;
        serialize_agent_issue_result(
            self.session_manager
                .agent_restore_issue(caller, params)
                .await,
        )
    }

    pub(super) async fn handle_agent_list_issue_events(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await.map_err(|_| {
            crate::error::agent_issue_error(
                rsi_common::rpc::AgentIssueErrorCodeV1::AuthorityDenied,
                None,
                None,
            )
        })?;
        let params = crate::agent_issue_validation::decode_list_events(&request.params)
            .map_err(crate::error::agent_issue_invalid_request)?;
        serialize_agent_issue_result(
            self.session_manager
                .agent_list_issue_events(caller, params)
                .await,
        )
    }
}
