//! `AgentManagerLaunchIssueWorker` (#1100) RPC handler. The caller is the
//! token-resolved session; every authority check is daemon-side.

use super::{RpcRequest, RpcServer};
use crate::error::{DaemonError, Result};
use rsi_common::manager_issue_worker::{
    AgentManagerLaunchIssueWorkerRequestV1, MANAGER_ISSUE_WORKER_INVALID_REQUEST,
};

impl RpcServer {
    pub(super) async fn handle_agent_manager_launch_issue_worker(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: AgentManagerLaunchIssueWorkerRequestV1 =
            serde_json::from_value(request.params.clone()).map_err(|_| {
                DaemonError::InvalidParam(MANAGER_ISSUE_WORKER_INVALID_REQUEST.into())
            })?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_launch_issue_worker(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }
}
