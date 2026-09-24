//! Workflow query operations for SessionManager.

use super::SessionManager;
use crate::error::{DaemonError, Result};
use rsi_common::types::{Workflow, WorkflowDocument};
use uuid::Uuid;

impl SessionManager {
    /// List all workflows, optionally filtered by project.
    pub async fn list_workflows(&self, project_id: Option<Uuid>) -> Result<Vec<Workflow>> {
        let store = self.store.clone();
        let workflows = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            match project_id {
                Some(pid) => store.list_workflows_by_project(Some(pid)),
                None => store.list_workflows(),
            }
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        Ok(workflows)
    }

    /// Get a single workflow by ID.
    pub async fn get_workflow(&self, workflow_id: Uuid) -> Result<Option<Workflow>> {
        let store = self.store.clone();
        let workflow = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_workflow(workflow_id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        Ok(workflow)
    }

    /// Get a workflow definition-bearing document by ID.
    pub async fn get_workflow_definition(
        &self,
        workflow_id: Uuid,
    ) -> Result<Option<WorkflowDocument>> {
        let store = self.store.clone();
        let document = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_workflow_definition(workflow_id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        Ok(document)
    }

    /// Insert or update a workflow metadata+definition document transactionally.
    pub async fn upsert_workflow_definition(
        &self,
        document: WorkflowDocument,
    ) -> Result<WorkflowDocument> {
        let store = self.store.clone();
        let saved = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.upsert_workflow_definition(&document)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        Ok(saved)
    }
}
