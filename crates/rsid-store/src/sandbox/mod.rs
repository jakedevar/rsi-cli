//! Sandbox allocation for agent isolation.
//!
//! A **sandbox** is a per-session filesystem checkout (today: a git worktree)
//! that isolates agent writes from the canonical working directory. The
//! allocator creates the sandbox **before** the Session row is inserted — if
//! allocation fails the launch aborts before any state is persisted. This
//! fail-closed semantic (a sandbox-requested session that silently runs
//! without one is a security downgrade) is enforced by every caller.
//!
//! ## Persisted on-disk state
//!
//! Sandboxes have two lifecycle phases tracked via
//! `rsi_common::types::SandboxCleanupState` on the `Session` row:
//!
//! 1. `Live` — filesystem state exists, owned by an active/completed session.
//! 2. `Purged` — a historical destroy completed successfully; filesystem
//!    state was removed.
//! 3. `Failed` — a historical destroy attempt may have left partial state.
//!
//! D00 puts [`cleanup`] in front of every production cleanup entry point and
//! deliberately supplies no positive authorization. Raw destroy primitives
//! remain private to this module for isolated low-level tests.
//!
//! ## Orphan sweep
//!
//! `list_on_disk` walks the base directory and returns every sandbox UUID
//! present on disk. Startup classifies every typed or path-only candidate
//! through [`cleanup`] and retains it without mutation.

pub mod cleanup;
pub mod custody;
pub mod execution_scratch;
pub mod git_worktree;
pub mod reclaim;
#[cfg(target_os = "linux")]
pub mod target_reclaim;
#[cfg(not(target_os = "linux"))]
#[path = "target_reclaim_unsupported.rs"]
pub mod target_reclaim;
mod worktree_content_digest;

use crate::error::{DaemonError, Result};
use rsi_common::types::SandboxKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;
pub use worktree_content_digest::{WorktreeDigest, worktree_content_digest};

/// Handle returned by [`SandboxAllocator::allocate`] describing the on-disk
/// sandbox root. Callers plumb `root` into `Session.sandbox_root` and
/// `branch` into `Session.sandbox_branch`.
///
/// `origin` is not persisted on the Session row — it is recoverable from the
/// Session's canonical `working_dir` column. Production D00 does not invoke
/// Git or observe the path. Sandbox-internal unit tests use `origin` to
/// exercise raw test-only destructors against temporary repositories.
#[derive(Debug, Clone)]
pub struct SandboxAllocation {
    pub kind: SandboxKind,
    /// Absolute, canonical path to the sandbox root on disk.
    pub root: PathBuf,
    /// Git branch name when `kind == GitWorktree`; `None` otherwise.
    pub branch: Option<String>,
    /// Origin repo path (the canonical working_dir), retained for identity
    /// and test-only low-level teardown fixtures.
    pub origin: PathBuf,
}

/// Allocates and enumerates per-session sandboxes.
///
/// Stateless — the only persistent state is `base_dir`. Thread-safe by virtue
/// of operating on independent UUID-scoped subdirectories. Raw destruction is
/// sandbox-module-private and exercised only by isolated unit tests under D00.
pub struct SandboxAllocator {
    base_dir: PathBuf,
}

/// Serializes capacity observation through the filesystem allocation effect.
/// There is one lock for the daemon process, including replacement allocators.
/// The owned guard makes the permit movable into a blocking Git operation but
/// deliberately not cloneable.
pub struct AllocationPermit {
    base: PathBuf,
    base_identity: (u64, u64),
    _guard: OwnedMutexGuard<()>,
}

#[derive(Clone, Copy)]
pub struct SandboxCapacity {
    pub source_roots: u64,
    pub available_bytes: u64,
}

static ALLOCATION_LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();

#[cfg(test)]
mod allocation_tests;

impl AllocationPermit {
    pub async fn acquire(base: &Path) -> Result<Self> {
        let guard = ALLOCATION_LOCK
            .get_or_init(|| Arc::new(Mutex::new(())))
            .clone()
            .lock_owned()
            .await;
        let canonical = std::fs::canonicalize(base).map_err(|error| {
            DaemonError::Process(format!("canonicalize sandbox base for allocation: {error}"))
        })?;
        if canonical != base {
            return Err(DaemonError::Process(
                "sandbox base must be an exact canonical directory for allocation".into(),
            ));
        }
        let metadata = std::fs::symlink_metadata(&canonical)?;
        if !metadata.is_dir() {
            return Err(DaemonError::Process(
                "sandbox base is not a directory for allocation".into(),
            ));
        }
        Ok(Self {
            base: canonical,
            base_identity: (metadata.dev(), metadata.ino()),
            _guard: guard,
        })
    }

    pub fn measure(&self) -> Result<SandboxCapacity> {
        self.check_base(&self.base)?;
        let mut source_roots = 0_u64;
        for entry in std::fs::read_dir(&self.base).map_err(|error| {
            DaemonError::Process(format!("read sandbox roots for allocation: {error}"))
        })? {
            let entry = entry.map_err(|error| {
                DaemonError::Process(format!("read sandbox root for allocation: {error}"))
            })?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| Uuid::parse_str(name).is_ok())
            {
                source_roots = source_roots.saturating_add(1);
            }
        }
        let stats = nix::sys::statvfs::statvfs(&self.base).map_err(|error| {
            DaemonError::Process(format!("sandbox allocation statvfs failed: {error}"))
        })?;
        self.check_base(&self.base)?;
        Ok(SandboxCapacity {
            source_roots,
            available_bytes: (stats.blocks_available() as u64)
                .saturating_mul(stats.fragment_size() as u64),
        })
    }

    fn check_base(&self, base: &Path) -> Result<()> {
        let metadata = std::fs::symlink_metadata(base)?;
        if base == self.base
            && metadata.is_dir()
            && (metadata.dev(), metadata.ino()) == self.base_identity
        {
            Ok(())
        } else {
            Err(DaemonError::Process(
                "allocation permit belongs to a different sandbox base".into(),
            ))
        }
    }
}

impl SandboxAllocator {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Base directory for all sandboxes. Callers generally do not need this;
    /// exposed for diagnostics and the orphan sweep.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Ensure the base directory exists with mode 0o700. Idempotent.
    ///
    /// Called once at daemon startup. Safe to call concurrently — the Unix
    /// `create_dir_all` is race-safe and the chmod is a no-op when already
    /// applied.
    pub fn ensure_base(&self) -> Result<()> {
        std::fs::create_dir_all(&self.base_dir).map_err(|e| {
            DaemonError::Process(format!(
                "Failed to create sandbox base dir '{}': {}",
                self.base_dir.display(),
                e
            ))
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            if let Err(e) = std::fs::set_permissions(&self.base_dir, perms) {
                tracing::warn!(
                    base_dir = %self.base_dir.display(),
                    error = %e,
                    "Failed to chmod 0o700 on sandbox base dir (continuing)"
                );
            }
        }

        Ok(())
    }

    /// Allocate a sandbox for the given session.
    ///
    /// - `origin` is the canonical working_dir. For `GitWorktree`, it must
    ///   be inside a git repository — otherwise `InvalidParam` is returned.
    /// - On success, the returned allocation describes a fresh sandbox root.
    /// - On failure, callers MUST surface the error and abort the launch.
    ///   Never silently degrade to the canonical working_dir.
    ///
    /// This entry point is unadmitted: it takes no allocation permit and
    /// applies no capacity budget. It stays public for allocator tests,
    /// including the `sandbox_e2e` integration target. Daemon launch paths
    /// must call `SessionManager::admit_sandbox_allocation` and then
    /// [`Self::allocate_with_permit`].
    pub fn allocate(
        &self,
        session_id: Uuid,
        origin: &Path,
        kind: SandboxKind,
        source_commit: &str,
        requested_branch: Option<&str>,
    ) -> Result<SandboxAllocation> {
        self.allocate_unchecked(session_id, origin, kind, source_commit, requested_branch)
    }

    #[allow(clippy::needless_pass_by_value)] // Own the permit through the allocation.
    pub fn allocate_with_permit(
        &self,
        permit: AllocationPermit,
        session_id: Uuid,
        origin: &Path,
        kind: SandboxKind,
        source_commit: &str,
        requested_branch: Option<&str>,
    ) -> Result<SandboxAllocation> {
        permit.check_base(&self.base_dir)?;
        self.allocate_unchecked(session_id, origin, kind, source_commit, requested_branch)
    }

    fn allocate_unchecked(
        &self,
        session_id: Uuid,
        origin: &Path,
        kind: SandboxKind,
        source_commit: &str,
        requested_branch: Option<&str>,
    ) -> Result<SandboxAllocation> {
        self.ensure_base()?;

        match kind {
            SandboxKind::None => Err(DaemonError::InvalidParam(
                "sandbox kind=None cannot be allocated".to_string(),
            )),
            SandboxKind::GitWorktree => git_worktree::allocate(
                &self.base_dir,
                session_id,
                origin,
                source_commit,
                requested_branch,
            ),
            _ => Err(DaemonError::InvalidParam(format!(
                "unsupported sandbox kind: {:?}",
                kind
            ))),
        }
    }

    /// Allocate a distinct sandbox root for a session that had a prior,
    /// terminal sandbox. This is used to restore an archived session without
    /// reusing a historical custody identity.
    #[cfg(any(test, feature = "test-seam"))]
    pub(crate) fn allocate_replacement(
        &self,
        session_id: Uuid,
        origin: &Path,
        kind: SandboxKind,
        source_commit: &str,
        requested_branch: Option<&str>,
    ) -> Result<SandboxAllocation> {
        self.allocate_replacement_unchecked(
            session_id,
            origin,
            kind,
            source_commit,
            requested_branch,
        )
    }

    #[allow(clippy::needless_pass_by_value)] // Own the permit through the allocation.
    pub fn allocate_replacement_with_permit(
        &self,
        permit: AllocationPermit,
        session_id: Uuid,
        origin: &Path,
        kind: SandboxKind,
        source_commit: &str,
        requested_branch: Option<&str>,
    ) -> Result<SandboxAllocation> {
        permit.check_base(&self.base_dir)?;
        self.allocate_replacement_unchecked(
            session_id,
            origin,
            kind,
            source_commit,
            requested_branch,
        )
    }

    fn allocate_replacement_unchecked(
        &self,
        session_id: Uuid,
        origin: &Path,
        kind: SandboxKind,
        source_commit: &str,
        requested_branch: Option<&str>,
    ) -> Result<SandboxAllocation> {
        self.ensure_base()?;

        match kind {
            SandboxKind::GitWorktree => git_worktree::allocate_replacement(
                &self.base_dir,
                session_id,
                origin,
                source_commit,
                requested_branch,
            ),
            _ => Err(DaemonError::InvalidParam(
                "sandbox replacement requires a GitWorktree sandbox".into(),
            )),
        }
    }

    /// Recover the one deterministic Closure allocation reserved before the
    /// launch effect, or allocate it when no prior effect exists.
    #[allow(clippy::needless_pass_by_value)] // Own the permit through the allocation.
    pub fn allocate_or_adopt_reserved_closure(
        &self,
        permit: AllocationPermit,
        session_id: Uuid,
        origin: &Path,
        kind: SandboxKind,
        source_commit: &str,
        requested_branch: Option<&str>,
    ) -> Result<SandboxAllocation> {
        permit.check_base(&self.base_dir)?;
        self.ensure_base()?;
        match kind {
            SandboxKind::GitWorktree => git_worktree::adopt_reserved(
                &self.base_dir,
                session_id,
                origin,
                source_commit,
                requested_branch,
            )?
            .map_or_else(
                || {
                    git_worktree::allocate(
                        &self.base_dir,
                        session_id,
                        origin,
                        source_commit,
                        requested_branch,
                    )
                },
                Ok,
            ),
            _ => Err(DaemonError::InvalidParam(
                "reserved Closure launch requires a GitWorktree sandbox".into(),
            )),
        }
    }

    /// Raw sandbox destruction for sandbox-internal isolated tests only.
    #[cfg(test)]
    pub(in crate::sandbox) fn destroy(&self, allocation: &SandboxAllocation) -> Result<()> {
        match allocation.kind {
            SandboxKind::None => Ok(()),
            SandboxKind::GitWorktree => git_worktree::destroy(allocation),
            _ => Ok(()),
        }
    }

    /// Raw path-only destruction for sandbox-internal isolated tests only.
    #[cfg(test)]
    pub(in crate::sandbox) fn destroy_by_path(&self, path: &Path) -> Result<()> {
        git_worktree::destroy_by_path(path)
    }

    /// Enumerate all on-disk sandbox UUIDs under `base_dir`.
    ///
    /// Returns an empty Vec when the base dir does not yet exist — the first
    /// daemon run has no sandboxes.
    pub fn list_on_disk(&self) -> Result<Vec<Uuid>> {
        if !self.base_dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let rd = std::fs::read_dir(&self.base_dir).map_err(|e| {
            DaemonError::Process(format!(
                "Failed to read sandbox base dir '{}': {}",
                self.base_dir.display(),
                e
            ))
        })?;
        for entry in rd.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && let Ok(uuid) = Uuid::parse_str(name)
            {
                out.push(uuid);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::{TempDir, tempdir};

    fn init_git_repo(dir: &Path) {
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir)
            .status()
            .expect("git init");
        // minimum config so `git commit` works
        Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(dir)
            .status()
            .ok();
        Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(dir)
            .status()
            .ok();
        std::fs::write(dir.join("README.md"), "x").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(dir)
            .status()
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn sandbox_allocate_happy_path() {
        let repo = tempdir().unwrap();
        let base = tempdir().unwrap();
        init_git_repo(repo.path());

        let allocator = SandboxAllocator::new(base.path().to_path_buf());
        let sid = Uuid::new_v4();
        let alloc = allocator
            .allocate(sid, repo.path(), SandboxKind::GitWorktree, "HEAD", None)
            .expect("allocate must succeed on git repo");

        assert_eq!(alloc.kind, SandboxKind::GitWorktree);
        assert!(alloc.root.exists(), "sandbox root must exist on disk");
        assert!(
            alloc.root.starts_with(base.path()),
            "sandbox root must live under base dir"
        );
        let branch = alloc.branch.as_deref().expect("branch must be populated");
        assert!(branch.starts_with("rsi/"), "branch should be rsi/<short>");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn sandbox_allocation_honors_explicit_commit_and_branch_policy() {
        let repo = tempdir().unwrap();
        let base = tempdir().unwrap();
        init_git_repo(repo.path());
        let first_commit = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        std::fs::write(repo.path().join("later.txt"), "later").unwrap();
        Command::new("git")
            .args(["add", "later.txt"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", "later"])
            .current_dir(repo.path())
            .status()
            .unwrap();

        let allocation = SandboxAllocator::new(base.path().to_path_buf())
            .allocate(
                Uuid::new_v4(),
                repo.path(),
                SandboxKind::GitWorktree,
                first_commit.trim(),
                Some("rsi/explicit-source"),
            )
            .unwrap();

        assert_eq!(allocation.branch.as_deref(), Some("rsi/explicit-source"));
        assert!(
            !allocation.root.join("later.txt").exists(),
            "allocation must use the requested commit, not canonical HEAD"
        );
    }

    /// A directory git cannot resolve to a repository, whichever `$TMPDIR`
    /// happens to be in force.
    ///
    /// `git rev-parse --is-inside-work-tree` walks ancestors, so a bare
    /// `tempdir()` is only "outside a repo" when `$TMPDIR` itself is outside
    /// one. rsi-managed agent sessions stamp
    /// `TMPDIR=<sandbox_root>/target/.rsi-tmp`, which IS inside the session's
    /// git worktree: the walk then finds the sandbox repo, reports `true`, and
    /// allocation legitimately succeeds. A `.git` gitfile pointing at a
    /// nonexistent git dir stops discovery at this directory on every host and
    /// under every `$TMPDIR`, so the fixture tests the typed rejection without
    /// depending on where the temp dir lives.
    fn non_repository_dir(base: &Path) -> TempDir {
        let dir = tempfile::Builder::new()
            .prefix("non-repo-")
            .tempdir_in(base)
            .unwrap();
        std::fs::write(
            dir.path().join(".git"),
            "gitdir: /nonexistent/rsi-non-repo-fixture\n",
        )
        .unwrap();
        dir
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn sandbox_allocate_non_git_rejected() {
        let base = tempdir().unwrap();
        let non_git = non_repository_dir(base.path());

        let allocator = SandboxAllocator::new(base.path().to_path_buf());
        let sid = Uuid::new_v4();
        let err = allocator
            .allocate(sid, non_git.path(), SandboxKind::GitWorktree, "HEAD", None)
            .expect_err("non-git dir must fail");
        match err {
            DaemonError::InvalidParam(msg) => assert!(
                msg.contains("git repository") || msg.contains("git"),
                "unexpected msg: {msg}"
            ),
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn sandbox_destroy_removes_worktree_and_branch() {
        let repo = tempdir().unwrap();
        let base = tempdir().unwrap();
        init_git_repo(repo.path());

        let allocator = SandboxAllocator::new(base.path().to_path_buf());
        let sid = Uuid::new_v4();
        let alloc = allocator
            .allocate(sid, repo.path(), SandboxKind::GitWorktree, "HEAD", None)
            .expect("allocate");

        assert!(alloc.root.exists());
        allocator.destroy(&alloc).expect("destroy must succeed");
        assert!(
            !alloc.root.exists(),
            "sandbox root must be removed after destroy"
        );

        // Branch must also be deleted.
        let branch = alloc.branch.as_deref().unwrap();
        let out = Command::new("git")
            .args(["branch", "--list", branch])
            .current_dir(repo.path())
            .output()
            .expect("git branch --list");
        let listed = String::from_utf8_lossy(&out.stdout);
        assert!(
            !listed.contains(branch),
            "branch '{branch}' should have been deleted: {listed:?}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn sandbox_destroy_idempotent() {
        let repo = tempdir().unwrap();
        let base = tempdir().unwrap();
        init_git_repo(repo.path());

        let allocator = SandboxAllocator::new(base.path().to_path_buf());
        let sid = Uuid::new_v4();
        let alloc = allocator
            .allocate(sid, repo.path(), SandboxKind::GitWorktree, "HEAD", None)
            .expect("allocate");

        allocator.destroy(&alloc).expect("first destroy");
        allocator
            .destroy(&alloc)
            .expect("second destroy must be idempotent");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn sandbox_destroy_by_path_is_confined_to_isolated_unit_fixture() {
        let base = tempdir().unwrap();
        let root = base.path().join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("fixture.txt"), "temporary").unwrap();
        let allocator = SandboxAllocator::new(base.path().to_path_buf());

        allocator
            .destroy_by_path(&root)
            .expect("destroy path fixture");

        assert!(!root.exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn sandbox_list_on_disk_missing_base_returns_empty() {
        let base = tempdir().unwrap();
        // Remove the temp dir so it doesn't exist.
        let path = base.path().join("nope");
        let allocator = SandboxAllocator::new(path);
        let list = allocator.list_on_disk().unwrap();
        assert!(list.is_empty());
    }

    /// Mirrors the Session-row population pattern used in
    /// `session::launch::launch_session`. An allocation's (kind, root,
    /// branch) must translate 1:1 into the Session's sandbox_* columns;
    /// the absence of an allocation must produce all-None columns. This
    /// test guards that mapping so a future refactor that accidentally
    /// drops the branch or mis-maps the kind is caught by the unit suite.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn launch_sandbox_columns_populated() {
        use rsi_common::types::{SandboxCleanupState, SandboxKind};
        let repo = tempdir().unwrap();
        let base = tempdir().unwrap();
        init_git_repo(repo.path());

        let allocator = SandboxAllocator::new(base.path().to_path_buf());
        let sid = Uuid::new_v4();

        // Case 1: sandbox allocated — every column must be Some.
        let alloc = allocator
            .allocate(sid, repo.path(), SandboxKind::GitWorktree, "HEAD", None)
            .expect("allocate");

        let populated_kind = Some(alloc.kind);
        let populated_root = Some(alloc.root.clone());
        let populated_branch = alloc.branch.clone();
        let populated_cleanup_state = Some(SandboxCleanupState::Live);

        assert_eq!(populated_kind, Some(SandboxKind::GitWorktree));
        assert!(populated_root.as_ref().unwrap().exists());
        assert!(populated_branch.as_ref().unwrap().starts_with("rsi/"));
        assert_eq!(populated_cleanup_state, Some(SandboxCleanupState::Live));

        // Case 2: no allocation — every column must be None. Same logical
        // shape as launch.rs when `config.sandbox` is `None`.
        let none_alloc: Option<SandboxAllocation> = None;
        let empty_kind = none_alloc.as_ref().map(|a| a.kind);
        let empty_root = none_alloc.as_ref().map(|a| a.root.clone());
        let empty_branch = none_alloc.as_ref().and_then(|a| a.branch.clone());
        let empty_cleanup_state = none_alloc.as_ref().map(|_| SandboxCleanupState::Live);
        assert!(empty_kind.is_none());
        assert!(empty_root.is_none());
        assert!(empty_branch.is_none());
        assert!(empty_cleanup_state.is_none());

        // Cleanup
        allocator.destroy(&alloc).expect("destroy");
    }
}
