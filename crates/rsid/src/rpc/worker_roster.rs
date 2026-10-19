//! `AgentWorkerRoster` (#2311): read-only; the caller is the transport-bound
//! session, and the store resolves the scope (its own launched workers, or a
//! project's workers for its appointed manager and a covering portfolio seat).

use super::{RpcRequest, RpcServer};
use crate::error::{DaemonError, Result};
use rsi_common::agent_worker_roster::{AgentWorkerRosterRequestV1, WORKER_ROSTER_INVALID_REQUEST};

impl RpcServer {
    pub(super) async fn handle_agent_worker_roster(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: AgentWorkerRosterRequestV1 = if request.params.is_null() {
            AgentWorkerRosterRequestV1::default()
        } else {
            serde_json::from_value(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam(WORKER_ROSTER_INVALID_REQUEST.into()))?
        };
        let result = self
            .session_manager
            .agent_control()
            .agent_worker_roster(caller, params)
            .await?;
        Ok(serde_json::to_value(&result)?)
    }
}
