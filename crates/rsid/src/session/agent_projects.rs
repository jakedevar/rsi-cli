//! `AgentCreateProject` / `AgentUpdateProject` (#1626 slice 1).
//!
//! Authority and every write live in the store
//! (`Store::agent_project_scope`, `agent_register_project`,
//! `agent_edit_project`). This module adds what only the daemon knows: the
//! workspace-root containment of the path, and the refresh of the in-memory
//! project index and RSI.md cache the operator's `CreateProject` path also
//! refreshes. There is no agent delete or archive: `projects` has no archive
//! state and a hard delete stays operator-only.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rsi_common::agent_projects::{
    AgentCreateProjectRequestV1, AgentProjectResultV1, AgentUpdateProjectRequestV1,
    PROJECT_ADMIN_UNAVAILABLE, PROJECT_PATH_INVALID,
};
use rsi_common::types::Project;
use tokio::sync::RwLock;
use uuid::Uuid;

use super::agent_verbs::AgentControlHandle;
use crate::error::{DaemonError, Result};
use crate::project_cache::ProjectIndex;
use crate::store::Store;
use crate::store::agent_projects::AgentProjectEdit;

fn invalid(code: &'static str) -> DaemonError {
    DaemonError::InvalidParam(code.into())
}

/// What a handle needs to publish a project change to the running daemon.
/// Absent on handles built without a `SessionManager` (a rotation-built
/// handle): the verbs then refuse `project_admin_unavailable` rather than
/// leave the launch-time project index stale.
#[derive(Clone)]
pub(crate) struct ProjectAdminContext {
    pub(crate) workspace_roots: Vec<PathBuf>,
    /// The daemon user's home directory: with no workspace roots configured,
    /// project paths must be strict descendants of it.
    pub(crate) home_dir: Option<PathBuf>,
    /// The discovered harness root, protected from agent project edits.
    pub(crate) harness_root: Option<PathBuf>,
    pub(crate) project_index: Arc<RwLock<ProjectIndex>>,
    pub(crate) workflow_config_cache: Arc<RwLock<crate::project_workflow::ProjectWorkflowCache>>,
}

impl ProjectAdminContext {
    /// The context the running daemon uses: its workspace roots, the daemon
    /// user's home directory and the discovered harness root.
    pub(crate) fn for_daemon(
        workspace_roots: Vec<PathBuf>,
        project_index: Arc<RwLock<ProjectIndex>>,
        workflow_config_cache: Arc<RwLock<crate::project_workflow::ProjectWorkflowCache>>,
    ) -> Self {
        Self {
            workspace_roots,
            home_dir: dirs::home_dir(),
            harness_root: super::preamble::harness_root().map(Path::to_path_buf),
            project_index,
            workflow_config_cache,
        }
    }

    /// The canonical harness root, when the daemon has one.
    fn canonical_harness_root(&self) -> Option<PathBuf> {
        self.harness_root
            .as_deref()
            .map(|root| root.canonicalize().unwrap_or_else(|_| root.to_path_buf()))
    }

    /// Canonical, existing directory inside the workspace roots. With no
    /// roots configured the directory must be a strict descendant of the
    /// daemon user's home directory and not under the RSI data directory
    /// (`~/.rsi`), so `/`, `$HOME` and the daemon's own state are never
    /// registrable. Symlinks and `..` are resolved before any check.
    async fn canonical_dir(&self, raw: String) -> Result<PathBuf> {
        let roots = self.workspace_roots.clone();
        let home = self.home_dir.clone();
        tokio::task::spawn_blocking(move || {
            let canonical =
                crate::path_safety::canonicalize_working_dir(std::path::Path::new(&raw))
                    .map_err(|_| invalid(PROJECT_PATH_INVALID))?;
            if roots.is_empty() {
                let home = home
                    .and_then(|home| home.canonicalize().ok())
                    .ok_or_else(|| invalid(PROJECT_PATH_INVALID))?;
                let data_dir = home.join(rsi_common::identity::PROJECT_DIR_NAME);
                // `~/.rsi` may itself be a symlink to somewhere else under
                // home; the canonical path is what a project path resolves to.
                let canonical_data_dir = data_dir.canonicalize().ok();
                if canonical == home
                    || !canonical.starts_with(&home)
                    || canonical.starts_with(&data_dir)
                    || canonical_data_dir
                        .as_ref()
                        .is_some_and(|dir| canonical.starts_with(dir))
                {
                    return Err(invalid(PROJECT_PATH_INVALID));
                }
            } else {
                crate::path_safety::validate_containment(&canonical, &roots)
                    .map_err(|_| invalid(PROJECT_PATH_INVALID))?;
            }
            Ok(canonical)
        })
        .await
        .map_err(|error| DaemonError::Store(error.to_string()))?
    }

    /// Rebuild the project index and reload the project's RSI.md.
    async fn refresh(&self, store: &Arc<tokio::sync::Mutex<Store>>, project: &Project) {
        let store = Arc::clone(store);
        let projects = tokio::task::spawn_blocking(move || store.blocking_lock().load_projects())
            .await
            .ok()
            .and_then(std::result::Result::ok)
            .unwrap_or_default();
        self.project_index.write().await.invalidate(projects);
        let mut cache = self.workflow_config_cache.write().await;
        // A moved project may have no RSI.md at its new path; drop the old one.
        cache.remove(&project.id);
        if let Some(path) = &project.path {
            crate::project_workflow::load_project_workflow(project.id, path, &mut cache);
        }
    }
}

fn result(project: &Project, deduplicated: bool) -> AgentProjectResultV1 {
    AgentProjectResultV1 {
        project_id: project.id,
        name: project.name.clone(),
        path: project
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        description: project.description.clone(),
        color: project.color.clone(),
        deduplicated,
    }
}

impl AgentControlHandle {
    fn project_admin_context(&self) -> Result<ProjectAdminContext> {
        self.project_admin
            .clone()
            .ok_or_else(|| invalid(PROJECT_ADMIN_UNAVAILABLE))
    }

    /// `AgentCreateProject`: register a project for an Execute-mode project
    /// manager or portfolio seat.
    ///
    /// # Errors
    /// `project_not_authorized`, `project_path_invalid`,
    /// `project_name_taken`, `project_path_taken`,
    /// `project_harness_protected`, `project_admin_unavailable` or a
    /// persistence error.
    pub async fn agent_create_project(
        &self,
        caller: Uuid,
        request: AgentCreateProjectRequestV1,
    ) -> Result<AgentProjectResultV1> {
        request.validate().map_err(invalid)?;
        let context = self.project_admin_context()?;
        // Authority first: an unauthorized caller never probes the filesystem.
        self.store.lock().await.agent_project_scope(caller)?;
        let canonical = context.canonical_dir(request.path).await?;
        let now = chrono::Utc::now();
        let project = Project {
            id: Uuid::new_v4(),
            name: request.name.trim().to_string(),
            path: Some(canonical),
            description: request.description,
            color: request
                .color
                .unwrap_or_else(|| Project::DEFAULT_COLOR.to_string()),
            context_files: None,
            created_at: now,
            updated_at: now,
        };
        let (stored, deduplicated) = self.store.lock().await.agent_register_project(
            caller,
            &project,
            context.canonical_harness_root().as_deref(),
        )?;
        if !deduplicated {
            context.refresh(&self.store, &stored).await;
            // Agent creates only: the operator's CreateProject publishes
            // nothing, so the TUI can tell who acted. A replay is not new.
            self.event_bus
                .publish(crate::bus::DaemonEvent::ProjectCreated {
                    project_id: stored.id,
                    name: stored.name.clone(),
                    source: rsi_common::agent_projects::PROJECT_CREATED_SOURCE_AGENT.to_string(),
                    created_by_session_id: caller,
                });
        }
        Ok(result(&stored, deduplicated))
    }

    /// `AgentUpdateProject`: edit a project inside the caller's coverage.
    ///
    /// # Errors
    /// `project_not_authorized`, `project_not_in_scope`,
    /// `project_path_invalid`, `project_name_taken`, `project_path_taken`,
    /// `project_harness_protected`, `project_has_live_sessions`, `project_admin_unavailable` or a
    /// persistence error.
    pub async fn agent_update_project(
        &self,
        caller: Uuid,
        request: AgentUpdateProjectRequestV1,
    ) -> Result<AgentProjectResultV1> {
        request.validate().map_err(invalid)?;
        let context = self.project_admin_context()?;
        let scope = self.store.lock().await.agent_project_scope(caller)?;
        if !scope.covers(request.project_id) {
            return Err(invalid(rsi_common::agent_projects::PROJECT_NOT_IN_SCOPE));
        }
        let path = match request.path {
            Some(raw) => Some(context.canonical_dir(raw).await?),
            None => None,
        };
        let edit = AgentProjectEdit {
            name: request.name.map(|name| name.trim().to_string()),
            path,
            description: request.description,
            color: request.color,
        };
        let updated = self.store.lock().await.agent_edit_project(
            caller,
            request.project_id,
            edit,
            context.canonical_harness_root().as_deref(),
        )?;
        context.refresh(&self.store, &updated).await;
        Ok(result(&updated, false))
    }
}

#[cfg(test)]
#[path = "agent_projects_tests.rs"]
mod tests;
