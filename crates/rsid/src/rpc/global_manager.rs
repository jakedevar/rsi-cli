//! Global manager v0 (#872 Slice B) and portfolio node (#1236) RPC handlers.
//!
//! `ConfigureGlobalManager`, `GetGlobalManager`, `RevokeGlobalManager` and
//! `GetGlobalManagerWorkspace` (#1213) are operator-only: they are not declared in the attributed verb registry, so a
//! tokened caller is default-denied (AGENTS.md rule 10). Since #1236 they are
//! shims over the single active root labelled `global`.
//! `ListPortfolioNodes`, `GetPortfolioNode`, `ConfigurePortfolioNode` and
//! `RevokePortfolioNode` are operator-only on the same terms, and so is
//! `GetManagerNodeWorkspace` (#1240); its agent counterpart is
//! `AgentManagerOverview` (the caller's own node).

use super::{RpcRequest, RpcServer};
use crate::error::{DaemonError, Result};
use crate::store::portfolio_nodes::PortfolioGrantor;
use rsi_common::global_manager::{
    ConfigureGlobalManagerRequestV1, GLOBAL_MANAGER_INVALID_REQUEST, GetGlobalManagerRequestV1,
    GetGlobalManagerWorkspaceRequestV1, RevokeGlobalManagerRequestV1,
};
use rsi_common::manager_node_workspace::GetManagerNodeWorkspaceRequestV1;
use rsi_common::manager_tier_routing::{
    AcknowledgeOperatorNoticeRequestV1, ListOperatorEscalationsRequestV1,
    MANAGER_TIER_INVALID_REQUEST, RuleOperatorEscalationRequestV1,
};
use rsi_common::portfolio_nodes::{
    ConfigurePortfolioNodeRequestV1, GetPortfolioNodeRequestV1, ListPortfolioNodesRequestV1,
    ListPortfolioNodesResultV1, PORTFOLIO_INVALID_REQUEST, RevokePortfolioNodeRequestV1,
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
        let confirmed: rsi_common::portfolio_nodes::PortfolioCapConfirmation<
            ConfigureGlobalManagerRequestV1,
        > = decode(request)?;
        let params = confirmed.request;
        let mut launches = params.allowed_launches.clone();
        launches.extend(params.project_policy.allowed_launches.iter().cloned());
        self.session_manager
            .validate_manager_launch_models(&launches)
            .await?;
        let store = self.session_manager.store();
        let grant = store.lock().await.configure_global_manager_confirmed(
            &params,
            "operator_rpc",
            confirmed.confirm_cap_reductions,
        )?;
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

    pub(super) async fn handle_get_global_manager_workspace(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let _: GetGlobalManagerWorkspaceRequestV1 = decode(request)?;
        let workspace = self
            .session_manager
            .agent_control()
            .operator_global_workspace()
            .await?;
        Ok(serde_json::to_value(workspace)?)
    }

    /// #1240: operator-only snapshot of any manager node.
    pub(super) async fn handle_get_manager_node_workspace(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetManagerNodeWorkspaceRequestV1 = decode_tier(request)?;
        params
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let workspace = self
            .session_manager
            .agent_control()
            .operator_manager_node_workspace(params.node)
            .await?;
        Ok(serde_json::to_value(workspace)?)
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

fn decode_portfolio<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T> {
    let params = if request.params.is_null() {
        serde_json::json!({})
    } else {
        request.params.clone()
    };
    serde_json::from_value(params)
        .map_err(|_| DaemonError::InvalidParam(PORTFOLIO_INVALID_REQUEST.into()))
}

/// Operator-only portfolio node RPCs (#1236).
impl RpcServer {
    pub(super) async fn handle_list_portfolio_nodes(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListPortfolioNodesRequestV1 = decode_portfolio(request)?;
        let store = self.session_manager.store();
        let nodes = store
            .lock()
            .await
            .list_portfolio_nodes(params.include_revoked)?;
        Ok(serde_json::to_value(ListPortfolioNodesResultV1 { nodes })?)
    }

    pub(super) async fn handle_get_portfolio_node(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetPortfolioNodeRequestV1 = decode_portfolio(request)?;
        let store = self.session_manager.store();
        let node = store.lock().await.get_portfolio_node(params.node_id)?;
        Ok(serde_json::to_value(node)?)
    }

    pub(super) async fn handle_configure_portfolio_node(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let confirmed: rsi_common::portfolio_nodes::PortfolioCapConfirmation<
            ConfigurePortfolioNodeRequestV1,
        > = decode_portfolio(request)?;
        let params = confirmed.request;
        let mut launches = params.allowed_launches.clone();
        launches.extend(params.policy.allowed_launches.iter().cloned());
        if let Some(policy) = &params.child_policy {
            launches.extend(policy.allowed_launches.iter().cloned());
        }
        self.session_manager
            .validate_manager_launch_models(&launches)
            .await?;
        let store = self.session_manager.store();
        let node = store.lock().await.configure_portfolio_node_confirmed(
            &params,
            PortfolioGrantor::Operator,
            "operator_rpc",
            confirmed.confirm_cap_reductions,
        )?;
        Ok(serde_json::to_value(node)?)
    }

    pub(super) async fn handle_revoke_portfolio_node(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RevokePortfolioNodeRequestV1 = decode_portfolio(request)?;
        let store = self.session_manager.store();
        let node = store.lock().await.revoke_portfolio_node(&params)?;
        Ok(serde_json::to_value(node)?)
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

    /// #1240: the caller's own manager node snapshot.
    pub(super) async fn handle_agent_manager_overview(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params = decode_tier(request)?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_overview(caller, params)
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

    pub(super) async fn handle_agent_manager_appoint_child(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params = decode(request)?;
        let result = self
            .session_manager
            .agent_manager_appoint_child(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_manager_revoke_child(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params = decode(request)?;
        let result = self
            .session_manager
            .agent_control()
            .agent_manager_revoke_child(caller, params)
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

fn decode_tier<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T> {
    let params = if request.params.is_null() {
        serde_json::json!({})
    } else {
        request.params.clone()
    };
    serde_json::from_value(params)
        .map_err(|_| DaemonError::InvalidParam(MANAGER_TIER_INVALID_REQUEST.into()))
}

/// #1238: N-level routing. `AgentReportUp` and `AgentSendDown` are agent verbs
/// (caller from the token); `ListOperatorEscalations`,
/// `RuleOperatorEscalation` and `AcknowledgeOperatorNotice` are operator-only:
/// they are not in the attributed verb registry, so a tokened caller is
/// default-denied (AGENTS.md rule 10).
impl RpcServer {
    pub(super) async fn handle_agent_report_up(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params = decode_tier(request)?;
        let result = self
            .session_manager
            .agent_control()
            .agent_report_up(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_agent_send_down(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let caller = self.resolve_caller_session_id(request).await?;
        let params = decode_tier(request)?;
        let result = self
            .session_manager
            .agent_control()
            .agent_send_down(caller, params)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_list_operator_escalations(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListOperatorEscalationsRequestV1 = decode_tier(request)?;
        let store = self.session_manager.store();
        let result = store.lock().await.list_operator_escalations_page(
            params.include_closed,
            params.undelivered_after.as_deref(),
        )?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_rule_operator_escalation(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: RuleOperatorEscalationRequestV1 = decode_tier(request)?;
        let store = self.session_manager.store();
        let result = store.lock().await.rule_operator_escalation(&params)?;
        Ok(serde_json::to_value(result)?)
    }

    pub(super) async fn handle_acknowledge_operator_notice(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: AcknowledgeOperatorNoticeRequestV1 = decode_tier(request)?;
        let store = self.session_manager.store();
        let result = store
            .lock()
            .await
            .acknowledge_operator_notice(params.message_id)?;
        Ok(serde_json::to_value(result)?)
    }
}
