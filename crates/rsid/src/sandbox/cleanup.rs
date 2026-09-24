//! Fail-closed sandbox-cleanup admission.
//!
//! D00 deliberately has no positive cleanup authorization. This module is the
//! single read-only boundary used by lifecycle, launch-abort, and startup
//! reconciliation callers. A real sandbox candidate is always blocked; only a
//! verified non-sandbox row or a complete `Purged` tombstone needs no cleanup.

use rsi_common::types::{SandboxCleanupState, SandboxKind, Session};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(test)]
use std::process::Command;
use uuid::Uuid;

/// A cleanup request after the durable sandbox tuple has been interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupCandidate {
    /// A row that provably has no destructive target.
    NoTarget { session_id: Uuid },
    /// A complete, row-backed sandbox allocation.
    Row(RowCleanupCandidate),
    /// A partially populated or otherwise contradictory row.
    InconsistentRow {
        session_id: Uuid,
        detail: &'static str,
    },
    /// Startup attributed a path to a row but could not re-read that row.
    RowReadFailure { session_id: Uuid, root: PathBuf },
    /// Allocation succeeded, but launch aborted before durable ownership.
    LaunchAbort {
        session_id: Uuid,
        root: PathBuf,
        branch: Option<String>,
        origin: PathBuf,
    },
    /// Startup found only a UUID path, with no typed durable owner.
    PathOnly {
        attributed_session_id: Option<Uuid>,
        root: PathBuf,
    },
}

/// Complete durable identity for a row-backed sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowCleanupCandidate {
    pub session_id: Uuid,
    pub kind: SandboxKind,
    pub root: PathBuf,
    pub branch: String,
    pub origin: PathBuf,
    pub cleanup_state: SandboxCleanupState,
}

/// The only two possible D00 outcomes. There is intentionally no eligible or
/// authorized variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupDecision {
    NoCleanupRequired,
    Blocked(CleanupBlockedReason),
}

/// Stable, structured reasons for retaining a cleanup candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupBlockedReason {
    #[cfg(test)]
    DirtyWorktree,
    UnreadableWorktree,
    #[cfg(test)]
    MissingSourceIdentity,
    #[cfg(test)]
    ChangedSourceIdentity,
    #[cfg(test)]
    UnknownTarget,
    #[cfg(test)]
    MismatchedTarget,
    #[cfg(test)]
    UnconfiguredTarget,
    MissingIndependentlyVerifiedProof,
    #[cfg(test)]
    MismatchedProof,
    #[cfg(test)]
    MissingCherryPickMapping,
    #[cfg(test)]
    FailedPostHeadVerification,
    SharedLiveOwnership,
    OwnershipReadFailure,
    ConcurrentDrift,
    InconsistentSandboxColumns,
    RowReadFailure,
    LaunchAbortWithoutOwner,
    PathOnlyAttribution,
}

impl CleanupBlockedReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            #[cfg(test)]
            Self::DirtyWorktree => "dirty_worktree",
            Self::UnreadableWorktree => "unreadable_worktree",
            #[cfg(test)]
            Self::MissingSourceIdentity => "missing_source_identity",
            #[cfg(test)]
            Self::ChangedSourceIdentity => "changed_source_identity",
            #[cfg(test)]
            Self::UnknownTarget => "unknown_target",
            #[cfg(test)]
            Self::MismatchedTarget => "mismatched_target",
            #[cfg(test)]
            Self::UnconfiguredTarget => "unconfigured_target",
            Self::MissingIndependentlyVerifiedProof => "missing_independently_verified_proof",
            #[cfg(test)]
            Self::MismatchedProof => "mismatched_proof",
            #[cfg(test)]
            Self::MissingCherryPickMapping => "missing_cherry_pick_mapping",
            #[cfg(test)]
            Self::FailedPostHeadVerification => "failed_post_head_verification",
            Self::SharedLiveOwnership => "shared_live_ownership",
            Self::OwnershipReadFailure => "ownership_read_failure",
            Self::ConcurrentDrift => "concurrent_drift",
            Self::InconsistentSandboxColumns => "inconsistent_sandbox_columns",
            Self::RowReadFailure => "row_read_failure",
            Self::LaunchAbortWithoutOwner => "launch_abort_without_owner",
            Self::PathOnlyAttribution => "path_only_attribution",
        }
    }
}

/// Result of a read-only shared-owner lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipObservation {
    Exclusive,
    Shared,
    Unreadable,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct IdentityObservation {
    canonical_root: PathBuf,
    source_head: String,
    registered_head: String,
    registered_branch: String,
}

#[cfg(test)]
trait IdentityObserver {
    fn observe(
        &self,
        candidate: &RowCleanupCandidate,
    ) -> std::result::Result<IdentityObservation, CleanupBlockedReason>;

    /// Deterministic seam for tests that need drift between observations.
    fn between_observations(&self) {}
}

#[cfg(test)]
struct SystemIdentityObserver;

#[cfg(test)]
impl IdentityObserver for SystemIdentityObserver {
    fn observe(
        &self,
        candidate: &RowCleanupCandidate,
    ) -> std::result::Result<IdentityObservation, CleanupBlockedReason> {
        observe_system_identity(candidate)
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
enum TargetDiagnostic {
    Unknown,
    Mismatched,
    Unconfigured,
    SyntheticMatch,
}

#[cfg(test)]
impl TargetDiagnostic {
    const ALL: [Self; 4] = [
        Self::Unknown,
        Self::Mismatched,
        Self::Unconfigured,
        Self::SyntheticMatch,
    ];

    const fn canonical(self) -> Self {
        Self::ALL[self as usize]
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
enum ProofDiagnostic {
    Missing,
    Mismatched,
    SyntheticMatch,
}

#[cfg(test)]
impl ProofDiagnostic {
    const ALL: [Self; 3] = [Self::Missing, Self::Mismatched, Self::SyntheticMatch];

    const fn canonical(self) -> Self {
        Self::ALL[self as usize]
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
enum MappingDiagnostic {
    Missing,
    SyntheticMatch,
}

#[cfg(test)]
impl MappingDiagnostic {
    const ALL: [Self; 2] = [Self::Missing, Self::SyntheticMatch];

    const fn canonical(self) -> Self {
        Self::ALL[self as usize]
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
enum VerificationDiagnostic {
    Failed,
    SyntheticPass,
}

#[cfg(test)]
impl VerificationDiagnostic {
    const ALL: [Self; 2] = [Self::Failed, Self::SyntheticPass];

    const fn canonical(self) -> Self {
        Self::ALL[self as usize]
    }
}

/// Diagnostic-only inputs. They can refine a blocked reason but can never
/// construct cleanup authorization.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
struct D00Diagnostics {
    target: TargetDiagnostic,
    proof: ProofDiagnostic,
    mapping: MappingDiagnostic,
    verification: VerificationDiagnostic,
}

#[cfg(test)]
impl D00Diagnostics {
    const fn production() -> Self {
        Self {
            target: TargetDiagnostic::Unknown,
            proof: ProofDiagnostic::Missing,
            mapping: MappingDiagnostic::Missing,
            verification: VerificationDiagnostic::Failed,
        }
    }
}

/// Interpret a persisted session tuple without reading or mutating Git,
/// filesystem, or store state.
pub fn candidate_from_session(session: &Session) -> CleanupCandidate {
    let session_id = session.id;
    match (
        session.sandbox_kind,
        session.sandbox_root.as_ref(),
        session.sandbox_branch.as_ref(),
        session.sandbox_cleanup_state,
    ) {
        (None | Some(SandboxKind::None), None, None, None)
        | (Some(SandboxKind::GitWorktree), None, None, Some(SandboxCleanupState::Purged)) => {
            CleanupCandidate::NoTarget { session_id }
        }
        (
            Some(SandboxKind::GitWorktree),
            Some(root),
            Some(branch),
            Some(cleanup_state @ (SandboxCleanupState::Live | SandboxCleanupState::Failed)),
        ) => CleanupCandidate::Row(RowCleanupCandidate {
            session_id,
            kind: SandboxKind::GitWorktree,
            root: root.clone(),
            branch: branch.clone(),
            origin: session.working_dir.clone(),
            cleanup_state,
        }),
        _ => CleanupCandidate::InconsistentRow {
            session_id,
            detail: "sandbox tuple is partial, tombstoned inconsistently, or unsupported",
        },
    }
}

/// Classify a candidate after two read-only ownership observations.
///
/// Production classification deliberately stops before invoking Git or
/// touching the candidate path. Generic lifecycle code has no positive
/// cleanup authority, so external identity diagnostics cannot improve its
/// decision and could themselves trigger configured Git helpers.
pub fn classify_candidate(
    candidate: &CleanupCandidate,
    initial_ownership: OwnershipObservation,
    final_ownership: OwnershipObservation,
) -> CleanupDecision {
    match classify_without_external_observation(candidate, initial_ownership, final_ownership) {
        Ok(_) => CleanupDecision::Blocked(CleanupBlockedReason::MissingIndependentlyVerifiedProof),
        Err(decision) => decision,
    }
}

fn classify_without_external_observation(
    candidate: &CleanupCandidate,
    initial_ownership: OwnershipObservation,
    final_ownership: OwnershipObservation,
) -> std::result::Result<&RowCleanupCandidate, CleanupDecision> {
    let blocked = |reason| Err(CleanupDecision::Blocked(reason));
    let row = match candidate {
        CleanupCandidate::NoTarget { .. } => return Err(CleanupDecision::NoCleanupRequired),
        CleanupCandidate::InconsistentRow { .. } => {
            return blocked(CleanupBlockedReason::InconsistentSandboxColumns);
        }
        CleanupCandidate::RowReadFailure { .. } => {
            return blocked(CleanupBlockedReason::RowReadFailure);
        }
        CleanupCandidate::LaunchAbort { .. } => {
            return blocked(CleanupBlockedReason::LaunchAbortWithoutOwner);
        }
        CleanupCandidate::PathOnly { .. } => {
            return blocked(CleanupBlockedReason::PathOnlyAttribution);
        }
        CleanupCandidate::Row(row) => row,
    };

    if matches!(initial_ownership, OwnershipObservation::Unreadable)
        || matches!(final_ownership, OwnershipObservation::Unreadable)
    {
        return blocked(CleanupBlockedReason::OwnershipReadFailure);
    }
    if initial_ownership != final_ownership {
        return blocked(CleanupBlockedReason::ConcurrentDrift);
    }
    if matches!(initial_ownership, OwnershipObservation::Shared)
        || matches!(final_ownership, OwnershipObservation::Shared)
    {
        return blocked(CleanupBlockedReason::SharedLiveOwnership);
    }
    if row.kind != SandboxKind::GitWorktree
        || !matches!(
            row.cleanup_state,
            SandboxCleanupState::Live | SandboxCleanupState::Failed
        )
        || row.root.as_os_str().is_empty()
        || row.origin.as_os_str().is_empty()
        || row.branch.trim().is_empty()
    {
        return blocked(CleanupBlockedReason::InconsistentSandboxColumns);
    }

    Ok(row)
}

#[cfg(test)]
fn classify_with(
    candidate: &CleanupCandidate,
    initial_ownership: OwnershipObservation,
    final_ownership: OwnershipObservation,
    observer: &dyn IdentityObserver,
    diagnostics: D00Diagnostics,
) -> CleanupDecision {
    let row = match classify_without_external_observation(
        candidate,
        initial_ownership,
        final_ownership,
    ) {
        Ok(row) => row,
        Err(decision) => return decision,
    };

    let initial_identity = match observer.observe(row) {
        Ok(identity) => identity,
        Err(reason) => return CleanupDecision::Blocked(reason),
    };
    observer.between_observations();
    let Ok(final_identity) = observer.observe(row) else {
        return CleanupDecision::Blocked(CleanupBlockedReason::ConcurrentDrift);
    };
    if initial_identity != final_identity {
        return CleanupDecision::Blocked(CleanupBlockedReason::ConcurrentDrift);
    }

    // Proof is checked first because D00's empty positive set is the ultimate
    // reason a clean, readable, stable candidate remains retained.
    match diagnostics.proof.canonical() {
        ProofDiagnostic::Missing => {
            return CleanupDecision::Blocked(
                CleanupBlockedReason::MissingIndependentlyVerifiedProof,
            );
        }
        ProofDiagnostic::Mismatched => {
            return CleanupDecision::Blocked(CleanupBlockedReason::MismatchedProof);
        }
        ProofDiagnostic::SyntheticMatch => {}
    }
    match diagnostics.target.canonical() {
        TargetDiagnostic::Unknown => {
            return CleanupDecision::Blocked(CleanupBlockedReason::UnknownTarget);
        }
        TargetDiagnostic::Mismatched => {
            return CleanupDecision::Blocked(CleanupBlockedReason::MismatchedTarget);
        }
        TargetDiagnostic::Unconfigured => {
            return CleanupDecision::Blocked(CleanupBlockedReason::UnconfiguredTarget);
        }
        TargetDiagnostic::SyntheticMatch => {}
    }
    if matches!(diagnostics.mapping.canonical(), MappingDiagnostic::Missing) {
        return CleanupDecision::Blocked(CleanupBlockedReason::MissingCherryPickMapping);
    }
    if matches!(
        diagnostics.verification.canonical(),
        VerificationDiagnostic::Failed
    ) {
        return CleanupDecision::Blocked(CleanupBlockedReason::FailedPostHeadVerification);
    }

    // Synthetic diagnostics can exercise every negative dimension, but never
    // turn into authority.
    CleanupDecision::Blocked(CleanupBlockedReason::MissingIndependentlyVerifiedProof)
}

#[cfg(test)]
fn observe_system_identity(
    candidate: &RowCleanupCandidate,
) -> std::result::Result<IdentityObservation, CleanupBlockedReason> {
    if !candidate.root.exists() {
        return Err(CleanupBlockedReason::MissingSourceIdentity);
    }
    let canonical_root = candidate
        .root
        .canonicalize()
        .map_err(|_| CleanupBlockedReason::UnreadableWorktree)?;

    let source_ref = format!("refs/heads/{}", candidate.branch);
    let source_head = git_text(
        &candidate.origin,
        &["rev-parse", "--verify", &format!("{source_ref}^{{commit}}")],
        CleanupBlockedReason::MissingSourceIdentity,
    )?;
    let porcelain = git_text(
        &candidate.origin,
        &["worktree", "list", "--porcelain"],
        CleanupBlockedReason::UnreadableWorktree,
    )?;
    let registration = parse_worktree_registration(&porcelain, &canonical_root)
        .ok_or(CleanupBlockedReason::MissingSourceIdentity)?;
    if registration.branch != source_ref || registration.head != source_head {
        return Err(CleanupBlockedReason::ChangedSourceIdentity);
    }

    let status = git_output(
        &candidate.root,
        &["status", "--porcelain", "--untracked-files=all"],
    )?;
    if !status.stdout.is_empty() {
        return Err(CleanupBlockedReason::DirtyWorktree);
    }

    Ok(IdentityObservation {
        canonical_root,
        source_head,
        registered_head: registration.head,
        registered_branch: registration.branch,
    })
}

#[cfg(test)]
struct GitOutput {
    stdout: Vec<u8>,
}

#[cfg(test)]
fn git_output(
    directory: &Path,
    args: &[&str],
) -> std::result::Result<GitOutput, CleanupBlockedReason> {
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .current_dir(directory)
        .output()
        .map_err(|_| CleanupBlockedReason::UnreadableWorktree)?;
    if !output.status.success() {
        return Err(CleanupBlockedReason::UnreadableWorktree);
    }
    Ok(GitOutput {
        stdout: output.stdout,
    })
}

#[cfg(test)]
fn git_text(
    directory: &Path,
    args: &[&str],
    failure: CleanupBlockedReason,
) -> std::result::Result<String, CleanupBlockedReason> {
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .current_dir(directory)
        .output()
        .map_err(|_| failure)?;
    if !output.status.success() {
        return Err(failure);
    }
    String::from_utf8(output.stdout)
        .map(|text| text.trim().to_string())
        .map_err(|_| failure)
}

#[cfg(test)]
struct WorktreeRegistration {
    head: String,
    branch: String,
}

#[cfg(test)]
fn parse_worktree_registration(
    porcelain: &str,
    expected_root: &Path,
) -> Option<WorktreeRegistration> {
    porcelain.split("\n\n").find_map(|entry| {
        let mut root = None;
        let mut head = None;
        let mut branch = None;
        for line in entry.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                root = Path::new(value).canonicalize().ok();
            } else if let Some(value) = line.strip_prefix("HEAD ") {
                head = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("branch ") {
                branch = Some(value.to_string());
            }
        }
        if root.as_deref() == Some(expected_root) {
            Some(WorktreeRegistration {
                head: head?,
                branch: branch?,
            })
        } else {
            None
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::bus::{DaemonEvent, EventBus};
    use crate::store::Store;
    use std::collections::VecDeque;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;
    use walkdir::WalkDir;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct IndexSnapshot {
        bytes: Vec<u8>,
        mode: u32,
        size: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
        lock_exists: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct MatrixTreeEntry {
        relative_path: PathBuf,
        kind: &'static str,
        mode: u32,
        size: u64,
        content: Option<Vec<u8>>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct MatrixSessionSnapshot {
        status: String,
        pending_archive: bool,
        sandbox_kind: Option<String>,
        sandbox_root: Option<String>,
        sandbox_branch: Option<String>,
        sandbox_cleanup_state: Option<String>,
        parent_id: Option<String>,
        lead_session_id: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct MatrixEvidence {
        tree: Vec<MatrixTreeEntry>,
        refs: String,
        worktrees: String,
        index: IndexSnapshot,
        session: MatrixSessionSnapshot,
        success_events: Vec<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct MatrixStaticEvidence {
        tree: Vec<MatrixTreeEntry>,
        refs: String,
        worktrees: String,
        index: IndexSnapshot,
        session: MatrixSessionSnapshot,
    }

    struct MatrixFixture {
        repo: TempDir,
        _base: TempDir,
        _db_dir: TempDir,
        db_path: PathBuf,
        _store: Store,
        candidate: CleanupCandidate,
        root: PathBuf,
        _event_bus: Arc<EventBus>,
        events: tokio::sync::broadcast::Receiver<Arc<DaemonEvent>>,
    }

    fn row_candidate() -> CleanupCandidate {
        CleanupCandidate::Row(RowCleanupCandidate {
            session_id: Uuid::new_v4(),
            kind: SandboxKind::GitWorktree,
            root: PathBuf::from("/tmp/d00-root"),
            branch: "rsi/d00".to_string(),
            origin: PathBuf::from("/tmp/d00-origin"),
            cleanup_state: SandboxCleanupState::Live,
        })
    }

    fn identity(head: &str) -> IdentityObservation {
        IdentityObservation {
            canonical_root: PathBuf::from("/tmp/d00-root"),
            source_head: head.to_string(),
            registered_head: head.to_string(),
            registered_branch: "refs/heads/rsi/d00".to_string(),
        }
    }

    struct ScriptedObserver {
        observations:
            Mutex<VecDeque<std::result::Result<IdentityObservation, CleanupBlockedReason>>>,
        barriers: Option<(Arc<Barrier>, Arc<Barrier>)>,
    }

    struct BarrierSystemObserver {
        before_drift: Arc<Barrier>,
        after_drift: Arc<Barrier>,
    }

    impl IdentityObserver for BarrierSystemObserver {
        fn observe(
            &self,
            candidate: &RowCleanupCandidate,
        ) -> std::result::Result<IdentityObservation, CleanupBlockedReason> {
            observe_system_identity(candidate)
        }

        fn between_observations(&self) {
            self.before_drift.wait();
            self.after_drift.wait();
        }
    }

    fn git(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git output utf8")
    }

    fn init_git_repo(path: &Path) {
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.email", "d00@example.invalid"]);
        git(path, &["config", "user.name", "D00 Fixture"]);
        std::fs::write(path.join("README.md"), "initial\n").expect("write fixture");
        git(path, &["add", "README.md"]);
        git(path, &["commit", "-q", "-m", "initial"]);
    }

    fn immutable_snapshot(repo: &Path, root: &Path) -> (String, String, Vec<u8>) {
        (
            git(
                repo,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/heads",
                ],
            ),
            git(repo, &["worktree", "list", "--porcelain"]),
            std::fs::read(root.join("README.md")).expect("read worktree content"),
        )
    }

    fn matrix_tree(root: &Path) -> Vec<MatrixTreeEntry> {
        let mut entries = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .map(|entry| entry.expect("walk isolated sandbox"))
            .map(|entry| {
                let path = entry.path();
                let metadata = std::fs::symlink_metadata(path).expect("sandbox metadata");
                let file_type = metadata.file_type();
                let (kind, content) = if file_type.is_file() {
                    (
                        "file",
                        Some(std::fs::read(path).expect("read sandbox file")),
                    )
                } else if file_type.is_dir() {
                    ("dir", None)
                } else if file_type.is_symlink() {
                    (
                        "symlink",
                        Some(
                            std::fs::read_link(path)
                                .expect("read sandbox symlink")
                                .as_os_str()
                                .as_encoded_bytes()
                                .to_vec(),
                        ),
                    )
                } else {
                    ("other", None)
                };
                MatrixTreeEntry {
                    relative_path: path
                        .strip_prefix(root)
                        .expect("sandbox-relative path")
                        .to_path_buf(),
                    kind,
                    mode: metadata.permissions().mode(),
                    size: metadata.size(),
                    content,
                }
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        entries
    }

    fn linked_worktree_index(root: &Path) -> PathBuf {
        let raw = git(root, &["rev-parse", "--git-path", "index"]);
        let path = PathBuf::from(raw.trim());
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    }

    fn index_snapshot(root: &Path) -> IndexSnapshot {
        let index = linked_worktree_index(root);
        let metadata = std::fs::metadata(&index).expect("linked worktree index metadata");
        IndexSnapshot {
            bytes: std::fs::read(&index).expect("linked worktree index bytes"),
            mode: metadata.mode(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            lock_exists: index.with_extension("lock").exists(),
        }
    }

    fn matrix_session_snapshot(db_path: &Path, session_id: Uuid) -> MatrixSessionSnapshot {
        rusqlite::Connection::open(db_path)
            .expect("open matrix snapshot database")
            .query_row(
                "SELECT status, pending_archive, sandbox_kind, sandbox_root,
                        sandbox_branch, sandbox_cleanup_state, parent_id, lead_session_id
                 FROM sessions WHERE id = ?1",
                rusqlite::params![session_id.to_string()],
                |row| {
                    Ok(MatrixSessionSnapshot {
                        status: row.get(0)?,
                        pending_archive: row.get::<_, i64>(1)? != 0,
                        sandbox_kind: row.get(2)?,
                        sandbox_root: row.get(3)?,
                        sandbox_branch: row.get(4)?,
                        sandbox_cleanup_state: row.get(5)?,
                        parent_id: row.get(6)?,
                        lead_session_id: row.get(7)?,
                    })
                },
            )
            .expect("snapshot matrix session")
    }

    fn matrix_static_evidence(
        repo: &Path,
        root: &Path,
        db_path: &Path,
        session_id: Uuid,
    ) -> MatrixStaticEvidence {
        MatrixStaticEvidence {
            tree: matrix_tree(root),
            refs: git(
                repo,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/heads",
                ],
            ),
            worktrees: git(repo, &["worktree", "list", "--porcelain"]),
            index: index_snapshot(root),
            session: matrix_session_snapshot(db_path, session_id),
        }
    }

    impl MatrixFixture {
        fn new() -> Self {
            let repo = TempDir::new().expect("repo tempdir");
            let base = TempDir::new().expect("sandbox tempdir");
            let db_dir = TempDir::new().expect("db tempdir");
            init_git_repo(repo.path());
            let session_id = Uuid::new_v4();
            let allocation = crate::sandbox::SandboxAllocator::new(base.path().to_path_buf())
                .allocate(
                    session_id,
                    repo.path(),
                    SandboxKind::GitWorktree,
                    "HEAD",
                    None,
                )
                .expect("allocate sandbox");
            let branch = allocation.branch.clone().expect("sandbox branch");
            let db_path = db_dir.path().join("matrix.db");
            let store = Store::open(&db_path).expect("open matrix store");
            let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            store
                .conn
                .execute(
                    "INSERT INTO sessions (
                         id, query, working_dir, status, created_at, updated_at,
                         sandbox_kind, sandbox_root, sandbox_branch, sandbox_cleanup_state
                     ) VALUES (?1, 'D00 matrix', ?2, 'Completed', ?3, ?3,
                               'GitWorktree', ?4, ?5, 'Live')",
                    rusqlite::params![
                        session_id.to_string(),
                        repo.path().display().to_string(),
                        timestamp,
                        allocation.root.display().to_string(),
                        branch,
                    ],
                )
                .expect("insert matrix session");
            let event_bus = Arc::new(EventBus::new(16));
            let events = event_bus.subscribe();
            let candidate = CleanupCandidate::Row(RowCleanupCandidate {
                session_id,
                kind: SandboxKind::GitWorktree,
                root: allocation.root.clone(),
                branch: allocation.branch.expect("sandbox branch"),
                origin: repo.path().to_path_buf(),
                cleanup_state: SandboxCleanupState::Live,
            });
            Self {
                repo,
                _base: base,
                _db_dir: db_dir,
                db_path,
                _store: store,
                candidate,
                root: allocation.root,
                _event_bus: event_bus,
                events,
            }
        }

        fn snapshot(&mut self) -> MatrixEvidence {
            let session_id = match &self.candidate {
                CleanupCandidate::Row(row) => row.session_id,
                other => panic!("matrix fixture requires row candidate, got {other:?}"),
            };
            let mut success_events = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                if matches!(
                    event.as_ref(),
                    DaemonEvent::SandboxOrphanCleaned { .. }
                        | DaemonEvent::SessionArchived { .. }
                        | DaemonEvent::SessionDeleted { .. }
                ) {
                    success_events.push(format!("{event:?}"));
                }
            }
            let static_evidence =
                matrix_static_evidence(self.repo.path(), &self.root, &self.db_path, session_id);
            let evidence = MatrixEvidence {
                tree: static_evidence.tree,
                refs: static_evidence.refs,
                worktrees: static_evidence.worktrees,
                index: static_evidence.index,
                session: static_evidence.session,
                success_events,
            };
            assert!(
                evidence.success_events.is_empty(),
                "classification emitted cleanup success events"
            );
            evidence
        }
    }

    impl ScriptedObserver {
        fn stable() -> Self {
            Self {
                observations: Mutex::new(VecDeque::from([Ok(identity("a")), Ok(identity("a"))])),
                barriers: None,
            }
        }

        fn with_results(
            results: impl IntoIterator<
                Item = std::result::Result<IdentityObservation, CleanupBlockedReason>,
            >,
        ) -> Self {
            Self {
                observations: Mutex::new(results.into_iter().collect()),
                barriers: None,
            }
        }
    }

    impl IdentityObserver for ScriptedObserver {
        fn observe(
            &self,
            _candidate: &RowCleanupCandidate,
        ) -> std::result::Result<IdentityObservation, CleanupBlockedReason> {
            self.observations
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted observation")
        }

        fn between_observations(&self) {
            if let Some((before, after)) = &self.barriers {
                before.wait();
                after.wait();
            }
        }
    }

    fn synthetic_diagnostics() -> D00Diagnostics {
        D00Diagnostics {
            target: TargetDiagnostic::SyntheticMatch,
            proof: ProofDiagnostic::SyntheticMatch,
            mapping: MappingDiagnostic::SyntheticMatch,
            verification: VerificationDiagnostic::SyntheticPass,
        }
    }

    fn decision_with(
        observer: &dyn IdentityObserver,
        diagnostics: D00Diagnostics,
        ownership: (OwnershipObservation, OwnershipObservation),
    ) -> CleanupDecision {
        classify_with(
            &row_candidate(),
            ownership.0,
            ownership.1,
            observer,
            diagnostics,
        )
    }

    #[test]
    fn special_sources_are_always_blocked_without_observation() {
        let sid = Uuid::new_v4();
        let cases = [
            (
                CleanupCandidate::InconsistentRow {
                    session_id: sid,
                    detail: "partial",
                },
                CleanupBlockedReason::InconsistentSandboxColumns,
            ),
            (
                CleanupCandidate::RowReadFailure {
                    session_id: sid,
                    root: PathBuf::from("/tmp/root"),
                },
                CleanupBlockedReason::RowReadFailure,
            ),
            (
                CleanupCandidate::LaunchAbort {
                    session_id: sid,
                    root: PathBuf::from("/tmp/root"),
                    branch: Some("rsi/d00".to_string()),
                    origin: PathBuf::from("/tmp/origin"),
                },
                CleanupBlockedReason::LaunchAbortWithoutOwner,
            ),
            (
                CleanupCandidate::PathOnly {
                    attributed_session_id: None,
                    root: PathBuf::from("/tmp/root"),
                },
                CleanupBlockedReason::PathOnlyAttribution,
            ),
        ];
        for (candidate, expected) in cases {
            assert_eq!(
                classify_candidate(
                    &candidate,
                    OwnershipObservation::Exclusive,
                    OwnershipObservation::Exclusive,
                ),
                CleanupDecision::Blocked(expected)
            );
        }
    }

    #[test]
    fn dirty_unreadable_missing_and_changed_identity_are_blocked() {
        for expected in [
            CleanupBlockedReason::DirtyWorktree,
            CleanupBlockedReason::UnreadableWorktree,
            CleanupBlockedReason::MissingSourceIdentity,
            CleanupBlockedReason::ChangedSourceIdentity,
        ] {
            let observer = ScriptedObserver::with_results([Err(expected)]);
            assert_eq!(
                decision_with(
                    &observer,
                    synthetic_diagnostics(),
                    (
                        OwnershipObservation::Exclusive,
                        OwnershipObservation::Exclusive,
                    ),
                ),
                CleanupDecision::Blocked(expected)
            );
        }
    }

    #[test]
    fn target_proof_mapping_and_verification_diagnostics_never_authorize() {
        let cases = [
            (
                D00Diagnostics {
                    target: TargetDiagnostic::Unknown,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::UnknownTarget,
            ),
            (
                D00Diagnostics {
                    target: TargetDiagnostic::Mismatched,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::MismatchedTarget,
            ),
            (
                D00Diagnostics {
                    target: TargetDiagnostic::Unconfigured,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::UnconfiguredTarget,
            ),
            (
                D00Diagnostics {
                    proof: ProofDiagnostic::Missing,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::MissingIndependentlyVerifiedProof,
            ),
            (
                D00Diagnostics {
                    proof: ProofDiagnostic::Mismatched,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::MismatchedProof,
            ),
            (
                D00Diagnostics {
                    mapping: MappingDiagnostic::Missing,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::MissingCherryPickMapping,
            ),
            (
                D00Diagnostics {
                    verification: VerificationDiagnostic::Failed,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::FailedPostHeadVerification,
            ),
        ];
        for (diagnostics, expected) in cases {
            assert_eq!(
                decision_with(
                    &ScriptedObserver::stable(),
                    diagnostics,
                    (
                        OwnershipObservation::Exclusive,
                        OwnershipObservation::Exclusive,
                    ),
                ),
                CleanupDecision::Blocked(expected)
            );
        }
        assert_eq!(
            decision_with(
                &ScriptedObserver::stable(),
                synthetic_diagnostics(),
                (
                    OwnershipObservation::Exclusive,
                    OwnershipObservation::Exclusive,
                ),
            ),
            CleanupDecision::Blocked(CleanupBlockedReason::MissingIndependentlyVerifiedProof)
        );
    }

    #[derive(Clone, Copy)]
    enum MatrixSetup {
        None,
        MismatchedTarget,
        NonMainTarget,
        CherryPick,
    }

    type AdverseMatrixCase = (
        &'static str,
        D00Diagnostics,
        CleanupBlockedReason,
        MatrixSetup,
        bool,
    );

    fn adverse_matrix_cases() -> [AdverseMatrixCase; 8] {
        [
            (
                "unreadable worktree",
                synthetic_diagnostics(),
                CleanupBlockedReason::UnreadableWorktree,
                MatrixSetup::None,
                true,
            ),
            (
                "unknown target",
                D00Diagnostics {
                    target: TargetDiagnostic::Unknown,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::UnknownTarget,
                MatrixSetup::None,
                false,
            ),
            (
                "mismatched target",
                D00Diagnostics {
                    target: TargetDiagnostic::Mismatched,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::MismatchedTarget,
                MatrixSetup::MismatchedTarget,
                false,
            ),
            (
                "non-main unconfigured target",
                D00Diagnostics {
                    target: TargetDiagnostic::Unconfigured,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::UnconfiguredTarget,
                MatrixSetup::NonMainTarget,
                false,
            ),
            (
                "absent proof",
                D00Diagnostics::production(),
                CleanupBlockedReason::MissingIndependentlyVerifiedProof,
                MatrixSetup::None,
                false,
            ),
            (
                "mismatched proof",
                D00Diagnostics {
                    proof: ProofDiagnostic::Mismatched,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::MismatchedProof,
                MatrixSetup::None,
                false,
            ),
            (
                "missing cherry-pick mapping",
                D00Diagnostics {
                    mapping: MappingDiagnostic::Missing,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::MissingCherryPickMapping,
                MatrixSetup::CherryPick,
                false,
            ),
            (
                "failed post-head verification",
                D00Diagnostics {
                    verification: VerificationDiagnostic::Failed,
                    ..synthetic_diagnostics()
                },
                CleanupBlockedReason::FailedPostHeadVerification,
                MatrixSetup::None,
                false,
            ),
        ]
    }

    #[test]
    fn shared_boundary_adverse_rows_use_isolated_full_snapshots() {
        for (name, diagnostics, expected, setup, inject_unreadable) in adverse_matrix_cases() {
            let mut fixture = MatrixFixture::new();
            match setup {
                MatrixSetup::None => {}
                MatrixSetup::MismatchedTarget => {
                    git(fixture.repo.path(), &["branch", "expected-target", "main"]);
                    std::fs::write(fixture.repo.path().join("target.txt"), "different target\n")
                        .expect("write mismatched target");
                    git(fixture.repo.path(), &["add", "target.txt"]);
                    git(
                        fixture.repo.path(),
                        &["commit", "-q", "-m", "advance observed target"],
                    );
                }
                MatrixSetup::NonMainTarget => {
                    git(fixture.repo.path(), &["branch", "idea/d00", "main"]);
                }
                MatrixSetup::CherryPick => {
                    std::fs::write(fixture.root.join("source.txt"), "source change\n")
                        .expect("write source change");
                    git(&fixture.root, &["add", "source.txt"]);
                    git(&fixture.root, &["commit", "-q", "-m", "source change"]);
                    let source_commit = git(&fixture.root, &["rev-parse", "HEAD"]);
                    git(
                        fixture.repo.path(),
                        &["checkout", "-q", "-b", "idea/d00", "main"],
                    );
                    git(fixture.repo.path(), &["cherry-pick", source_commit.trim()]);
                    git(fixture.repo.path(), &["checkout", "-q", "main"]);
                }
            }

            let before = fixture.snapshot();
            assert!(!before.index.lock_exists, "{name}: stale index lock");
            let scripted;
            let observer: &dyn IdentityObserver = if inject_unreadable {
                scripted =
                    ScriptedObserver::with_results([Err(CleanupBlockedReason::UnreadableWorktree)]);
                &scripted
            } else {
                &SystemIdentityObserver
            };
            assert_eq!(
                classify_with(
                    &fixture.candidate,
                    OwnershipObservation::Exclusive,
                    OwnershipObservation::Exclusive,
                    observer,
                    diagnostics,
                ),
                CleanupDecision::Blocked(expected),
                "{name}"
            );
            assert_eq!(fixture.snapshot(), before, "{name}");
        }
    }

    #[test]
    fn shared_unreadable_and_changed_ownership_are_blocked() {
        let cases = [
            (
                (OwnershipObservation::Shared, OwnershipObservation::Shared),
                CleanupBlockedReason::SharedLiveOwnership,
            ),
            (
                (
                    OwnershipObservation::Unreadable,
                    OwnershipObservation::Unreadable,
                ),
                CleanupBlockedReason::OwnershipReadFailure,
            ),
            (
                (
                    OwnershipObservation::Exclusive,
                    OwnershipObservation::Shared,
                ),
                CleanupBlockedReason::ConcurrentDrift,
            ),
        ];
        for (ownership, expected) in cases {
            assert_eq!(
                decision_with(
                    &ScriptedObserver::stable(),
                    synthetic_diagnostics(),
                    ownership,
                ),
                CleanupDecision::Blocked(expected)
            );
        }
    }

    #[test]
    fn barrier_controlled_identity_drift_is_blocked_without_sleeping() {
        let before = Arc::new(Barrier::new(2));
        let after = Arc::new(Barrier::new(2));
        let observer = ScriptedObserver {
            observations: Mutex::new(VecDeque::from([
                Ok(identity("before")),
                Ok(identity("after")),
            ])),
            barriers: Some((Arc::clone(&before), Arc::clone(&after))),
        };
        std::thread::scope(|scope| {
            scope.spawn(|| {
                before.wait();
                after.wait();
            });
            assert_eq!(
                decision_with(
                    &observer,
                    synthetic_diagnostics(),
                    (
                        OwnershipObservation::Exclusive,
                        OwnershipObservation::Exclusive,
                    ),
                ),
                CleanupDecision::Blocked(CleanupBlockedReason::ConcurrentDrift)
            );
        });
    }

    #[test]
    fn real_ref_drift_is_barrier_controlled_and_classifier_is_inert() {
        let mut fixture = MatrixFixture::new();
        let (session_id, branch) = match &fixture.candidate {
            CleanupCandidate::Row(row) => (row.session_id, row.branch.clone()),
            other => panic!("matrix fixture requires row candidate, got {other:?}"),
        };
        std::fs::write(fixture.repo.path().join("advance.txt"), "advance\n")
            .expect("write advance");
        git(fixture.repo.path(), &["add", "advance.txt"]);
        git(fixture.repo.path(), &["commit", "-q", "-m", "advance main"]);
        let advanced_head = git(fixture.repo.path(), &["rev-parse", "main"])
            .trim()
            .to_string();
        let candidate = fixture.candidate.clone();
        let before_drift = Arc::new(Barrier::new(2));
        let after_drift = Arc::new(Barrier::new(2));
        let observer = BarrierSystemObserver {
            before_drift: Arc::clone(&before_drift),
            after_drift: Arc::clone(&after_drift),
        };
        let post_external = Arc::new(Mutex::new(None));
        let repo_path = fixture.repo.path();
        let sandbox_root = fixture.root.as_path();
        let db_path = fixture.db_path.as_path();

        std::thread::scope(|scope| {
            let post_external = Arc::clone(&post_external);
            scope.spawn(move || {
                before_drift.wait();
                git(
                    repo_path,
                    &[
                        "update-ref",
                        &format!("refs/heads/{branch}"),
                        &advanced_head,
                    ],
                );
                *post_external.lock().unwrap() = Some(matrix_static_evidence(
                    repo_path,
                    sandbox_root,
                    db_path,
                    session_id,
                ));
                after_drift.wait();
            });
            assert_eq!(
                classify_with(
                    &candidate,
                    OwnershipObservation::Exclusive,
                    OwnershipObservation::Exclusive,
                    &observer,
                    synthetic_diagnostics(),
                ),
                CleanupDecision::Blocked(CleanupBlockedReason::ConcurrentDrift)
            );
        });

        let expected = post_external
            .lock()
            .unwrap()
            .clone()
            .expect("post-external snapshot");
        assert_eq!(
            matrix_static_evidence(
                fixture.repo.path(),
                &fixture.root,
                &fixture.db_path,
                session_id,
            ),
            expected
        );
        assert!(fixture.snapshot().success_events.is_empty());
    }

    #[test]
    fn production_classifier_does_not_invoke_git_or_refresh_stale_index() {
        let repo = TempDir::new().expect("repo tempdir");
        let base = TempDir::new().expect("sandbox tempdir");
        init_git_repo(repo.path());
        let session_id = Uuid::new_v4();
        let allocation = crate::sandbox::SandboxAllocator::new(base.path().to_path_buf())
            .allocate(
                session_id,
                repo.path(),
                SandboxKind::GitWorktree,
                "HEAD",
                None,
            )
            .expect("allocate sandbox");
        let branch = allocation.branch.clone().expect("sandbox branch");
        let candidate = CleanupCandidate::Row(RowCleanupCandidate {
            session_id,
            kind: SandboxKind::GitWorktree,
            root: allocation.root.clone(),
            branch,
            origin: repo.path().to_path_buf(),
            cleanup_state: SandboxCleanupState::Live,
        });

        let tracked = std::fs::File::options()
            .write(true)
            .open(allocation.root.join("README.md"))
            .expect("open tracked file");
        tracked
            .set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1)))
            .expect("make index stat data deterministically stale");
        let fsmonitor_marker = base.path().join("fsmonitor-invoked");
        let fsmonitor_hook = base.path().join("fsmonitor-hook");
        std::fs::write(
            &fsmonitor_hook,
            format!(
                "#!/bin/sh\nprintf invoked > '{}'\nprintf '\\n'\n",
                fsmonitor_marker.display()
            ),
        )
        .expect("write fsmonitor hook");
        let mut permissions = std::fs::metadata(&fsmonitor_hook)
            .expect("fsmonitor metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fsmonitor_hook, permissions).expect("make fsmonitor executable");
        git(
            repo.path(),
            &[
                "config",
                "core.fsmonitor",
                fsmonitor_hook.to_str().expect("utf8 fsmonitor hook"),
            ],
        );

        let identity_before = immutable_snapshot(repo.path(), &allocation.root);
        let index_before = index_snapshot(&allocation.root);
        assert!(!index_before.lock_exists);

        assert_eq!(
            classify_candidate(
                &candidate,
                OwnershipObservation::Exclusive,
                OwnershipObservation::Exclusive,
            ),
            CleanupDecision::Blocked(CleanupBlockedReason::MissingIndependentlyVerifiedProof)
        );

        assert_eq!(
            immutable_snapshot(repo.path(), &allocation.root),
            identity_before
        );
        assert_eq!(index_snapshot(&allocation.root), index_before);
        assert!(
            !fsmonitor_marker.exists(),
            "production cleanup classification invoked configured Git fsmonitor"
        );
    }

    #[test]
    fn generic_lifecycle_has_no_destructive_worktree_reachability() {
        let lifecycle = include_str!("../session/lifecycle.rs");
        let launch = include_str!("../session/launch.rs");
        for source in [lifecycle, launch] {
            for forbidden in [
                "git_worktree::destroy",
                "destroy_clean_worktree",
                "remove_worktree_non_force_locked",
                "remove_worktree_after_source_ref_delete_non_force_locked",
                "delete_ref_compare_locked",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "generic lifecycle source reaches destructive primitive {forbidden}"
                );
            }
        }

        let git_worktree = include_str!("git_worktree.rs");
        for signature in ["pub fn destroy(", "pub fn destroy_by_path("] {
            let offset = git_worktree
                .find(signature)
                .unwrap_or_else(|| panic!("missing raw helper {signature}"));
            let prefix_start = offset.saturating_sub(160);
            assert!(
                git_worktree[prefix_start..offset].contains("#[cfg(test)]"),
                "raw helper {signature} is not test-only"
            );
        }
    }
}
