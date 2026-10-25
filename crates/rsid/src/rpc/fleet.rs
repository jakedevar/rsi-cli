use super::*;
impl RpcServer {
    pub(super) async fn handle_get_fleet_overview(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let store = self.session_manager.store().clone();
        let mut snapshot = tokio::task::spawn_blocking(move || {
            store.blocking_lock().fleet_overview(chrono::Utc::now())
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Fleet read: {e}")))??;
        let mut sessions: Vec<_> = snapshot.agents.iter().map(|a| a.session.clone()).collect();
        self.session_manager
            .stamp_context_fill_pct(&mut sessions)
            .await;
        for (agent, session) in snapshot.agents.iter_mut().zip(sessions) {
            agent.session = session;
        }
        Ok(serde_json::to_value(snapshot)?)
    }
}

impl RpcServer {
    /// #3316: `GetGlobalDashboard {window}`, an operator-only bounded snapshot
    /// of every project's agents, tokens, cost and Issue flow. The home
    /// project (#3314 A) is not recorded on this base yet, so none is marked.
    pub(super) async fn handle_get_global_dashboard(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::global_dashboard::GetGlobalDashboardParams =
            if request.params.is_null() {
                Default::default()
            } else {
                serde_json::from_value(request.params.clone())
                    .map_err(|e| DaemonError::Rpc(format!("Invalid params: {e}")))?
            };
        let store = self.session_manager.store().clone();
        let snapshot = tokio::task::spawn_blocking(move || {
            store
                .blocking_lock()
                .global_dashboard(chrono::Utc::now(), params.window, None)
        })
        .await
        .map_err(|e| DaemonError::Process(format!("Dashboard read: {e}")))??;
        Ok(serde_json::to_value(snapshot)?)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn fleet_rpc_is_operator_only() {
        assert!(!crate::rpc::agent_gate::AGENT_VERBS.contains(&"GetFleetOverview"));
        assert!(!crate::rpc::agent_gate::READ_VERBS.contains(&"GetFleetOverview"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn global_dashboard_rpc_is_operator_only() {
        assert!(!crate::rpc::agent_gate::AGENT_VERBS.contains(&"GetGlobalDashboard"));
        assert!(!crate::rpc::agent_gate::READ_VERBS.contains(&"GetGlobalDashboard"));
    }
}
