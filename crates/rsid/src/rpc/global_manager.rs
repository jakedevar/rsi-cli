//! Global manager v0 (#872 Slice B) RPC handlers.
//!
//! `ConfigureGlobalManager`, `GetGlobalManager` and `RevokeGlobalManager` are
//! operator-only: they are not declared in the attributed verb registry, so a
//! tokened caller is default-denied (AGENTS.md rule 10).

use super::{RpcRequest, RpcServer};
use crate::error::{DaemonError, Result};
use rsi_common::global_manager::{
    ConfigureGlobalManagerRequestV1, GLOBAL_MANAGER_INVALID_REQUEST, GetGlobalManagerRequestV1,
    RevokeGlobalManagerRequestV1,
};

fn decode<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T> {
    let params = if request.params.is_null() {
        serde_json::json!({})
    } else {
        request.params.clone()
    };
    serde_json::from_value(params)
        .map_err(|_| DaemonError::InvalidParam(GLOBAL_MANAGER_INVALID_REQUEST.into()))
}

impl RpcServer {
    pub(super) async fn handle_configure_global_manager(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ConfigureGlobalManagerRequestV1 = decode(request)?;
        let store = self.session_manager.store();
        let grant = store
            .lock()
            .await
            .configure_global_manager(&params, "operator_rpc")?;
        Ok(serde_json::to_value(grant)?)
    }

    pub(super) async fn handle_get_global_manager(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let _: GetGlobalManagerRequestV1 = decode(request)?;
        let store = self.session_manager.store();
        let grant = store.lock().await.active_global_grant()?;
        Ok(serde_json::to_value(grant)?)
    }

    pub(super) async fn handle_revoke_global_manager(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RevokeGlobalManagerRequestV1 = decode(request)?;
        let store = self.session_manager.store();
        let grant = store.lock().await.revoke_global_manager(&params)?;
        Ok(serde_json::to_value(grant)?)
    }
}

/// Agent verbs of the global seat and of a reporting PM. The caller is always
/// the token-resolved session; every authority check is daemon-side.
impl RpcServer {
    pub(super) async fn handle_agent_global_overview(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params = decode(request)?;
        let result = self
            .session_manager
            .agent_control()
            .agent_global_overview(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_global_send(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params = decode(request)?;
        let result = self
            .session_manager
            .agent_control()
            .agent_global_send(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_global_appoint_manager(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params = decode(request)?;
        let result = self
            .session_manager
            .agent_global_appoint_manager(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_report_to_global(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params = decode(request)?;
        let result = self
            .session_manager
            .agent_control()
            .agent_report_to_global(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }
}
