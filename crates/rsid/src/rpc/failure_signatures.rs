//! `AgentQueryFailureSignatures` (#1016): read-only, every leaf session,
//! scoped to the caller's own project.

use super::{RpcRequest, RpcServer};
use crate::error::{DaemonError, Result};
use rsi_common::agent_failure_signatures::{
    AgentQueryFailureSignaturesRequestV1, FAILURE_SIGNATURE_QUERY_INVALID,
};

impl RpcServer {
    pub(super) async fn handle_agent_query_failure_signatures(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params: AgentQueryFailureSignaturesRequestV1 =
            serde_json::from_value(request.params.clone())
                .map_err(|_| DaemonError::InvalidParam(FAILURE_SIGNATURE_QUERY_INVALID.into()))?;
        let result = self
            .session_manager
            .agent_control()
            .agent_query_failure_signatures(caller, params)
            .await?;
        Ok(serde_json::to_value(&result)?)
    }
}
