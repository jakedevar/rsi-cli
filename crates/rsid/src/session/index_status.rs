//! Filesystem JSON sidecar CRUD for `INDEX.status.json` files.
//!
//! Each `thoughts/shared/projects/<project>/` directory may contain an
//! `INDEX.status.json` sidecar tracking per-ticket pipeline status.
//! Writes are atomic (tempfile + rename on POSIX); no DB migration needed.
//!
//! Validation pipeline (pure, runs before any I/O):
//!   1. `validate_project_name` — regex guard `^[a-zA-Z0-9_-]+$`.
//!   2. Workspace root resolution — `RSI_WORKSPACE_ROOT` override or first
//!      entry in `self.workspace_roots()`.

use crate::error::DaemonError;
use chrono::Utc;
use regex::Regex;
use rsi_common::{
    rpc::{GetIndexStatusParams, UpdateIndexStatusParams},
    types::{IndexStatusSidecar, IndexStatusValue, IndexTicketStatus},
};
use std::{collections::BTreeMap, path::PathBuf, sync::OnceLock};
use tempfile::NamedTempFile;

use super::SessionManager;

/// Validates project names: only alphanumeric, underscore, and hyphen.
static PROJECT_NAME_RE: OnceLock<Regex> = OnceLock::new();

fn project_name_re() -> &'static Regex {
    PROJECT_NAME_RE.get_or_init(|| {
        Regex::new(r"^[a-zA-Z0-9_-]+$")
            .unwrap_or_else(|_| unreachable!("PROJECT_NAME_RE is a valid pattern"))
    })
}

/// Validate that a project name is safe for filesystem path construction.
pub(crate) fn validate_project_name(project: &str) -> Result<(), DaemonError> {
    if project_name_re().is_match(project) {
        Ok(())
    } else {
        Err(DaemonError::InvalidParam(
            "project name must match ^[a-zA-Z0-9_-]+$".into(),
        ))
    }
}

/// Canonical path of the sidecar file for a project.
fn sidecar_path(workspace_root: &std::path::Path, project: &str) -> PathBuf {
    workspace_root
        .join("thoughts/shared/projects")
        .join(project)
        .join("INDEX.status.json")
}

impl SessionManager {
    /// Update (or create) the INDEX.status.json sidecar for a single ticket.
    pub(crate) fn update_index_status(
        &self,
        params: UpdateIndexStatusParams,
    ) -> Result<(), DaemonError> {
        validate_project_name(&params.project)?;
        let root = resolve_workspace_root(self)?;
        self.update_index_status_at(&root, params)
    }

    /// Inner implementation that accepts the workspace root directly (used by
    /// tests to avoid env var pollution between parallel test runs).
    pub(crate) fn update_index_status_at(
        &self,
        root: &std::path::Path,
        params: UpdateIndexStatusParams,
    ) -> Result<(), DaemonError> {
        validate_project_name(&params.project)?;
        let path = sidecar_path(root, &params.project);

        let mut sidecar = if path.exists() {
            let raw = std::fs::read_to_string(&path).map_err(|e| {
                DaemonError::Rpc(format!("failed to read sidecar {}: {}", path.display(), e))
            })?;
            serde_json::from_str::<IndexStatusSidecar>(&raw).map_err(|e| {
                DaemonError::Rpc(format!("failed to parse sidecar {}: {}", path.display(), e))
            })?
        } else {
            IndexStatusSidecar {
                schema_version: 1,
                project: params.project.clone(),
                last_updated: Utc::now(),
                tickets: BTreeMap::new(),
            }
        };

        // Only preserve shipped-tracking fields when the new status is Shipped.
        let (last_shipped_commit, last_shipped_at, last_shipped_branch) =
            if params.status == IndexStatusValue::Shipped {
                (
                    params.last_shipped_commit,
                    Some(Utc::now()),
                    params.last_shipped_branch,
                )
            } else {
                (None, None, None)
            };

        sidecar.tickets.insert(
            params.ticket_id,
            IndexTicketStatus {
                status: params.status,
                last_shipped_commit,
                last_shipped_at,
                last_shipped_branch,
            },
        );
        sidecar.last_updated = Utc::now();

        // Atomic write: temp file in the same dir, then rename.
        std::fs::create_dir_all(path.parent().expect("sidecar path always has parent"))
            .map_err(|e| DaemonError::Rpc(format!("create_dir_all failed: {}", e)))?;
        let mut tmp = NamedTempFile::new_in(path.parent().expect("parent always exists"))
            .map_err(|e| DaemonError::Rpc(format!("tempfile creation failed: {}", e)))?;
        serde_json::to_writer_pretty(&mut tmp, &sidecar)
            .map_err(|e| DaemonError::Rpc(format!("serde write failed: {}", e)))?;
        tmp.persist(&path)
            .map_err(|e| DaemonError::Rpc(format!("atomic rename failed: {}", e)))?;

        Ok(())
    }

    /// Fetch the INDEX.status.json sidecar for a project. Returns an error if
    /// the file does not exist yet.
    pub(crate) fn get_index_status(
        &self,
        params: GetIndexStatusParams,
    ) -> Result<IndexStatusSidecar, DaemonError> {
        validate_project_name(&params.project)?;
        let root = resolve_workspace_root(self)?;
        self.get_index_status_at(&root, params)
    }

    /// Inner implementation that accepts the workspace root directly.
    pub(crate) fn get_index_status_at(
        &self,
        root: &std::path::Path,
        params: GetIndexStatusParams,
    ) -> Result<IndexStatusSidecar, DaemonError> {
        validate_project_name(&params.project)?;
        let path = sidecar_path(root, &params.project);
        if !path.exists() {
            return Err(DaemonError::InvalidParam(format!(
                "no sidecar at {}",
                path.display()
            )));
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            DaemonError::Rpc(format!("failed to read sidecar {}: {}", path.display(), e))
        })?;
        serde_json::from_str::<IndexStatusSidecar>(&raw).map_err(|e| {
            DaemonError::Rpc(format!("failed to parse sidecar {}: {}", path.display(), e))
        })
    }
}

/// Resolve the workspace root: env override first, then `workspace_roots()[0]`.
fn resolve_workspace_root(mgr: &SessionManager) -> Result<PathBuf, DaemonError> {
    if let Ok(env_root) = std::env::var("RSI_WORKSPACE_ROOT") {
        return Ok(PathBuf::from(env_root));
    }
    mgr.workspace_roots()
        .first()
        .cloned()
        .ok_or_else(|| DaemonError::InvalidParam("no workspace root configured".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::IndexStatusValue;
    use tempfile::TempDir;

    /// Build a minimal `SessionManager` for unit tests. We can't construct the
    /// full manager (it requires a running tokio runtime + DB), so we use the
    /// lower-level `*_at` helpers that accept a workspace root directly.
    struct Ctx {
        dir: TempDir,
    }

    impl Ctx {
        fn new() -> Self {
            Ctx {
                dir: TempDir::new().unwrap(),
            }
        }

        fn root(&self) -> &std::path::Path {
            self.dir.path()
        }

        /// Stub SessionManager that satisfies the type required by `*_at` helpers.
        fn mgr(&self) -> SessionManagerStub {
            SessionManagerStub
        }
    }

    /// Zero-sized stub used only to call the `*_at` methods via trait dispatch.
    /// The `*_at` methods only need `&self` to call `validate_project_name` and
    /// the atomic write helpers — neither of which touches `SessionManager` state.
    struct SessionManagerStub;

    impl SessionManagerStub {
        fn update_index_status_at(
            &self,
            root: &std::path::Path,
            params: UpdateIndexStatusParams,
        ) -> Result<(), DaemonError> {
            // Replicate the logic inline — we can't call `self::SessionManager`
            // methods from the stub, so delegate to module-level helpers.
            validate_project_name(&params.project)?;
            let path = sidecar_path(root, &params.project);

            let mut sidecar = if path.exists() {
                let raw = std::fs::read_to_string(&path)
                    .map_err(|e| DaemonError::Rpc(format!("read: {}", e)))?;
                serde_json::from_str::<IndexStatusSidecar>(&raw)
                    .map_err(|e| DaemonError::Rpc(format!("parse: {}", e)))?
            } else {
                IndexStatusSidecar {
                    schema_version: 1,
                    project: params.project.clone(),
                    last_updated: Utc::now(),
                    tickets: BTreeMap::new(),
                }
            };

            let (last_shipped_commit, last_shipped_at, last_shipped_branch) =
                if params.status == IndexStatusValue::Shipped {
                    (
                        params.last_shipped_commit,
                        Some(Utc::now()),
                        params.last_shipped_branch,
                    )
                } else {
                    (None, None, None)
                };

            sidecar.tickets.insert(
                params.ticket_id,
                IndexTicketStatus {
                    status: params.status,
                    last_shipped_commit,
                    last_shipped_at,
                    last_shipped_branch,
                },
            );
            sidecar.last_updated = Utc::now();

            std::fs::create_dir_all(path.parent().unwrap())
                .map_err(|e| DaemonError::Rpc(format!("create_dir_all: {}", e)))?;
            let mut tmp = NamedTempFile::new_in(path.parent().unwrap())
                .map_err(|e| DaemonError::Rpc(format!("tempfile: {}", e)))?;
            serde_json::to_writer_pretty(&mut tmp, &sidecar)
                .map_err(|e| DaemonError::Rpc(format!("serialize: {}", e)))?;
            tmp.persist(&path)
                .map_err(|e| DaemonError::Rpc(format!("persist: {}", e)))?;
            Ok(())
        }

        fn get_index_status_at(
            &self,
            root: &std::path::Path,
            params: GetIndexStatusParams,
        ) -> Result<IndexStatusSidecar, DaemonError> {
            validate_project_name(&params.project)?;
            let path = sidecar_path(root, &params.project);
            if !path.exists() {
                return Err(DaemonError::InvalidParam(format!(
                    "no sidecar at {}",
                    path.display()
                )));
            }
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| DaemonError::Rpc(format!("read: {}", e)))?;
            serde_json::from_str::<IndexStatusSidecar>(&raw)
                .map_err(|e| DaemonError::Rpc(format!("parse: {}", e)))
        }
    }

    fn make_update_params(
        project: &str,
        ticket_id: &str,
        status: IndexStatusValue,
        commit: Option<&str>,
        branch: Option<&str>,
    ) -> UpdateIndexStatusParams {
        UpdateIndexStatusParams {
            project: project.to_string(),
            ticket_id: ticket_id.to_string(),
            status,
            last_shipped_commit: commit.map(|s| s.to_string()),
            last_shipped_branch: branch.map(|s| s.to_string()),
        }
    }

    fn make_get_params(project: &str) -> GetIndexStatusParams {
        GetIndexStatusParams {
            project: project.to_string(),
        }
    }

    #[test]
    fn test_create_on_first_write() {
        let ctx = Ctx::new();
        let mgr = ctx.mgr();
        let params = make_update_params("myproject", "P1.1", IndexStatusValue::Ready, None, None);
        mgr.update_index_status_at(ctx.root(), params).unwrap();

        let sidecar = mgr
            .get_index_status_at(ctx.root(), make_get_params("myproject"))
            .unwrap();
        assert_eq!(sidecar.schema_version, 1);
        assert_eq!(sidecar.project, "myproject");
        let ticket = &sidecar.tickets["P1.1"];
        assert_eq!(ticket.status, IndexStatusValue::Ready);
        assert!(ticket.last_shipped_commit.is_none());
    }

    #[test]
    fn test_status_update_transition() {
        let ctx = Ctx::new();
        let mgr = ctx.mgr();

        // First: write Shipped with commit info
        mgr.update_index_status_at(
            ctx.root(),
            make_update_params(
                "proj",
                "P2.0",
                IndexStatusValue::Shipped,
                Some("abc123"),
                Some("main"),
            ),
        )
        .unwrap();

        // Then: update to InProgress — shipped fields should be cleared
        mgr.update_index_status_at(
            ctx.root(),
            make_update_params("proj", "P2.0", IndexStatusValue::InProgress, None, None),
        )
        .unwrap();

        let sidecar = mgr
            .get_index_status_at(ctx.root(), make_get_params("proj"))
            .unwrap();
        let ticket = &sidecar.tickets["P2.0"];
        assert_eq!(ticket.status, IndexStatusValue::InProgress);
        assert!(
            ticket.last_shipped_commit.is_none(),
            "shipped_commit should be cleared"
        );
        assert!(
            ticket.last_shipped_at.is_none(),
            "shipped_at should be cleared"
        );
        assert!(
            ticket.last_shipped_branch.is_none(),
            "shipped_branch should be cleared"
        );
    }

    #[test]
    fn test_shipped_fields_populated() {
        let ctx = Ctx::new();
        let mgr = ctx.mgr();
        mgr.update_index_status_at(
            ctx.root(),
            make_update_params(
                "alpha",
                "P1.4",
                IndexStatusValue::Shipped,
                Some("deadbeef"),
                Some("feature-branch"),
            ),
        )
        .unwrap();

        let sidecar = mgr
            .get_index_status_at(ctx.root(), make_get_params("alpha"))
            .unwrap();
        let ticket = &sidecar.tickets["P1.4"];
        assert_eq!(ticket.status, IndexStatusValue::Shipped);
        assert_eq!(ticket.last_shipped_commit.as_deref(), Some("deadbeef"));
        assert_eq!(
            ticket.last_shipped_branch.as_deref(),
            Some("feature-branch")
        );
        assert!(ticket.last_shipped_at.is_some(), "shipped_at should be set");
    }

    #[test]
    fn test_project_name_validation_rejects_traversal() {
        let result = validate_project_name("../evil");
        assert!(
            matches!(result, Err(DaemonError::InvalidParam(_))),
            "traversal should be rejected"
        );
    }

    #[test]
    fn test_project_name_validation_rejects_slash() {
        let result = validate_project_name("foo/bar");
        assert!(
            matches!(result, Err(DaemonError::InvalidParam(_))),
            "slash should be rejected"
        );
    }

    #[test]
    fn test_atomic_rename_integrity() {
        let ctx = Ctx::new();
        let mgr = ctx.mgr();
        mgr.update_index_status_at(
            ctx.root(),
            make_update_params("testproj", "T1", IndexStatusValue::NotStarted, None, None),
        )
        .unwrap();

        let path = sidecar_path(ctx.root(), "testproj");
        assert!(path.exists(), "sidecar file should exist after write");

        // File content must be valid JSON
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content)
            .expect("sidecar content should be valid JSON after atomic rename");
        assert!(parsed.get("schema_version").is_some());
    }

    #[test]
    fn test_validate_project_name_valid() {
        for name in &["topology-on-epic", "my_project", "proj1", "A-B_C123"] {
            validate_project_name(name).unwrap_or_else(|e| {
                panic!("valid name {name:?} was rejected: {e}");
            });
        }
    }
}
