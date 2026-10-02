use super::*;

/// RPC params for creating a project.
#[derive(Debug, Deserialize)]
pub struct CreateProjectParams {
    pub name: String,
    #[serde(default)]
    pub path: Option<std::path::PathBuf>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

/// RPC params for updating a project.
#[derive(Debug, Deserialize)]
pub struct UpdateProjectParams {
    pub id: Uuid,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub path: Option<std::path::PathBuf>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

/// RPC params for getting or deleting a project.
#[derive(Debug, Deserialize)]
pub struct ProjectIdParams {
    pub id: Uuid,
}

/// RPC params for creating a session label.
#[derive(Debug, Deserialize)]
pub struct CreateLabelParams {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub color: Option<String>,
}

/// RPC params for updating a session label.
#[derive(Debug, Deserialize)]
pub struct UpdateLabelParams {
    pub id: Uuid,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
}

/// RPC params for deleting or getting a session label.
#[derive(Debug, Deserialize)]
pub struct LabelIdParams {
    pub id: Uuid,
}

/// RPC params for listing session labels.
#[derive(Debug, Default, Deserialize)]
pub struct ListLabelsParams {
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

impl RpcServer {
    pub(super) async fn handle_create_project(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: CreateProjectParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let project = self
            .session_manager
            .create_project(params.name, params.path, params.description, params.color)
            .await?;

        Ok(serde_json::to_value(&project)?)
    }

    pub(super) async fn handle_update_project(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateProjectParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let project = self
            .session_manager
            .update_project(
                params.id,
                params.name,
                params.path,
                params.description,
                params.color,
            )
            .await?;

        Ok(serde_json::to_value(&project)?)
    }

    pub(super) async fn handle_delete_project(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ProjectIdParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        self.session_manager.delete_project(params.id).await?;

        Ok(serde_json::json!({ "success": true }))
    }

    pub(super) async fn handle_get_project(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ProjectIdParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let project = self.session_manager.get_project(params.id).await?;

        Ok(serde_json::to_value(&project)?)
    }

    pub(super) async fn handle_list_projects(
        &self,
        _request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let projects = self.session_manager.list_projects().await?;
        Ok(serde_json::to_value(&projects)?)
    }

    pub(super) async fn handle_get_project_workflow(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ProjectIdParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        let cache = self.session_manager.workflow_config_cache().read().await;
        if let Some(wf) = cache.get(&params.id) {
            Ok(serde_json::json!({
                "exists": true,
                "healthy": wf.last_error.is_none(),
                "last_error": wf.last_error,
                "loaded_at": wf.loaded_at.to_rfc3339(),
                "settings": wf.settings,
                "template_length": wf.template_body.len(),
            }))
        } else {
            Ok(serde_json::json!({
                "exists": false,
                "healthy": true,
                "last_error": null,
            }))
        }
    }

    pub(super) async fn handle_reload_project_workflow(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ProjectIdParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;

        // Get project path
        let project = self
            .session_manager
            .get_project(params.id)
            .await?
            .ok_or_else(|| DaemonError::Store("Project not found".to_string()))?;

        let project_path = project
            .path
            .ok_or_else(|| DaemonError::InvalidParam("Project has no path".to_string()))?;

        // Force re-read and re-parse
        let mut cache = self.session_manager.workflow_config_cache().write().await;
        crate::project_workflow::load_project_workflow(params.id, &project_path, &mut cache);

        Ok(serde_json::json!({ "success": true }))
    }

    pub(super) async fn handle_create_label(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: CreateLabelParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let label = self
            .session_manager
            .create_label(
                params.name,
                params.description,
                params.project_id,
                params.color,
            )
            .await?;
        Ok(serde_json::to_value(&label)?)
    }

    pub(super) async fn handle_update_label(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UpdateLabelParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let label = self
            .session_manager
            .update_label(params.id, params.name, params.description, params.color)
            .await?;
        Ok(serde_json::to_value(&label)?)
    }

    pub(super) async fn handle_delete_label(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: LabelIdParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        self.session_manager.delete_label(params.id).await?;
        Ok(serde_json::json!({ "ok": true }))
    }

    pub(super) async fn handle_get_label(&self, request: &RpcRequest) -> Result<serde_json::Value> {
        let params: LabelIdParams = serde_json::from_value(request.params.clone())
            .map_err(|e| DaemonError::Rpc(format!("Invalid params: {}", e)))?;
        let label = self
            .session_manager
            .get_label(params.id)
            .await?
            .ok_or_else(|| DaemonError::Store(format!("Label not found: {}", params.id)))?;
        Ok(serde_json::to_value(&label)?)
    }

    pub(super) async fn handle_list_labels(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let _params: ListLabelsParams =
            serde_json::from_value(request.params.clone()).unwrap_or_default();
        let labels = self.session_manager.list_labels().await?;
        Ok(serde_json::to_value(&labels)?)
    }

    pub(super) async fn handle_get_issue_in_project(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetIssueInProjectRequestV1 = decode_issue_workspace_params(request)?;
        let store = self.session_manager.store().lock().await;
        let result = store
            .get_issue_workspace_in_project(&params)
            .map_err(normalize_issue_workspace_failure)?;
        Ok(serde_json::to_value(result)?)
    }
}
