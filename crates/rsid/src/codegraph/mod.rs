//! Isolated codegraph indexing lifecycle. Daemon startup and durable status are
//! connected only after the shared integration and schema seams are allocated.

mod discovery;
mod manager;
mod native;
mod retention;
mod runtime;
mod service;
mod status;
mod watcher;
mod worker;

pub use manager::{IndexHandle, IndexManager, IndexRequestError, IndexWorkspaceBinding};
pub use native::{NativeCodegraphBinding, NativeCodegraphToolKind, execute_native_read};
pub use retention::{DetailedGeneration, RetentionPlan, RetentionPolicy, plan_retention};
pub use runtime::IndexRuntime;
pub use service::{BoundCodegraphScope, CodegraphReadService, CodegraphServiceError};
pub use status::{IndexPhase, IndexStatus};
pub use watcher::IndexWatcher;

use std::path::{Path, PathBuf};

pub(crate) const NATIVE_MAX_OUTPUT_BYTES: usize = 48 * 1024;
pub(crate) const NATIVE_MAX_OUTPUT_TOKENS: usize = 8_000;

/// Serializes an acknowledged operator gate change with the final ready-head
/// promotion. The worker uses `blocking_lock` only on its blocking writer lane.
pub(crate) static PUBLICATION_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use rsi_codegraph::WorkspaceInstanceKey;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("unsafe or inaccessible codegraph workspace: {0}")]
    UnsafeWorkspace(String),
    #[error("codegraph discovery limit exceeded: {0}")]
    DiscoveryLimit(&'static str),
    #[error("codegraph source changed during indexing")]
    SourceChanged,
    #[error("codegraph request superseded by a newer request")]
    Superseded,
    #[error("codegraph physical write budget exhausted ({used_bytes} of {ceiling_bytes} bytes)")]
    DiskBudget { used_bytes: u64, ceiling_bytes: u64 },
    #[error("codegraph write admission deferred by active database reader")]
    ActiveReader,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] rsi_codegraph::CodegraphError),
}

pub type Result<T> = std::result::Result<T, IndexError>;

/// A trusted project registration resolved from daemon project data. Callers
/// must not construct it from a session-supplied path.
#[derive(Debug, Clone)]
pub struct RegisteredWorkspace {
    project_id: Uuid,
    project_root: PathBuf,
    root: PathBuf,
    instance: WorkspaceInstanceKey,
    workspace_id: Uuid,
    repository_common_dir: Option<PathBuf>,
}

impl RegisteredWorkspace {
    /// Bind the registered project's own canonical root.
    pub fn primary(project_id: Uuid, registered_project_root: &Path) -> Result<Self> {
        let root = canonical_dir(registered_project_root)?;
        Self::new(
            project_id,
            root.clone(),
            root,
            WorkspaceInstanceKey::Primary,
            None,
        )
    }

    /// Bind a daemon-registered external checkout to the same Git common dir
    /// as the registered project. The UUID must come from the durable worktree
    /// or sandbox registration, never a session or branch name.
    pub fn registered_checkout(
        project_id: Uuid,
        registered_project_root: &Path,
        registered_root: &Path,
        instance: WorkspaceInstanceKey,
    ) -> Result<Self> {
        if !matches!(
            instance,
            WorkspaceInstanceKey::GitWorktree(_) | WorkspaceInstanceKey::RsiSandbox(_)
        ) {
            return Err(IndexError::UnsafeWorkspace(
                "external checkout has primary identity".into(),
            ));
        }
        let project_root = canonical_dir(registered_project_root)?;
        let root = canonical_dir(registered_root)?;
        let common_dir = git_common_dir(&project_root)?;
        if common_dir != git_common_dir(&root)? {
            return Err(IndexError::UnsafeWorkspace(
                "checkout belongs to a different repository".into(),
            ));
        }
        Self::new(project_id, project_root, root, instance, Some(common_dir))
    }

    /// Derive an identity for a trusted, otherwise unregistered checkout from
    /// its canonical root. The repository check still applies.
    pub fn detached_checkout(
        project_id: Uuid,
        registered_project_root: &Path,
        root: &Path,
    ) -> Result<Self> {
        let canonical = canonical_dir(root)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"rsid-codegraph-detached-root-v1");
        let identity = canonical
            .to_str()
            .ok_or_else(|| IndexError::UnsafeWorkspace("non-UTF-8 detached root".into()))?;
        hasher.update(identity.as_bytes());
        let instance = WorkspaceInstanceKey::DetachedRootHash(*hasher.finalize().as_bytes());
        let project_root = canonical_dir(registered_project_root)?;
        let common_dir = git_common_dir(&project_root)?;
        if common_dir != git_common_dir(&canonical)? {
            return Err(IndexError::UnsafeWorkspace(
                "detached checkout belongs to a different repository".into(),
            ));
        }
        Self::new(
            project_id,
            project_root,
            canonical,
            instance,
            Some(common_dir),
        )
    }

    pub fn project_id(&self) -> Uuid {
        self.project_id
    }
    pub fn workspace_id(&self) -> Uuid {
        self.workspace_id
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn new(
        project_id: Uuid,
        project_root: PathBuf,
        root: PathBuf,
        instance: WorkspaceInstanceKey,
        repository_common_dir: Option<PathBuf>,
    ) -> Result<Self> {
        // Workspace IDs use the same typed derivation as the store. No file is
        // opened here: the worker binds the scope after opening the project DB.
        let workspace_id = match &instance {
            WorkspaceInstanceKey::Primary => {
                rsi_codegraph::CodegraphStore::workspace_id(project_id, "primary")?
            }
            WorkspaceInstanceKey::GitWorktree(id) => rsi_codegraph::CodegraphStore::workspace_id(
                project_id,
                &format!("git-worktree:{id}"),
            )?,
            WorkspaceInstanceKey::RsiSandbox(id) => rsi_codegraph::CodegraphStore::workspace_id(
                project_id,
                &format!("rsi-sandbox:{id}"),
            )?,
            WorkspaceInstanceKey::DetachedRootHash(hash) => {
                rsi_codegraph::CodegraphStore::workspace_id(
                    project_id,
                    &format!("detached:{}", blake3::Hash::from_bytes(*hash).to_hex()),
                )?
            }
        };
        Ok(Self {
            project_id,
            project_root,
            root,
            instance,
            workspace_id,
            repository_common_dir,
        })
    }

    fn validate_current_root(&self) -> Result<()> {
        if self.root.canonicalize()? != self.root {
            return Err(IndexError::UnsafeWorkspace(
                "registered root changed".into(),
            ));
        }
        if let Some(common) = &self.repository_common_dir
            && (git_common_dir(&self.project_root)? != *common
                || git_common_dir(&self.root)? != *common)
        {
            return Err(IndexError::UnsafeWorkspace(
                "registered checkout repository changed".into(),
            ));
        }
        Ok(())
    }
}

fn canonical_dir(path: &Path) -> Result<PathBuf> {
    let canonical = path.canonicalize()?;
    if !canonical.is_dir() {
        return Err(IndexError::UnsafeWorkspace(
            "root is not a directory".into(),
        ));
    }
    Ok(canonical)
}

fn git_common_dir(root: &Path) -> Result<PathBuf> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git.canonicalize()?
    } else {
        let marker = std::fs::read_to_string(&dot_git)?;
        let relative = marker
            .trim()
            .strip_prefix("gitdir: ")
            .ok_or_else(|| IndexError::UnsafeWorkspace("invalid Git checkout marker".into()))?;
        let path = Path::new(relative);
        if path.is_absolute() {
            path.canonicalize()?
        } else {
            root.join(path).canonicalize()?
        }
    };
    let common_marker = git_dir.join("commondir");
    if common_marker.is_file() {
        let relative = std::fs::read_to_string(common_marker)?;
        git_dir
            .join(relative.trim())
            .canonicalize()
            .map_err(Into::into)
    } else {
        Ok(git_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_checkout_requires_same_registered_repository() {
        let primary = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(primary.path().join(".git")).unwrap();
        std::fs::create_dir(other.path().join(".git")).unwrap();
        std::fs::write(
            external.path().join(".git"),
            format!("gitdir: {}\n", primary.path().join(".git").display()),
        )
        .unwrap();
        let project = Uuid::new_v4();
        let sandbox = RegisteredWorkspace::registered_checkout(
            project,
            primary.path(),
            external.path(),
            WorkspaceInstanceKey::RsiSandbox(Uuid::new_v4()),
        )
        .unwrap();
        assert_ne!(
            sandbox.workspace_id,
            RegisteredWorkspace::primary(project, primary.path())
                .unwrap()
                .workspace_id
        );
        assert!(matches!(
            RegisteredWorkspace::registered_checkout(
                project,
                other.path(),
                external.path(),
                WorkspaceInstanceKey::GitWorktree(Uuid::new_v4())
            ),
            Err(IndexError::UnsafeWorkspace(_))
        ));
        let detached =
            RegisteredWorkspace::detached_checkout(project, primary.path(), external.path())
                .unwrap();
        assert_eq!(
            detached.workspace_id,
            RegisteredWorkspace::detached_checkout(project, primary.path(), external.path())
                .unwrap()
                .workspace_id
        );
    }
}
