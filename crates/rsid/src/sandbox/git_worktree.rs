//! Git-worktree-backed sandbox implementation.
//!
//! Allocation:
//! 1. Verify `origin` is inside a git working tree via
//!    `git -C origin rev-parse --is-inside-work-tree`.
//! 2. Pre-validate the proposed sandbox root with
//!    `path_safety::canonicalize_non_strict` so we reject nonsense paths
//!    (symlink cycles) before asking git to create them.
//! 3. `git -C origin worktree add -b <branch> --quiet <root> <commit>` — creates
//!    the directory and the branch atomically from an authenticated source
//!    commit. If the add fails for any reason
//!    (dirty index, name collision, disk full), git cleans up after itself
//!    and we surface the stderr to the caller.
//!
//! The raw force/prune/recursive destroy helpers are compiled only for isolated
//! tests. Production source-root deletion is owned by the branch-first cohort
//! settlement primitives below: they require repository locking, exact direct
//! ref proof, non-force removal, durable recovery authority, and final
//! postcondition checks. The one narrow exception is `retained_successor`:
//! a never-published master-successor allocation proven pristine and
//! unique-commit-free is removed without force under the repository lock.

use super::SandboxAllocation;
use crate::error::{DaemonError, Result};
use crate::path_safety::canonicalize_non_strict;
use crate::process_control::{
    ProcessContainment, configure_std_process_group, terminate_process_group,
};
use rsi_common::types::SandboxKind;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

mod retained_successor;
pub(crate) use retained_successor::{
    RETAINED_SUCCESSOR_ROOT_FOREIGN, RetainedSuccessorRoot, freshest_successor_source,
    reclaim_retained_successor_root,
};

const MAX_GIT_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_GIT_STREAM_BYTES: usize = 16 * 1024 * 1024;
const MAX_GIT_STREAM_RECORDS: usize = 1_000_000;
const MAX_GIT_RECORD_BYTES: usize = 16 * 1024;
const MAX_GIT_WORKTREE_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_GIT_WORKTREE_RECORDS: usize = 262_144;
const MAX_GIT_WORKTREE_RECORD_BYTES: usize = 16 * 1024;
const MAX_GIT_WORKTREE_ENTRIES: usize = 65_536;
const MAX_GIT_STDIN_BYTES: usize = 32 * 1024;
const GIT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
const ROLLING_FETCH_TIMEOUT: Duration = Duration::from_secs(8);
const GIT_POST_EXIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(1);
const QUARANTINE_DIRECTORY_MODE: u32 = 0o700;
const QUARANTINE_TREE_MAX_ENTRIES: usize = 262_144;
const QUARANTINE_TREE_MAX_RELATIVE_PATH_BYTES: usize = 64 * 1024 * 1024;
const QUARANTINE_TREE_MAX_DEPTH: usize = 1_024;
const QUARANTINE_TREE_DEADLINE: Duration = Duration::from_secs(10);
const QUARANTINE_MOUNTINFO_MAX_BYTES: usize = 16 * 1024 * 1024;
const QUARANTINE_MOUNTINFO_MAX_RECORDS: usize = 131_072;
const WORKTREE_ADMIN_FILE_MAX_BYTES: usize = 16 * 1024;
const MAX_RECOVERY_STATE_DISPATCHES: usize = 2;

static REPOSITORY_MUTATION_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
thread_local! {
    static ATOMIC_REF_PRE_SPAWN_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static DIRECT_REF_EMPTY_LOOKUP_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static WORKTREE_REMOVE_POST_EFFECT_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static WORKTREE_REMOVE_LOST_ACK_TEST: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    static RESTORE_REATTACH_LOST_ACK_TEST: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepositoryTargetObservation {
    pub repository_identity: String,
    pub canonical_repo_dir: PathBuf,
    pub target_ref: String,
    pub target_oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeObservation {
    pub root_exists: bool,
    pub root_is_symlink: bool,
    pub registered: bool,
    pub registered_head: Option<String>,
    pub registered_branch: Option<String>,
    pub head_oid: Option<String>,
    pub head_ref: Option<String>,
    pub clean: bool,
    pub clean_state_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct FilesystemIdentity {
    device: u64,
    inode: u64,
}

impl FilesystemIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    pub(crate) const fn device(self) -> u64 {
        self.device
    }

    pub(crate) const fn inode(self) -> u64 {
        self.inode
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuarantinePathProof {
    pub(crate) original_root: PathBuf,
    pub(crate) quarantine_root: PathBuf,
    pub(crate) root_identity: FilesystemIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeAdminIdentity {
    pub(crate) repository_identity: PathBuf,
    pub(crate) admin_directory: PathBuf,
    pub(crate) admin_id: String,
    pub(crate) root_identity: FilesystemIdentity,
}

#[derive(Debug, Clone)]
pub(crate) struct QuarantineTreeProof {
    root: PathBuf,
    root_identity: FilesystemIdentity,
    tree_digest: String,
    identities: HashSet<FilesystemIdentity>,
}

impl QuarantineTreeProof {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) const fn root_identity(&self) -> FilesystemIdentity {
        self.root_identity
    }

    pub(crate) fn tree_digest(&self) -> &str {
        &self.tree_digest
    }

    pub(crate) fn contains_identity(&self, device: u64, inode: u64) -> bool {
        self.identities
            .contains(&FilesystemIdentity { device, inode })
    }

    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.identities.len()
    }
}

#[derive(Debug, Clone)]
struct RegisteredWorktree {
    root: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    prunable: bool,
}

struct QuarantineTreeDigestEntry {
    relative_path: Vec<u8>,
    identity: FilesystemIdentity,
    mode: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl QuarantineTreeDigestEntry {
    fn from_metadata(relative_path: Vec<u8>, metadata: &std::fs::Metadata) -> Self {
        Self {
            relative_path,
            identity: FilesystemIdentity::from_metadata(metadata),
            mode: metadata.mode(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

/// Serialize every daemon Git mutation for one repository. Callers must not
/// await while inside `operation`; all Git work is synchronous and intended
/// for a bounded blocking task.
pub(crate) fn with_repository_mutation<T>(
    origin: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let identity = repository_identity_path(origin)?;
    let lock = {
        let mut registry = REPOSITORY_MUTATION_LOCKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            registry
                .entry(identity)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    };
    let _guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    operation()
}

pub(crate) fn observe_repository_target_locked(
    origin: &Path,
) -> Result<RepositoryTargetObservation> {
    let canonical_repo_dir = std::fs::canonicalize(origin).map_err(|_| {
        DaemonError::InvalidParam("settlement repository path is unavailable".into())
    })?;
    let repository_identity = repository_identity_path(origin)?
        .to_string_lossy()
        .to_string();
    let target_ref = run_git_text(
        origin,
        &["symbolic-ref", "-q", "HEAD"],
        "resolve target ref",
    )?;
    if !target_ref.starts_with("refs/heads/") {
        return Err(DaemonError::InvalidParam(
            "settlement target is detached or not a local branch".into(),
        ));
    }
    let target_oid = resolve_ref_locked(origin, &target_ref)?.ok_or_else(|| {
        DaemonError::InvalidParam("settlement target branch is unavailable".into())
    })?;
    Ok(RepositoryTargetObservation {
        repository_identity,
        canonical_repo_dir,
        target_ref,
        target_oid,
    })
}

pub(crate) fn observe_worktree_locked(origin: &Path, root: &Path) -> Result<WorktreeObservation> {
    let metadata = std::fs::symlink_metadata(root).ok();
    let root_exists = metadata.is_some();
    let root_is_symlink = metadata.is_some_and(|value| value.file_type().is_symlink());
    let registered = list_worktrees_locked(origin)?
        .into_iter()
        .find(|entry| same_path(&entry.root, root));
    let (head_oid, head_ref, clean, clean_state_digest) = if root_exists && !root_is_symlink {
        let head_oid = run_git_text(
            root,
            &["rev-parse", "--verify", "HEAD^{commit}"],
            "resolve worktree head",
        )
        .ok();
        let head_ref = run_git_text(
            root,
            &["symbolic-ref", "-q", "HEAD"],
            "resolve worktree branch",
        )
        .ok();
        let status = run_git_bytes(
            root,
            &[
                "-c",
                "core.fsmonitor=false",
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignored=matching",
                "--ignore-submodules=none",
            ],
            "inspect worktree cleanliness",
        )?;
        let (visibility_safe, visibility_digest) = inspect_index_visibility_locked(root)?;
        let clean = status.is_empty() && visibility_safe;
        let mut digest = Sha256::new();
        digest.update(b"rsi-worktree-clean-state-v2\0");
        digest.update((status.len() as u64).to_be_bytes());
        digest.update(&status);
        digest.update((visibility_digest.len() as u64).to_be_bytes());
        digest.update(visibility_digest.as_bytes());
        let clean_state_digest = format!("sha256:{:x}", digest.finalize());
        (head_oid, head_ref, clean, clean_state_digest)
    } else {
        (
            None,
            None,
            false,
            format!("sha256:{:x}", Sha256::digest(b"missing")),
        )
    };
    Ok(WorktreeObservation {
        root_exists,
        root_is_symlink,
        registered: registered.is_some(),
        registered_head: registered.as_ref().and_then(|entry| entry.head.clone()),
        registered_branch: registered.as_ref().and_then(|entry| entry.branch.clone()),
        head_oid,
        head_ref,
        clean,
        clean_state_digest,
    })
}

/// Authenticate one exact clean registered worktree without mutating Git.
/// This is the neutral pre-intent counterpart to the quarantine move proof.
pub(crate) fn prove_registered_worktree_exact_locked(
    origin: &Path,
    root: &Path,
    expected_branch: &str,
    expected_oid: &str,
) -> Result<WorktreeAdminIdentity> {
    validate_expected_worktree_identity(expected_branch, expected_oid)?;
    let observation = observe_worktree_locked(origin, root)?;
    if observation.root_is_symlink
        || !observation.root_exists
        || !observation.registered
        || !observation.clean
        || observation.registered_branch.as_deref() != Some(expected_branch)
        || observation.head_ref.as_deref() != Some(expected_branch)
        || observation.registered_head.as_deref() != Some(expected_oid)
        || observation.head_oid.as_deref() != Some(expected_oid)
        || resolve_ref_locked(origin, expected_branch)?.as_deref() != Some(expected_oid)
        || !source_ref_is_registered_only_at_locked(origin, expected_branch, root, expected_oid)?
        || source_ref_has_symref_dependents_locked(origin, expected_branch)?
    {
        return Err(DaemonError::Process(
            "registered worktree identity or cleanliness did not match".into(),
        ));
    }
    prove_admin_identity(origin, root, &[root])
}

/// Derive the only quarantine path authorized for one journaled settlement
/// item.  The durable sandbox root, run id, and Session id are sufficient to
/// reconstruct it after a crash; no mutable configuration path participates.
pub(crate) fn derive_settlement_quarantine_path(
    original_root: &Path,
    run_id: Uuid,
    session_id: Uuid,
) -> Result<PathBuf> {
    let session_name = session_id.to_string();
    if !original_root.is_absolute()
        || original_root.file_name().and_then(|name| name.to_str()) != Some(session_name.as_str())
    {
        return Err(DaemonError::InvalidParam(
            "settlement root is not the exact absolute Session path".into(),
        ));
    }
    let base = original_root
        .parent()
        .ok_or_else(|| DaemonError::InvalidParam("settlement root has no sandbox parent".into()))?;
    Ok(base
        .join(".settlement-quarantine")
        .join(run_id.to_string())
        .join(session_name))
}

/// Create and authenticate the deterministic private quarantine parents before
/// `git worktree move`. The destination itself must remain absent, so a
/// collision can never be adopted as the candidate being settled.
pub(crate) fn prepare_settlement_quarantine_path(
    original_root: &Path,
    run_id: Uuid,
    session_id: Uuid,
) -> Result<QuarantinePathProof> {
    let quarantine_root = derive_settlement_quarantine_path(original_root, run_id, session_id)?;
    let base = original_root.parent().expect("derived path has a parent");
    let base_metadata = prove_private_sandbox_base(base)?;
    let original_metadata = exact_directory_metadata(original_root, "settlement root")?;
    if original_metadata.dev() != base_metadata.dev() {
        return Err(DaemonError::InvalidParam(
            "settlement root is a cross-device or nested mount".into(),
        ));
    }

    let quarantine_parent = base.join(".settlement-quarantine");
    let run_parent = quarantine_parent.join(run_id.to_string());
    create_or_prove_private_directory(&quarantine_parent, base_metadata.dev())?;
    create_or_prove_private_directory(&run_parent, base_metadata.dev())?;
    require_path_absent(&quarantine_root, "settlement quarantine destination")?;

    Ok(QuarantinePathProof {
        original_root: original_root.to_path_buf(),
        quarantine_root,
        root_identity: FilesystemIdentity::from_metadata(&original_metadata),
    })
}

/// Move an exact registered worktree by the bounded Git runner. No force flag
/// or fallback filesystem deletion is permitted. The postcondition binds the
/// same directory inode and Git administrative identity at the quarantine.
pub(crate) fn move_worktree_to_quarantine_non_force_locked(
    origin: &Path,
    path_proof: &QuarantinePathProof,
    expected_branch: &str,
    expected_oid: &str,
) -> Result<WorktreeAdminIdentity> {
    validate_expected_worktree_identity(expected_branch, expected_oid)?;
    require_path_absent(
        &path_proof.quarantine_root,
        "settlement quarantine destination",
    )?;
    let before = prove_admin_identity(
        origin,
        &path_proof.original_root,
        &[path_proof.original_root.as_path()],
    )?;
    if before.root_identity != path_proof.root_identity {
        return Err(DaemonError::Process(
            "settlement root inode drifted before quarantine move".into(),
        ));
    }
    prove_single_registration(
        origin,
        &path_proof.original_root,
        expected_branch,
        expected_oid,
    )?;
    if registration_for_path(origin, &path_proof.quarantine_root)?.is_some() {
        return Err(DaemonError::Process(
            "settlement quarantine path was already registered".into(),
        ));
    }

    let original = path_proof
        .original_root
        .to_str()
        .ok_or_else(|| DaemonError::InvalidParam("settlement root is not UTF-8".into()))?;
    let quarantine = path_proof.quarantine_root.to_str().ok_or_else(|| {
        DaemonError::InvalidParam("settlement quarantine path is not UTF-8".into())
    })?;
    let output = run_git_raw(
        origin,
        &["worktree", "move", original, quarantine],
        "move worktree to settlement quarantine",
    )?;
    if !output.status.success() {
        return Err(DaemonError::Process(
            "non-force Git worktree quarantine move refused".into(),
        ));
    }

    let after = prove_moved_worktree_exact_locked(
        origin,
        &path_proof.original_root,
        &path_proof.quarantine_root,
        expected_branch,
        expected_oid,
    )?;
    if after.root_identity != path_proof.root_identity
        || after.admin_directory != before.admin_directory
        || after.admin_id != before.admin_id
        || after.repository_identity != before.repository_identity
    {
        return Err(DaemonError::Process(
            "quarantined worktree identity changed during move".into(),
        ));
    }
    Ok(after)
}

/// Authenticate the complete post-move state: the old path is absent, the
/// deterministic destination is a real directory, Git registers only that
/// destination, and both sides of the linked-worktree admin pointer agree.
pub(crate) fn prove_moved_worktree_exact_locked(
    origin: &Path,
    original_root: &Path,
    quarantine_root: &Path,
    expected_branch: &str,
    expected_oid: &str,
) -> Result<WorktreeAdminIdentity> {
    validate_expected_worktree_identity(expected_branch, expected_oid)?;
    require_path_absent(original_root, "original settlement root")?;
    prove_existing_quarantine_shape(original_root, quarantine_root)?;
    if registration_for_path(origin, original_root)?.is_some() {
        return Err(DaemonError::Process(
            "original settlement worktree registration remained after move".into(),
        ));
    }
    prove_single_registration(origin, quarantine_root, expected_branch, expected_oid)?;
    if !source_ref_is_registered_only_at_locked(
        origin,
        expected_branch,
        quarantine_root,
        expected_oid,
    )? {
        return Err(DaemonError::Process(
            "source ref is not registered only at the settlement quarantine".into(),
        ));
    }
    prove_worktree_head(quarantine_root, expected_branch, expected_oid)?;
    prove_admin_identity(origin, quarantine_root, &[quarantine_root])
}

/// Recover only Git's narrow crash window where the directory rename completed
/// but the administrative `gitdir` pointer still names the original path.
/// Generic or ambiguous `git worktree repair` is never attempted.
pub(crate) fn repair_moved_worktree_if_exact_locked(
    origin: &Path,
    original_root: &Path,
    quarantine_root: &Path,
    expected_branch: &str,
    expected_oid: &str,
) -> Result<WorktreeAdminIdentity> {
    validate_expected_worktree_identity(expected_branch, expected_oid)?;
    require_path_absent(original_root, "original settlement root")?;
    prove_existing_quarantine_shape(original_root, quarantine_root)?;
    prove_worktree_head(quarantine_root, expected_branch, expected_oid)?;
    let original_registration = registration_for_path(origin, original_root)?;
    let quarantine_registration = registration_for_path(origin, quarantine_root)?;
    match (&original_registration, &quarantine_registration) {
        (None, Some(entry))
            if entry.branch.as_deref() == Some(expected_branch)
                && entry.head.as_deref() == Some(expected_oid) =>
        {
            return prove_moved_worktree_exact_locked(
                origin,
                original_root,
                quarantine_root,
                expected_branch,
                expected_oid,
            );
        }
        (Some(entry), None)
            if entry.branch.as_deref() == Some(expected_branch)
                && entry.head.as_deref() == Some(expected_oid) => {}
        _ => {
            return Err(DaemonError::Process(
                "settlement move repair precondition is ambiguous".into(),
            ));
        }
    }
    let before = prove_admin_identity(origin, quarantine_root, &[original_root])?;

    let quarantine = quarantine_root.to_str().ok_or_else(|| {
        DaemonError::InvalidParam("settlement quarantine path is not UTF-8".into())
    })?;
    let output = run_git_raw(
        origin,
        &["worktree", "repair", quarantine],
        "repair exact settlement worktree move",
    )?;
    if !output.status.success() {
        return Err(DaemonError::Process(
            "exact Git worktree move repair refused".into(),
        ));
    }
    let after = prove_moved_worktree_exact_locked(
        origin,
        original_root,
        quarantine_root,
        expected_branch,
        expected_oid,
    )?;
    if before.root_identity != after.root_identity
        || before.admin_directory != after.admin_directory
        || before.admin_id != after.admin_id
        || before.repository_identity != after.repository_identity
    {
        return Err(DaemonError::Process(
            "worktree identity changed during exact move repair".into(),
        ));
    }
    Ok(after)
}

/// Walk a quarantined tree without following symlinks and produce the bounded
/// pathname/metadata snapshot consumed by the process-holder proof.
/// Cross-device and mounted subtrees, special files, and multiply-linked
/// regular files are retained rather than handed to Git's recursive worktree
/// removal. This is not an atomic filesystem transaction: the caller must
/// re-prove the digest immediately before removal, and the single-user threat
/// boundary still excludes an adversarial same-UID pathname racer.
pub(crate) fn prove_quarantine_tree_safe(root: &Path) -> Result<QuarantineTreeProof> {
    prove_quarantine_tree_safe_at(
        root,
        Path::new("/proc/self/mountinfo"),
        QUARANTINE_TREE_MAX_ENTRIES,
        QUARANTINE_TREE_MAX_RELATIVE_PATH_BYTES,
        QUARANTINE_TREE_MAX_DEPTH,
        QUARANTINE_TREE_DEADLINE,
    )
}

fn prove_quarantine_tree_safe_at(
    root: &Path,
    mountinfo_path: &Path,
    max_entries: usize,
    max_relative_path_bytes: usize,
    max_depth: usize,
    timeout: Duration,
) -> Result<QuarantineTreeProof> {
    // Filesystem calls below are blocking. The deadline bounds cooperative
    // progress between calls; the platform must separately bound a wedged
    // filesystem syscall.
    let deadline = Instant::now() + timeout;
    let metadata = exact_directory_metadata(root, "settlement quarantine root")?;
    let canonical = std::fs::canonicalize(root).map_err(|error| {
        DaemonError::Process(format!("quarantine root canonicalization failed: {error}"))
    })?;
    if canonical != root {
        return Err(DaemonError::InvalidParam(
            "settlement quarantine root is not an exact canonical path".into(),
        ));
    }
    ensure_quarantine_tree_deadline(deadline)?;
    reject_mounts_at_or_below(root, mountinfo_path)?;
    ensure_quarantine_tree_deadline(deadline)?;
    reject_extended_attributes(root)?;
    ensure_quarantine_tree_deadline(deadline)?;

    let root_identity = FilesystemIdentity::from_metadata(&metadata);
    let root_device = metadata.dev();
    let mut identities = HashSet::from([root_identity]);
    let mut digest_entries = vec![QuarantineTreeDigestEntry::from_metadata(
        Vec::new(),
        &metadata,
    )];
    let mut stack = vec![(root.to_path_buf(), 0_usize, Vec::<u8>::new())];
    let mut entries = 1_usize;
    let mut relative_path_bytes = 0_usize;
    while let Some((directory, depth, relative_directory)) = stack.pop() {
        ensure_quarantine_tree_deadline(deadline)?;
        if depth >= max_depth {
            return Err(DaemonError::Process(
                "settlement quarantine tree depth exceeded bound".into(),
            ));
        }
        let children = std::fs::read_dir(&directory).map_err(|error| {
            DaemonError::Process(format!(
                "settlement quarantine directory enumeration failed: {error}"
            ))
        })?;
        for child in children {
            ensure_quarantine_tree_deadline(deadline)?;
            entries = entries.checked_add(1).ok_or_else(|| {
                DaemonError::Process("settlement quarantine entry count overflowed".into())
            })?;
            if entries > max_entries {
                return Err(DaemonError::Process(
                    "settlement quarantine entry count exceeded bound".into(),
                ));
            }
            let child = child.map_err(|error| {
                DaemonError::Process(format!(
                    "settlement quarantine directory entry failed: {error}"
                ))
            })?;
            let child_path = child.path();
            let child_name = child.file_name();
            let child_name = child_name.as_bytes();
            let mut relative_path = Vec::with_capacity(
                relative_directory
                    .len()
                    .saturating_add(usize::from(!relative_directory.is_empty()))
                    .saturating_add(child_name.len()),
            );
            relative_path.extend_from_slice(&relative_directory);
            if !relative_path.is_empty() {
                relative_path.push(b'/');
            }
            relative_path.extend_from_slice(child_name);
            relative_path_bytes = relative_path_bytes
                .checked_add(relative_path.len())
                .ok_or_else(|| {
                    DaemonError::Process(
                        "settlement quarantine relative-path bytes overflowed".into(),
                    )
                })?;
            if relative_path_bytes > max_relative_path_bytes {
                return Err(DaemonError::Process(
                    "settlement quarantine relative-path bytes exceeded bound".into(),
                ));
            }
            let child_metadata = std::fs::symlink_metadata(&child_path).map_err(|error| {
                DaemonError::Process(format!(
                    "settlement quarantine entry identity failed: {error}"
                ))
            })?;
            reject_extended_attributes(&child_path)?;
            ensure_quarantine_tree_deadline(deadline)?;
            if child_metadata.dev() != root_device {
                return Err(DaemonError::Process(
                    "settlement quarantine contains a cross-device entry".into(),
                ));
            }
            let file_type = child_metadata.file_type();
            if !(file_type.is_dir() || file_type.is_file() || file_type.is_symlink())
                || file_type.is_block_device()
                || file_type.is_char_device()
                || file_type.is_fifo()
                || file_type.is_socket()
            {
                return Err(DaemonError::Process(
                    "settlement quarantine contains a special file".into(),
                ));
            }
            if file_type.is_file() && child_metadata.nlink() != 1 {
                return Err(DaemonError::Process(
                    "settlement quarantine contains a multiply-linked regular file".into(),
                ));
            }
            let identity = FilesystemIdentity::from_metadata(&child_metadata);
            if !identities.insert(identity) {
                return Err(DaemonError::Process(
                    "settlement quarantine contains a repeated filesystem identity".into(),
                ));
            }
            digest_entries.push(QuarantineTreeDigestEntry::from_metadata(
                relative_path.clone(),
                &child_metadata,
            ));
            if file_type.is_dir() {
                stack.push((child_path, depth + 1, relative_path));
            }
        }
    }

    digest_entries.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let mut tree_digest = Sha256::new();
    tree_digest.update(b"rsi-quarantine-tree-proof-v1\0");
    for entry in digest_entries {
        tree_digest.update((entry.relative_path.len() as u64).to_be_bytes());
        tree_digest.update(&entry.relative_path);
        tree_digest.update(entry.identity.device.to_be_bytes());
        tree_digest.update(entry.identity.inode.to_be_bytes());
        tree_digest.update(entry.mode.to_be_bytes());
        tree_digest.update(entry.size.to_be_bytes());
        tree_digest.update(entry.modified_seconds.to_be_bytes());
        tree_digest.update(entry.modified_nanoseconds.to_be_bytes());
        tree_digest.update(entry.changed_seconds.to_be_bytes());
        tree_digest.update(entry.changed_nanoseconds.to_be_bytes());
    }

    Ok(QuarantineTreeProof {
        root: root.to_path_buf(),
        root_identity,
        tree_digest: format!("sha256:{:x}", tree_digest.finalize()),
        identities,
    })
}

fn ensure_quarantine_tree_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(DaemonError::Process(
            "settlement quarantine tree proof exceeded its deadline".into(),
        ));
    }
    Ok(())
}

pub(crate) fn reprove_quarantine_tree_unchanged(
    expected: &QuarantineTreeProof,
) -> Result<QuarantineTreeProof> {
    let current = prove_quarantine_tree_safe(expected.root())?;
    if current.root_identity != expected.root_identity
        || current.tree_digest != expected.tree_digest
    {
        return Err(DaemonError::Process(
            "settlement quarantine tree changed after its safety proof".into(),
        ));
    }
    Ok(current)
}

fn exact_directory_metadata(path: &Path, label: &str) -> Result<std::fs::Metadata> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| DaemonError::Process(format!("{label} is unavailable: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DaemonError::InvalidParam(format!(
            "{label} is not an exact directory"
        )));
    }
    Ok(metadata)
}

fn prove_private_sandbox_base(base: &Path) -> Result<std::fs::Metadata> {
    // SAFETY: `geteuid` has no preconditions and cannot mutate process state.
    prove_private_sandbox_base_for_uid(base, unsafe { nix::libc::geteuid() })
}

fn prove_private_sandbox_base_for_uid(base: &Path, expected_uid: u32) -> Result<std::fs::Metadata> {
    let metadata = exact_directory_metadata(base, "sandbox base")?;
    let canonical_base = std::fs::canonicalize(base).map_err(|error| {
        DaemonError::Process(format!("sandbox base canonicalization failed: {error}"))
    })?;
    if canonical_base != base
        || metadata.uid() != expected_uid
        || metadata.mode() & 0o7777 != QUARANTINE_DIRECTORY_MODE
    {
        return Err(DaemonError::InvalidParam(
            "sandbox base is not an exact canonical private owned directory".into(),
        ));
    }
    reject_extended_attributes(base)?;
    Ok(metadata)
}

fn create_or_prove_private_directory(path: &Path, expected_device: u64) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(QUARANTINE_DIRECTORY_MODE);
            builder.create(path).map_err(|error| {
                DaemonError::Process(format!(
                    "failed to create private settlement quarantine directory: {error}"
                ))
            })?;
        }
        Err(error) => {
            return Err(DaemonError::Process(format!(
                "failed to inspect settlement quarantine parent: {error}"
            )));
        }
    }
    let metadata = exact_directory_metadata(path, "settlement quarantine parent")?;
    // SAFETY: `geteuid` has no preconditions and cannot mutate process state.
    let effective_uid = unsafe { nix::libc::geteuid() };
    if metadata.uid() != effective_uid
        || metadata.dev() != expected_device
        || metadata.mode() & 0o7777 != QUARANTINE_DIRECTORY_MODE
    {
        return Err(DaemonError::InvalidParam(
            "settlement quarantine parent is not private, owned, and same-device".into(),
        ));
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        DaemonError::Process(format!(
            "settlement quarantine parent canonicalization failed: {error}"
        ))
    })?;
    if canonical != path {
        return Err(DaemonError::InvalidParam(
            "settlement quarantine parent traverses a symlink".into(),
        ));
    }
    reject_extended_attributes(path)?;
    Ok(())
}

fn prove_existing_quarantine_shape(original_root: &Path, quarantine_root: &Path) -> Result<()> {
    if !original_root.is_absolute() || !quarantine_root.is_absolute() {
        return Err(DaemonError::InvalidParam(
            "settlement worktree paths must be absolute".into(),
        ));
    }
    let base = original_root.parent().ok_or_else(|| {
        DaemonError::InvalidParam("original settlement root has no parent".into())
    })?;
    let run_parent = quarantine_root.parent().ok_or_else(|| {
        DaemonError::InvalidParam("settlement quarantine has no run parent".into())
    })?;
    let quarantine_parent = run_parent.parent().ok_or_else(|| {
        DaemonError::InvalidParam("settlement quarantine has no private parent".into())
    })?;
    let run_name = run_parent.file_name().and_then(|value| value.to_str());
    let session_name = original_root.file_name().and_then(|value| value.to_str());
    let canonical_run = run_name
        .is_some_and(|name| Uuid::parse_str(name).is_ok_and(|value| value.to_string() == name));
    let canonical_session = session_name
        .is_some_and(|name| Uuid::parse_str(name).is_ok_and(|value| value.to_string() == name));
    if quarantine_parent != base.join(".settlement-quarantine")
        || quarantine_root.file_name() != original_root.file_name()
        || !canonical_run
        || !canonical_session
    {
        return Err(DaemonError::InvalidParam(
            "settlement quarantine path is not deterministic".into(),
        ));
    }
    let base_metadata = prove_private_sandbox_base(base)?;
    create_or_prove_private_directory(quarantine_parent, base_metadata.dev())?;
    create_or_prove_private_directory(run_parent, base_metadata.dev())?;
    let quarantine_metadata = exact_directory_metadata(quarantine_root, "settlement quarantine")?;
    if quarantine_metadata.dev() != base_metadata.dev() {
        return Err(DaemonError::InvalidParam(
            "settlement quarantine is a cross-device or nested mount".into(),
        ));
    }
    let canonical_quarantine = std::fs::canonicalize(quarantine_root).map_err(|error| {
        DaemonError::Process(format!(
            "settlement quarantine canonicalization failed: {error}"
        ))
    })?;
    if canonical_quarantine != quarantine_root {
        return Err(DaemonError::InvalidParam(
            "settlement quarantine traverses a symlink".into(),
        ));
    }
    Ok(())
}

fn original_root_from_settlement_quarantine_path(quarantine_root: &Path) -> Result<PathBuf> {
    if !quarantine_root.is_absolute() {
        return Err(DaemonError::InvalidParam(
            "settlement quarantine path must be absolute".into(),
        ));
    }
    let run_parent = quarantine_root.parent().ok_or_else(|| {
        DaemonError::InvalidParam("settlement quarantine has no run parent".into())
    })?;
    let quarantine_parent = run_parent.parent().ok_or_else(|| {
        DaemonError::InvalidParam("settlement quarantine has no private parent".into())
    })?;
    let base = quarantine_parent.parent().ok_or_else(|| {
        DaemonError::InvalidParam("settlement quarantine has no sandbox base".into())
    })?;
    let run_name = run_parent.file_name().and_then(|value| value.to_str());
    let session_name = quarantine_root.file_name().and_then(|value| value.to_str());
    let canonical_run = run_name
        .is_some_and(|name| Uuid::parse_str(name).is_ok_and(|value| value.to_string() == name));
    let canonical_session = session_name
        .is_some_and(|name| Uuid::parse_str(name).is_ok_and(|value| value.to_string() == name));
    if quarantine_parent.file_name() != Some(OsStr::new(".settlement-quarantine"))
        || !canonical_run
        || !canonical_session
    {
        return Err(DaemonError::InvalidParam(
            "settlement quarantine path is not deterministic".into(),
        ));
    }
    prove_private_sandbox_base(base)?;
    Ok(base.join(session_name.expect("canonical session name was proved")))
}

fn validate_expected_worktree_identity(expected_branch: &str, expected_oid: &str) -> Result<()> {
    if !expected_branch.starts_with("refs/heads/")
        || expected_branch.len() <= "refs/heads/".len()
        || expected_branch.len() > 4096
        || expected_branch
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        || !valid_object_id(expected_oid)
    {
        return Err(DaemonError::InvalidParam(
            "expected worktree branch or object identity is invalid".into(),
        ));
    }
    Ok(())
}

fn require_commit_object_locked(origin: &Path, expected_oid: &str) -> Result<()> {
    if !valid_object_id(expected_oid) {
        return Err(DaemonError::InvalidParam(
            "settlement expected object id is invalid".into(),
        ));
    }
    let object_type = run_git_text(
        origin,
        &["cat-file", "-t", expected_oid],
        "authenticate expected settlement commit object",
    )?;
    if object_type != "commit" {
        return Err(DaemonError::Process(
            "settlement expected object is not an exact commit".into(),
        ));
    }
    Ok(())
}

fn prove_worktree_head(root: &Path, expected_branch: &str, expected_oid: &str) -> Result<()> {
    let head = run_git_text(
        root,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        "resolve quarantined worktree head",
    )?;
    let branch = run_git_text(
        root,
        &["symbolic-ref", "-q", "HEAD"],
        "resolve quarantined worktree branch",
    )?;
    if head != expected_oid || branch != expected_branch {
        return Err(DaemonError::Process(
            "quarantined worktree HEAD identity drifted".into(),
        ));
    }
    Ok(())
}

fn registration_for_path(origin: &Path, root: &Path) -> Result<Option<RegisteredWorktree>> {
    let mut matches = list_worktrees_locked(origin)?
        .into_iter()
        .filter(|entry| same_path(&entry.root, root));
    let found = matches.next();
    if matches.next().is_some() {
        return Err(DaemonError::Process(
            "Git returned duplicate worktree registrations".into(),
        ));
    }
    Ok(found)
}

fn prove_single_registration(
    origin: &Path,
    root: &Path,
    expected_branch: &str,
    expected_oid: &str,
) -> Result<()> {
    let registration = registration_for_path(origin, root)?
        .ok_or_else(|| DaemonError::Process("expected worktree registration is missing".into()))?;
    if registration.branch.as_deref() != Some(expected_branch)
        || registration.head.as_deref() != Some(expected_oid)
    {
        return Err(DaemonError::Process(
            "worktree registration branch or HEAD drifted".into(),
        ));
    }
    Ok(())
}

fn prove_admin_identity(
    origin: &Path,
    root: &Path,
    allowed_admin_gitdir_roots: &[&Path],
) -> Result<WorktreeAdminIdentity> {
    let root_metadata = exact_directory_metadata(root, "linked worktree root")?;
    let git_file = root.join(".git");
    let gitdir_text = read_bounded_regular_text(&git_file, "linked worktree .git pointer")?;
    let gitdir = gitdir_text
        .strip_prefix("gitdir: ")
        .ok_or_else(|| DaemonError::Process("linked worktree .git pointer is malformed".into()))?;
    let admin_directory = PathBuf::from(gitdir);
    if !admin_directory.is_absolute() {
        return Err(DaemonError::Process(
            "linked worktree admin pointer is not absolute".into(),
        ));
    }
    let admin_directory = std::fs::canonicalize(&admin_directory).map_err(|error| {
        DaemonError::Process(format!(
            "linked worktree admin directory is unavailable: {error}"
        ))
    })?;
    let repository_identity = repository_identity_path(origin)?;
    let worktrees_directory = std::fs::canonicalize(repository_identity.join("worktrees"))
        .map_err(|error| {
            DaemonError::Process(format!(
                "repository worktree registry is unavailable: {error}"
            ))
        })?;
    if admin_directory.parent() != Some(worktrees_directory.as_path()) {
        return Err(DaemonError::Process(
            "linked worktree admin directory escaped the repository registry".into(),
        ));
    }
    let admin_id = admin_directory
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty() && value.len() <= 255)
        .ok_or_else(|| DaemonError::Process("linked worktree admin id is invalid".into()))?
        .to_string();
    let commondir = read_bounded_regular_text(
        &admin_directory.join("commondir"),
        "linked worktree common-dir pointer",
    )?;
    let common_target =
        std::fs::canonicalize(admin_directory.join(commondir)).map_err(|error| {
            DaemonError::Process(format!("linked worktree common-dir target failed: {error}"))
        })?;
    if common_target != repository_identity {
        return Err(DaemonError::Process(
            "linked worktree common-dir identity drifted".into(),
        ));
    }
    let admin_gitdir = PathBuf::from(read_bounded_regular_text(
        &admin_directory.join("gitdir"),
        "linked worktree reverse gitdir pointer",
    )?);
    if !admin_gitdir.is_absolute()
        || !allowed_admin_gitdir_roots
            .iter()
            .any(|allowed| admin_gitdir == allowed.join(".git"))
    {
        return Err(DaemonError::Process(
            "linked worktree reverse gitdir pointer is not an authorized path".into(),
        ));
    }
    Ok(WorktreeAdminIdentity {
        repository_identity,
        admin_directory,
        admin_id,
        root_identity: FilesystemIdentity::from_metadata(&root_metadata),
    })
}

fn read_bounded_regular_text(path: &Path, label: &str) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| DaemonError::Process(format!("{label} is unavailable: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.nlink() != 1 {
        return Err(DaemonError::Process(format!(
            "{label} is not an exact single-link regular file"
        )));
    }
    let mut bytes = Vec::with_capacity(WORKTREE_ADMIN_FILE_MAX_BYTES.min(4096));
    std::fs::File::open(path)
        .and_then(|file| {
            file.take(WORKTREE_ADMIN_FILE_MAX_BYTES.saturating_add(1) as u64)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| DaemonError::Process(format!("{label} read failed: {error}")))?;
    if bytes.len() > WORKTREE_ADMIN_FILE_MAX_BYTES || bytes.contains(&0) {
        return Err(DaemonError::Process(format!(
            "{label} exceeded its byte bound or contained NUL"
        )));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| DaemonError::Process(format!("{label} is not UTF-8")))?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    if text.is_empty() || text.contains(['\n', '\r']) {
        return Err(DaemonError::Process(format!("{label} is ambiguous")));
    }
    Ok(text.to_string())
}

fn require_path_absent(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(DaemonError::Process(format!("{label} still exists"))),
        Err(error) => Err(DaemonError::Process(format!(
            "{label} absence proof failed: {error}"
        ))),
    }
}

#[cfg(target_os = "linux")]
fn reject_extended_attributes(path: &Path) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| DaemonError::InvalidParam("settlement quarantine path contains NUL".into()))?;
    // SAFETY: `path` is NUL terminated and a null/zero buffer asks the kernel
    // only for the no-follow xattr-list length; no caller memory is written.
    let length = unsafe { nix::libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
    if length < 0 {
        return Err(DaemonError::Process(format!(
            "settlement quarantine xattr inventory failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    if length != 0 {
        return Err(DaemonError::Process(
            "settlement quarantine contains extended attributes or ACLs".into(),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn reject_extended_attributes(_path: &Path) -> Result<()> {
    Err(DaemonError::Process(
        "settlement quarantine xattr proof is unsupported on this platform".into(),
    ))
}

fn reject_mounts_at_or_below(root: &Path, mountinfo_path: &Path) -> Result<()> {
    let mut mountinfo = Vec::with_capacity(64 * 1024);
    std::fs::File::open(mountinfo_path)
        .and_then(|file| {
            file.take(QUARANTINE_MOUNTINFO_MAX_BYTES.saturating_add(1) as u64)
                .read_to_end(&mut mountinfo)
        })
        .map_err(|error| {
            DaemonError::Process(format!("settlement mount inventory read failed: {error}"))
        })?;
    if mountinfo.len() > QUARANTINE_MOUNTINFO_MAX_BYTES {
        return Err(DaemonError::Process(
            "settlement mount inventory exceeded byte bound".into(),
        ));
    }
    let mut records = 0_usize;
    for line in mountinfo.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        records = records.checked_add(1).ok_or_else(|| {
            DaemonError::Process("settlement mount inventory count overflowed".into())
        })?;
        if records > QUARANTINE_MOUNTINFO_MAX_RECORDS {
            return Err(DaemonError::Process(
                "settlement mount inventory count exceeded bound".into(),
            ));
        }
        let mount_field = line.split(|byte| *byte == b' ').nth(4).ok_or_else(|| {
            DaemonError::Process("settlement mount inventory record is malformed".into())
        })?;
        let mount_point = PathBuf::from(OsString::from_vec(decode_mountinfo_path(mount_field)?));
        if mount_point == root || mount_point.starts_with(root) {
            return Err(DaemonError::Process(
                "settlement quarantine contains a mount point".into(),
            ));
        }
    }
    Ok(())
}

fn decode_mountinfo_path(encoded: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0_usize;
    while index < encoded.len() {
        if encoded[index] != b'\\' {
            decoded.push(encoded[index]);
            index += 1;
            continue;
        }
        let escape = encoded.get(index + 1..index + 4).ok_or_else(|| {
            DaemonError::Process("settlement mount path has a truncated escape".into())
        })?;
        let value = match escape {
            b"040" => b' ',
            b"011" => b'\t',
            b"012" => b'\n',
            b"134" => b'\\',
            _ => {
                return Err(DaemonError::Process(
                    "settlement mount path has an unsupported escape".into(),
                ));
            }
        };
        decoded.push(value);
        index += 4;
    }
    Ok(decoded)
}

/// Read an exact committed handoff without consulting or changing dirty files.
/// All Git commands use the existing contained, deadline/output-bounded runner.
pub(crate) fn read_manager_handoff(
    cwd: &Path,
    handoff: &rsi_common::harness_manager_v2::ManagerCommittedHandoffV2,
) -> Result<String> {
    use crate::store::harness_manager_v2::refused;
    handoff.validate().map_err(refused)?;
    let check_head = || -> Result<()> {
        let head = run_git_text(
            cwd,
            &["rev-parse", "--verify", "HEAD^{commit}"],
            "manager handoff HEAD",
        )?;
        if head != handoff.source_commit {
            return Err(refused("manager_succession_handoff_head_changed"));
        }
        Ok(())
    };
    check_head()?;
    let entry = run_git_bytes(
        cwd,
        &[
            "--literal-pathspecs",
            "ls-tree",
            "-z",
            "--full-tree",
            &handoff.source_commit,
            "--",
            &handoff.relative_path,
        ],
        "manager handoff tree entry",
    )?;
    let entry = std::str::from_utf8(&entry)
        .map_err(|_| refused("manager_succession_handoff_blob_invalid"))?;
    let expected = format!("blob {}\t{}\0", handoff.blob_oid, handoff.relative_path);
    if !["100644 ", "100755 "]
        .iter()
        .any(|mode| entry == format!("{mode}{expected}"))
    {
        return Err(refused("manager_succession_handoff_blob_changed"));
    }
    let size = run_git_text(
        cwd,
        &["cat-file", "-s", &handoff.blob_oid],
        "manager handoff size",
    )?
    .parse::<usize>()
    .map_err(|_| refused("manager_succession_handoff_blob_invalid"))?;
    const MAX_HANDOFF_BYTES: usize = 256 * 1024;
    if size == 0 || size > MAX_HANDOFF_BYTES {
        return Err(refused("manager_succession_handoff_size"));
    }
    let mut command = git_command();
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["cat-file", "blob", &handoff.blob_oid])
        .current_dir(cwd);
    let output = capture_bounded_with_limits(
        &mut command,
        "manager handoff blob",
        None,
        ProcessLimits {
            max_stdout_bytes: MAX_HANDOFF_BYTES,
            ..ProcessLimits::default()
        },
    )?;
    if !output.status.success() || output.stdout.len() != size || output.stdout.contains(&0) {
        return Err(refused("manager_succession_handoff_blob_invalid"));
    }
    let content = String::from_utf8(output.stdout)
        .map_err(|_| refused("manager_succession_handoff_blob_invalid"))?;
    check_head()?;
    Ok(content)
}

/// Observe the exact clean/HEAD tuple used by authenticated child forks.
///
/// This deliberately bypasses repository fsmonitor configuration and routes
/// both commands through the same hardened, byte-bounded Git runner used by
/// settlement. Ignored files remain outside this fork-read cleanliness check:
/// the child starts from the captured commit and does not delete its source.
pub(crate) fn observe_clean_head_bounded(cwd: &Path) -> Result<(bool, String)> {
    let status = run_git_bytes(
        cwd,
        &[
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        "inspect child-fork source cleanliness",
    )?;
    Ok((status.is_empty(), observe_head_bounded(cwd)?))
}

/// Read HEAD through the bounded Git runner without inspecting dirty files.
pub(crate) fn observe_head_bounded(cwd: &Path) -> Result<String> {
    let head = run_git_text(
        cwd,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        "resolve child-fork source revision",
    )?;
    if !valid_object_id(&head) {
        return Err(DaemonError::Process(
            "Git child-fork source revision was not a canonical object id".into(),
        ));
    }
    Ok(head)
}

/// Output cap for one review-seal Git command (the #599 S1 admission bound).
const REVIEW_SEAL_GIT_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
/// Deadline for one review-seal Git command (the #599 S1 admission bound).
const REVIEW_SEAL_GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Run one review-seal Git read with the #599 S1 evidence hardening: canonical
/// objects only (no replace objects or grafts), no system/global config, no
/// hooks or fsmonitor, and no inherited repository-redirecting environment.
fn run_review_seal_git(root: &Path, args: &[&str]) -> Result<std::process::Output> {
    let mut command = git_command();
    command
        .args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(key);
    }
    let limits = ProcessLimits {
        max_stdout_bytes: REVIEW_SEAL_GIT_OUTPUT_BYTES,
        execution_timeout: REVIEW_SEAL_GIT_TIMEOUT,
        ..ProcessLimits::default()
    };
    capture_bounded_with_limits(&mut command, "inspect review seal", None, limits)
        .map_err(|_| DaemonError::InvalidParam("manager_v2_git_failed".into()))
}

/// A successful review-seal Git read; any other exit is a Git failure, never a
/// source change.
fn read_review_seal_git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = run_review_seal_git(root, args)?;
    if !output.status.success() {
        return Err(DaemonError::InvalidParam("manager_v2_git_failed".into()));
    }
    Ok(output.stdout)
}

/// The commit-bound DB review seal (#599). Both initial admission and
/// infrastructure relaunch require the exact sealed commit to be an ancestor
/// of the holder's HEAD. Later commits cannot change the reviewed object.
/// A Git failure that proves nothing about ancestry is `manager_v2_git_failed`.
pub fn review_sealed_source_holds_bounded(root: &Path, commit: &str, head: &str) -> Result<()> {
    let changed = || DaemonError::InvalidParam("manager_review_source_changed".into());
    let resolved = read_review_seal_git(
        root,
        &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
    )
    .ok()
    .and_then(|bytes| String::from_utf8(bytes).ok());
    if commit.len() != 40
        || !valid_object_id(commit)
        || resolved.as_deref().map(str::trim) != Some(commit)
    {
        return Err(changed());
    }
    let ancestor = run_review_seal_git(root, &["merge-base", "--is-ancestor", commit, head])?;
    match ancestor.status.code() {
        Some(0) => {}
        Some(1) => return Err(changed()),
        _ => return Err(DaemonError::InvalidParam("manager_v2_git_failed".into())),
    }
    Ok(())
}

/// Authenticate the custody root before a DB review relaunch inspects its seal.
pub fn review_source_custody_holds_bounded(
    root: &Path,
    branch: &str,
    repository_identity: &str,
) -> Result<()> {
    let unavailable = || DaemonError::InvalidParam("manager_review_infra_relaunch_refused".into());
    if std::fs::canonicalize(root).map_err(|_| unavailable())? != root {
        return Err(unavailable());
    }
    let actual_branch = read_review_seal_git(root, &["symbolic-ref", "--short", "HEAD"])
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(unavailable)?;
    if actual_branch.trim() != branch {
        return Err(unavailable());
    }
    let common = read_review_seal_git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .and_then(|bytes| String::from_utf8(bytes).ok())
    .ok_or_else(unavailable)?;
    let actual = std::fs::canonicalize(common.trim()).map_err(|_| unavailable())?;
    let expected = repository_identity
        .strip_prefix("git-common-dir:")
        .unwrap_or(repository_identity);
    if actual != Path::new(expected) {
        return Err(unavailable());
    }
    Ok(())
}

/// Authenticate that an exact historical review source is still a commit in
/// the already-authorized repository object store. This does not move a ref or
/// inspect the caller's current HEAD.
pub(crate) fn require_commit_object_bounded(cwd: &Path, expected_oid: &str) -> Result<()> {
    require_commit_object_locked(cwd, expected_oid)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DirectRefObservation {
    Missing,
    Commit(String),
    Symbolic,
    NonCommit,
}

pub(crate) fn observe_direct_ref_locked(
    origin: &Path,
    reference: &str,
) -> Result<DirectRefObservation> {
    if probe_symbolic_ref_locked(origin, reference, "pre-probe raw direct ref")? {
        return Ok(DirectRefObservation::Symbolic);
    }
    let output = run_git_raw(
        origin,
        &[
            "for-each-ref",
            "--format=%(objectname)%00%(symref)%00%(objecttype)",
            reference,
        ],
        "observe raw direct ref",
    )?;
    if !output.status.success() {
        return Err(DaemonError::Process(
            "Git raw direct-ref observation failed".into(),
        ));
    }
    let raw = std::str::from_utf8(&output.stdout)
        .map_err(|_| DaemonError::Process("Git raw ref observation was not UTF-8".into()))?
        .trim_end_matches(['\r', '\n']);
    if raw.is_empty() {
        #[cfg(test)]
        run_direct_ref_empty_lookup_test_hook();
        if probe_symbolic_ref_locked(origin, reference, "post-probe empty direct ref")? {
            return Ok(DirectRefObservation::Symbolic);
        }
        return Ok(DirectRefObservation::Missing);
    }
    let fields = raw.split('\0').collect::<Vec<_>>();
    if fields.len() != 3 {
        return Err(DaemonError::Process(
            "Git raw direct-ref observation was ambiguous".into(),
        ));
    }
    if !fields[1].is_empty() {
        return Ok(DirectRefObservation::Symbolic);
    }
    if fields[2] != "commit" {
        return Ok(DirectRefObservation::NonCommit);
    }
    if !valid_object_id(fields[0]) {
        return Err(DaemonError::Process(
            "Git raw direct-ref observation returned an invalid OID".into(),
        ));
    }
    Ok(DirectRefObservation::Commit(fields[0].to_string()))
}

fn probe_symbolic_ref_locked(origin: &Path, reference: &str, label: &str) -> Result<bool> {
    let output = run_git_raw(
        origin,
        &["symbolic-ref", "-q", "--no-recurse", reference],
        label,
    )?;
    match output.status.code() {
        Some(0) => {
            let target = decode_git_text(&output.stdout, label)?;
            if target.is_empty() {
                return Err(DaemonError::Process(format!(
                    "Git {label} returned an empty symbolic-ref target"
                )));
            }
            Ok(true)
        }
        Some(1) if output.stdout.is_empty() => Ok(false),
        _ => Err(DaemonError::Process(format!("Git {label} failed"))),
    }
}

pub(crate) fn source_ref_has_symref_dependents_locked(
    origin: &Path,
    source_ref: &str,
) -> Result<bool> {
    let mut dependent = false;
    stream_git_lines(
        origin,
        &["for-each-ref", "--format=%(symref)"],
        "inspect symbolic ref dependencies",
        |line| {
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line == source_ref.as_bytes() {
                dependent = true;
            }
            Ok(())
        },
    )?;
    Ok(dependent)
}

fn stream_git_lines(
    cwd: &Path,
    args: &[&str],
    label: &str,
    mut consume: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut command = git_command();
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .current_dir(cwd);
    run_bounded_records(command, None, b'\n', true, label, &mut consume)
}

pub(crate) fn resolve_ref_locked(origin: &Path, reference: &str) -> Result<Option<String>> {
    match observe_direct_ref_locked(origin, reference)? {
        DirectRefObservation::Missing => Ok(None),
        DirectRefObservation::Commit(oid) => Ok(Some(oid)),
        DirectRefObservation::Symbolic => Err(DaemonError::InvalidParam(
            "settlement requires a direct local ref, not a symbolic ref".into(),
        )),
        DirectRefObservation::NonCommit => Err(DaemonError::InvalidParam(
            "settlement local ref does not point directly to a commit".into(),
        )),
    }
}

pub(crate) fn is_ancestor_locked(origin: &Path, source: &str, target: &str) -> Result<bool> {
    let output = run_git_raw(
        origin,
        &["merge-base", "--is-ancestor", source, target],
        "prove ancestry",
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(DaemonError::Process("Git ancestry proof failed".into())),
    }
}

pub(crate) fn source_ref_in_use_locked(origin: &Path, source_ref: &str) -> Result<bool> {
    Ok(list_worktrees_locked(origin)?
        .iter()
        .any(|entry| entry.branch.as_deref() == Some(source_ref)))
}

pub(crate) fn source_ref_is_registered_only_at_locked(
    origin: &Path,
    source_ref: &str,
    expected_root: &Path,
    expected_oid: &str,
) -> Result<bool> {
    validate_expected_worktree_identity(source_ref, expected_oid)?;
    let mut registrations = list_worktrees_locked(origin)?
        .into_iter()
        .filter(|entry| entry.branch.as_deref() == Some(source_ref));
    let Some(registration) = registrations.next() else {
        return Ok(false);
    };
    Ok(registrations.next().is_none()
        && registration.root == expected_root
        && registration.head.as_deref() == Some(expected_oid))
}

/// Prove the branch-first settlement sentinel after the source ref has been
/// deleted. Git reports an all-zero worktree-list OID for the dangling
/// symbolic HEAD, so the expected commit is authenticated independently in
/// the object database while the worktree registration and administrative
/// HEAD remain bound to the source ref.
pub(crate) fn source_ref_is_registered_only_at_missing_ref_locked(
    origin: &Path,
    source_ref: &str,
    root: &Path,
    expected_oid: &str,
) -> Result<bool> {
    Ok(
        missing_source_ref_quarantine_identity_locked(origin, source_ref, root, expected_oid)?
            .is_some(),
    )
}

/// Identity-returning form of the missing-source-ref sentinel proof. The
/// caller uses this to compare the same administrative directory, id, and
/// filesystem inode recorded before the branch-first effect.
pub(crate) fn prove_missing_source_ref_quarantine_exact_locked(
    origin: &Path,
    source_ref: &str,
    root: &Path,
    expected_oid: &str,
) -> Result<WorktreeAdminIdentity> {
    missing_source_ref_quarantine_identity_locked(origin, source_ref, root, expected_oid)?
        .ok_or_else(|| {
            DaemonError::Process(
                "missing-source-ref settlement quarantine proof did not match".into(),
            )
        })
}

/// Prove the sole crash intermediate created by branch-first non-force
/// removal: the source ref is still missing, the exact quarantine remains,
/// and its administrative HEAD is detached at the recorded commit. The
/// session layer additionally compares the returned identity and current tree
/// proof with its retained durable marker before authorizing recovery.
pub(crate) fn prove_detached_missing_source_ref_quarantine_exact_locked(
    origin: &Path,
    source_ref: &str,
    root: &Path,
    expected_oid: &str,
) -> Result<WorktreeAdminIdentity> {
    validate_expected_worktree_identity(source_ref, expected_oid)?;
    require_commit_object_locked(origin, expected_oid)?;
    if observe_direct_ref_locked(origin, source_ref)? != DirectRefObservation::Missing {
        return Err(DaemonError::Process(
            "detached quarantine recovery requires a missing source ref".into(),
        ));
    }
    prove_detached_quarantine_identity_locked(origin, source_ref, root, expected_oid)
}

fn prove_detached_exact_source_ref_quarantine_locked(
    origin: &Path,
    source_ref: &str,
    root: &Path,
    expected_oid: &str,
) -> Result<WorktreeAdminIdentity> {
    validate_expected_worktree_identity(source_ref, expected_oid)?;
    require_commit_object_locked(origin, expected_oid)?;
    if observe_direct_ref_locked(origin, source_ref)?
        != DirectRefObservation::Commit(expected_oid.to_string())
    {
        return Err(DaemonError::Process(
            "detached quarantine recovery requires the exact source ref".into(),
        ));
    }
    prove_detached_quarantine_identity_locked(origin, source_ref, root, expected_oid)
}

fn prove_detached_quarantine_identity_locked(
    origin: &Path,
    source_ref: &str,
    root: &Path,
    expected_oid: &str,
) -> Result<WorktreeAdminIdentity> {
    let original_root = original_root_from_settlement_quarantine_path(root)?;
    prove_existing_quarantine_shape(&original_root, root)?;
    let registrations = list_worktrees_locked(origin)?;
    if registrations
        .iter()
        .any(|entry| same_path(&entry.root, &original_root))
    {
        return Err(DaemonError::Process(
            "detached quarantine recovery found an original-path registration".into(),
        ));
    }
    let mut root_registrations = registrations
        .iter()
        .filter(|entry| same_path(&entry.root, root));
    let root_registration = root_registrations.next().ok_or_else(|| {
        DaemonError::Process("detached quarantine registration is missing".into())
    })?;
    if root_registrations.next().is_some() {
        return Err(DaemonError::Process(
            "Git returned duplicate detached quarantine registrations".into(),
        ));
    }
    if root_registration.branch.is_some()
        || root_registration.head.as_deref() != Some(expected_oid)
        || registrations
            .iter()
            .any(|entry| entry.branch.as_deref() == Some(source_ref))
    {
        return Err(DaemonError::Process(
            "detached quarantine registration identity drifted".into(),
        ));
    }
    let admin = prove_admin_identity(origin, root, &[root])?;
    let admin_head = read_bounded_regular_text(
        &admin.admin_directory.join("HEAD"),
        "detached linked worktree administrative HEAD",
    )?;
    if admin_head != expected_oid {
        return Err(DaemonError::Process(
            "detached quarantine administrative HEAD drifted".into(),
        ));
    }
    let worktree_head = run_git_text(
        root,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        "resolve detached quarantine head",
    )?;
    if worktree_head != expected_oid {
        return Err(DaemonError::Process(
            "detached quarantine worktree HEAD drifted".into(),
        ));
    }
    Ok(admin)
}

fn missing_source_ref_quarantine_identity_locked(
    origin: &Path,
    source_ref: &str,
    root: &Path,
    expected_oid: &str,
) -> Result<Option<WorktreeAdminIdentity>> {
    validate_expected_worktree_identity(source_ref, expected_oid)?;
    require_commit_object_locked(origin, expected_oid)?;
    if observe_direct_ref_locked(origin, source_ref)? != DirectRefObservation::Missing {
        return Ok(None);
    }

    let original_root = original_root_from_settlement_quarantine_path(root)?;
    prove_existing_quarantine_shape(&original_root, root)?;
    let registrations = list_worktrees_locked(origin)?;
    if registrations
        .iter()
        .any(|entry| same_path(&entry.root, &original_root))
    {
        return Ok(None);
    }
    let mut root_registrations = registrations
        .iter()
        .filter(|entry| same_path(&entry.root, root));
    let Some(root_registration) = root_registrations.next() else {
        return Ok(None);
    };
    if root_registrations.next().is_some() {
        return Err(DaemonError::Process(
            "Git returned duplicate settlement quarantine registrations".into(),
        ));
    }
    let zero_oid = "0".repeat(expected_oid.len());
    if root_registration.branch.as_deref() != Some(source_ref)
        || root_registration.head.as_deref() != Some(zero_oid.as_str())
    {
        return Ok(None);
    }
    if registrations
        .iter()
        .filter(|entry| entry.branch.as_deref() == Some(source_ref))
        .any(|entry| !same_path(&entry.root, root))
    {
        return Ok(None);
    }

    let admin = prove_admin_identity(origin, root, &[root])?;
    let admin_head = read_bounded_regular_text(
        &admin.admin_directory.join("HEAD"),
        "linked worktree administrative HEAD",
    )?;
    if admin_head != format!("ref: {source_ref}") {
        return Ok(None);
    }
    Ok(Some(admin))
}

pub(crate) fn source_ref_has_zero_registrations_locked(
    origin: &Path,
    source_ref: &str,
) -> Result<bool> {
    validate_transaction_ref(source_ref, "source worktree")?;
    Ok(!list_worktrees_locked(origin)?
        .iter()
        .any(|entry| entry.branch.as_deref() == Some(source_ref)))
}

pub(crate) fn other_ref_state_digest_locked(origin: &Path, excluded_ref: &str) -> Result<String> {
    let mut digest = Sha256::new();
    stream_git_lines(
        origin,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(symref)",
        ],
        "snapshot unrelated refs",
        |line| {
            if line.starts_with(excluded_ref.as_bytes())
                && line.get(excluded_ref.len()) == Some(&b' ')
            {
                return Ok(());
            }
            digest.update(line);
            Ok(())
        },
    )?;
    Ok(format!("sha256:{:x}", digest.finalize()))
}

pub(crate) fn remove_worktree_non_force_locked(
    origin: &Path,
    original_root: &Path,
    root: &Path,
    source_ref: &str,
    expected_oid: &str,
) -> Result<()> {
    prove_existing_quarantine_shape(original_root, root)?;
    if registration_for_path(origin, original_root)?.is_some()
        || !source_ref_is_registered_only_at_locked(origin, source_ref, root, expected_oid)?
    {
        return Err(DaemonError::Process(
            "worktree removal precondition has ambiguous registrations".into(),
        ));
    }
    let root_text = root
        .to_str()
        .ok_or_else(|| DaemonError::InvalidParam("settlement root is not UTF-8".into()))?;
    let output = run_git_raw(
        origin,
        &["worktree", "remove", root_text],
        "remove worktree",
    )?;
    if !output.status.success() {
        return Err(DaemonError::Process(
            "non-force Git worktree removal refused".into(),
        ));
    }
    #[cfg(test)]
    WORKTREE_REMOVE_POST_EFFECT_TEST_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(DaemonError::Process(
                "worktree removal left filesystem residue".into(),
            ));
        }
        Err(error) => {
            return Err(DaemonError::Process(format!(
                "worktree removal residue check failed: {error}"
            )));
        }
    }
    if registration_for_path(origin, original_root)?.is_some()
        || registration_for_path(origin, root)?.is_some()
        || !source_ref_has_zero_registrations_locked(origin, source_ref)?
    {
        return Err(DaemonError::Process(
            "worktree removal left original, quarantine, or source-ref registration residue".into(),
        ));
    }
    Ok(())
}

/// Remove a clean quarantine after the source ref was deleted, without ever
/// exposing an unregistered live source branch. A verify-only `update-ref`
/// transaction holds the missing source ref lock while its daemon-authored
/// prepared hook temporarily detaches the dangling worktree HEAD and invokes
/// `git worktree remove` without force. Ordinary concurrent branch recreation
/// therefore either wins before the lock (and the transaction refuses before
/// removal) or loses to the lock; it cannot register a replacement worktree in
/// the removal window.
pub(crate) fn remove_worktree_after_source_ref_delete_non_force_locked(
    origin: &Path,
    original_root: &Path,
    root: &Path,
    source_ref: &str,
    expected_oid: &str,
) -> Result<()> {
    validate_expected_worktree_identity(source_ref, expected_oid)?;
    require_commit_object_locked(origin, expected_oid)?;
    let derived_original = original_root_from_settlement_quarantine_path(root)?;
    if derived_original != original_root {
        return Err(DaemonError::InvalidParam(
            "settlement original root does not match the deterministic quarantine".into(),
        ));
    }

    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            prove_branch_first_worktree_removed_locked(
                origin,
                original_root,
                root,
                source_ref,
                expected_oid,
            )?;
            let zero_oid = "0".repeat(expected_oid.len());
            require_reacquirable_ref_lock_locked(origin, source_ref, &zero_oid)?;
            return prove_branch_first_worktree_removed_locked(
                origin,
                original_root,
                root,
                source_ref,
                expected_oid,
            );
        }
        Ok(_) => {}
        Err(error) => {
            return Err(DaemonError::Process(format!(
                "settlement quarantine inspection failed: {error}"
            )));
        }
    }

    prove_existing_quarantine_shape(original_root, root)?;
    let admin =
        prove_missing_source_ref_quarantine_exact_locked(origin, source_ref, root, expected_oid)?;
    let root_text = root
        .to_str()
        .ok_or_else(|| DaemonError::InvalidParam("settlement root is not UTF-8".into()))?;
    let origin_text = origin
        .to_str()
        .ok_or_else(|| DaemonError::InvalidParam("settlement repository is not UTF-8".into()))?;
    let admin_text = admin.admin_directory.to_str().ok_or_else(|| {
        DaemonError::InvalidParam("settlement worktree admin directory is not UTF-8".into())
    })?;
    let zero_oid = "0".repeat(expected_oid.len());
    let transaction = format!("start\nverify {source_ref} {zero_oid}\nprepare\ncommit\n");
    let hook_directory = settlement_missing_ref_removal_hook_directory()?;
    let hooks_path = hook_directory.path().to_str().ok_or_else(|| {
        DaemonError::Process("settlement reference-hook path was not UTF-8".into())
    })?;
    let hooks_config = format!("core.hooksPath={hooks_path}");
    let mut command = git_command();
    command
        .arg("-c")
        .arg(&hooks_config)
        .args(["update-ref", "--no-deref", "--stdin"])
        .env("RSI_SETTLEMENT_ORIGIN", origin_text)
        .env("RSI_SETTLEMENT_QUARANTINE_ROOT", root_text)
        .env("RSI_SETTLEMENT_ADMIN_DIR", admin_text)
        .env("RSI_SETTLEMENT_SOURCE_REF", source_ref)
        .env("RSI_SETTLEMENT_SOURCE_OID", expected_oid)
        .current_dir(origin);
    let execution = capture_bounded_with_input(
        command,
        "remove dangling settlement worktree under source-ref lock",
        transaction.as_bytes(),
    );
    #[cfg(test)]
    let execution = WORKTREE_REMOVE_LOST_ACK_TEST.with(|slot| {
        if slot.replace(false) {
            Err(DaemonError::Process(
                "injected worktree removal acknowledgement loss".into(),
            ))
        } else {
            execution
        }
    });
    #[cfg(test)]
    WORKTREE_REMOVE_POST_EFFECT_TEST_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
    let postcondition = prove_branch_first_worktree_removed_locked(
        origin,
        original_root,
        root,
        source_ref,
        expected_oid,
    );
    if postcondition.is_ok() {
        // The exact postcondition is authoritative after a lost command ack.
        let zero_oid = "0".repeat(expected_oid.len());
        require_acknowledged_or_reacquirable_ref_lock_locked(
            origin, source_ref, &zero_oid, &execution,
        )?;
        return prove_branch_first_worktree_removed_locked(
            origin,
            original_root,
            root,
            source_ref,
            expected_oid,
        );
    }
    match execution {
        Err(error) => Err(error),
        Ok(output) if !output.status.success() => Err(DaemonError::Process(
            "non-force dangling Git worktree removal refused".into(),
        )),
        Ok(_) => postcondition,
    }
}

fn prove_branch_first_worktree_removed_locked(
    origin: &Path,
    original_root: &Path,
    root: &Path,
    source_ref: &str,
    expected_oid: &str,
) -> Result<()> {
    require_commit_object_locked(origin, expected_oid)?;
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(DaemonError::Process(
                "worktree removal left quarantine filesystem residue".into(),
            ));
        }
        Err(error) => {
            return Err(DaemonError::Process(format!(
                "worktree removal quarantine residue check failed: {error}"
            )));
        }
    }
    if observe_direct_ref_locked(origin, source_ref)? != DirectRefObservation::Missing
        || registration_for_path(origin, original_root)?.is_some()
        || registration_for_path(origin, root)?.is_some()
        || !source_ref_has_zero_registrations_locked(origin, source_ref)?
    {
        return Err(DaemonError::Process(
            "worktree removal left source ref or worktree registration residue".into(),
        ));
    }
    Ok(())
}

fn require_acknowledged_or_reacquirable_ref_lock_locked(
    origin: &Path,
    reference: &str,
    expected_old_oid: &str,
    execution: &Result<std::process::Output>,
) -> Result<()> {
    if matches!(execution, Ok(output) if output.status.success()) {
        return Ok(());
    }
    require_reacquirable_ref_lock_locked(origin, reference, expected_old_oid)
}

fn require_reacquirable_ref_lock_locked(
    origin: &Path,
    reference: &str,
    expected_old_oid: &str,
) -> Result<()> {
    validate_transaction_ref(reference, "source")?;
    if !valid_object_id(expected_old_oid) {
        return Err(DaemonError::InvalidParam(
            "settlement ref-lock proof requires a canonical object id".into(),
        ));
    }
    let transaction = format!("start\nverify {reference} {expected_old_oid}\nprepare\ncommit\n");
    let mut command = git_command();
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "update-ref",
            "--no-deref",
            "--stdin",
        ])
        .current_dir(origin);
    let output = capture_bounded_with_input(
        command,
        "prove exact settlement source ref lock",
        transaction.as_bytes(),
    )?;
    if !output.status.success() {
        return Err(DaemonError::Process(
            "source ref lock was not reacquirable during exact settlement proof".into(),
        ));
    }
    if expected_old_oid.bytes().all(|byte| byte == b'0') {
        if observe_direct_ref_locked(origin, reference)? != DirectRefObservation::Missing {
            return Err(DaemonError::Process(
                "source ref changed after its missing-lock replay proof".into(),
            ));
        }
        Ok(())
    } else {
        require_direct_ref_at(origin, reference, expected_old_oid, "source lock replay")
    }
}

pub(crate) fn delete_ref_compare_locked(
    origin: &Path,
    source_ref: &str,
    expected_oid: &str,
) -> Result<()> {
    require_direct_ref_at(origin, source_ref, expected_oid, "source")?;
    let output = run_git_raw(
        origin,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "update-ref",
            "--no-deref",
            "-d",
            source_ref,
            expected_oid,
        ],
        "delete exact local ref",
    )?;
    if !output.status.success() {
        return Err(DaemonError::Process(
            "exact local ref compare-delete refused".into(),
        ));
    }
    if resolve_ref_locked(origin, source_ref)?.is_some() {
        return Err(DaemonError::Process(
            "local ref remained after compare-delete".into(),
        ));
    }
    Ok(())
}

/// Atomically prove the target ref and delete the exact source ref.
///
/// `update-ref --stdin` gives the target verification and source deletion one
/// reference transaction. Git's OID verification can follow a symbolic ref
/// even with `--no-deref`, so a daemon-authored `reference-transaction` hook
/// re-observes both refs as direct exact commits in the `prepared` phase while
/// Git holds their locks. Repository hooks are replaced only for this mutation;
/// a configured hook cannot rewrite, reject, or delay the authorized operation.
pub(crate) fn delete_source_ref_atomically_locked(
    origin: &Path,
    target_ref: &str,
    expected_target_oid: &str,
    source_ref: &str,
    expected_source_oid: &str,
) -> Result<()> {
    if target_ref == source_ref {
        return Err(DaemonError::InvalidParam(
            "settlement source and target refs must be distinct".into(),
        ));
    }
    validate_transaction_ref(target_ref, "target")?;
    validate_transaction_ref(source_ref, "source")?;
    if !valid_object_id(expected_target_oid) || !valid_object_id(expected_source_oid) {
        return Err(DaemonError::InvalidParam(
            "settlement ref transaction requires canonical object ids".into(),
        ));
    }
    require_direct_ref_at(origin, target_ref, expected_target_oid, "target")?;
    require_direct_ref_at(origin, source_ref, expected_source_oid, "source")?;
    #[cfg(test)]
    run_atomic_ref_pre_spawn_test_hook();

    let transaction = format!(
        "start\nverify {target_ref} {expected_target_oid}\ndelete {source_ref} {expected_source_oid}\nprepare\ncommit\n"
    );
    let hook_directory = settlement_reference_hook_directory()?;
    let hooks_path = hook_directory.path().to_str().ok_or_else(|| {
        DaemonError::Process("settlement reference-hook path was not UTF-8".into())
    })?;
    let hooks_config = format!("core.hooksPath={hooks_path}");
    let mut command = git_command();
    command
        .arg("-c")
        .arg(&hooks_config)
        .args(["update-ref", "--no-deref", "--stdin"])
        .env("RSI_SETTLEMENT_TARGET_REF", target_ref)
        .env("RSI_SETTLEMENT_TARGET_OID", expected_target_oid)
        .env("RSI_SETTLEMENT_SOURCE_REF", source_ref)
        .env("RSI_SETTLEMENT_SOURCE_OID", expected_source_oid)
        .current_dir(origin);
    let output = capture_bounded_with_input(
        command,
        "verify target and delete exact source ref",
        transaction.as_bytes(),
    )?;
    if !output.status.success() {
        return Err(DaemonError::Process(
            "atomic target verification/source deletion refused".into(),
        ));
    }

    require_direct_ref_at(origin, target_ref, expected_target_oid, "target")?;
    match observe_direct_ref_locked(origin, source_ref)? {
        DirectRefObservation::Missing => Ok(()),
        _ => Err(DaemonError::Process(
            "source ref remained after atomic target verification/deletion".into(),
        )),
    }
}

/// Restore only the recorded source branch when branch-first removal needs to
/// compensate. This deliberately does not depend on the target ref: target
/// drift is evaluated by the caller after the quarantine sentinel is made
/// non-dangling again. Exact replay is accepted, but any symbolic, non-commit,
/// or different-OID source state is retained without overwrite. When a
/// quarantine worktree still exists, the caller must first prove its exact
/// symbolic missing-source sentinel state; detached recovery must use
/// `restore_source_ref_and_reattach_detached_quarantine_atomically_locked`.
pub(crate) fn restore_source_ref_if_missing_atomically_locked(
    origin: &Path,
    source_ref: &str,
    expected_source_oid: &str,
) -> Result<()> {
    validate_transaction_ref(source_ref, "source")?;
    require_commit_object_locked(origin, expected_source_oid)?;
    match observe_direct_ref_locked(origin, source_ref)? {
        DirectRefObservation::Commit(actual) if actual == expected_source_oid => {
            require_reacquirable_ref_lock_locked(origin, source_ref, expected_source_oid)?;
            return require_direct_ref_at(
                origin,
                source_ref,
                expected_source_oid,
                "restored source replay",
            );
        }
        DirectRefObservation::Missing => {}
        DirectRefObservation::Commit(_) => {
            return Err(DaemonError::Process(
                "settlement source ref changed before exact restoration".into(),
            ));
        }
        DirectRefObservation::Symbolic => {
            return Err(DaemonError::InvalidParam(
                "settlement source ref became symbolic before restoration".into(),
            ));
        }
        DirectRefObservation::NonCommit => {
            return Err(DaemonError::InvalidParam(
                "settlement source ref became a non-commit before restoration".into(),
            ));
        }
    }

    #[cfg(test)]
    run_atomic_ref_pre_spawn_test_hook();
    let transaction =
        format!("start\ncreate {source_ref} {expected_source_oid}\nprepare\ncommit\n");
    let mut command = git_command();
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "update-ref",
            "--no-deref",
            "--stdin",
        ])
        .current_dir(origin);
    let execution = capture_bounded_with_input(
        command,
        "atomically restore exact missing source ref",
        transaction.as_bytes(),
    );
    match observe_direct_ref_locked(origin, source_ref)? {
        DirectRefObservation::Commit(actual) if actual == expected_source_oid => {
            require_acknowledged_or_reacquirable_ref_lock_locked(
                origin,
                source_ref,
                expected_source_oid,
                &execution,
            )?;
            require_direct_ref_at(origin, source_ref, expected_source_oid, "restored source")
        }
        _ => match execution {
            Err(error) => Err(error),
            Ok(output) if !output.status.success() => Err(DaemonError::Process(
                "atomic missing source-ref restoration refused".into(),
            )),
            Ok(_) => Err(DaemonError::Process(
                "source ref did not match after atomic restoration".into(),
            )),
        },
    }
}

/// Recover the exact detached+missing intermediate without exposing a live
/// source branch while the quarantine is detached. The source compare-create
/// transaction locks the missing ref first; its prepared hook reattaches the
/// quarantine while the ref is still missing, and only the subsequent commit
/// makes the source branch live. If an ordinary exact source recreation wins
/// that transaction, replay uses a verify-only transaction to reattach while
/// holding the exact source lock. A crash before either hook retains the exact
/// detached state; a crash after a hook leaves a replayable symbolic state.
pub(crate) fn restore_source_ref_and_reattach_detached_quarantine_atomically_locked(
    origin: &Path,
    original_root: &Path,
    root: &Path,
    source_ref: &str,
    expected_oid: &str,
) -> Result<WorktreeAdminIdentity> {
    validate_expected_worktree_identity(source_ref, expected_oid)?;
    require_commit_object_locked(origin, expected_oid)?;
    let derived_original = original_root_from_settlement_quarantine_path(root)?;
    if derived_original != original_root {
        return Err(DaemonError::InvalidParam(
            "settlement original root does not match the detached quarantine".into(),
        ));
    }

    // A same-OID ordinary source recreation can land between the initial ref
    // observation and either missing-state proof. Redispatch that one stable
    // transition in this invocation; a second inconsistent classification is
    // retained rather than spun on indefinitely.
    for dispatch in 0..MAX_RECOVERY_STATE_DISPATCHES {
        match observe_direct_ref_locked(origin, source_ref)? {
            DirectRefObservation::Commit(actual) if actual == expected_oid => {
                return restore_exact_source_ref_quarantine_locked(
                    origin,
                    original_root,
                    root,
                    source_ref,
                    expected_oid,
                );
            }
            DirectRefObservation::Missing => {}
            DirectRefObservation::Commit(_) => {
                return Err(DaemonError::Process(
                    "settlement source ref changed before detached recovery".into(),
                ));
            }
            DirectRefObservation::Symbolic | DirectRefObservation::NonCommit => {
                return Err(DaemonError::InvalidParam(
                    "settlement source ref became non-direct before detached recovery".into(),
                ));
            }
        }

        let missing_identity = match missing_source_ref_quarantine_identity_locked(
            origin,
            source_ref,
            root,
            expected_oid,
        ) {
            Ok(identity) => identity,
            Err(error) => {
                if dispatch + 1 < MAX_RECOVERY_STATE_DISPATCHES {
                    continue;
                }
                return Err(error);
            }
        };
        if let Some(before) = missing_identity {
            restore_source_ref_if_missing_atomically_locked(origin, source_ref, expected_oid)?;
            let after = prove_moved_worktree_exact_locked(
                origin,
                original_root,
                root,
                source_ref,
                expected_oid,
            )?;
            require_same_worktree_admin_identity(&before, &after)?;
            return Ok(after);
        }

        let before = match prove_detached_missing_source_ref_quarantine_exact_locked(
            origin,
            source_ref,
            root,
            expected_oid,
        ) {
            Ok(identity) => identity,
            Err(error) => {
                if dispatch + 1 < MAX_RECOVERY_STATE_DISPATCHES {
                    continue;
                }
                return Err(error);
            }
        };
        #[cfg(test)]
        run_atomic_ref_pre_spawn_test_hook();
        let transaction = format!("start\ncreate {source_ref} {expected_oid}\nprepare\ncommit\n");
        let create_result = reattach_detached_quarantine_under_source_ref_lock_locked(
            origin,
            original_root,
            root,
            source_ref,
            expected_oid,
            &before,
            &transaction,
        );
        if create_result.is_ok() {
            return create_result;
        }

        // An ordinary exact source creation can win after the detached+missing
        // proof but before our compare-create acquires the ref lock. Recover
        // that exact collision without accepting any identity drift.
        let raced_before = match prove_detached_exact_source_ref_quarantine_locked(
            origin,
            source_ref,
            root,
            expected_oid,
        ) {
            Ok(identity) => identity,
            Err(_) => return create_result,
        };
        require_same_worktree_admin_identity(&before, &raced_before)?;
        let transaction = format!("start\nverify {source_ref} {expected_oid}\nprepare\ncommit\n");
        return reattach_detached_quarantine_under_source_ref_lock_locked(
            origin,
            original_root,
            root,
            source_ref,
            expected_oid,
            &raced_before,
            &transaction,
        );
    }

    Err(DaemonError::Process(
        "settlement quarantine recovery state was ambiguous".into(),
    ))
}

fn restore_exact_source_ref_quarantine_locked(
    origin: &Path,
    original_root: &Path,
    root: &Path,
    source_ref: &str,
    expected_oid: &str,
) -> Result<WorktreeAdminIdentity> {
    if let Ok(before) =
        prove_moved_worktree_exact_locked(origin, original_root, root, source_ref, expected_oid)
    {
        require_reacquirable_ref_lock_locked(origin, source_ref, expected_oid)?;
        let after = prove_moved_worktree_exact_locked(
            origin,
            original_root,
            root,
            source_ref,
            expected_oid,
        )?;
        require_same_worktree_admin_identity(&before, &after)?;
        return Ok(after);
    }

    let before =
        prove_detached_exact_source_ref_quarantine_locked(origin, source_ref, root, expected_oid)?;
    #[cfg(test)]
    run_atomic_ref_pre_spawn_test_hook();
    let transaction = format!("start\nverify {source_ref} {expected_oid}\nprepare\ncommit\n");
    reattach_detached_quarantine_under_source_ref_lock_locked(
        origin,
        original_root,
        root,
        source_ref,
        expected_oid,
        &before,
        &transaction,
    )
}

#[allow(clippy::too_many_arguments)]
fn reattach_detached_quarantine_under_source_ref_lock_locked(
    origin: &Path,
    original_root: &Path,
    root: &Path,
    source_ref: &str,
    expected_oid: &str,
    before: &WorktreeAdminIdentity,
    transaction: &str,
) -> Result<WorktreeAdminIdentity> {
    let hook_directory = settlement_reattach_hook_directory()?;
    let hooks_path = hook_directory.path().to_str().ok_or_else(|| {
        DaemonError::Process("settlement reattach-hook path was not UTF-8".into())
    })?;
    let hooks_config = format!("core.hooksPath={hooks_path}");
    let admin_text = before.admin_directory.to_str().ok_or_else(|| {
        DaemonError::InvalidParam("settlement worktree admin directory is not UTF-8".into())
    })?;
    let mut command = git_command();
    command
        .arg("-c")
        .arg(&hooks_config)
        .args(["update-ref", "--no-deref", "--stdin"])
        .env("RSI_SETTLEMENT_ADMIN_DIR", admin_text)
        .env("RSI_SETTLEMENT_SOURCE_REF", source_ref)
        .env("RSI_SETTLEMENT_SOURCE_OID", expected_oid)
        .current_dir(origin);
    let execution = capture_bounded_with_input(
        command,
        "atomically restore source ref and reattach quarantine",
        transaction.as_bytes(),
    );
    #[cfg(test)]
    let execution = RESTORE_REATTACH_LOST_ACK_TEST.with(|slot| {
        if slot.get() && matches!(&execution, Ok(output) if output.status.success()) {
            slot.set(false);
            Err(DaemonError::Process(
                "injected source restoration acknowledgement loss".into(),
            ))
        } else {
            execution
        }
    });
    let postcondition =
        prove_moved_worktree_exact_locked(origin, original_root, root, source_ref, expected_oid)
            .and_then(|after| {
                require_same_worktree_admin_identity(before, &after)?;
                Ok(after)
            });
    match postcondition {
        // The exact final state is authoritative after a lost transaction ack.
        Ok(_) => {
            require_acknowledged_or_reacquirable_ref_lock_locked(
                origin,
                source_ref,
                expected_oid,
                &execution,
            )?;
            let final_after = prove_moved_worktree_exact_locked(
                origin,
                original_root,
                root,
                source_ref,
                expected_oid,
            )?;
            require_same_worktree_admin_identity(before, &final_after)?;
            Ok(final_after)
        }
        Err(post_error) => match execution {
            Err(error) => Err(error),
            Ok(output) if !output.status.success() => Err(DaemonError::Process(
                "atomic source restoration/quarantine reattach refused".into(),
            )),
            Ok(_) => Err(post_error),
        },
    }
}

fn require_same_worktree_admin_identity(
    before: &WorktreeAdminIdentity,
    after: &WorktreeAdminIdentity,
) -> Result<()> {
    if before != after {
        return Err(DaemonError::Process(
            "settlement worktree identity changed during recovery".into(),
        ));
    }
    Ok(())
}

fn settlement_reference_hook_directory() -> Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("rsi-settlement-ref-hooks-")
        .tempdir()
        .map_err(|error| {
            DaemonError::Process(format!(
                "failed to create settlement reference-hook directory: {error}"
            ))
        })?;
    let hook = directory.path().join("reference-transaction");
    std::fs::write(
        &hook,
        r#"#!/bin/sh
set -eu
while IFS= read -r ignored; do :; done
if [ "$#" -ne 1 ] || [ "$1" != prepared ]; then
    exit 0
fi
check_direct_exact_ref() {
    ref=$1
    expected=$2
    if git symbolic-ref -q "$ref" >/dev/null 2>&1; then
        return 1
    fi
    actual=$(git rev-parse --verify "${ref}^{commit}") || return 1
    [ "$actual" = "$expected" ]
}
check_direct_exact_ref "$RSI_SETTLEMENT_TARGET_REF" "$RSI_SETTLEMENT_TARGET_OID"
check_direct_exact_ref "$RSI_SETTLEMENT_SOURCE_REF" "$RSI_SETTLEMENT_SOURCE_OID"
"#,
    )
    .map_err(|error| {
        DaemonError::Process(format!(
            "failed to write settlement reference hook: {error}"
        ))
    })?;
    let mut permissions = std::fs::metadata(&hook)
        .map_err(|error| {
            DaemonError::Process(format!(
                "failed to inspect settlement reference hook: {error}"
            ))
        })?
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&hook, permissions).map_err(|error| {
        DaemonError::Process(format!(
            "failed to make settlement reference hook executable: {error}"
        ))
    })?;
    Ok(directory)
}

fn settlement_missing_ref_removal_hook_directory() -> Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("rsi-settlement-remove-hooks-")
        .tempdir()
        .map_err(|error| {
            DaemonError::Process(format!(
                "failed to create settlement removal-hook directory: {error}"
            ))
        })?;
    let hook = directory.path().join("reference-transaction");
    std::fs::write(
        &hook,
        r#"#!/bin/sh
set -eu
while IFS= read -r ignored; do :; done
if [ "$#" -ne 1 ] || [ "$1" != prepared ]; then
    exit 0
fi
restore_symbolic_head() {
    if [ -d "$RSI_SETTLEMENT_ADMIN_DIR" ]; then
        git -c core.hooksPath=/dev/null --git-dir="$RSI_SETTLEMENT_ADMIN_DIR" \
            symbolic-ref HEAD "$RSI_SETTLEMENT_SOURCE_REF" >/dev/null 2>&1 || :
    fi
}
trap restore_symbolic_head EXIT HUP INT TERM
git -c core.hooksPath=/dev/null --git-dir="$RSI_SETTLEMENT_ADMIN_DIR" \
    update-ref --no-deref HEAD "$RSI_SETTLEMENT_SOURCE_OID"
git -c core.hooksPath=/dev/null -C "$RSI_SETTLEMENT_ORIGIN" \
    worktree remove "$RSI_SETTLEMENT_QUARANTINE_ROOT"
trap - EXIT HUP INT TERM
"#,
    )
    .map_err(|error| {
        DaemonError::Process(format!("failed to write settlement removal hook: {error}"))
    })?;
    let mut permissions = std::fs::metadata(&hook)
        .map_err(|error| {
            DaemonError::Process(format!(
                "failed to inspect settlement removal hook: {error}"
            ))
        })?
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&hook, permissions).map_err(|error| {
        DaemonError::Process(format!(
            "failed to make settlement removal hook executable: {error}"
        ))
    })?;
    Ok(directory)
}

fn settlement_reattach_hook_directory() -> Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("rsi-settlement-reattach-hooks-")
        .tempdir()
        .map_err(|error| {
            DaemonError::Process(format!(
                "failed to create settlement reattach-hook directory: {error}"
            ))
        })?;
    let hook = directory.path().join("reference-transaction");
    std::fs::write(
        &hook,
        r#"#!/bin/sh
set -eu
while IFS= read -r ignored; do :; done
if [ "$#" -ne 1 ] || [ "$1" != prepared ]; then
    exit 0
fi
if git -c core.hooksPath=/dev/null --git-dir="$RSI_SETTLEMENT_ADMIN_DIR" \
    symbolic-ref -q HEAD >/dev/null 2>&1; then
    exit 1
fi
actual=$(git -c core.hooksPath=/dev/null --git-dir="$RSI_SETTLEMENT_ADMIN_DIR" \
    rev-parse --verify 'HEAD^{commit}')
[ "$actual" = "$RSI_SETTLEMENT_SOURCE_OID" ]
git -c core.hooksPath=/dev/null --git-dir="$RSI_SETTLEMENT_ADMIN_DIR" \
    symbolic-ref HEAD "$RSI_SETTLEMENT_SOURCE_REF"
"#,
    )
    .map_err(|error| {
        DaemonError::Process(format!("failed to write settlement reattach hook: {error}"))
    })?;
    let mut permissions = std::fs::metadata(&hook)
        .map_err(|error| {
            DaemonError::Process(format!(
                "failed to inspect settlement reattach hook: {error}"
            ))
        })?
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&hook, permissions).map_err(|error| {
        DaemonError::Process(format!(
            "failed to make settlement reattach hook executable: {error}"
        ))
    })?;
    Ok(directory)
}

#[cfg(test)]
pub(crate) fn set_atomic_ref_pre_spawn_test_hook(hook: impl FnOnce() + 'static) {
    ATOMIC_REF_PRE_SPAWN_TEST_HOOK.with(|slot| {
        assert!(
            slot.borrow_mut().replace(Box::new(hook)).is_none(),
            "atomic-ref test hook is already armed on this thread"
        );
    });
}

#[cfg(test)]
fn inject_worktree_remove_lost_ack() {
    WORKTREE_REMOVE_LOST_ACK_TEST.with(|slot| {
        assert!(
            !slot.replace(true),
            "worktree removal lost ack already armed"
        );
    });
}

#[cfg(test)]
fn inject_restore_reattach_lost_ack() {
    RESTORE_REATTACH_LOST_ACK_TEST.with(|slot| {
        assert!(
            !slot.replace(true),
            "restore/reattach lost ack already armed"
        );
    });
}

#[cfg(test)]
fn run_atomic_ref_pre_spawn_test_hook() {
    ATOMIC_REF_PRE_SPAWN_TEST_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
fn set_direct_ref_empty_lookup_test_hook(hook: impl FnOnce() + 'static) {
    DIRECT_REF_EMPTY_LOOKUP_TEST_HOOK.with(|slot| {
        assert!(
            slot.borrow_mut().replace(Box::new(hook)).is_none(),
            "direct-ref empty-lookup test hook is already armed on this thread"
        );
    });
}

#[cfg(test)]
fn run_direct_ref_empty_lookup_test_hook() {
    DIRECT_REF_EMPTY_LOOKUP_TEST_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

fn require_direct_ref_at(
    origin: &Path,
    reference: &str,
    expected_oid: &str,
    role: &str,
) -> Result<()> {
    match observe_direct_ref_locked(origin, reference)? {
        DirectRefObservation::Commit(actual) if actual == expected_oid => Ok(()),
        DirectRefObservation::Commit(_) => Err(DaemonError::Process(format!(
            "settlement {role} ref changed from its expected object id"
        ))),
        DirectRefObservation::Symbolic => Err(DaemonError::InvalidParam(format!(
            "settlement {role} ref is symbolic"
        ))),
        DirectRefObservation::Missing => Err(DaemonError::Process(format!(
            "settlement {role} ref is missing"
        ))),
        DirectRefObservation::NonCommit => Err(DaemonError::InvalidParam(format!(
            "settlement {role} ref is not a commit"
        ))),
    }
}

fn validate_transaction_ref(reference: &str, role: &str) -> Result<()> {
    if !reference.starts_with("refs/heads/")
        || reference.len() <= "refs/heads/".len()
        || reference.len() > 4096
        || reference
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(DaemonError::InvalidParam(format!(
            "settlement {role} ref is not a bounded local branch ref"
        )));
    }
    Ok(())
}

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn repository_identity_path(origin: &Path) -> Result<PathBuf> {
    let common = run_git_text(
        origin,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        "resolve repository identity",
    )?;
    std::fs::canonicalize(common).map_err(|_| {
        DaemonError::InvalidParam("repository identity is unavailable or noncanonical".into())
    })
}

fn list_worktrees_locked(origin: &Path) -> Result<Vec<RegisteredWorktree>> {
    list_worktrees_with_limit_locked(origin, MAX_GIT_WORKTREE_ENTRIES)
}

/// Return only live, exact canonical Git registrations from this repository.
/// Call from a blocking lane: each path needs filesystem and Git verification.
pub(crate) fn discover_registered_worktree_roots(
    origin: &Path,
    max_entries: usize,
) -> Result<Vec<PathBuf>> {
    let expected_common = repository_identity_path(origin)?;
    let entries = list_worktrees_with_limit_locked(origin, max_entries)?;
    let mut seen = HashSet::new();
    let mut roots = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.prunable || !entry.root.is_absolute() {
            continue;
        }
        let Ok(canonical) = std::fs::canonicalize(&entry.root) else {
            continue;
        };
        // A listed symlink (including a symlink in an ancestor) is not an
        // exact worktree root. Containment must never rely on its spelling.
        if canonical != entry.root || !canonical.is_dir() {
            continue;
        }
        if !seen.insert(canonical.clone()) {
            return Err(DaemonError::InvalidParam(
                "Git worktree list contains duplicate canonical roots".into(),
            ));
        }
        if repository_identity_path(&canonical).ok().as_ref() != Some(&expected_common) {
            continue;
        }
        roots.push(canonical);
    }
    Ok(roots)
}

fn list_worktrees_with_limit_locked(
    origin: &Path,
    max_entries: usize,
) -> Result<Vec<RegisteredWorktree>> {
    let mut command = git_command();
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["worktree", "list", "--porcelain", "-z"])
        .current_dir(origin);
    let mut parser = WorktreeListParser::new(max_entries.min(MAX_GIT_WORKTREE_ENTRIES));
    run_bounded_records_with_limits(
        &mut command,
        None,
        b'\0',
        true,
        "list worktrees",
        ProcessLimits::worktree_list(),
        &mut |record| parser.consume(record),
    )?;
    parser.finish()
}

struct WorktreeListParser {
    entries: Vec<RegisteredWorktree>,
    current: Option<RegisteredWorktree>,
    max_entries: usize,
}

impl WorktreeListParser {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: Vec::new(),
            current: None,
            max_entries,
        }
    }

    fn consume(&mut self, record: &[u8]) -> Result<()> {
        let record = record.strip_suffix(b"\0").ok_or_else(|| {
            DaemonError::Process("Git worktree list returned an unterminated record".into())
        })?;
        if record.is_empty() {
            let entry = self.current.take().ok_or_else(|| {
                DaemonError::Process("Git worktree list returned an empty entry".into())
            })?;
            if self.entries.len() == self.max_entries {
                return Err(DaemonError::Process(
                    "Git worktree list entry count exceeded bound".into(),
                ));
            }
            self.entries.push(entry);
            return Ok(());
        }
        if let Some(path) = record.strip_prefix(b"worktree ") {
            if path.is_empty() || self.current.is_some() {
                return Err(DaemonError::Process(
                    "Git worktree list returned an ambiguous worktree entry".into(),
                ));
            }
            self.current = Some(RegisteredWorktree {
                root: PathBuf::from(OsString::from_vec(path.to_vec())),
                head: None,
                branch: None,
                prunable: false,
            });
            return Ok(());
        }
        let entry = self.current.as_mut().ok_or_else(|| {
            DaemonError::Process("Git worktree list field preceded its worktree path".into())
        })?;
        if let Some(head) = record.strip_prefix(b"HEAD ") {
            if entry.head.is_some() {
                return Err(DaemonError::Process(
                    "Git worktree list returned duplicate HEAD fields".into(),
                ));
            }
            entry.head = Some(decode_worktree_field(head, "HEAD")?);
        } else if let Some(branch) = record.strip_prefix(b"branch ") {
            if entry.branch.is_some() {
                return Err(DaemonError::Process(
                    "Git worktree list returned duplicate branch fields".into(),
                ));
            }
            entry.branch = Some(decode_worktree_field(branch, "branch")?);
        } else if record == b"prunable" || record.starts_with(b"prunable ") {
            entry.prunable = true;
        }
        Ok(())
    }

    fn finish(self) -> Result<Vec<RegisteredWorktree>> {
        if self.current.is_some() {
            return Err(DaemonError::Process(
                "Git worktree list ended before its entry separator".into(),
            ));
        }
        Ok(self.entries)
    }
}

fn decode_worktree_field(bytes: &[u8], field: &str) -> Result<String> {
    if bytes.is_empty() {
        return Err(DaemonError::Process(format!(
            "Git worktree list returned an empty {field} field"
        )));
    }
    std::str::from_utf8(bytes)
        .map(ToOwned::to_owned)
        .map_err(|_| DaemonError::Process(format!("Git worktree list {field} was not UTF-8")))
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn run_git_text(cwd: &Path, args: &[&str], label: &str) -> Result<String> {
    let output = run_git_raw(cwd, args, label)?;
    if !output.status.success() {
        return Err(DaemonError::Process(format!("Git {label} failed")));
    }
    decode_git_text(&output.stdout, label)
}

fn run_git_bytes(cwd: &Path, args: &[&str], label: &str) -> Result<Vec<u8>> {
    let output = run_git_raw(cwd, args, label)?;
    if !output.status.success() {
        return Err(DaemonError::Process(format!("Git {label} failed")));
    }
    Ok(output.stdout)
}

fn run_git_raw(cwd: &Path, args: &[&str], label: &str) -> Result<std::process::Output> {
    let mut command = git_command();
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .current_dir(cwd);
    capture_bounded(command, label)
}

fn inspect_index_visibility_locked(cwd: &Path) -> Result<(bool, String)> {
    let mut command = git_command();
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["-c", "core.fsmonitor=false", "ls-files", "-v", "-z"])
        .current_dir(cwd);
    let mut digest = Sha256::new();
    digest.update(b"rsi-index-visibility-v1\0");
    let mut safe = true;
    run_bounded_records(
        command,
        None,
        b'\0',
        true,
        "inspect worktree index visibility",
        &mut |record| {
            if record.len() < 3 || record.last() != Some(&b'\0') {
                return Err(DaemonError::Process(
                    "Git index visibility returned a malformed record".into(),
                ));
            }
            let tag = record[0];
            safe &= !tag.is_ascii_lowercase() && tag != b'S';
            digest.update(record);
            Ok(())
        },
    )?;
    let digest = format!("sha256:{:x}", digest.finalize());
    Ok((safe, digest))
}

fn capture_bounded(mut command: Command, label: &str) -> Result<std::process::Output> {
    capture_bounded_with_limits(&mut command, label, None, ProcessLimits::default())
}

fn capture_bounded_with_input(
    mut command: Command,
    label: &str,
    input: &[u8],
) -> Result<std::process::Output> {
    capture_bounded_with_limits(&mut command, label, Some(input), ProcessLimits::default())
}

fn capture_bounded_with_limits(
    command: &mut Command,
    label: &str,
    input: Option<&[u8]>,
    limits: ProcessLimits,
) -> Result<std::process::Output> {
    let mut stdout = CaptureSink::default();
    let result = run_bounded_process(command, input, label, limits, &mut stdout)?;
    Ok(std::process::Output {
        status: result.status,
        stdout: stdout.bytes,
        stderr: result.stderr,
    })
}

fn run_bounded_records(
    mut command: Command,
    input: Option<&[u8]>,
    delimiter: u8,
    require_terminator: bool,
    label: &str,
    consume: &mut impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    run_bounded_records_with_limits(
        &mut command,
        input,
        delimiter,
        require_terminator,
        label,
        ProcessLimits::record_stream(),
        consume,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_bounded_records_with_limits(
    command: &mut Command,
    input: Option<&[u8]>,
    delimiter: u8,
    require_terminator: bool,
    label: &str,
    limits: ProcessLimits,
    consume: &mut impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut stdout = RecordSink {
        delimiter,
        require_terminator,
        max_record_bytes: limits.max_record_bytes,
        max_records: limits.max_records,
        records: 0,
        current: Vec::new(),
        consume,
    };
    let result = run_bounded_process(command, input, label, limits, &mut stdout)?;
    if !result.status.success() {
        return Err(DaemonError::Process(format!("Git {label} failed")));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ProcessLimits {
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
    max_record_bytes: usize,
    max_records: usize,
    max_stdin_bytes: usize,
    execution_timeout: Duration,
    post_exit_drain_timeout: Duration,
}

impl Default for ProcessLimits {
    fn default() -> Self {
        Self {
            max_stdout_bytes: MAX_GIT_OUTPUT_BYTES,
            max_stderr_bytes: MAX_GIT_OUTPUT_BYTES,
            max_record_bytes: MAX_GIT_RECORD_BYTES,
            max_records: MAX_GIT_STREAM_RECORDS,
            max_stdin_bytes: MAX_GIT_STDIN_BYTES,
            execution_timeout: GIT_EXECUTION_TIMEOUT,
            post_exit_drain_timeout: GIT_POST_EXIT_DRAIN_TIMEOUT,
        }
    }
}

impl ProcessLimits {
    fn record_stream() -> Self {
        Self {
            max_stdout_bytes: MAX_GIT_STREAM_BYTES,
            ..Self::default()
        }
    }

    fn worktree_list() -> Self {
        Self {
            max_stdout_bytes: MAX_GIT_WORKTREE_OUTPUT_BYTES,
            max_record_bytes: MAX_GIT_WORKTREE_RECORD_BYTES,
            max_records: MAX_GIT_WORKTREE_RECORDS,
            ..Self::default()
        }
    }
}

struct BoundedProcessResult {
    status: std::process::ExitStatus,
    stderr: Vec<u8>,
}

trait OutputSink {
    fn push(&mut self, bytes: &[u8], label: &str) -> Result<()>;
    fn finish(&mut self, label: &str) -> Result<()>;
}

#[derive(Default)]
struct CaptureSink {
    bytes: Vec<u8>,
}

impl OutputSink for CaptureSink {
    fn push(&mut self, bytes: &[u8], _label: &str) -> Result<()> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn finish(&mut self, _label: &str) -> Result<()> {
        Ok(())
    }
}

struct RecordSink<'a, F> {
    delimiter: u8,
    require_terminator: bool,
    max_record_bytes: usize,
    max_records: usize,
    records: usize,
    current: Vec<u8>,
    consume: &'a mut F,
}

impl<F> OutputSink for RecordSink<'_, F>
where
    F: FnMut(&[u8]) -> Result<()>,
{
    fn push(&mut self, bytes: &[u8], label: &str) -> Result<()> {
        for byte in bytes {
            if self.current.len() == self.max_record_bytes {
                return Err(DaemonError::Process(format!(
                    "Git {label} stdout record exceeded bound"
                )));
            }
            self.current.push(*byte);
            if *byte == self.delimiter {
                if self.records == self.max_records {
                    return Err(DaemonError::Process(format!(
                        "Git {label} stdout record count exceeded bound"
                    )));
                }
                self.records += 1;
                (self.consume)(&self.current)?;
                self.current.clear();
            }
        }
        Ok(())
    }

    fn finish(&mut self, label: &str) -> Result<()> {
        if self.current.is_empty() {
            return Ok(());
        }
        if self.require_terminator {
            return Err(DaemonError::Process(format!(
                "Git {label} stdout ended with an unterminated record"
            )));
        }
        if self.records == self.max_records {
            return Err(DaemonError::Process(format!(
                "Git {label} stdout record count exceeded bound"
            )));
        }
        self.records += 1;
        (self.consume)(&self.current)?;
        self.current.clear();
        Ok(())
    }
}

fn run_bounded_process(
    command: &mut Command,
    input: Option<&[u8]>,
    label: &str,
    limits: ProcessLimits,
    stdout_sink: &mut dyn OutputSink,
) -> Result<BoundedProcessResult> {
    if input.is_some_and(|bytes| bytes.len() > limits.max_stdin_bytes) {
        return Err(DaemonError::Process(format!(
            "Git {label} stdin exceeded bound"
        )));
    }
    configure_std_process_group(command, ProcessContainment::GroupNoEscape)?;
    if input.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| DaemonError::Process(format!("failed to invoke Git for {label}")))?;
    let pgid = nix::unistd::Pid::from_raw(child.id() as i32);
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        terminate_process_group(pgid);
        drop(child.stdin.take());
        let _ = wait_child(&mut child);
        return Err(DaemonError::Process(format!(
            "Git {label} output pipes were unavailable"
        )));
    };
    let mut stdin = child.stdin.take();
    if let Err(error) = set_nonblocking(&stdout, label, "stdout")
        .and_then(|()| set_nonblocking(&stderr, label, "stderr"))
        .and_then(|()| {
            stdin
                .as_ref()
                .map_or(Ok(()), |pipe| set_nonblocking(pipe, label, "stdin"))
        })
    {
        terminate_process_group(pgid);
        drop(stdin);
        drop(stdout);
        drop(stderr);
        let _ = wait_child(&mut child);
        return Err(error);
    }
    let mut failure = None;
    let input = input.unwrap_or_default();
    let mut input_written = 0_usize;
    if input.is_empty() {
        stdin = None;
    }

    let mut stderr_sink = CaptureSink::default();
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut stdout_observed = 0_usize;
    let mut stderr_observed = 0_usize;
    let mut status = None;
    let started = Instant::now();
    let execution_deadline = started + limits.execution_timeout;
    let mut shutdown_deadline = None;
    let mut termination_started = false;

    if failure.is_some() {
        terminate_process_group(pgid);
        termination_started = true;
        shutdown_deadline = Some(Instant::now() + limits.post_exit_drain_timeout);
    }
    loop {
        if let Some(pipe) = stdin.as_mut() {
            match pipe.write(&input[input_written..]) {
                Ok(0) => {
                    if failure.is_none() {
                        failure = Some(DaemonError::Process(format!(
                            "Git {label} stdin closed before accepting bounded input"
                        )));
                    }
                    stdin = None;
                }
                Ok(written) => {
                    input_written += written;
                    if input_written == input.len() {
                        stdin = None;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(DaemonError::Process(format!(
                            "Git {label} stdin write failed: {error}"
                        )));
                    }
                    stdin = None;
                }
            }
        }
        if !stdout_eof {
            match drain_pipe(
                &mut stdout,
                stdout_sink,
                &mut stdout_observed,
                limits.max_stdout_bytes,
                label,
                "stdout",
            ) {
                Ok(eof) => stdout_eof = eof,
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
        }
        if !stderr_eof {
            match drain_pipe(
                &mut stderr,
                &mut stderr_sink,
                &mut stderr_observed,
                limits.max_stderr_bytes,
                label,
                "stderr",
            ) {
                Ok(eof) => stderr_eof = eof,
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
        }

        if failure.is_some() && !termination_started {
            terminate_process_group(pgid);
            termination_started = true;
            shutdown_deadline = Some(Instant::now() + limits.post_exit_drain_timeout);
        }

        if status.is_none() {
            #[cfg(any(
                target_os = "android",
                target_os = "freebsd",
                target_os = "haiku",
                all(target_os = "linux", not(target_env = "uclibc"))
            ))]
            {
                use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
                match waitid(
                    Id::Pid(pgid),
                    WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG,
                ) {
                    Ok(WaitStatus::StillAlive) => {}
                    Ok(_) => {
                        terminate_process_group(pgid);
                        termination_started = true;
                        match wait_child(&mut child) {
                            Ok(observed) => status = Some(observed),
                            Err(error) if failure.is_none() => {
                                failure = Some(DaemonError::Process(format!(
                                    "Git {label} reap failed: {error}"
                                )));
                            }
                            Err(_) => {}
                        }
                        shutdown_deadline
                            .get_or_insert(Instant::now() + limits.post_exit_drain_timeout);
                    }
                    Err(error) => {
                        if failure.is_none() {
                            failure = Some(DaemonError::Process(format!(
                                "Git {label} wait failed: {error}"
                            )));
                        }
                    }
                }
            }
            #[cfg(not(any(
                target_os = "android",
                target_os = "freebsd",
                target_os = "haiku",
                all(target_os = "linux", not(target_env = "uclibc"))
            )))]
            match child.try_wait() {
                Ok(None) => {}
                Ok(Some(observed)) => {
                    status = Some(observed);
                    terminate_process_group(pgid);
                    termination_started = true;
                    shutdown_deadline
                        .get_or_insert(Instant::now() + limits.post_exit_drain_timeout);
                }
                Err(error) if failure.is_none() => {
                    failure = Some(DaemonError::Process(format!(
                        "Git {label} wait failed: {error}"
                    )));
                }
                Err(_) => {}
            }
        }

        let now = Instant::now();
        if status.is_none() && failure.is_none() && now >= execution_deadline {
            failure = Some(DaemonError::Process(format!(
                "Git {label} execution timed out"
            )));
            terminate_process_group(pgid);
            termination_started = true;
            shutdown_deadline = Some(now + limits.post_exit_drain_timeout);
        }
        if status.is_some() && stdout_eof && stderr_eof {
            break;
        }
        if shutdown_deadline.is_some_and(|deadline| now >= deadline) {
            break;
        }
        std::thread::sleep(PROCESS_POLL_INTERVAL);
    }
    drop(stdin);
    drop(stdout);
    drop(stderr);
    if status.is_none() {
        terminate_process_group(pgid);
        status = Some(wait_child(&mut child).map_err(|error| {
            DaemonError::Process(format!("Git {label} final reap failed: {error}"))
        })?);
    }
    if let Some(error) = failure {
        return Err(error);
    }
    let status =
        status.ok_or_else(|| DaemonError::Process(format!("Git {label} could not be reaped")))?;
    if !stdout_eof || !stderr_eof {
        return Err(DaemonError::Process(format!(
            "Git {label} output drain timed out"
        )));
    }
    stdout_sink.finish(label)?;
    stderr_sink.finish(label)?;
    Ok(BoundedProcessResult {
        status,
        stderr: stderr_sink.bytes,
    })
}

fn set_nonblocking(pipe: &impl std::os::fd::AsRawFd, label: &str, stream: &str) -> Result<()> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    let flags = fcntl(pipe.as_raw_fd(), FcntlArg::F_GETFL).map_err(|error| {
        DaemonError::Process(format!("Git {label} {stream} flags failed: {error}"))
    })?;
    fcntl(
        pipe.as_raw_fd(),
        FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
    )
    .map_err(|error| {
        DaemonError::Process(format!("Git {label} {stream} nonblocking failed: {error}"))
    })?;
    Ok(())
}

fn drain_pipe(
    reader: &mut impl Read,
    sink: &mut dyn OutputSink,
    observed: &mut usize,
    max_output_bytes: usize,
    label: &str,
    stream: &str,
) -> Result<bool> {
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                *observed = observed.saturating_add(read);
                if *observed > max_output_bytes {
                    return Err(DaemonError::Process(format!(
                        "Git {label} {stream} output exceeded bound"
                    )));
                }
                sink.push(&buffer[..read], label)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => {
                return Err(DaemonError::Process(format!(
                    "Git {label} {stream} read failed: {error}"
                )));
            }
        }
    }
}

fn wait_child(child: &mut std::process::Child) -> std::io::Result<std::process::ExitStatus> {
    loop {
        match child.wait() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null");
    command
}

fn decode_git_text(bytes: &[u8], label: &str) -> Result<String> {
    std::str::from_utf8(bytes)
        .map(str::trim)
        .map(ToOwned::to_owned)
        .map_err(|_| DaemonError::Process(format!("Git {label} returned non-UTF-8 output")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RollingBaseFallbackReason {
    RemoteUnavailable,
    FetchTimedOut,
    FetchFailed,
    RemoteNotFastForward,
    LocalAhead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RollingBasePolicy {
    RemoteTip,
    FastForwardOnly,
}

#[derive(Debug)]
pub(crate) struct RollingBaseSelection {
    pub commit: String,
    pub fallback_reason: Option<RollingBaseFallbackReason>,
    private_ref: Option<String>,
    origin: PathBuf,
}

impl RollingBaseSelection {
    /// Keep the fetched commit reachable until allocation finishes. Cleanup is
    /// attempted on both allocation outcomes; a failed deletion refuses launch.
    pub(crate) fn allocate_with_cleanup<T>(
        self,
        allocate: impl FnOnce(&str) -> Result<T>,
    ) -> Result<T> {
        if let Some(reason) = self.fallback_reason {
            tracing::warn!(
                ?reason,
                "Rolling sandbox base fetch fell back to local source"
            );
        }
        let result = allocate(&self.commit);
        if let Some(private_ref) = self.private_ref.as_deref() {
            let mut delete = git_command();
            delete
                .args(["update-ref", "-d", private_ref])
                .current_dir(&self.origin);
            let limits = ProcessLimits {
                execution_timeout: ROLLING_FETCH_TIMEOUT,
                ..ProcessLimits::default()
            };
            let deletion = capture_bounded_with_limits(
                &mut delete,
                "delete private rolling sandbox base",
                None,
                limits,
            )?;
            if !deletion.status.success() {
                return Err(DaemonError::Process(
                    "could not delete private rolling sandbox base ref".into(),
                ));
            }
        }
        result
    }
}

/// Observe origin's rolling tip through a private ref, without moving either
/// the local branch or the ordinary remote-tracking ref.
pub(crate) fn fresh_rolling_base(
    origin: &Path,
    allocation_id: Uuid,
    local_source: String,
    policy: RollingBasePolicy,
) -> Result<RollingBaseSelection> {
    let branch = run_git_raw(
        origin,
        &["symbolic-ref", "--quiet", "HEAD"],
        "identify rolling checkout",
    )?;
    if !branch.status.success()
        || decode_git_text(&branch.stdout, "identify rolling checkout")? != "refs/heads/rolling"
    {
        return Ok(RollingBaseSelection {
            commit: local_source,
            fallback_reason: None,
            private_ref: None,
            origin: origin.to_path_buf(),
        });
    }

    let private_ref = format!("refs/rsi/sandbox-base/{allocation_id}");
    let refspec = format!("+refs/heads/rolling:{private_ref}");
    let mut fetch = git_command();
    fetch
        .args([
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            "--refmap=",
            "origin",
            refspec.as_str(),
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(origin);
    let limits = ProcessLimits {
        execution_timeout: ROLLING_FETCH_TIMEOUT,
        ..ProcessLimits::default()
    };
    let fetched =
        capture_bounded_with_limits(&mut fetch, "fetch rolling sandbox base", None, limits);
    let mut reason = match &fetched {
        Ok(output) if output.status.success() => None,
        Err(error) if error.to_string().contains("execution timed out") => {
            Some(RollingBaseFallbackReason::FetchTimedOut)
        }
        Ok(_) => Some(RollingBaseFallbackReason::RemoteUnavailable),
        _ => Some(RollingBaseFallbackReason::FetchFailed),
    };
    // A failed fetch can have written its ref before reporting failure, so
    // every rolling observation carries the cleanup obligation.
    let observed = if reason.is_none() {
        match run_git_text(
            origin,
            &[
                "rev-parse",
                "--verify",
                &format!("{private_ref}^{{commit}}"),
            ],
            "resolve fetched rolling sandbox base",
        ) {
            Ok(commit) => Some(commit),
            Err(_) => {
                reason = Some(RollingBaseFallbackReason::FetchFailed);
                None
            }
        }
    } else {
        None
    };
    let reason = match (&observed, policy) {
        (Some(remote), RollingBasePolicy::FastForwardOnly) => {
            let ancestry = run_git_raw(
                origin,
                &["merge-base", "--is-ancestor", &local_source, remote],
                "compare successor rolling source",
            );
            match ancestry.ok().and_then(|output| output.status.code()) {
                Some(0) => None,
                Some(1) => Some(RollingBaseFallbackReason::RemoteNotFastForward),
                _ => Some(RollingBaseFallbackReason::FetchFailed),
            }
        }
        (Some(remote), RollingBasePolicy::RemoteTip) if remote != &local_source => {
            // A remote that is strictly behind local rolling must not discard
            // committed work in an interactive or unarchive allocation.
            let ancestry = run_git_raw(
                origin,
                &["merge-base", "--is-ancestor", remote, &local_source],
                "compare local rolling source",
            );
            match ancestry.ok().and_then(|output| output.status.code()) {
                Some(0) => Some(RollingBaseFallbackReason::LocalAhead),
                Some(1) => None,
                _ => Some(RollingBaseFallbackReason::FetchFailed),
            }
        }
        _ => reason,
    };
    let (commit, fallback_reason) = match (observed, reason) {
        (Some(remote), None) => (remote, None),
        (_, Some(reason)) => (local_source, Some(reason)),
        (None, None) => (local_source, Some(RollingBaseFallbackReason::FetchFailed)),
    };
    Ok(RollingBaseSelection {
        commit,
        fallback_reason,
        private_ref: Some(private_ref),
        origin: origin.to_path_buf(),
    })
}

/// Adopt only the exact deterministic allocation left by an interrupted
/// reserved Closure launch. Any identity drift fails closed.
pub(crate) fn adopt_reserved(
    base_dir: &Path,
    session_id: Uuid,
    origin: &Path,
    source_commit: &str,
    requested_branch: Option<&str>,
) -> Result<Option<SandboxAllocation>> {
    with_repository_mutation(origin, || {
        adopt_reserved_locked(
            base_dir,
            session_id,
            origin,
            source_commit,
            requested_branch,
        )
    })
}

fn adopt_reserved_locked(
    base_dir: &Path,
    session_id: Uuid,
    origin: &Path,
    source_commit: &str,
    requested_branch: Option<&str>,
) -> Result<Option<SandboxAllocation>> {
    let root =
        canonicalize_non_strict(&base_dir.join(session_id.to_string())).map_err(|error| {
            DaemonError::InvalidParam(format!("reserved Closure sandbox root is invalid: {error}"))
        })?;
    if !root.exists() {
        return Ok(None);
    }
    let branch = requested_branch
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            DaemonError::InvalidParam(
                "reserved Closure sandbox recovery requires its explicit branch".into(),
            )
        })?;
    let command = |cwd: &Path, args: &[&str]| -> Result<String> {
        let mut command = git_command();
        command.args(args).current_dir(cwd);
        let output = capture_bounded(command, "reserved Closure sandbox observation")?;
        if !output.status.success() {
            return Err(DaemonError::Process(format!(
                "reserved Closure sandbox observation failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let expected_common = command(
        origin,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let actual_common = command(
        &root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let expected_common = std::fs::canonicalize(expected_common)?;
    let actual_common = std::fs::canonicalize(actual_common)?;
    let head = command(&root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let branch_ref = command(&root, &["symbolic-ref", "-q", "HEAD"])?;
    let clean = command(&root, &["status", "--porcelain=v1"])?;
    let registered = command(origin, &["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .any(|path| path == root);
    if expected_common != actual_common
        || head != source_commit
        || branch_ref != format!("refs/heads/{branch}")
        || !clean.is_empty()
        || !registered
    {
        return Err(DaemonError::InvalidParam(
            "reserved Closure sandbox exists but its repository/branch/head/cleanliness identity drifted"
                .into(),
        ));
    }
    Ok(Some(SandboxAllocation {
        kind: SandboxKind::GitWorktree,
        root,
        branch: Some(branch.to_string()),
        origin: origin.to_path_buf(),
    }))
}

/// Allocate a fresh git-worktree sandbox under `base_dir` for `session_id`.
///
/// `source_commit` is explicit so child and fresh allocation cannot silently
/// inherit whatever commit happens to be checked out in the canonical tree.
/// `requested_branch`, when present, is the caller's authenticated branch
/// policy; otherwise the session-derived branch remains the default.
pub fn allocate(
    base_dir: &Path,
    session_id: Uuid,
    origin: &Path,
    source_commit: &str,
    requested_branch: Option<&str>,
) -> Result<SandboxAllocation> {
    validate_allocation_origin(origin)?;
    with_repository_mutation(origin, || {
        allocate_locked(
            base_dir,
            session_id,
            session_id,
            origin,
            source_commit,
            requested_branch,
        )
    })
}

/// Allocate a new worktree generation for a session that previously owned a
/// sandbox. The path identity must differ from the former root so custody
/// history remains immutable. Its default branch uses the same fresh
/// allocation identity so a retained `rsi/<session_id>` source ref cannot
/// collide with ordinary unarchive.
pub(crate) fn allocate_replacement(
    base_dir: &Path,
    _session_id: Uuid,
    origin: &Path,
    source_commit: &str,
    requested_branch: Option<&str>,
) -> Result<SandboxAllocation> {
    validate_allocation_origin(origin)?;
    let root_id = Uuid::new_v4();
    with_repository_mutation(origin, || {
        allocate_locked(
            base_dir,
            root_id,
            root_id,
            origin,
            source_commit,
            requested_branch,
        )
    })
}

fn allocate_locked(
    base_dir: &Path,
    root_id: Uuid,
    session_id: Uuid,
    origin: &Path,
    source_commit: &str,
    requested_branch: Option<&str>,
) -> Result<SandboxAllocation> {
    // The public allocation entry points validate the origin before the
    // repository-identity lock is resolved, preserving the documented typed
    // error for non-Git directories.

    // 1. Compute & pre-validate the proposed root. The dirname is an
    //    allocation UUID so `list_on_disk()` can round-trip by parsing it.
    let dir_name = root_id.to_string();
    let proposed_root = base_dir.join(&dir_name);
    let canonical_root = canonicalize_non_strict(&proposed_root).map_err(|e| {
        DaemonError::InvalidParam(format!("sandbox root path rejected by pre-flight: {}", e))
    })?;

    // Safety: never clobber an existing directory. If a prior allocation
    // left something on disk, let the orphan sweep handle it.
    if canonical_root.exists() {
        return Err(DaemonError::InvalidParam(format!(
            "sandbox root already exists at '{}' (possible orphan)",
            canonical_root.display()
        )));
    }

    // 2. Resolve the requested source to an immutable commit before creating
    // the new branch. `worktree add` otherwise defaults to the current HEAD.
    let mut source_command = git_command();
    source_command
        .args([
            "rev-parse",
            "--verify",
            &format!("{source_commit}^{{commit}}"),
        ])
        .current_dir(origin);
    let source = capture_bounded(source_command, "resolve sandbox source")?;
    if !source.status.success() {
        return Err(DaemonError::InvalidParam(format!(
            "sandbox source commit is unavailable: {source_commit}"
        )));
    }
    let source_commit = String::from_utf8_lossy(&source.stdout).trim().to_string();

    // 3. Honor explicit caller branch policy when supplied.
    let branch = requested_branch
        .filter(|branch| !branch.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| default_branch(&session_id));

    // 4. `git worktree add -b <branch> --quiet <root> <commit>` is atomic wrt
    //    directory + branch creation and is explicit about the source revision.
    let root_str = canonical_root
        .to_str()
        .ok_or_else(|| DaemonError::InvalidParam("sandbox root is not valid UTF-8".to_string()))?;
    let mut add_command = git_command();
    add_command
        .args([
            "worktree",
            "add",
            "-b",
            &branch,
            "--quiet",
            root_str,
            &source_commit,
        ])
        .current_dir(origin);
    let add = capture_bounded(add_command, "git worktree add")?;

    if !add.status.success() {
        let stderr = String::from_utf8_lossy(&add.stderr);
        return Err(DaemonError::Process(format!(
            "`git worktree add` failed (exit {}): {}",
            add.status.code().unwrap_or(-1),
            stderr.trim()
        )));
    }

    tracing::info!(
        session_id = %session_id,
        root = %canonical_root.display(),
        branch = %branch,
        "Allocated git-worktree sandbox"
    );

    Ok(SandboxAllocation {
        kind: SandboxKind::GitWorktree,
        root: canonical_root,
        branch: Some(branch),
        origin: origin.to_path_buf(),
    })
}

fn validate_allocation_origin(origin: &Path) -> Result<()> {
    let mut rev_parse_command = git_command();
    rev_parse_command
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(origin);
    let rev_parse = capture_bounded(rev_parse_command, "sandbox pre-flight")?;
    if !rev_parse.status.success() || String::from_utf8_lossy(&rev_parse.stdout).trim() != "true" {
        return Err(DaemonError::InvalidParam(format!(
            "sandbox requires a git repository at '{}'",
            origin.display()
        )));
    }
    Ok(())
}

/// Test-only force/prune/recursive destructor. Never production authority.
#[cfg(test)]
pub fn destroy(allocation: &SandboxAllocation) -> Result<()> {
    let root = &allocation.root;
    let origin = &allocation.origin;

    // 1. Remove the worktree. Use `--force` to blow through modified
    //    files; this is the whole point of a sandbox — the contents are
    //    disposable. A non-zero exit here is logged but doesn't abort:
    //    the belt-and-suspenders fs::remove_dir_all below handles it.
    if root.exists() {
        let mut command = git_command();
        command
            .args(["worktree", "remove", "--force"])
            .arg(root)
            .current_dir(origin);
        let out = capture_bounded(command, "test-only force worktree removal");
        match out {
            Ok(o) if o.status.success() => {
                tracing::debug!(root = %root.display(), "git worktree remove ok");
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                tracing::warn!(
                    root = %root.display(),
                    stderr = %stderr.trim(),
                    "git worktree remove non-zero (continuing)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    root = %root.display(),
                    error = %e,
                    "git worktree remove spawn failed (continuing)"
                );
            }
        }
    }

    // 2. Prune stale worktree metadata (idempotent).
    let mut command = git_command();
    command.args(["worktree", "prune"]).current_dir(origin);
    let _ = capture_bounded(command, "test-only worktree prune");

    // 3. Delete the branch. `-D` forces delete even if unmerged. Non-zero
    //    is ignored — branch may already be gone.
    if let Some(branch) = allocation.branch.as_deref() {
        let mut command = git_command();
        command.args(["branch", "-D", branch]).current_dir(origin);
        let _ = capture_bounded(command, "test-only branch removal");
    }

    // 4. Residual cleanup: if the root directory still exists (git left
    //    stragglers or worktree remove failed above), nuke it. Idempotent.
    if root.exists()
        && let Err(e) = std::fs::remove_dir_all(root)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(DaemonError::Process(format!(
            "failed to remove residual sandbox root '{}': {}",
            root.display(),
            e
        )));
    }

    tracing::info!(
        root = %root.display(),
        branch = ?allocation.branch,
        "Destroyed git-worktree sandbox"
    );

    Ok(())
}

/// Test-only path destructor. Best-effort:
/// recursively removes the directory. The parent allocator docs explain
/// why we can't run `git worktree remove` here.
#[cfg(test)]
pub fn destroy_by_path(root: &Path) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    match std::fs::remove_dir_all(root) {
        Ok(()) => {
            tracing::info!(
                root = %root.display(),
                "Destroyed orphan sandbox by path"
            );
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DaemonError::Process(format!(
            "failed to remove orphan sandbox '{}': {}",
            root.display(),
            e
        ))),
    }
}

/// Canonical session UUID used for default branch names.
fn default_branch(session_id: &Uuid) -> String {
    format!("rsi/{session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    fn git(root: &Path, args: &[&str]) -> String {
        let mut command = git_command();
        command.arg("-C").arg(root).args(args);
        let output = capture_bounded(command, "fixture command").expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 git output")
            .trim()
            .to_string()
    }

    fn initialize_repository(repository: &Path) -> String {
        std::fs::create_dir_all(repository).expect("repository directory");
        git(repository, &["init", "-q", "-b", "main"]);
        git(
            repository,
            &["config", "user.email", "git-worktree@example.test"],
        );
        git(repository, &["config", "user.name", "Git Worktree Test"]);
        std::fs::write(repository.join("tracked"), "base\n").expect("tracked file");
        git(repository, &["add", "tracked"]);
        git(repository, &["commit", "-qm", "base"]);
        git(repository, &["rev-parse", "HEAD"])
    }

    fn ref_lock_path(repository: &Path, reference: &str) -> PathBuf {
        let ref_path = PathBuf::from(git(
            repository,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                reference,
            ],
        ));
        let mut lock_name = ref_path.into_os_string();
        lock_name.push(".lock");
        PathBuf::from(lock_name)
    }

    fn create_stale_ref_lock(repository: &Path, reference: &str) -> PathBuf {
        let lock_path = ref_lock_path(repository, reference);
        std::fs::create_dir_all(lock_path.parent().expect("source lock parent"))
            .expect("create source lock parent");
        std::fs::write(&lock_path, b"timeout residue\n").expect("inject stale source lock");
        lock_path
    }

    fn make_executable(path: &Path) {
        let mut permissions = std::fs::metadata(path)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("executable script");
    }

    fn rolling_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, String) {
        let temp = tempfile::tempdir().expect("temporary fixture");
        let remote = temp.path().join("remote");
        let checkout = temp.path().join("checkout");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir(&remote).expect("remote directory");
        std::fs::create_dir(&checkout).expect("checkout directory");
        std::fs::create_dir(&sandboxes).expect("sandbox directory");
        git(&remote, &["init", "-q", "-b", "rolling"]);
        git(&remote, &["config", "user.email", "fixture@example.test"]);
        git(&remote, &["config", "user.name", "Rolling Fixture"]);
        std::fs::write(remote.join("tracked"), "base\n").expect("base file");
        git(&remote, &["add", "tracked"]);
        git(&remote, &["commit", "-qm", "base"]);
        git(
            temp.path(),
            &[
                "clone",
                "-q",
                remote.to_str().expect("remote path"),
                checkout.to_str().expect("checkout path"),
            ],
        );
        let stale = git(&checkout, &["rev-parse", "HEAD"]);
        std::fs::write(remote.join("tracked"), "fresh\n").expect("fresh file");
        git(&remote, &["add", "tracked"]);
        git(&remote, &["commit", "-qm", "fresh"]);
        (temp, remote, checkout, sandboxes, stale)
    }

    #[test]
    fn fresh_rolling_base_allocates_from_remote_without_moving_checkout_refs() {
        let (_temp, remote, checkout, sandboxes, stale) = rolling_fixture();
        let fresh = git(&remote, &["rev-parse", "HEAD"]);
        let remote_tracking = git(&checkout, &["rev-parse", "refs/remotes/origin/rolling"]);
        assert_eq!(remote_tracking, stale);
        let id = Uuid::new_v4();
        let source = fresh_rolling_base(&checkout, id, stale.clone(), RollingBasePolicy::RemoteTip)
            .expect("fresh observation");
        assert_eq!(source.commit, fresh);
        assert_eq!(source.fallback_reason, None);
        let private_ref = format!("refs/rsi/sandbox-base/{id}");
        let allocation = source
            .allocate_with_cleanup(|commit| {
                assert_eq!(git(&checkout, &["rev-parse", &private_ref]), fresh);
                allocate(&sandboxes, id, &checkout, commit, None)
            })
            .expect("allocation");
        assert_eq!(git(&allocation.root, &["rev-parse", "HEAD"]), fresh);
        assert_eq!(git(&checkout, &["rev-parse", "refs/heads/rolling"]), stale);
        assert_eq!(
            git(&checkout, &["rev-parse", "refs/remotes/origin/rolling"]),
            remote_tracking
        );
        assert!(git(&checkout, &["for-each-ref", "refs/rsi/sandbox-base"]).is_empty());

        // An explicit fork still uses its frozen source commit.
        let fork = allocate(&sandboxes, Uuid::new_v4(), &checkout, &stale, None)
            .expect("explicit fork allocation");
        assert_eq!(git(&fork.root, &["rev-parse", "HEAD"]), stale);
    }

    #[test]
    fn fresh_rolling_base_preserves_strictly_ahead_local_rolling() {
        let (_temp, remote, checkout, sandboxes, _stale) = rolling_fixture();
        git(&checkout, &["fetch", "-q", "origin"]);
        git(&checkout, &["merge", "--ff-only", "origin/rolling"]);
        git(&checkout, &["config", "user.email", "fixture@example.test"]);
        git(&checkout, &["config", "user.name", "Rolling Fixture"]);
        std::fs::write(checkout.join("local"), "local ahead\n").expect("local work");
        git(&checkout, &["add", "local"]);
        git(&checkout, &["commit", "-qm", "local ahead"]);

        let local = git(&checkout, &["rev-parse", "refs/heads/rolling"]);
        let remote_tip = git(&remote, &["rev-parse", "HEAD"]);
        let tracking = git(&checkout, &["rev-parse", "refs/remotes/origin/rolling"]);
        assert_eq!(tracking, remote_tip);
        assert_ne!(local, remote_tip);
        let id = Uuid::new_v4();
        let source = fresh_rolling_base(&checkout, id, local.clone(), RollingBasePolicy::RemoteTip)
            .expect("rolling observation");
        assert_eq!(source.commit, local);
        assert_eq!(
            source.fallback_reason,
            Some(RollingBaseFallbackReason::LocalAhead)
        );
        let private_ref = format!("refs/rsi/sandbox-base/{id}");
        let allocation = source
            .allocate_with_cleanup(|commit| {
                assert_eq!(git(&checkout, &["rev-parse", &private_ref]), remote_tip);
                allocate(&sandboxes, id, &checkout, commit, None)
            })
            .expect("local-ahead allocation");
        assert_eq!(git(&allocation.root, &["rev-parse", "HEAD"]), local);
        assert_eq!(git(&checkout, &["rev-parse", "refs/heads/rolling"]), local);
        assert_eq!(
            git(&checkout, &["rev-parse", "refs/remotes/origin/rolling"]),
            tracking
        );
        assert!(git(&checkout, &["for-each-ref", "refs/rsi/sandbox-base"]).is_empty());
    }

    #[test]
    fn fresh_rolling_base_fetch_failure_falls_back_and_cleans_private_ref() {
        let (_temp, _remote, checkout, sandboxes, stale) = rolling_fixture();
        git(
            &checkout,
            &["remote", "set-url", "origin", "/no/such/rsi-rolling-remote"],
        );
        let id = Uuid::new_v4();
        let source = fresh_rolling_base(&checkout, id, stale.clone(), RollingBasePolicy::RemoteTip)
            .expect("fallback source");
        assert_eq!(source.commit, stale);
        assert_eq!(
            source.fallback_reason,
            Some(RollingBaseFallbackReason::RemoteUnavailable)
        );
        let allocation = source
            .allocate_with_cleanup(|commit| allocate(&sandboxes, id, &checkout, commit, None))
            .expect("fallback allocation");
        assert_eq!(git(&allocation.root, &["rev-parse", "HEAD"]), stale);
        assert!(git(&checkout, &["for-each-ref", "refs/rsi/sandbox-base"]).is_empty());
    }

    #[test]
    fn fresh_rolling_base_preserves_local_only_successor_source() {
        let (_temp, remote, checkout, sandboxes, _stale) = rolling_fixture();
        git(&checkout, &["config", "user.email", "fixture@example.test"]);
        git(&checkout, &["config", "user.name", "Rolling Fixture"]);
        std::fs::write(checkout.join("local"), "local only\n").expect("local work");
        git(&checkout, &["add", "local"]);
        git(&checkout, &["commit", "-qm", "local only"]);
        let local = git(&checkout, &["rev-parse", "HEAD"]);
        let remote_tip = git(&remote, &["rev-parse", "HEAD"]);
        assert_ne!(local, remote_tip);
        let established = freshest_successor_source(&checkout).expect("successor source");
        assert_eq!(established, local);
        let id = Uuid::new_v4();
        let source = fresh_rolling_base(
            &checkout,
            id,
            established,
            RollingBasePolicy::FastForwardOnly,
        )
        .expect("rolling observation");
        assert_eq!(source.commit, local);
        assert_eq!(
            source.fallback_reason,
            Some(RollingBaseFallbackReason::RemoteNotFastForward)
        );
        let private_ref = format!("refs/rsi/sandbox-base/{id}");
        let allocation = source
            .allocate_with_cleanup(|commit| {
                assert_eq!(git(&checkout, &["rev-parse", &private_ref]), remote_tip);
                allocate(&sandboxes, id, &checkout, commit, None)
            })
            .expect("successor allocation");
        assert_eq!(git(&allocation.root, &["rev-parse", "HEAD"]), local);
        assert!(git(&checkout, &["for-each-ref", "refs/rsi/sandbox-base"]).is_empty());
    }

    #[test]
    fn fresh_rolling_base_cleans_ref_after_allocation_failure() {
        let (_temp, remote, checkout, _sandboxes, stale) = rolling_fixture();
        let id = Uuid::new_v4();
        let source = fresh_rolling_base(&checkout, id, stale, RollingBasePolicy::RemoteTip)
            .expect("fresh observation");
        let private_ref = format!("refs/rsi/sandbox-base/{id}");
        let error = source
            .allocate_with_cleanup::<()>(|_| {
                assert_eq!(
                    git(&checkout, &["rev-parse", &private_ref]),
                    git(&remote, &["rev-parse", "HEAD"])
                );
                Err(DaemonError::Process("fixture allocation failed".into()))
            })
            .expect_err("allocation failure");
        assert!(error.to_string().contains("fixture allocation failed"));
        assert!(git(&checkout, &["for-each-ref", "refs/rsi/sandbox-base"]).is_empty());
    }

    #[test]
    fn fresh_rolling_base_ref_deletion_failure_refuses_allocation() {
        let (_temp, _remote, checkout, sandboxes, stale) = rolling_fixture();
        let id = Uuid::new_v4();
        let source = fresh_rolling_base(&checkout, id, stale, RollingBasePolicy::RemoteTip)
            .expect("fresh observation");
        let private_ref = format!("refs/rsi/sandbox-base/{id}");
        let lock = create_stale_ref_lock(&checkout, &private_ref);
        let error = source
            .allocate_with_cleanup(|commit| allocate(&sandboxes, id, &checkout, commit, None))
            .expect_err("ref deletion must fail closed");
        assert!(
            error
                .to_string()
                .contains("could not delete private rolling sandbox base ref")
        );
        assert!(sandboxes.join(id.to_string()).exists());
        assert!(!git(&checkout, &["rev-parse", &private_ref]).is_empty());
        std::fs::remove_file(lock).expect("remove fixture lock");
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn review_seal_holds_across_later_commits_and_reports_git_failure_distinctly() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repository = temp.path().join("repository");
        let sealed = initialize_repository(&repository);
        review_sealed_source_holds_bounded(&repository, &sealed, &sealed)
            .expect("seal holds at its own commit");
        std::fs::create_dir_all(repository.join("thoughts")).expect("thoughts directory");
        std::fs::write(repository.join("thoughts/note.md"), "note\n").expect("note");
        git(&repository, &["add", "thoughts/note.md"]);
        git(&repository, &["commit", "-qm", "note"]);
        let head = git(&repository, &["rev-parse", "HEAD"]);
        review_sealed_source_holds_bounded(&repository, &sealed, &head)
            .expect("a thoughts-only commit keeps the seal");

        // An unreadable head proves nothing about the source: the refusal is
        // the Git failure code, exactly as #599 S1 admission reported it.
        let missing = "1".repeat(40);
        let error = review_sealed_source_holds_bounded(&repository, &sealed, &missing)
            .expect_err("unresolvable head refuses");
        assert!(
            matches!(&error, DaemonError::InvalidParam(code) if code == "manager_v2_git_failed"),
            "{error:?}"
        );

        std::fs::write(repository.join("tracked"), "changed\n").expect("code change");
        git(&repository, &["commit", "-qam", "code"]);
        let head = git(&repository, &["rev-parse", "HEAD"]);
        review_sealed_source_holds_bounded(&repository, &sealed, &head)
            .expect("a later code commit keeps the sealed object");
    }

    #[test]
    fn default_branch_uses_full_canonical_session_uuid() {
        let session_id = Uuid::parse_str("b85caa0d-e1ed-4dbd-b9ca-5e0f49c6e53d")
            .expect("canonical session UUID");

        assert_eq!(
            default_branch(&session_id),
            "rsi/b85caa0d-e1ed-4dbd-b9ca-5e0f49c6e53d"
        );
    }

    #[test]
    fn default_allocations_with_same_eight_hex_prefix_use_distinct_branches() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir(&sandboxes).expect("sandbox base");
        let source_commit = initialize_repository(&repository);
        let first_session_id = Uuid::parse_str("b85caa0d-e1ed-4dbd-b9ca-5e0f49c6e53d")
            .expect("first canonical session UUID");
        let second_session_id = Uuid::parse_str("b85caa0d-0000-4000-8000-000000000001")
            .expect("second canonical session UUID");

        let first = allocate(
            &sandboxes,
            first_session_id,
            &repository,
            &source_commit,
            None,
        )
        .expect("first default allocation");
        let second = allocate(
            &sandboxes,
            second_session_id,
            &repository,
            &source_commit,
            None,
        )
        .expect("second default allocation");
        let first_branch = default_branch(&first_session_id);
        let second_branch = default_branch(&second_session_id);

        assert_eq!(first.branch.as_deref(), Some(first_branch.as_str()));
        assert_eq!(second.branch.as_deref(), Some(second_branch.as_str()));
        assert_ne!(first.branch, second.branch);
        assert_eq!(first.root, sandboxes.join(first_session_id.to_string()));
        assert_eq!(second.root, sandboxes.join(second_session_id.to_string()));
        assert_ne!(first.root, second.root);
        assert_eq!(
            git(&first.root, &["symbolic-ref", "-q", "HEAD"]),
            format!("refs/heads/{first_branch}")
        );
        assert_eq!(
            git(&second.root, &["symbolic-ref", "-q", "HEAD"]),
            format!("refs/heads/{second_branch}")
        );
    }

    #[test]
    fn registered_worktree_proof_is_exact_clean_and_read_only() {
        let temp = tempfile::tempdir().expect("registered proof fixture");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir(&sandboxes).expect("sandbox base");
        let oid = initialize_repository(&repository);
        let session_id = Uuid::new_v4();
        let branch = format!("rsi/{session_id}");
        let allocation = allocate(&sandboxes, session_id, &repository, &oid, Some(&branch))
            .expect("allocate exact worktree");
        let source_ref = format!("refs/heads/{branch}");

        with_repository_mutation(&repository, || {
            let proof = prove_registered_worktree_exact_locked(
                &repository,
                &allocation.root,
                &source_ref,
                &oid,
            )?;
            assert_eq!(
                proof.root_identity,
                FilesystemIdentity::from_metadata(
                    &std::fs::metadata(&allocation.root).expect("root metadata")
                )
            );
            assert_eq!(
                resolve_ref_locked(&repository, &source_ref)?,
                Some(oid.clone())
            );
            Ok(())
        })
        .expect("exact registered proof");

        std::fs::write(allocation.root.join("untracked"), "retain me\n")
            .expect("inject untracked output");
        with_repository_mutation(&repository, || {
            assert!(
                prove_registered_worktree_exact_locked(
                    &repository,
                    &allocation.root,
                    &source_ref,
                    &oid,
                )
                .is_err(),
                "untracked output must fail the positive cleanup proof"
            );
            assert_eq!(
                resolve_ref_locked(&repository, &source_ref)?,
                Some(oid.clone())
            );
            Ok(())
        })
        .expect("dirty refusal preserves the branch");
    }

    #[test]
    fn quarantine_path_is_deterministic_private_and_collision_safe() {
        let temp = tempfile::tempdir().expect("quarantine fixture root");
        let base = temp.path().join("sandboxes");
        std::fs::create_dir(&base).expect("sandbox base");
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            .expect("private sandbox base");
        let session_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let original = base.join(session_id.to_string());
        std::fs::create_dir(&original).expect("original root");

        let proof = prepare_settlement_quarantine_path(&original, run_id, session_id)
            .expect("prepare deterministic quarantine");
        assert_eq!(
            proof.quarantine_root,
            base.join(".settlement-quarantine")
                .join(run_id.to_string())
                .join(session_id.to_string())
        );
        for parent in [
            base.join(".settlement-quarantine"),
            base.join(".settlement-quarantine").join(run_id.to_string()),
        ] {
            assert_eq!(
                std::fs::metadata(parent).expect("private parent").mode() & 0o777,
                0o700
            );
        }
        std::fs::create_dir(&proof.quarantine_root).expect("quarantine collision");
        assert!(
            prepare_settlement_quarantine_path(&original, run_id, session_id).is_err(),
            "an existing destination must not be adopted"
        );
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755))
            .expect("make sandbox base unsafe");
        assert!(
            prepare_settlement_quarantine_path(&original, Uuid::new_v4(), session_id).is_err(),
            "a group/world-accessible sandbox base must fail closed"
        );
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            .expect("restore private sandbox base");
        let uid = std::fs::metadata(&base)
            .expect("sandbox base identity")
            .uid();
        assert!(
            prove_private_sandbox_base_for_uid(&base, uid.wrapping_add(1)).is_err(),
            "a sandbox base owned by another uid must fail closed"
        );

        let symlink_base = temp.path().join("symlink-sandboxes");
        let redirect = temp.path().join("redirect");
        std::fs::create_dir(&symlink_base).expect("symlink sandbox base");
        std::fs::set_permissions(&symlink_base, std::fs::Permissions::from_mode(0o700))
            .expect("private symlink sandbox base");
        std::fs::create_dir(&redirect).expect("redirect directory");
        let symlink_session = Uuid::new_v4();
        let symlink_root = symlink_base.join(symlink_session.to_string());
        std::fs::create_dir(&symlink_root).expect("symlink fixture root");
        std::os::unix::fs::symlink(&redirect, symlink_base.join(".settlement-quarantine"))
            .expect("quarantine parent symlink");
        assert!(
            prepare_settlement_quarantine_path(&symlink_root, Uuid::new_v4(), symlink_session,)
                .is_err(),
            "a symlinked quarantine parent must fail closed"
        );
    }

    #[test]
    fn quarantine_move_is_inode_bound_and_preserves_old_path_injection() {
        let temp = tempfile::tempdir().expect("quarantine move fixture");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir(&sandboxes).expect("sandbox base");
        std::fs::set_permissions(&sandboxes, std::fs::Permissions::from_mode(0o700))
            .expect("private sandbox base");
        let oid = initialize_repository(&repository);
        let session_id = Uuid::new_v4();
        let branch = format!("rsi/quarantine/{session_id}");
        let allocation = allocate(&sandboxes, session_id, &repository, &oid, Some(&branch))
            .expect("linked worktree");
        let other_session_id = Uuid::new_v4();
        let other_branch = format!("rsi/quarantine-other/{other_session_id}");
        let other = allocate(
            &sandboxes,
            other_session_id,
            &repository,
            &oid,
            Some(&other_branch),
        )
        .expect("second linked worktree");
        let branch_ref = format!("refs/heads/{branch}");
        let original = allocation.root.clone();

        with_repository_mutation(&repository, || {
            let path = prepare_settlement_quarantine_path(&original, Uuid::new_v4(), session_id)?;
            let admin = move_worktree_to_quarantine_non_force_locked(
                &repository,
                &path,
                &branch_ref,
                &oid,
            )?;
            assert_eq!(admin.root_identity, path.root_identity);
            let other_admin = prove_admin_identity(&repository, &other.root, &[&other.root])?;
            let other_head = other_admin.admin_directory.join("HEAD");
            let other_head_before = std::fs::read(&other_head).expect("other worktree HEAD");
            std::fs::write(&other_head, format!("ref: {branch_ref}\n"))
                .expect("forge second same-ref registration");
            assert!(
                !source_ref_is_registered_only_at_locked(
                    &repository,
                    &branch_ref,
                    &path.quarantine_root,
                    &oid,
                )?,
                "a second same-ref registration must defeat sole-use proof"
            );
            assert!(
                prove_moved_worktree_exact_locked(
                    &repository,
                    &path.original_root,
                    &path.quarantine_root,
                    &branch_ref,
                    &oid,
                )
                .is_err(),
                "a moved worktree with a second same-ref registration must retain"
            );
            std::fs::write(&other_head, other_head_before).expect("restore other worktree HEAD");
            prove_moved_worktree_exact_locked(
                &repository,
                &path.original_root,
                &path.quarantine_root,
                &branch_ref,
                &oid,
            )?;
            let tree = prove_quarantine_tree_safe(&path.quarantine_root)?;
            assert_eq!(tree.root_identity(), path.root_identity);
            assert!(tree.entry_count() >= 3);

            std::fs::create_dir(&original).expect("stale writer recreated old root");
            std::fs::write(original.join("ignored.secret"), "unique bytes\n")
                .expect("stale ignored injection");
            remove_worktree_non_force_locked(
                &repository,
                &path.original_root,
                &path.quarantine_root,
                &branch_ref,
                &oid,
            )?;
            assert_eq!(
                std::fs::read_to_string(original.join("ignored.secret"))
                    .expect("injected bytes retained"),
                "unique bytes\n"
            );
            Ok(())
        })
        .expect("quarantine move and non-force removal");
    }

    #[test]
    fn branch_first_sentinel_blocks_add_and_removes_dangling_non_force() {
        let temp = tempfile::tempdir().expect("branch-first fixture");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir(&sandboxes).expect("sandbox base");
        std::fs::set_permissions(&sandboxes, std::fs::Permissions::from_mode(0o700))
            .expect("private sandbox base");
        let oid = initialize_repository(&repository);
        let session_id = Uuid::new_v4();
        let branch = format!("rsi/branch-first/{session_id}");
        let branch_ref = format!("refs/heads/{branch}");
        let allocation = allocate(&sandboxes, session_id, &repository, &oid, Some(&branch))
            .expect("source worktree");
        let other_session_id = Uuid::new_v4();
        let other_branch = format!("rsi/branch-first-other/{other_session_id}");
        let other = allocate(
            &sandboxes,
            other_session_id,
            &repository,
            &oid,
            Some(&other_branch),
        )
        .expect("other worktree");

        with_repository_mutation(&repository, || {
            let path =
                prepare_settlement_quarantine_path(&allocation.root, Uuid::new_v4(), session_id)?;
            let before = move_worktree_to_quarantine_non_force_locked(
                &repository,
                &path,
                &branch_ref,
                &oid,
            )?;
            delete_source_ref_atomically_locked(
                &repository,
                "refs/heads/main",
                &oid,
                &branch_ref,
                &oid,
            )?;
            assert!(source_ref_is_registered_only_at_missing_ref_locked(
                &repository,
                &branch_ref,
                &path.quarantine_root,
                &oid,
            )?);
            let missing = prove_missing_source_ref_quarantine_exact_locked(
                &repository,
                &branch_ref,
                &path.quarantine_root,
                &oid,
            )?;
            assert_eq!(missing, before);

            let replacement = sandboxes.join(Uuid::new_v4().to_string());
            let mut external_add = git_command();
            external_add
                .arg("-C")
                .arg(&repository)
                .args(["worktree", "add", "-b", &branch])
                .arg(&replacement)
                .arg(&oid);
            let output = capture_bounded(external_add, "attempt ordinary replacement worktree")?;
            assert!(
                !output.status.success(),
                "the quarantine sentinel must block an ordinary replacement worktree"
            );
            assert!(registration_for_path(&repository, &replacement)?.is_none());
            assert_eq!(
                observe_direct_ref_locked(&repository, &branch_ref)?,
                DirectRefObservation::Commit(oid.clone()),
                "Git may recreate the branch before refusing its second checkout"
            );
            delete_source_ref_atomically_locked(
                &repository,
                "refs/heads/main",
                &oid,
                &branch_ref,
                &oid,
            )?;

            let other_admin = prove_admin_identity(&repository, &other.root, &[&other.root])?;
            let other_head = other_admin.admin_directory.join("HEAD");
            let other_head_before = std::fs::read(&other_head).expect("other worktree HEAD");
            std::fs::write(&other_head, format!("ref: {branch_ref}\n"))
                .expect("forge extra missing-source registration");
            assert!(!source_ref_is_registered_only_at_missing_ref_locked(
                &repository,
                &branch_ref,
                &path.quarantine_root,
                &oid,
            )?);
            assert!(
                remove_worktree_after_source_ref_delete_non_force_locked(
                    &repository,
                    &path.original_root,
                    &path.quarantine_root,
                    &branch_ref,
                    &oid,
                )
                .is_err(),
                "an extra source-ref registration must retain the quarantine"
            );
            assert!(path.quarantine_root.exists());
            std::fs::write(&other_head, other_head_before).expect("restore other worktree HEAD");

            // An unacknowledged successful effect is accepted only after the
            // missing source lock can be acquired and released again.
            inject_worktree_remove_lost_ack();
            remove_worktree_after_source_ref_delete_non_force_locked(
                &repository,
                &path.original_root,
                &path.quarantine_root,
                &branch_ref,
                &oid,
            )?;
            assert_eq!(
                observe_direct_ref_locked(&repository, &branch_ref)?,
                DirectRefObservation::Missing
            );
            assert!(!path.quarantine_root.exists());
            assert!(registration_for_path(&repository, &path.quarantine_root)?.is_none());

            // Exact replay models a lost success acknowledgement.
            remove_worktree_after_source_ref_delete_non_force_locked(
                &repository,
                &path.original_root,
                &path.quarantine_root,
                &branch_ref,
                &oid,
            )
        })
        .expect("branch-first removal and replay");
    }

    #[test]
    fn branch_first_lost_ack_rejects_stale_missing_source_lock() {
        let temp = tempfile::tempdir().expect("stale source-lock fixture");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir(&sandboxes).expect("sandbox base");
        std::fs::set_permissions(&sandboxes, std::fs::Permissions::from_mode(0o700))
            .expect("private sandbox base");
        let oid = initialize_repository(&repository);
        let session_id = Uuid::new_v4();
        let branch = format!("rsi/stale-source-lock/{session_id}");
        let branch_ref = format!("refs/heads/{branch}");
        let allocation = allocate(&sandboxes, session_id, &repository, &oid, Some(&branch))
            .expect("source worktree");

        with_repository_mutation(&repository, || {
            let path =
                prepare_settlement_quarantine_path(&allocation.root, Uuid::new_v4(), session_id)?;
            move_worktree_to_quarantine_non_force_locked(&repository, &path, &branch_ref, &oid)?;
            delete_source_ref_atomically_locked(
                &repository,
                "refs/heads/main",
                &oid,
                &branch_ref,
                &oid,
            )?;

            let lock_path = ref_lock_path(&repository, &branch_ref);
            let injected_lock = lock_path.clone();
            WORKTREE_REMOVE_POST_EFFECT_TEST_HOOK.with(|slot| {
                assert!(
                    slot.borrow_mut()
                        .replace(Box::new(move || {
                            std::fs::create_dir_all(
                                injected_lock.parent().expect("source lock parent"),
                            )
                            .expect("create source lock parent");
                            std::fs::write(&injected_lock, b"timeout residue\n")
                                .expect("inject stale source lock");
                        }))
                        .is_none(),
                    "post-effect test hook already armed"
                );
            });
            inject_worktree_remove_lost_ack();
            let error = remove_worktree_after_source_ref_delete_non_force_locked(
                &repository,
                &path.original_root,
                &path.quarantine_root,
                &branch_ref,
                &oid,
            )
            .expect_err("an unacknowledged effect with a stale ref lock must not be accepted");
            assert!(
                error.to_string().contains("not reacquirable"),
                "unexpected stale-lock error: {error}"
            );
            assert!(!path.quarantine_root.exists());
            assert_eq!(
                observe_direct_ref_locked(&repository, &branch_ref)?,
                DirectRefObservation::Missing
            );
            assert!(lock_path.exists(), "the unknown lock residue is retained");
            std::fs::remove_file(lock_path).expect("remove stale source-lock fixture");
            Ok(())
        })
        .expect("stale source lock is rejected after an unacknowledged effect");
    }

    #[test]
    fn branch_first_removed_replay_rejects_stale_source_lock() {
        let temp = tempfile::tempdir().expect("removed replay stale-lock fixture");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir(&sandboxes).expect("sandbox base");
        std::fs::set_permissions(&sandboxes, std::fs::Permissions::from_mode(0o700))
            .expect("private sandbox base");
        let oid = initialize_repository(&repository);
        let session_id = Uuid::new_v4();
        let branch = format!("rsi/removed-replay-stale/{session_id}");
        let branch_ref = format!("refs/heads/{branch}");
        let allocation = allocate(&sandboxes, session_id, &repository, &oid, Some(&branch))
            .expect("source worktree");

        with_repository_mutation(&repository, || {
            let path =
                prepare_settlement_quarantine_path(&allocation.root, Uuid::new_v4(), session_id)?;
            move_worktree_to_quarantine_non_force_locked(&repository, &path, &branch_ref, &oid)?;
            delete_source_ref_atomically_locked(
                &repository,
                "refs/heads/main",
                &oid,
                &branch_ref,
                &oid,
            )?;
            remove_worktree_after_source_ref_delete_non_force_locked(
                &repository,
                &path.original_root,
                &path.quarantine_root,
                &branch_ref,
                &oid,
            )?;

            let lock_path = create_stale_ref_lock(&repository, &branch_ref);
            let error = remove_worktree_after_source_ref_delete_non_force_locked(
                &repository,
                &path.original_root,
                &path.quarantine_root,
                &branch_ref,
                &oid,
            )
            .expect_err("an exact removed replay must re-prove the missing source lock");
            assert!(
                error.to_string().contains("not reacquirable"),
                "unexpected stale-lock replay error: {error}"
            );
            assert!(lock_path.exists(), "the unknown lock residue is retained");
            assert!(!path.quarantine_root.exists());
            Ok(())
        })
        .expect("removed replay rejects a stale source lock");
    }

    #[test]
    fn source_ref_restore_replay_rejects_stale_source_lock() {
        let temp = tempfile::tempdir().expect("source replay stale-lock fixture");
        let repository = temp.path().join("repository");
        let oid = initialize_repository(&repository);
        let source_ref = "refs/heads/rsi/restore-replay-stale";
        git(&repository, &["update-ref", source_ref, &oid]);
        let lock_path = create_stale_ref_lock(&repository, source_ref);

        with_repository_mutation(&repository, || {
            let error =
                restore_source_ref_if_missing_atomically_locked(&repository, source_ref, &oid)
                    .expect_err("an exact source replay must re-prove its ref lock");
            assert!(
                error.to_string().contains("not reacquirable"),
                "unexpected stale-lock replay error: {error}"
            );
            assert_eq!(
                observe_direct_ref_locked(&repository, source_ref)?,
                DirectRefObservation::Commit(oid.clone())
            );
            assert!(lock_path.exists(), "the unknown lock residue is retained");
            Ok(())
        })
        .expect("source replay rejects a stale ref lock");
    }

    #[test]
    fn source_ref_restore_is_missing_only_and_target_independent() {
        let temp = tempfile::tempdir().expect("source restore fixture");
        let repository = temp.path().join("repository");
        let source_oid = initialize_repository(&repository);
        let source_ref = "refs/heads/rsi/restore-source";
        git(&repository, &["update-ref", source_ref, &source_oid]);

        with_repository_mutation(&repository, || {
            delete_source_ref_atomically_locked(
                &repository,
                "refs/heads/main",
                &source_oid,
                source_ref,
                &source_oid,
            )?;
            std::fs::write(repository.join("tracked"), "target drift\n")
                .expect("advance target contents");
            git(&repository, &["commit", "-qam", "advance target"]);
            let drifted_target = git(&repository, &["rev-parse", "HEAD"]);

            restore_source_ref_if_missing_atomically_locked(&repository, source_ref, &source_oid)?;
            assert_eq!(
                observe_direct_ref_locked(&repository, source_ref)?,
                DirectRefObservation::Commit(source_oid.clone())
            );
            assert_eq!(
                observe_direct_ref_locked(&repository, "refs/heads/main")?,
                DirectRefObservation::Commit(drifted_target.clone()),
                "target drift must not block exact source compensation"
            );
            restore_source_ref_if_missing_atomically_locked(&repository, source_ref, &source_oid)?;

            delete_source_ref_atomically_locked(
                &repository,
                "refs/heads/main",
                &drifted_target,
                source_ref,
                &source_oid,
            )?;
            let tree = git(&repository, &["rev-parse", "HEAD^{tree}"]);
            let other_oid = git(
                &repository,
                &["commit-tree", &tree, "-m", "source restoration race"],
            );
            let hook_repository = repository.clone();
            let hook_oid = other_oid.clone();
            set_atomic_ref_pre_spawn_test_hook(move || {
                git(&hook_repository, &["update-ref", source_ref, &hook_oid]);
            });
            assert!(
                restore_source_ref_if_missing_atomically_locked(
                    &repository,
                    source_ref,
                    &source_oid,
                )
                .is_err(),
                "a raced source ref must not be overwritten"
            );
            assert_eq!(
                observe_direct_ref_locked(&repository, source_ref)?,
                DirectRefObservation::Commit(other_oid)
            );
            Ok(())
        })
        .expect("exact source restoration");
    }

    #[test]
    fn detached_missing_quarantine_recovery_is_atomic_and_replayable() {
        for intermediate in ["symbolic-missing", "detached-missing"] {
            let temp = tempfile::tempdir().expect("detached recovery fixture");
            let repository = temp.path().join("repository");
            let sandboxes = temp.path().join("sandboxes");
            std::fs::create_dir(&sandboxes).expect("sandbox base");
            std::fs::set_permissions(&sandboxes, std::fs::Permissions::from_mode(0o700))
                .expect("private sandbox base");
            let oid = initialize_repository(&repository);
            let session_id = Uuid::new_v4();
            let branch = format!("rsi/detached-recovery/{session_id}");
            let branch_ref = format!("refs/heads/{branch}");
            let allocation = allocate(&sandboxes, session_id, &repository, &oid, Some(&branch))
                .expect("source worktree");

            with_repository_mutation(&repository, || {
                let path = prepare_settlement_quarantine_path(
                    &allocation.root,
                    Uuid::new_v4(),
                    session_id,
                )?;
                let before = move_worktree_to_quarantine_non_force_locked(
                    &repository,
                    &path,
                    &branch_ref,
                    &oid,
                )?;
                delete_source_ref_atomically_locked(
                    &repository,
                    "refs/heads/main",
                    &oid,
                    &branch_ref,
                    &oid,
                )?;
                if intermediate == "detached-missing" {
                    git(
                        &path.quarantine_root,
                        &["update-ref", "--no-deref", "HEAD", &oid],
                    );
                    assert_eq!(
                        prove_detached_missing_source_ref_quarantine_exact_locked(
                            &repository,
                            &branch_ref,
                            &path.quarantine_root,
                            &oid,
                        )?,
                        before
                    );
                    inject_restore_reattach_lost_ack();
                } else {
                    assert!(source_ref_is_registered_only_at_missing_ref_locked(
                        &repository,
                        &branch_ref,
                        &path.quarantine_root,
                        &oid,
                    )?);
                }

                let recovered =
                    restore_source_ref_and_reattach_detached_quarantine_atomically_locked(
                        &repository,
                        &path.original_root,
                        &path.quarantine_root,
                        &branch_ref,
                        &oid,
                    )?;
                assert_eq!(recovered, before);
                assert_eq!(
                    observe_direct_ref_locked(&repository, &branch_ref)?,
                    DirectRefObservation::Commit(oid.clone())
                );
                assert_eq!(
                    prove_moved_worktree_exact_locked(
                        &repository,
                        &path.original_root,
                        &path.quarantine_root,
                        &branch_ref,
                        &oid,
                    )?,
                    before
                );
                assert_eq!(
                    restore_source_ref_and_reattach_detached_quarantine_atomically_locked(
                        &repository,
                        &path.original_root,
                        &path.quarantine_root,
                        &branch_ref,
                        &oid,
                    )?,
                    before,
                    "exact final replay must model a lost commit acknowledgement"
                );
                Ok(())
            })
            .unwrap_or_else(|error| panic!("recover {intermediate}: {error}"));
        }
    }

    #[test]
    fn reattached_quarantine_replay_rejects_stale_source_lock() {
        let temp = tempfile::tempdir().expect("reattached replay stale-lock fixture");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir(&sandboxes).expect("sandbox base");
        std::fs::set_permissions(&sandboxes, std::fs::Permissions::from_mode(0o700))
            .expect("private sandbox base");
        let oid = initialize_repository(&repository);
        let session_id = Uuid::new_v4();
        let branch = format!("rsi/reattached-replay-stale/{session_id}");
        let branch_ref = format!("refs/heads/{branch}");
        let allocation = allocate(&sandboxes, session_id, &repository, &oid, Some(&branch))
            .expect("source worktree");

        with_repository_mutation(&repository, || {
            let path =
                prepare_settlement_quarantine_path(&allocation.root, Uuid::new_v4(), session_id)?;
            let before = move_worktree_to_quarantine_non_force_locked(
                &repository,
                &path,
                &branch_ref,
                &oid,
            )?;
            delete_source_ref_atomically_locked(
                &repository,
                "refs/heads/main",
                &oid,
                &branch_ref,
                &oid,
            )?;
            assert_eq!(
                restore_source_ref_and_reattach_detached_quarantine_atomically_locked(
                    &repository,
                    &path.original_root,
                    &path.quarantine_root,
                    &branch_ref,
                    &oid,
                )?,
                before
            );

            let lock_path = create_stale_ref_lock(&repository, &branch_ref);
            let error = restore_source_ref_and_reattach_detached_quarantine_atomically_locked(
                &repository,
                &path.original_root,
                &path.quarantine_root,
                &branch_ref,
                &oid,
            )
            .expect_err("an exact reattached replay must re-prove its ref lock");
            assert!(
                error.to_string().contains("not reacquirable"),
                "unexpected stale-lock replay error: {error}"
            );
            assert_eq!(
                prove_moved_worktree_exact_locked(
                    &repository,
                    &path.original_root,
                    &path.quarantine_root,
                    &branch_ref,
                    &oid,
                )?,
                before
            );
            assert!(lock_path.exists(), "the unknown lock residue is retained");
            Ok(())
        })
        .expect("reattached replay rejects a stale ref lock");
    }

    #[test]
    fn recovery_redispatches_same_oid_recreation_before_state_proof() {
        for intermediate in ["symbolic-missing", "detached-missing"] {
            let temp = tempfile::tempdir().expect("pre-proof source race fixture");
            let repository = temp.path().join("repository");
            let sandboxes = temp.path().join("sandboxes");
            std::fs::create_dir(&sandboxes).expect("sandbox base");
            std::fs::set_permissions(&sandboxes, std::fs::Permissions::from_mode(0o700))
                .expect("private sandbox base");
            let oid = initialize_repository(&repository);
            let session_id = Uuid::new_v4();
            let branch = format!("rsi/pre-proof-source-race/{session_id}");
            let branch_ref = format!("refs/heads/{branch}");
            let allocation = allocate(&sandboxes, session_id, &repository, &oid, Some(&branch))
                .expect("source worktree");

            with_repository_mutation(&repository, || {
                let path = prepare_settlement_quarantine_path(
                    &allocation.root,
                    Uuid::new_v4(),
                    session_id,
                )?;
                let before = move_worktree_to_quarantine_non_force_locked(
                    &repository,
                    &path,
                    &branch_ref,
                    &oid,
                )?;
                delete_source_ref_atomically_locked(
                    &repository,
                    "refs/heads/main",
                    &oid,
                    &branch_ref,
                    &oid,
                )?;
                if intermediate == "detached-missing" {
                    git(
                        &path.quarantine_root,
                        &["update-ref", "--no-deref", "HEAD", &oid],
                    );
                }

                let hook_repository = repository.clone();
                let hook_ref = branch_ref.clone();
                let hook_oid = oid.clone();
                set_direct_ref_empty_lookup_test_hook(move || {
                    git(&hook_repository, &["update-ref", &hook_ref, &hook_oid]);
                });
                assert_eq!(
                    restore_source_ref_and_reattach_detached_quarantine_atomically_locked(
                        &repository,
                        &path.original_root,
                        &path.quarantine_root,
                        &branch_ref,
                        &oid,
                    )?,
                    before,
                    "same-call redispatch changed {intermediate} identity"
                );
                assert_eq!(
                    prove_moved_worktree_exact_locked(
                        &repository,
                        &path.original_root,
                        &path.quarantine_root,
                        &branch_ref,
                        &oid,
                    )?,
                    before
                );
                Ok(())
            })
            .unwrap_or_else(|error| panic!("redispatch {intermediate}: {error}"));
        }
    }

    #[test]
    fn detached_recovery_replays_same_oid_source_race_and_lost_ack() {
        let temp = tempfile::tempdir().expect("same-OID recovery race fixture");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir(&sandboxes).expect("sandbox base");
        std::fs::set_permissions(&sandboxes, std::fs::Permissions::from_mode(0o700))
            .expect("private sandbox base");
        let oid = initialize_repository(&repository);
        let session_id = Uuid::new_v4();
        let branch = format!("rsi/detached-same-oid-race/{session_id}");
        let branch_ref = format!("refs/heads/{branch}");
        let allocation = allocate(&sandboxes, session_id, &repository, &oid, Some(&branch))
            .expect("source worktree");

        with_repository_mutation(&repository, || {
            let path =
                prepare_settlement_quarantine_path(&allocation.root, Uuid::new_v4(), session_id)?;
            let before = move_worktree_to_quarantine_non_force_locked(
                &repository,
                &path,
                &branch_ref,
                &oid,
            )?;
            delete_source_ref_atomically_locked(
                &repository,
                "refs/heads/main",
                &oid,
                &branch_ref,
                &oid,
            )?;
            git(
                &path.quarantine_root,
                &["update-ref", "--no-deref", "HEAD", &oid],
            );

            let hook_repository = repository.clone();
            let hook_ref = branch_ref.clone();
            let hook_oid = oid.clone();
            set_atomic_ref_pre_spawn_test_hook(move || {
                git(&hook_repository, &["update-ref", &hook_ref, &hook_oid]);
            });
            // The create transaction loses to the exact ordinary Git race;
            // arm acknowledgement loss for the same call's successful
            // verify-only fallback reattach transaction.
            inject_restore_reattach_lost_ack();
            let raced = restore_source_ref_and_reattach_detached_quarantine_atomically_locked(
                &repository,
                &path.original_root,
                &path.quarantine_root,
                &branch_ref,
                &oid,
            )?;
            assert_eq!(
                raced, before,
                "the same-call race recovery changed identity"
            );
            assert_eq!(
                observe_direct_ref_locked(&repository, &branch_ref)?,
                DirectRefObservation::Commit(oid.clone())
            );
            assert_eq!(
                prove_moved_worktree_exact_locked(
                    &repository,
                    &path.original_root,
                    &path.quarantine_root,
                    &branch_ref,
                    &oid,
                )?,
                before
            );
            Ok(())
        })
        .expect("same-OID source race recovers under the exact ref lock");
    }

    #[test]
    fn detached_missing_recovery_refuses_source_create_collision() {
        let temp = tempfile::tempdir().expect("detached collision fixture");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir(&sandboxes).expect("sandbox base");
        std::fs::set_permissions(&sandboxes, std::fs::Permissions::from_mode(0o700))
            .expect("private sandbox base");
        let oid = initialize_repository(&repository);
        let session_id = Uuid::new_v4();
        let branch = format!("rsi/detached-collision/{session_id}");
        let branch_ref = format!("refs/heads/{branch}");
        let allocation = allocate(&sandboxes, session_id, &repository, &oid, Some(&branch))
            .expect("source worktree");

        with_repository_mutation(&repository, || {
            let path =
                prepare_settlement_quarantine_path(&allocation.root, Uuid::new_v4(), session_id)?;
            move_worktree_to_quarantine_non_force_locked(&repository, &path, &branch_ref, &oid)?;
            delete_source_ref_atomically_locked(
                &repository,
                "refs/heads/main",
                &oid,
                &branch_ref,
                &oid,
            )?;
            git(
                &path.quarantine_root,
                &["update-ref", "--no-deref", "HEAD", &oid],
            );
            let tree = git(&repository, &["rev-parse", "HEAD^{tree}"]);
            let collision_oid = git(
                &repository,
                &["commit-tree", &tree, "-m", "source collision"],
            );
            let hook_repository = repository.clone();
            let hook_ref = branch_ref.clone();
            let hook_oid = collision_oid.clone();
            set_atomic_ref_pre_spawn_test_hook(move || {
                git(&hook_repository, &["update-ref", &hook_ref, &hook_oid]);
            });
            assert!(
                restore_source_ref_and_reattach_detached_quarantine_atomically_locked(
                    &repository,
                    &path.original_root,
                    &path.quarantine_root,
                    &branch_ref,
                    &oid,
                )
                .is_err(),
                "a source-create collision must retain the detached quarantine"
            );
            assert_eq!(
                observe_direct_ref_locked(&repository, &branch_ref)?,
                DirectRefObservation::Commit(collision_oid)
            );
            let admin =
                prove_admin_identity(&repository, &path.quarantine_root, &[&path.quarantine_root])?;
            assert_eq!(
                std::fs::read_to_string(admin.admin_directory.join("HEAD"))
                    .expect("detached admin HEAD")
                    .trim(),
                oid
            );
            Ok(())
        })
        .expect("collision retains exact external ref");
    }

    #[test]
    fn non_force_remove_rejects_dangling_and_unreadable_residue() {
        for residue in ["dangling", "unreadable"] {
            let temp = tempfile::tempdir().expect("removal residue fixture");
            let repository = temp.path().join("repository");
            let sandboxes = temp.path().join("sandboxes");
            std::fs::create_dir(&sandboxes).expect("sandbox base");
            std::fs::set_permissions(&sandboxes, std::fs::Permissions::from_mode(0o700))
                .expect("private sandbox base");
            let oid = initialize_repository(&repository);
            let session_id = Uuid::new_v4();
            let branch = format!("rsi/remove-residue/{session_id}");
            let allocation = allocate(&sandboxes, session_id, &repository, &oid, Some(&branch))
                .expect("linked worktree");
            let branch_ref = format!("refs/heads/{branch}");

            with_repository_mutation(&repository, || {
                let path = prepare_settlement_quarantine_path(
                    &allocation.root,
                    Uuid::new_v4(),
                    session_id,
                )?;
                move_worktree_to_quarantine_non_force_locked(
                    &repository,
                    &path,
                    &branch_ref,
                    &oid,
                )?;
                let quarantine_root = path.quarantine_root.clone();
                let run_parent = quarantine_root.parent().expect("run parent").to_path_buf();
                WORKTREE_REMOVE_POST_EFFECT_TEST_HOOK.with(|slot| {
                    *slot.borrow_mut() = Some(match residue {
                        "dangling" => Box::new(move || {
                            std::os::unix::fs::symlink("missing-target", quarantine_root)
                                .expect("dangling removal residue");
                        }),
                        "unreadable" => Box::new(move || {
                            std::fs::set_permissions(
                                run_parent,
                                std::fs::Permissions::from_mode(0o000),
                            )
                            .expect("unreadable removal residue parent");
                        }),
                        _ => unreachable!(),
                    });
                });
                let error = remove_worktree_non_force_locked(
                    &repository,
                    &path.original_root,
                    &path.quarantine_root,
                    &branch_ref,
                    &oid,
                )
                .expect_err("ambiguous filesystem residue must fail closed");
                if residue == "dangling" {
                    assert!(error.to_string().contains("filesystem residue"), "{error}");
                    std::fs::remove_file(&path.quarantine_root)
                        .expect("remove dangling residue fixture");
                } else {
                    std::fs::set_permissions(
                        path.quarantine_root.parent().expect("run parent"),
                        std::fs::Permissions::from_mode(0o700),
                    )
                    .expect("restore readable run parent");
                    assert!(
                        error.to_string().contains("residue check failed"),
                        "{error}"
                    );
                }
                Ok(())
            })
            .expect("strict non-force removal residue proof");
        }
    }

    #[test]
    fn exact_repair_recovers_only_the_renamed_worktree() {
        let temp = tempfile::tempdir().expect("quarantine repair fixture");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir(&sandboxes).expect("sandbox base");
        std::fs::set_permissions(&sandboxes, std::fs::Permissions::from_mode(0o700))
            .expect("private sandbox base");
        let oid = initialize_repository(&repository);
        let session_id = Uuid::new_v4();
        let branch = format!("rsi/repair/{session_id}");
        let allocation = allocate(&sandboxes, session_id, &repository, &oid, Some(&branch))
            .expect("linked worktree");
        let branch_ref = format!("refs/heads/{branch}");

        with_repository_mutation(&repository, || {
            let path =
                prepare_settlement_quarantine_path(&allocation.root, Uuid::new_v4(), session_id)?;
            std::fs::rename(&path.original_root, &path.quarantine_root)
                .expect("simulate crash after directory rename");
            let repaired = repair_moved_worktree_if_exact_locked(
                &repository,
                &path.original_root,
                &path.quarantine_root,
                &branch_ref,
                &oid,
            )?;
            assert_eq!(repaired.root_identity, path.root_identity);
            prove_moved_worktree_exact_locked(
                &repository,
                &path.original_root,
                &path.quarantine_root,
                &branch_ref,
                &oid,
            )?;
            let gitdir_pointer = path.quarantine_root.join(".git");
            let exact_pointer = std::fs::read(&gitdir_pointer).expect("exact q gitdir pointer");
            let stale_pointer = "gitdir: /definitely/not/a/worktree/admin\n";
            std::fs::write(&gitdir_pointer, stale_pointer)
                .expect("corrupt already-quarantined admin pointer");
            assert!(
                repair_moved_worktree_if_exact_locked(
                    &repository,
                    &path.original_root,
                    &path.quarantine_root,
                    &branch_ref,
                    &oid,
                )
                .is_err(),
                "an already-quarantine registration with failed exact proof must not be repaired"
            );
            assert_eq!(
                std::fs::read(&gitdir_pointer).expect("unrepaired q gitdir pointer"),
                stale_pointer.as_bytes(),
                "the negative repair path must not invoke git worktree repair"
            );
            std::fs::write(&gitdir_pointer, exact_pointer).expect("restore exact q gitdir pointer");
            remove_worktree_non_force_locked(
                &repository,
                &path.original_root,
                &path.quarantine_root,
                &branch_ref,
                &oid,
            )
        })
        .expect("repair exact partial move");
    }

    #[test]
    fn quarantine_tree_proof_rejects_mounts_hardlinks_specials_and_bounds() {
        let temp = tempfile::tempdir().expect("quarantine tree fixture");
        let root = temp.path().join("quarantine");
        std::fs::create_dir(&root).expect("quarantine root");
        let mountinfo = temp.path().join("mountinfo");
        std::fs::write(&mountinfo, "").expect("empty mount inventory");
        std::fs::write(root.join("file"), "bytes\n").expect("regular file");
        let proof =
            prove_quarantine_tree_safe_at(&root, &mountinfo, 16, 1_024, 16, Duration::from_secs(1))
                .expect("ordinary tree is safe");
        assert_eq!(proof.entry_count(), 2);
        assert!(proof.tree_digest().starts_with("sha256:"));
        assert_eq!(
            reprove_quarantine_tree_unchanged(&proof)
                .expect("stable tree reproof")
                .tree_digest(),
            proof.tree_digest()
        );
        assert!(
            prove_quarantine_tree_safe_at(&root, &mountinfo, 16, 1, 16, Duration::from_secs(1))
                .is_err(),
            "total relative-path bytes must be bounded"
        );
        std::fs::write(root.join("late-injection"), "unique bytes\n").expect("late tree injection");
        assert!(
            reprove_quarantine_tree_unchanged(&proof).is_err(),
            "a pathname/inode snapshot drift must fail before removal"
        );
        std::fs::remove_file(root.join("late-injection")).expect("remove late injection fixture");

        let outside_link = temp.path().join("outside-link");
        std::fs::hard_link(root.join("file"), &outside_link).expect("hardlink fixture");
        assert!(
            prove_quarantine_tree_safe_at(&root, &mountinfo, 16, 1_024, 16, Duration::from_secs(1))
                .is_err(),
            "multiply-linked regular files must retain the tree"
        );
        std::fs::remove_file(outside_link).expect("remove hardlink fixture");

        #[cfg(target_os = "linux")]
        {
            let xattr_path =
                CString::new(root.join("file").as_os_str().as_bytes()).expect("xattr fixture path");
            let xattr_name = CString::new("user.rsi-quarantine-test").expect("xattr fixture name");
            let xattr_value = b"retained metadata";
            // SAFETY: all pointers reference live bounded buffers for this call.
            let xattr_result = unsafe {
                nix::libc::lsetxattr(
                    xattr_path.as_ptr(),
                    xattr_name.as_ptr(),
                    xattr_value.as_ptr().cast(),
                    xattr_value.len(),
                    0,
                )
            };
            if xattr_result == 0 {
                assert!(
                    prove_quarantine_tree_safe_at(
                        &root,
                        &mountinfo,
                        16,
                        1_024,
                        16,
                        Duration::from_secs(1)
                    )
                    .is_err(),
                    "xattrs and ACL metadata must retain the tree"
                );
                // SAFETY: fixture path/name remain valid NUL-terminated strings.
                assert_eq!(
                    unsafe { nix::libc::lremovexattr(xattr_path.as_ptr(), xattr_name.as_ptr()) },
                    0
                );
            } else {
                let error = std::io::Error::last_os_error();
                let code = error.raw_os_error();
                assert!(
                    code == Some(nix::libc::ENOTSUP)
                        || code == Some(nix::libc::EOPNOTSUPP)
                        || code == Some(nix::libc::EPERM),
                    "unexpected xattr fixture failure: {error}"
                );
            }
        }

        let socket = root.join("socket");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("socket fixture");
        assert!(
            prove_quarantine_tree_safe_at(&root, &mountinfo, 16, 1_024, 16, Duration::from_secs(1))
                .is_err(),
            "special files must retain the tree"
        );
        drop(listener);
        std::fs::remove_file(socket).expect("remove socket fixture");

        std::fs::write(
            &mountinfo,
            format!("1 0 0:1 / {} rw - tmpfs tmpfs rw\n", root.display()),
        )
        .expect("mounted-root fixture");
        assert!(
            prove_quarantine_tree_safe_at(&root, &mountinfo, 16, 1_024, 16, Duration::from_secs(1))
                .is_err(),
            "a mount at the quarantine must be retained"
        );
        std::fs::write(&mountinfo, "").expect("restore empty mount inventory");
        assert!(
            prove_quarantine_tree_safe_at(&root, &mountinfo, 1, 1_024, 16, Duration::from_secs(1))
                .is_err(),
            "tree work bounds must fail closed"
        );
    }

    #[test]
    fn quarantine_git_effects_have_no_force_prune_or_recursive_fallback() {
        let source = include_str!("git_worktree.rs");
        for (start, end) in [
            (
                "pub(crate) fn move_worktree_to_quarantine_non_force_locked",
                "/// Authenticate the complete post-move state",
            ),
            (
                "pub(crate) fn repair_moved_worktree_if_exact_locked",
                "/// Walk a quarantined tree without following symlinks",
            ),
            (
                "pub(crate) fn remove_worktree_non_force_locked",
                "pub(crate) fn delete_ref_compare_locked",
            ),
            (
                "fn settlement_missing_ref_removal_hook_directory",
                "fn settlement_reattach_hook_directory",
            ),
            (
                "fn settlement_reattach_hook_directory",
                "#[cfg(test)]\npub(crate) fn set_atomic_ref_pre_spawn_test_hook",
            ),
        ] {
            let body = source
                .split_once(start)
                .and_then(|(_, suffix)| suffix.split_once(end).map(|(body, _)| body))
                .expect("quarantine primitive source segment");
            assert!(!body.contains("--force"));
            assert!(!body.contains("worktree\", \"prune"));
            assert!(!body.contains("remove_dir_all"));
        }
    }

    fn short_process_limits() -> ProcessLimits {
        ProcessLimits {
            execution_timeout: Duration::from_secs(2),
            post_exit_drain_timeout: Duration::from_millis(150),
            ..ProcessLimits::default()
        }
    }

    #[cfg(target_os = "linux")]
    struct NoEscapeFixture {
        command: Command,
        attempted: PathBuf,
        survived: PathBuf,
    }

    #[cfg(target_os = "linux")]
    fn no_escape_fixture(root: &Path, mode: &str) -> NoEscapeFixture {
        std::fs::create_dir_all(root).expect("no-escape fixture directory");
        let attempted = root.join("attempted");
        let survived = root.join("survived");
        let mut command = Command::new("sh");
        command
            .env(
                "RSI_NO_ESCAPE_HELPER",
                std::env::current_exe().expect("current test executable"),
            )
            .env("RSI_NO_ESCAPE_ATTEMPTED", &attempted)
            .env("RSI_NO_ESCAPE_SURVIVED", &survived)
            .env("RSI_NO_ESCAPE_MODE", mode)
            .args([
                "-c",
                r#"
"$RSI_NO_ESCAPE_HELPER" --ignored --exact sandbox::git_worktree::tests::no_escape_syscall_helper --nocapture >/dev/null 2>&1 &
helper_pid=$!
while [ ! -e "$RSI_NO_ESCAPE_ATTEMPTED" ]; do
    if ! kill -0 "$helper_pid" 2>/dev/null; then
        exit 91
    fi
    sleep 0.01
done
case "$RSI_NO_ESCAPE_MODE" in
    success) exit 0 ;;
    error) exit 7 ;;
    timeout) while :; do sleep 1; done ;;
    *) exit 92 ;;
esac
"#,
            ]);
        NoEscapeFixture {
            command,
            attempted,
            survived,
        }
    }

    #[cfg(target_os = "linux")]
    fn process_state_and_start_time(pid: i32) -> Option<(u8, u64)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let suffix = stat.get(stat.rfind(") ")? + 2..)?;
        let mut fields = suffix.split_ascii_whitespace();
        let state = *fields.next()?.as_bytes().first()?;
        let start_time = fields.nth(18)?.parse().ok()?;
        Some((state, start_time))
    }

    #[cfg(target_os = "linux")]
    fn assert_no_escape_helper_reaped(fixture: &NoEscapeFixture) {
        assert!(
            fixture.attempted.exists(),
            "helper never proved syscall denial"
        );
        assert!(
            !fixture.survived.exists(),
            "helper observed a syscall or process-group escape"
        );
        let identity = std::fs::read_to_string(&fixture.attempted).expect("helper identity marker");
        let mut identity = identity.split_ascii_whitespace();
        let pid = identity
            .next()
            .expect("helper PID")
            .parse::<i32>()
            .expect("numeric helper PID");
        let expected_start_time = identity
            .next()
            .expect("helper start time")
            .parse::<u64>()
            .expect("numeric helper start time");
        assert!(
            identity.next().is_none(),
            "ambiguous helper identity marker"
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match process_state_and_start_time(pid) {
                None | Some((b'Z', _)) => return,
                Some((_, observed_start_time)) if observed_start_time != expected_start_time => {
                    return;
                }
                Some(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        panic!("exact no-escape helper remained live after bounded runner returned");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "subprocess-only no-escape syscall fixture"]
    fn no_escape_syscall_helper() {
        let Some(attempted) = std::env::var_os("RSI_NO_ESCAPE_ATTEMPTED") else {
            return;
        };
        let survived = PathBuf::from(
            std::env::var_os("RSI_NO_ESCAPE_SURVIVED").expect("survival marker path"),
        );
        // SAFETY: the helper operates only on itself and checks both syscall
        // results before publishing the containment marker.
        let (setsid_result, setsid_errno, setpgid_result, setpgid_errno) = unsafe {
            let setsid_result = nix::libc::setsid();
            let setsid_errno = std::io::Error::last_os_error().raw_os_error();
            let setpgid_result = nix::libc::setpgid(0, 0);
            let setpgid_errno = std::io::Error::last_os_error().raw_os_error();
            (setsid_result, setsid_errno, setpgid_result, setpgid_errno)
        };
        if setsid_result != -1
            || setsid_errno != Some(nix::libc::EPERM)
            || setpgid_result != -1
            || setpgid_errno != Some(nix::libc::EPERM)
        {
            std::fs::write(&survived, "syscall escape succeeded\n").expect("escape failure marker");
            std::thread::sleep(Duration::from_secs(5));
            return;
        }
        let pid = std::process::id() as i32;
        let (_, start_time) = process_state_and_start_time(pid).expect("helper process identity");
        std::fs::write(attempted, format!("{pid} {start_time}\n")).expect("attempt marker");
        std::thread::sleep(Duration::from_secs(5));
        std::fs::write(survived, "survived process-group cleanup\n").expect("survival marker");
    }

    #[test]
    fn reserved_closure_allocation_restarts_by_adopting_only_exact_identity() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir_all(&repository).expect("repository directory");
        std::fs::create_dir_all(&sandboxes).expect("sandbox directory");
        git(&repository, &["init", "-q"]);
        git(
            &repository,
            &["config", "user.email", "closure@example.test"],
        );
        git(&repository, &["config", "user.name", "Closure Test"]);
        std::fs::write(repository.join("tracked"), "base\n").expect("base file");
        git(&repository, &["add", "tracked"]);
        git(&repository, &["commit", "-qm", "base"]);
        let head = git(&repository, &["rev-parse", "HEAD"]);
        let session_id = Uuid::new_v4();
        let branch = format!("rsi/closure-source/{session_id}");
        let allocated = allocate(&sandboxes, session_id, &repository, &head, Some(&branch))
            .expect("first allocation effect");
        let adopted = adopt_reserved(&sandboxes, session_id, &repository, &head, Some(&branch))
            .expect("restart adoption")
            .expect("existing exact allocation");
        assert_eq!(adopted.root, allocated.root);
        assert_eq!(adopted.branch, allocated.branch);

        std::fs::write(adopted.root.join("untracked"), "drift\n").expect("dirty worktree");
        assert!(
            adopt_reserved(&sandboxes, session_id, &repository, &head, Some(&branch),).is_err(),
            "dirty orphan allocation must not be adopted"
        );
    }

    #[test]
    fn worktree_allocation_keeps_repository_hooks_enabled() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir_all(&sandboxes).expect("sandbox directory");
        let head = initialize_repository(&repository);
        let hook_dir = temp.path().join("hooks");
        std::fs::create_dir_all(&hook_dir).expect("hook directory");
        let marker = temp.path().join("post-checkout-ran");
        let hook = hook_dir.join("post-checkout");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\nprintf invoked > '{}'\n", marker.display()),
        )
        .expect("post-checkout hook");
        make_executable(&hook);
        git(
            &repository,
            &[
                "config",
                "core.hooksPath",
                hook_dir.to_str().expect("UTF-8 hook directory"),
            ],
        );

        let allocation = allocate(
            &sandboxes,
            Uuid::new_v4(),
            &repository,
            &head,
            Some("rsi/hook-test"),
        )
        .expect("worktree allocation");
        assert!(allocation.root.exists());
        assert!(
            marker.exists(),
            "worktree-add post-checkout hook was disabled"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn worktree_allocation_hooks_run_but_cannot_create_new_sessions() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let sandboxes = temp.path().join("sandboxes");
        std::fs::create_dir_all(&sandboxes).expect("sandbox directory");
        let head = initialize_repository(&repository);
        let hook_dir = temp.path().join("hooks");
        std::fs::create_dir_all(&hook_dir).expect("hook directory");
        let marker = temp.path().join("post-checkout-ran");
        let escaped = temp.path().join("post-checkout-escaped");
        let hook = hook_dir.join("post-checkout");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nif setsid sh -c \"printf escaped > '{}'\" 2>/dev/null; then exit 97; fi\nprintf invoked > '{}'\n",
                escaped.display(),
                marker.display()
            ),
        )
        .expect("post-checkout hook");
        make_executable(&hook);
        git(
            &repository,
            &[
                "config",
                "core.hooksPath",
                hook_dir.to_str().expect("UTF-8 hook directory"),
            ],
        );

        let allocation = allocate(
            &sandboxes,
            Uuid::new_v4(),
            &repository,
            &head,
            Some("rsi/no-escape-hook-test"),
        )
        .expect("worktree allocation with contained hook");
        assert!(allocation.root.exists());
        assert!(marker.exists(), "contained post-checkout hook did not run");
        assert!(!escaped.exists(), "post-checkout hook escaped its PGID");
    }

    #[test]
    fn direct_ref_observation_distinguishes_valid_dangling_and_missing_symrefs() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        initialize_repository(&repository);
        git(
            &repository,
            &[
                "symbolic-ref",
                "refs/heads/rsi/valid-alias",
                "refs/heads/main",
            ],
        );
        git(
            &repository,
            &[
                "symbolic-ref",
                "refs/heads/rsi/dangling-alias",
                "refs/heads/rsi/missing-target",
            ],
        );

        assert_eq!(
            observe_direct_ref_locked(&repository, "refs/heads/rsi/valid-alias")
                .expect("valid symref observation"),
            DirectRefObservation::Symbolic
        );
        assert_eq!(
            observe_direct_ref_locked(&repository, "refs/heads/rsi/dangling-alias")
                .expect("dangling symref observation"),
            DirectRefObservation::Symbolic
        );
        assert_eq!(
            observe_direct_ref_locked(&repository, "refs/heads/rsi/absent")
                .expect("missing ref observation"),
            DirectRefObservation::Missing
        );
    }

    #[test]
    fn direct_ref_empty_lookup_post_probe_catches_valid_and_dangling_symref_races() {
        for target in ["refs/heads/main", "refs/heads/rsi/missing-target"] {
            let temp = tempfile::tempdir().expect("temporary repository");
            let repository = temp.path().join("repository");
            initialize_repository(&repository);
            let hook_repository = repository.clone();
            set_direct_ref_empty_lookup_test_hook(move || {
                git(
                    &hook_repository,
                    &["symbolic-ref", "refs/heads/rsi/raced-alias", target],
                );
            });

            assert_eq!(
                observe_direct_ref_locked(&repository, "refs/heads/rsi/raced-alias")
                    .expect("raced symref observation"),
                DirectRefObservation::Symbolic
            );
        }
    }

    #[test]
    fn settlement_ref_observation_rejects_symrefs_and_never_deletes_their_targets() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let oid = initialize_repository(&repository);
        git(&repository, &["branch", "rsi/direct", &oid]);
        git(&repository, &["branch", "rsi/source", &oid]);
        git(
            &repository,
            &[
                "symbolic-ref",
                "refs/heads/rsi/alias",
                "refs/heads/rsi/direct",
            ],
        );

        with_repository_mutation(&repository, || {
            assert!(resolve_ref_locked(&repository, "refs/heads/rsi/alias").is_err());
            assert!(
                delete_source_ref_atomically_locked(
                    &repository,
                    "refs/heads/main",
                    &oid,
                    "refs/heads/rsi/alias",
                    &oid,
                )
                .is_err(),
                "a symbolic source must be refused before mutation"
            );
            assert!(
                delete_source_ref_atomically_locked(
                    &repository,
                    "refs/heads/rsi/alias",
                    &oid,
                    "refs/heads/rsi/source",
                    &oid,
                )
                .is_err(),
                "a symbolic target must be refused before mutation"
            );
            assert_eq!(
                resolve_ref_locked(&repository, "refs/heads/rsi/direct")?.as_deref(),
                Some(oid.as_str())
            );
            assert_eq!(
                resolve_ref_locked(&repository, "refs/heads/rsi/source")?.as_deref(),
                Some(oid.as_str())
            );
            Ok(())
        })
        .expect("symref checks remain local and closed");
        assert_eq!(
            git(&repository, &["symbolic-ref", "refs/heads/rsi/alias"]),
            "refs/heads/rsi/direct"
        );
    }

    #[test]
    fn atomic_ref_transaction_deletes_only_the_exact_source() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let oid = initialize_repository(&repository);
        git(&repository, &["branch", "rsi/source", &oid]);

        with_repository_mutation(&repository, || {
            delete_source_ref_atomically_locked(
                &repository,
                "refs/heads/main",
                &oid,
                "refs/heads/rsi/source",
                &oid,
            )?;
            assert_eq!(
                observe_direct_ref_locked(&repository, "refs/heads/main")?,
                DirectRefObservation::Commit(oid.clone())
            );
            assert_eq!(
                observe_direct_ref_locked(&repository, "refs/heads/rsi/source")?,
                DirectRefObservation::Missing
            );
            Ok(())
        })
        .expect("atomic source deletion");
    }

    #[test]
    fn atomic_ref_transaction_leaves_source_when_target_is_stale() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let stale_target = initialize_repository(&repository);
        git(&repository, &["branch", "rsi/source", &stale_target]);
        std::fs::write(repository.join("tracked"), "new target\n").expect("target change");
        git(&repository, &["commit", "-qam", "advance target"]);
        let current_target = git(&repository, &["rev-parse", "HEAD"]);

        with_repository_mutation(&repository, || {
            assert!(
                delete_source_ref_atomically_locked(
                    &repository,
                    "refs/heads/main",
                    &stale_target,
                    "refs/heads/rsi/source",
                    &stale_target,
                )
                .is_err()
            );
            assert_eq!(
                observe_direct_ref_locked(&repository, "refs/heads/main")?,
                DirectRefObservation::Commit(current_target.clone())
            );
            assert_eq!(
                observe_direct_ref_locked(&repository, "refs/heads/rsi/source")?,
                DirectRefObservation::Commit(stale_target.clone())
            );
            Ok(())
        })
        .expect("stale target fails without source mutation");
    }

    #[test]
    fn atomic_ref_transaction_closes_the_post_precheck_target_race() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let expected_target = initialize_repository(&repository);
        git(&repository, &["branch", "rsi/source", &expected_target]);
        let tree = git(&repository, &["rev-parse", "HEAD^{tree}"]);
        let raced_target = git(
            &repository,
            &[
                "commit-tree",
                &tree,
                "-p",
                &expected_target,
                "-m",
                "raced target",
            ],
        );
        let hook_repository = repository.clone();
        let hook_target = raced_target.clone();
        set_atomic_ref_pre_spawn_test_hook(move || {
            git(
                &hook_repository,
                &["update-ref", "refs/heads/main", &hook_target],
            );
        });

        with_repository_mutation(&repository, || {
            assert!(
                delete_source_ref_atomically_locked(
                    &repository,
                    "refs/heads/main",
                    &expected_target,
                    "refs/heads/rsi/source",
                    &expected_target,
                )
                .is_err(),
                "transaction must re-verify the target after its direct-ref precheck"
            );
            assert_eq!(
                observe_direct_ref_locked(&repository, "refs/heads/main")?,
                DirectRefObservation::Commit(raced_target.clone())
            );
            assert_eq!(
                observe_direct_ref_locked(&repository, "refs/heads/rsi/source")?,
                DirectRefObservation::Commit(expected_target.clone())
            );
            Ok(())
        })
        .expect("raced target leaves source untouched");
    }

    #[test]
    fn atomic_ref_transaction_refuses_equal_oid_target_to_source_symref_race() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let oid = initialize_repository(&repository);
        git(&repository, &["branch", "rsi/source", &oid]);
        let hook_repository = repository.clone();
        set_atomic_ref_pre_spawn_test_hook(move || {
            git(
                &hook_repository,
                &["symbolic-ref", "refs/heads/main", "refs/heads/rsi/source"],
            );
        });

        with_repository_mutation(&repository, || {
            assert!(
                delete_source_ref_atomically_locked(
                    &repository,
                    "refs/heads/main",
                    &oid,
                    "refs/heads/rsi/source",
                    &oid,
                )
                .is_err(),
                "prepared-phase direct-ref proof must reject an equal-OID symref"
            );
            assert_eq!(
                observe_direct_ref_locked(&repository, "refs/heads/main")?,
                DirectRefObservation::Symbolic,
                "the raced target symref must not be clobbered"
            );
            assert_eq!(
                observe_direct_ref_locked(&repository, "refs/heads/rsi/source")?,
                DirectRefObservation::Commit(oid.clone()),
                "the source must remain reachable after transaction abort"
            );
            Ok(())
        })
        .expect("equal-OID symref race aborts without mutation");
        assert_eq!(
            git(&repository, &["symbolic-ref", "refs/heads/main"]),
            "refs/heads/rsi/source"
        );
        assert_eq!(
            git(&repository, &["rev-parse", "refs/heads/main^{commit}"]),
            oid,
            "the preserved target symref must not dangle"
        );
    }

    #[test]
    fn atomic_ref_transaction_disables_reference_transaction_hooks() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let oid = initialize_repository(&repository);
        git(&repository, &["branch", "rsi/source", &oid]);
        let hook_dir = temp.path().join("hooks");
        std::fs::create_dir_all(&hook_dir).expect("hook directory");
        let marker = temp.path().join("reference-transaction-ran");
        let hook = hook_dir.join("reference-transaction");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nprintf invoked > '{}'\ngit -c core.hooksPath=/dev/null update-ref refs/heads/rsi/hook-rewrite {}\nwhile :; do sleep 1; done\n",
                marker.display(),
                oid
            ),
        )
        .expect("reference transaction hook");
        make_executable(&hook);
        git(
            &repository,
            &[
                "config",
                "core.hooksPath",
                hook_dir.to_str().expect("UTF-8 hook directory"),
            ],
        );

        let started = Instant::now();
        with_repository_mutation(&repository, || {
            delete_source_ref_atomically_locked(
                &repository,
                "refs/heads/main",
                &oid,
                "refs/heads/rsi/source",
                &oid,
            )
        })
        .expect("hook-disabled reference transaction");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!marker.exists(), "reference-transaction hook executed");
        assert_eq!(
            observe_direct_ref_locked(&repository, "refs/heads/rsi/hook-rewrite")
                .expect("observe forbidden hook rewrite"),
            DirectRefObservation::Missing
        );
    }

    #[test]
    fn settlement_proofs_ignore_replace_refs() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        std::fs::create_dir_all(&repository).expect("repository directory");
        git(&repository, &["init", "-q", "-b", "main"]);
        git(
            &repository,
            &["config", "user.email", "replace@example.test"],
        );
        git(&repository, &["config", "user.name", "Replace Test"]);
        std::fs::write(repository.join("tracked"), "base\n").expect("base file");
        git(&repository, &["add", "tracked"]);
        git(&repository, &["commit", "-qm", "source"]);
        let source = git(&repository, &["rev-parse", "HEAD"]);
        let tree = git(&repository, &["rev-parse", "HEAD^{tree}"]);
        let unrelated = git(&repository, &["commit-tree", &tree, "-m", "unrelated"]);
        let forged_descendant = git(
            &repository,
            &["commit-tree", &tree, "-p", &source, "-m", "forged"],
        );
        git(&repository, &["update-ref", "refs/heads/main", &unrelated]);
        git(
            &repository,
            &["update-ref", "refs/heads/rsi/source", &source],
        );
        git(&repository, &["replace", &unrelated, &forged_descendant]);
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(["merge-base", "--is-ancestor", &source, &unrelated])
                .status()
                .expect("run forged replace-ref control")
                .success(),
            "control must prove the replace ref would forge ancestry without the hardening"
        );

        with_repository_mutation(&repository, || {
            let target = observe_repository_target_locked(&repository)?;
            assert_eq!(target.target_oid, unrelated);
            assert!(!is_ancestor_locked(
                &repository,
                &source,
                &target.target_oid
            )?);
            Ok(())
        })
        .expect("replace refs cannot manufacture settlement ancestry");
    }

    #[test]
    fn settlement_proofs_ignore_legacy_grafts_with_a_positive_control() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let source = initialize_repository(&repository);
        let tree = git(&repository, &["rev-parse", "HEAD^{tree}"]);
        let unrelated = git(&repository, &["commit-tree", &tree, "-m", "unrelated"]);
        git(&repository, &["update-ref", "refs/heads/main", &unrelated]);
        let grafts = repository.join(".git/info/grafts");
        std::fs::write(&grafts, format!("{unrelated} {source}\n")).expect("legacy graft");

        let positive_control = Command::new("git")
            .env_remove("GIT_NO_REPLACE_OBJECTS")
            .env_remove("GIT_GRAFT_FILE")
            .arg("-C")
            .arg(&repository)
            .args(["merge-base", "--is-ancestor", &source, &unrelated])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run legacy graft control");
        assert!(
            positive_control.success(),
            "control must prove the graft would forge ancestry without hardening"
        );

        with_repository_mutation(&repository, || {
            assert!(!is_ancestor_locked(&repository, &source, &unrelated)?);
            Ok(())
        })
        .expect("legacy grafts cannot manufacture settlement ancestry");
    }

    #[test]
    fn settlement_cleanliness_includes_ignored_content() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        std::fs::create_dir_all(&repository).expect("repository directory");
        git(&repository, &["init", "-q", "-b", "main"]);
        git(
            &repository,
            &["config", "user.email", "ignored@example.test"],
        );
        git(&repository, &["config", "user.name", "Ignored Test"]);
        std::fs::write(repository.join(".gitignore"), "ignored.bin\n").expect("ignore file");
        git(&repository, &["add", ".gitignore"]);
        git(&repository, &["commit", "-qm", "base"]);
        std::fs::write(repository.join("ignored.bin"), "retained data\n").expect("ignored data");

        with_repository_mutation(&repository, || {
            let observation = observe_worktree_locked(&repository, &repository)?;
            assert!(!observation.clean, "ignored files are settlement data");
            Ok(())
        })
        .expect("observe ignored content");
    }

    #[test]
    fn settlement_cleanliness_rejects_assume_unchanged_content() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        std::fs::create_dir_all(&repository).expect("repository directory");
        git(&repository, &["init", "-q", "-b", "main"]);
        git(
            &repository,
            &["config", "user.email", "visibility@example.test"],
        );
        git(&repository, &["config", "user.name", "Visibility Test"]);
        std::fs::write(repository.join("tracked"), "base\n").expect("tracked file");
        git(&repository, &["add", "tracked"]);
        git(&repository, &["commit", "-qm", "base"]);
        git(
            &repository,
            &["update-index", "--assume-unchanged", "tracked"],
        );
        std::fs::write(repository.join("tracked"), "unique retained bytes\n")
            .expect("hidden tracked modification");

        with_repository_mutation(&repository, || {
            let observation = observe_worktree_locked(&repository, &repository)?;
            assert!(!observation.clean, "index visibility flags retain the root");
            Ok(())
        })
        .expect("observe assume-unchanged content");
        assert!(repository.join("tracked").exists());
    }

    #[test]
    fn cleanliness_observation_bypasses_a_lying_fsmonitor() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let head = initialize_repository(&repository);
        let monitor = temp.path().join("lying-fsmonitor");
        std::fs::write(&monitor, "#!/bin/sh\nprintf 'rsi-token\\0'\n").expect("lying fsmonitor");
        make_executable(&monitor);
        git(
            &repository,
            &[
                "config",
                "core.fsmonitor",
                monitor.to_str().expect("UTF-8 fsmonitor path"),
            ],
        );
        git(&repository, &["config", "core.fsmonitorHookVersion", "2"]);
        assert!(
            git(
                &repository,
                &[
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--untracked-files=all",
                    "--ignore-submodules=none",
                ],
            )
            .is_empty(),
            "clean baseline"
        );
        git(
            &repository,
            &["update-index", "--fsmonitor-valid", "tracked"],
        );
        std::fs::write(repository.join("tracked"), "bytes hidden by fsmonitor\n")
            .expect("hidden modification");
        assert!(
            git(
                &repository,
                &[
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--untracked-files=all",
                    "--ignore-submodules=none",
                ],
            )
            .is_empty(),
            "positive control must prove configured fsmonitor can lie"
        );

        let (clean, observed_head) =
            observe_clean_head_bounded(&repository).expect("bounded fork observation");
        assert!(!clean, "child-fork observation must bypass fsmonitor");
        assert_eq!(observed_head, head);
        with_repository_mutation(&repository, || {
            let observation = observe_worktree_locked(&repository, &repository)?;
            assert!(!observation.clean, "settlement must bypass fsmonitor");
            Ok(())
        })
        .expect("fsmonitor-independent settlement observation");
    }

    #[test]
    fn worktree_observation_accepts_a_repo_scale_index_stream() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        initialize_repository(&repository);
        let bulk = repository.join("bulk");
        std::fs::create_dir_all(&bulk).expect("bulk index directory");
        let mut minimum_index_stream_bytes = 0_usize;
        for index in 0..1_200 {
            let name = format!("entry-{index:04}-{}.txt", "x".repeat(72));
            minimum_index_stream_bytes += 2 + "bulk/".len() + name.len() + 1;
            std::fs::write(bulk.join(name), "indexed\n").expect("bulk indexed file");
        }
        assert!(minimum_index_stream_bytes > MAX_GIT_OUTPUT_BYTES);
        git(&repository, &["add", "bulk"]);
        git(&repository, &["commit", "-qm", "large index"]);

        with_repository_mutation(&repository, || {
            let observation = observe_worktree_locked(&repository, &repository)?;
            assert!(observation.clean);
            assert!(observation.clean_state_digest.starts_with("sha256:"));
            Ok(())
        })
        .expect("repo-scale index stream stays within the record work budget");
    }

    #[test]
    fn worktree_list_stream_accepts_repository_output_above_capture_limit() {
        let temp = tempfile::tempdir().expect("temporary repository");
        let repository = temp.path().join("repository");
        let oid = initialize_repository(&repository);
        let metadata_root = repository.join(".git/worktrees");
        std::fs::create_dir_all(&metadata_root).expect("worktree metadata root");
        let mut minimum_stream_bytes = 0_usize;
        let mut expected_root = None;
        for index in 0..512 {
            let metadata = metadata_root.join(format!("bulk-{index:04}"));
            std::fs::create_dir_all(&metadata).expect("worktree metadata directory");
            let root = temp
                .path()
                .join(format!("virtual-worktree-{index:04}-{}", "x".repeat(120)));
            std::fs::write(
                metadata.join("gitdir"),
                format!("{}/.git\n", root.display()),
            )
            .expect("worktree gitdir");
            std::fs::write(metadata.join("commondir"), "../..\n").expect("worktree commondir");
            std::fs::write(metadata.join("HEAD"), format!("{oid}\n")).expect("worktree HEAD");
            minimum_stream_bytes += b"worktree ".len()
                + root.as_os_str().as_bytes().len()
                + 1
                + b"HEAD ".len()
                + oid.len()
                + 1
                + b"detached\0\0".len();
            expected_root = Some(root);
        }
        assert!(minimum_stream_bytes > MAX_GIT_OUTPUT_BYTES);

        let entries = list_worktrees_locked(&repository)
            .expect("large worktree registry must stream above capture limit");
        assert_eq!(entries.len(), 513);
        assert!(entries.iter().any(|entry| {
            entry.root.as_path()
                == expected_root
                    .as_ref()
                    .expect("expected final worktree root")
                    .as_path()
        }));
    }

    #[test]
    fn worktree_list_parser_enforces_its_entry_cap() {
        let mut parser = WorktreeListParser::new(1);
        parser
            .consume(b"worktree /first\0")
            .expect("first worktree path");
        parser.consume(b"\0").expect("first worktree separator");
        parser
            .consume(b"worktree /second\0")
            .expect("second worktree path");
        let error = parser
            .consume(b"\0")
            .expect_err("second worktree exceeds configured entry cap");
        assert!(error.to_string().contains("entry count exceeded bound"));
    }

    #[test]
    fn bounded_capture_accepts_each_stream_at_the_exact_cap() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "(head -c 65536 /dev/zero) & (head -c 65536 /dev/zero >&2) & wait",
        ]);
        let output = capture_bounded(command, "exact bounded output").expect("exact cap accepted");
        assert_eq!(output.stdout.len(), MAX_GIT_OUTPUT_BYTES);
        assert_eq!(output.stderr.len(), MAX_GIT_OUTPUT_BYTES);
    }

    #[test]
    fn bounded_capture_rejects_either_stream_above_the_cap() {
        for script in ["head -c 65537 /dev/zero", "head -c 65537 /dev/zero >&2"] {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            let error = capture_bounded(command, "above bounded output")
                .expect_err("one extra byte must be rejected");
            assert!(error.to_string().contains("output exceeded bound"));
        }
    }

    #[test]
    fn bounded_record_stream_accepts_many_small_records_above_capture_limit() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "i=0; while [ $i -lt 10000 ]; do printf 'record\\n'; i=$((i+1)); done",
        ]);
        let mut records = 0_usize;
        let mut bytes = 0_usize;
        run_bounded_records(
            command,
            None,
            b'\n',
            true,
            "large small-record stream",
            &mut |record| {
                records += 1;
                bytes += record.len();
                Ok(())
            },
        )
        .expect("record streaming has a separate bounded work budget");
        assert_eq!(records, 10_000);
        assert!(bytes > MAX_GIT_OUTPUT_BYTES);
    }

    #[test]
    fn bounded_record_stream_refuses_byte_and_record_work_budget_overruns() {
        let mut byte_command = Command::new("sh");
        byte_command.args([
            "-c",
            "i=0; while [ $i -lt 200 ]; do printf '0123456789\\n'; i=$((i+1)); done",
        ]);
        let byte_error = run_bounded_records_with_limits(
            &mut byte_command,
            None,
            b'\n',
            true,
            "over-budget record bytes",
            ProcessLimits {
                max_stdout_bytes: 1_024,
                ..short_process_limits()
            },
            &mut |_| Ok(()),
        )
        .expect_err("record stream byte budget must be enforced");
        assert!(byte_error.to_string().contains("output exceeded bound"));

        let mut record_command = Command::new("sh");
        record_command.args([
            "-c",
            "i=0; while [ $i -lt 11 ]; do printf 'x\\n'; i=$((i+1)); done",
        ]);
        let record_error = run_bounded_records_with_limits(
            &mut record_command,
            None,
            b'\n',
            true,
            "over-budget record count",
            ProcessLimits {
                max_records: 10,
                ..short_process_limits()
            },
            &mut |_| Ok(()),
        )
        .expect_err("record stream count budget must be enforced");
        assert!(
            record_error
                .to_string()
                .contains("record count exceeded bound")
        );
    }

    #[test]
    fn bounded_record_modes_reject_newline_and_nul_free_overbound_records() {
        for (label, delimiter) in [("newline record", b'\n'), ("NUL record", b'\0')] {
            let mut command = Command::new("sh");
            command.args(["-c", "printf '%065d' 0"]);
            let limits = ProcessLimits {
                max_stdout_bytes: 1024,
                max_record_bytes: 64,
                ..short_process_limits()
            };
            let error = run_bounded_records_with_limits(
                &mut command,
                None,
                delimiter,
                true,
                label,
                limits,
                &mut |_| Ok(()),
            )
            .expect_err("an unterminated overbound record must be rejected while streaming");
            assert!(error.to_string().contains("record exceeded bound"));
        }
    }

    #[test]
    fn bounded_capture_kills_a_hung_direct_child_at_the_execution_deadline() {
        let temp = tempfile::tempdir().expect("PID marker directory");
        let pid_marker = temp.path().join("direct-child-pid");
        let mut command = Command::new("sh");
        command.env("PID_MARKER", &pid_marker).args([
            "-c",
            "printf '%s\\n' \"$$\" > \"$PID_MARKER\"; while :; do sleep 1; done",
        ]);
        let limits = ProcessLimits {
            execution_timeout: Duration::from_millis(150),
            post_exit_drain_timeout: Duration::from_millis(150),
            ..ProcessLimits::default()
        };
        let started = Instant::now();
        let error = capture_bounded_with_limits(&mut command, "hung direct child", None, limits)
            .expect_err("hung child must hit the overall execution deadline");
        assert!(error.to_string().contains("execution timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = std::fs::read_to_string(&pid_marker)
            .expect("direct child PID marker")
            .trim()
            .parse::<i32>()
            .expect("numeric direct child PID");
        assert_eq!(
            nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(pid),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ),
            Err(nix::errno::Errno::ECHILD),
            "bounded runner must reap its killed direct child before returning"
        );
    }

    #[test]
    fn bounded_capture_kills_descendants_that_inherit_both_pipes() {
        let temp = tempfile::tempdir().expect("marker directory");
        let marker = temp.path().join("descendant-finished");
        let mut command = Command::new("sh");
        command.env("MARKER", &marker).args([
            "-c",
            "(i=0; while [ $i -lt 200000 ]; do printf 0123456789abcdef; printf 0123456789abcdef >&2; i=$((i+1)); done; printf done > \"$MARKER\") & wait",
        ]);
        let error = capture_bounded(command, "descendant bounded output")
            .expect_err("process group must stop at the cap");
        assert!(error.to_string().contains("output exceeded bound"));
        assert!(!marker.exists(), "descendant survived process-group kill");
    }

    #[test]
    fn bounded_capture_kills_quiet_descendant_pipe_holders_after_parent_exit() {
        let temp = tempfile::tempdir().expect("marker directory");
        let marker = temp.path().join("quiet-descendant-finished");
        let mut command = Command::new("sh");
        command
            .env("MARKER", &marker)
            .args(["-c", "(sleep 5; printf done > \"$MARKER\") & exit 0"]);
        let started = Instant::now();
        let output = capture_bounded(command, "quiet descendant output")
            .expect("quiet descendant group is closed after direct exit");
        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!marker.exists(), "quiet descendant survived group closure");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_runner_contains_setsids_across_success_error_and_timeout() {
        let temp = tempfile::tempdir().expect("no-escape fixture directory");

        let mut success = no_escape_fixture(&temp.path().join("success"), "success");
        let started = Instant::now();
        let output = capture_bounded_with_limits(
            &mut success.command,
            "successful no-escape command",
            None,
            short_process_limits(),
        )
        .expect("successful command stays bounded");
        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_no_escape_helper_reaped(&success);

        let mut error = no_escape_fixture(&temp.path().join("error"), "error");
        let error_result = run_bounded_records_with_limits(
            &mut error.command,
            None,
            b'\n',
            true,
            "failed no-escape command",
            short_process_limits(),
            &mut |_| Ok(()),
        )
        .expect_err("nonzero command status remains an error");
        assert!(
            error_result
                .to_string()
                .contains("Git failed no-escape command failed")
        );
        assert_no_escape_helper_reaped(&error);

        let mut timeout = no_escape_fixture(&temp.path().join("timeout"), "timeout");
        let timeout_result = capture_bounded_with_limits(
            &mut timeout.command,
            "timed-out no-escape command",
            None,
            ProcessLimits {
                execution_timeout: Duration::from_millis(250),
                post_exit_drain_timeout: Duration::from_millis(250),
                ..ProcessLimits::default()
            },
        )
        .expect_err("hung command reaches its execution deadline");
        assert!(timeout_result.to_string().contains("execution timed out"));
        assert_no_escape_helper_reaped(&timeout);
    }
}
