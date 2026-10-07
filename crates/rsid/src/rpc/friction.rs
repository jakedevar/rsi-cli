//! Friction telemetry, the andon (#1333): the operator-only rollup RPC and
//! the agent-refusal recording point.
//!
//! `ListFrictionRollup` is not declared in the attributed verb registry, so a
//! tokened caller is default-denied (AGENTS.md rule 10). Managers read the
//! same rollup through `AgentManagerInspect {section:"friction"}`.

use super::{RpcRequest, RpcServer};
use crate::error::{DaemonError, Result};
use rsi_common::friction::ListFrictionRollupRequestV1;

impl RpcServer {
    pub(super) async fn handle_list_friction_rollup(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params = if request.params.is_null() {
            serde_json::json!({})
        } else {
            request.params.clone()
        };
        let params: ListFrictionRollupRequestV1 = serde_json::from_value(params)
            .map_err(|_| DaemonError::InvalidParam("friction_request_invalid".into()))?;
        let result = self
            .session_manager
            .store()
            .lock()
            .await
            .friction_rollup(&params, chrono::Utc::now())?;
        Ok(serde_json::to_value(result)?)
    }

    /// Recording point: a tokened `Agent*` verb returned an error. The
    /// attribution gate already admitted `request.method`, so it is a
    /// registered verb name; the code comes from the error's code, never its
    /// message prose.
    pub(super) async fn note_agent_refusal(&self, request: &RpcRequest, error: &DaemonError) {
        if request.session_token.is_none() || !request.method.starts_with("Agent") {
            return;
        }
        let caller = self.resolve_caller_session_id(request).await.ok();
        let event = crate::friction::agent_refusal_event(&request.method, caller, error);
        crate::friction::note(self.session_manager.store(), event).await;
    }
}
