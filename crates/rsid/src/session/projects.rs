//! Project CRUD operations on SessionManager.

use crate::error::{DaemonError, Result};
use rsi_common::types::Project;
use uuid::Uuid;

use super::SessionManager;

impl SessionManager {
    /// Rebuild the in-memory project index from the database.
    pub(super) async fn rebuild_project_index(&self) {
        let store = self.store.clone();
        let projects = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_projects()
        })
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();

        self.project_index.write().await.invalidate(projects);
    }

    /// Create a new project.
    pub async fn create_project(
        &self,
        name: String,
        path: Option<std::path::PathBuf>,
        description: Option<String>,
        color: Option<String>,
    ) -> Result<Project> {
        let project = Project {
            id: Uuid::new_v4(),
            name,
            path,
            description,
            color: color.unwrap_or_else(|| Project::DEFAULT_COLOR.to_string()),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        // Persist to database
        self.persistence.insert_project(project.clone()).await?;

        // Invalidate project cache
        self.rebuild_project_index().await;

        // Load FLYWHEEL.md for new project if path is set
        if let Some(ref path) = project.path {
            let mut cache = self.workflow_config_cache.write().await;
            crate::project_workflow::load_project_workflow(project.id, path, &mut cache);
        }

        Ok(project)
    }

    /// Update an existing project.
    pub async fn update_project(
        &self,
        id: Uuid,
        name: Option<String>,
        path: Option<std::path::PathBuf>,
        description: Option<String>,
        color: Option<String>,
    ) -> Result<Project> {
        // Load existing project
        let store = self.store.clone();
        let existing = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_project(id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??
        .ok_or_else(|| DaemonError::Store(format!("Project {} not found", id)))?;

        // Apply updates
        let updated = Project {
            id: existing.id,
            name: name.unwrap_or(existing.name),
            path: path.or(existing.path),
            description: description.or(existing.description),
            color: color.unwrap_or(existing.color),
            context_files: existing.context_files,
            created_at: existing.created_at,
            updated_at: chrono::Utc::now(),
        };

        // Persist
        self.persistence.update_project(updated.clone()).await?;

        // Invalidate project cache
        self.rebuild_project_index().await;

        // Reload FLYWHEEL.md for updated project
        {
            let mut cache = self.workflow_config_cache.write().await;
            if let Some(ref path) = updated.path {
                crate::project_workflow::load_project_workflow(updated.id, path, &mut cache);
            } else {
                cache.remove(&updated.id);
            }
        }

        Ok(updated)
    }

    /// Delete a project.
    pub async fn delete_project(&self, id: Uuid) -> Result<()> {
        self.persistence.delete_project(id).await?;

        // Invalidate project cache
        self.rebuild_project_index().await;

        // Remove workflow config for deleted project
        self.workflow_config_cache.write().await.remove(&id);

        // Session rows were just cleared to NULL; kick the memory worker so
        // indexed transcripts pick up the new unscoped project assignment.
        super::schedule_memory_sync(self.memory_handle.clone(), format!("project_deleted:{id}"));

        Ok(())
    }

    /// Get a project by ID.
    pub async fn get_project(&self, id: Uuid) -> Result<Option<Project>> {
        let store = self.store.clone();
        let project = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.get_project(id)
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        Ok(project)
    }

    /// Resolve the canonical working directory for a project-scoped operation.
    pub(crate) async fn resolve_project_working_dir(
        &self,
        project_id: Uuid,
    ) -> Result<std::path::PathBuf> {
        let project = self
            .get_project(project_id)
            .await?
            .ok_or_else(|| DaemonError::InvalidParam(format!("project not found: {project_id}")))?;
        let path = project.path.ok_or_else(|| {
            DaemonError::InvalidParam(format!(
                "project '{}' has no working directory configured",
                project.name
            ))
        })?;
        let working_dir = crate::path_safety::canonicalize_working_dir(&path)?;
        crate::path_safety::validate_containment(&working_dir, self.workspace_roots())?;
        Ok(working_dir)
    }

    /// List all projects.
    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        let store = self.store.clone();
        let projects = tokio::task::spawn_blocking(move || {
            let store = store.blocking_lock();
            store.load_projects()
        })
        .await
        .map_err(|e| DaemonError::Store(e.to_string()))??;

        Ok(projects)
    }
}
