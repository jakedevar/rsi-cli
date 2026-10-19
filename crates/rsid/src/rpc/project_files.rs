//! hub #3012 (slice 10A of #3000): read-only, project-root-scoped file RPCs
//! for the desktop file explorer, fuzzy picker and file viewer.
//!
//! `ListProjectDirectory`, `ListProjectFiles`, `ReadProjectFile` and
//! `GetProjectFileGitGutter` are operator-only: they are not in the attributed
//! verb registry, so a tokened caller is default-denied (AGENTS.md rule 10),
//! and the rsi-remote gateway never calls them (it only speaks the
//! `Remote*V1/V2` methods). The work is in `crate::project_files`.
//!
//! hub #3103: `WriteProjectFile`, `CreateProjectEntry`, `MoveProjectEntry`,
//! `CopyProjectEntry`, `TrashProjectEntry` and `UndoProjectFileOperation`
//! change files under the same root resolution and are operator-only the
//! same way (`crate::project_files::write`).

use super::*;
use crate::project_files::write::FileOps;
use crate::project_files::{self as files, INVALID_REQUEST, UNKNOWN_ROOT, refuse};
use rsi_common::project_files::{
    CreateProjectEntryRequestV1, GetProjectFileGitGutterRequestV1, ListProjectDirectoryRequestV1,
    ListProjectFilesRequestV1, ReadProjectFileRequestV1, TransferProjectEntryRequestV1,
    TrashProjectEntryRequestV1, UndoProjectFileOperationRequestV1, WriteProjectFileRequestV1,
};

fn decode<T: serde::de::DeserializeOwned>(request: &RpcRequest) -> Result<T> {
    let params = if request.params.is_null() {
        serde_json::json!({})
    } else {
        request.params.clone()
    };
    serde_json::from_value(params).map_err(|e| refuse(INVALID_REQUEST, e))
}

impl RpcServer {
    /// The canonical root a project-file request names: the project's `path`
    /// or the session's `working_dir`, exactly one of the two.
    pub(super) async fn project_file_root(
        &self,
        project_id: Option<Uuid>,
        session_id: Option<Uuid>,
    ) -> Result<std::path::PathBuf> {
        let root = match (project_id, session_id) {
            (Some(project_id), None) => {
                let project = self
                    .session_manager
                    .get_project(project_id)
                    .await?
                    .ok_or_else(|| {
                        refuse(UNKNOWN_ROOT, format!("project {project_id} not found"))
                    })?;
                project.path.ok_or_else(|| {
                    refuse(UNKNOWN_ROOT, format!("project {project_id} has no path"))
                })?
            }
            (None, Some(session_id)) => {
                self.session_manager
                    .get_session(session_id)
                    .await
                    .ok_or_else(|| refuse(UNKNOWN_ROOT, format!("session {session_id} not found")))?
                    .working_dir
            }
            _ => {
                return Err(refuse(
                    INVALID_REQUEST,
                    "name exactly one of project_id and session_id",
                ));
            }
        };
        tokio::task::spawn_blocking(move || files::canonical_root(&root))
            .await
            .map_err(|e| DaemonError::Process(e.to_string()))?
    }

    pub(super) async fn handle_list_project_directory(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListProjectDirectoryRequestV1 = decode(request)?;
        let root = self
            .project_file_root(params.project_id, params.session_id)
            .await?;
        let listing = files::list_directory(root, params.path, params.max_entries).await?;
        Ok(serde_json::to_value(listing)?)
    }

    pub(super) async fn handle_list_project_files(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ListProjectFilesRequestV1 = decode(request)?;
        let root = self
            .project_file_root(params.project_id, params.session_id)
            .await?;
        let feed = files::list_files(root, params.path, params.max_paths).await?;
        Ok(serde_json::to_value(feed)?)
    }

    pub(super) async fn handle_read_project_file(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: ReadProjectFileRequestV1 = decode(request)?;
        let root = self
            .project_file_root(params.project_id, params.session_id)
            .await?;
        let read = tokio::task::spawn_blocking(move || {
            files::read_file(
                &root,
                &params.path,
                params.max_bytes,
                params.start_line,
                params.end_line,
            )
        })
        .await
        .map_err(|e| DaemonError::Process(e.to_string()))??;
        Ok(serde_json::to_value(read)?)
    }

    pub(super) async fn handle_get_project_file_git_gutter(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: GetProjectFileGitGutterRequestV1 = decode(request)?;
        let root = self
            .project_file_root(params.project_id, params.session_id)
            .await?;
        let gutter = files::git_gutter(root, params.path).await?;
        Ok(serde_json::to_value(gutter)?)
    }

    /// Run one blocking project-file write against the daemon's [`FileOps`].
    async fn project_file_write<T: serde::Serialize + Send + 'static>(
        &self,
        project_id: Option<Uuid>,
        session_id: Option<Uuid>,
        op: impl FnOnce(&FileOps, &std::path::Path) -> Result<T> + Send + 'static,
    ) -> Result<serde_json::Value> {
        crate::project_files::write::ensure_supported()?;
        let root = self.project_file_root(project_id, session_id).await?;
        let value = tokio::task::spawn_blocking(move || op(FileOps::global(), &root))
            .await
            .map_err(|e| DaemonError::Process(e.to_string()))??;
        Ok(serde_json::to_value(value)?)
    }

    pub(super) async fn handle_write_project_file(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: WriteProjectFileRequestV1 = decode(request)?;
        self.project_file_write(params.project_id, params.session_id, move |ops, root| {
            ops.save(
                root,
                &params.path,
                &params.content,
                params.expected_version.as_deref(),
                params.force,
            )
        })
        .await
    }

    pub(super) async fn handle_create_project_entry(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: CreateProjectEntryRequestV1 = decode(request)?;
        self.project_file_write(params.project_id, params.session_id, move |ops, root| {
            ops.create(root, &params.path, params.kind)
        })
        .await
    }

    pub(super) async fn handle_move_project_entry(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: TransferProjectEntryRequestV1 = decode(request)?;
        self.project_file_write(params.project_id, params.session_id, move |ops, root| {
            ops.rename(root, &params.from, &params.to)
        })
        .await
    }

    pub(super) async fn handle_copy_project_entry(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: TransferProjectEntryRequestV1 = decode(request)?;
        self.project_file_write(params.project_id, params.session_id, move |ops, root| {
            ops.copy(root, &params.from, &params.to)
        })
        .await
    }

    pub(super) async fn handle_trash_project_entry(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: TrashProjectEntryRequestV1 = decode(request)?;
        self.project_file_write(params.project_id, params.session_id, move |ops, root| {
            ops.trash(root, &params.path)
        })
        .await
    }

    pub(super) async fn handle_undo_project_file_operation(
        &self,
        request: &RpcRequest,
    ) -> Result<serde_json::Value> {
        let params: UndoProjectFileOperationRequestV1 = decode(request)?;
        self.project_file_write(params.project_id, params.session_id, |ops, root| {
            ops.undo(root)
        })
        .await
    }
}
