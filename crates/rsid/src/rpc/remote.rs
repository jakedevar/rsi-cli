use super::*;

// Operator-only RSI Remote controls (#1096). Neither method may appear in
// AGENT_VERBS, READ_VERBS, native tools or the agent CLI catalog: an agent
// must not be able to widen the allowed-device or project lists.
impl RpcServer {
    pub(super) async fn handle_remote_get_status(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        if !request.params.is_null() && request.params != serde_json::json!({}) {
            return Err(DaemonError::InvalidParam(
                "RemoteGetStatus takes no parameters".into(),
            ));
        }
        let projects =
            crate::remote_control::project_rows(&self.session_manager.list_projects().await?);
        let status = self
            .remote
            .status(projects)
            .await
            .map_err(DaemonError::Rpc)?;
        Ok(serde_json::to_value(status)?)
    }

    pub(super) async fn handle_remote_set_config(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: rsi_common::remote_control::RemoteSetConfigRequestV1 =
            serde_json::from_value(if request.params.is_null() {
                serde_json::json!({})
            } else {
                request.params.clone()
            })
            .map_err(|error| {
                DaemonError::InvalidParam(format!("invalid remote config: {error}"))
            })?;
        let projects =
            crate::remote_control::project_rows(&self.session_manager.list_projects().await?);
        let status = self
            .remote
            .set_config(projects, params)
            .await
            .map_err(DaemonError::InvalidParam)?;
        Ok(serde_json::to_value(status)?)
    }
}
