//! Guarded integration engine: lands accepted source on an integration target.
//!
//! Slice 1 of `thoughts/shared/plans/2026-09-20-rolling-merge-engine-slices.md`.
//! This module is stateless Git mechanics only. It owns no persistence, RPC,
//! policy, or remote effect; a later slice supplies the durable single owner
//! and action journal that serialize callers.
//!
//! Invariants every entry point preserves:
//!
//! - Accepted source is never rebased or rewritten. A candidate is the source
//!   itself (fast-forward) or a two-parent merge commit `(target tip, source)`.
//! - Publication only ever fast-forwards the target from an exact expected tip.
//!   A concurrent writer wins; nothing is forced or overwritten.
//! - `prepare_candidate` moves no ref and touches no pre-existing worktree.
//! - A checked-out target is advanced only through a `TargetCustody` reference
//!   transaction. Foreign trees are never stashed, reset, or moved by a raw
//!   ref update.
//! - Publish fails closed when any worktree has an in-progress rebase, merge,
//!   cherry-pick, revert, am, or bisect whose origin or current branch is the
//!   target. The remaining list-to-effect TOCTOU is a precondition enforced by
//!   Slice 2's durable single owner, not by this stateless engine.
//! - `main` and `master` are refused even when the caller allowlists them.
//! - Only worktrees minted by this engine can be discarded by it.

mod engine;
mod git;
mod guard;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;

use crate::error::DaemonError;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

pub use engine::{
    TargetCustody, abort_target_custody, acquire_target_custody, discard_candidate,
    discover_custody_record, prepare_candidate, reconcile_target_custody,
};
// Temporary Stage B1 compilation seam for the pre-custody manager action.
// Stage B2 replaces its caller; this remains crate-private so no external
// integration path can bypass continuous custody.
pub(crate) use engine::publish;
pub use guard::{GuardCommand, GuardCommandReport, GuardReport, GuardSpec, GuardStatus, run_guard};

/// RME-S2A-002: Resolve a Git ref to its commit OID using the hardened
/// engine git helper. Returns `None` if the ref does not resolve.
pub async fn resolve_ref(
    config: &IntegrationConfig,
    repo: &std::path::Path,
    ref_name: &str,
) -> Option<String> {
    git::stdout(config, repo, &["rev-parse", "--verify", ref_name])
        .await
        .ok()
}

/// Branch names no caller allowlist can authorize as an integration target.
const PROTECTED_BRANCHES: [&str; 2] = ["main", "master"];
const HEADS_PREFIX: &str = "refs/heads/";

/// Identity recorded on merge commits the engine creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIdentity {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone)]
pub struct IntegrationConfig {
    /// Full ref names (`refs/heads/<branch>`) the caller authorizes as targets.
    pub allowed_targets: Vec<String>,
    pub identity: CommitIdentity,
    /// Upper bound for each individual Git invocation.
    pub git_timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CustodyPhase {
    /// The custody marker and index lock are in place and validated.
    Acquired,
    /// A candidate has been prepared under this custody.
    Prepared,
    /// The candidate is being applied to the target holder.
    Applying,
    /// The candidate has been applied and verified.
    Applied,
}

/// Durable proof of integration custody for one repo/target.
///
/// The engine writes a JSON copy of this record into the holder's private git
/// dir (`marker`) while the custody is active; every later cleanup re-reads it
/// and requires full equality before touching any artifact.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustodyRecord {
    pub version: u32,
    pub operation_id: Uuid,
    pub target_ref: String,
    pub expected_tip: String,
    pub candidate: Option<String>,
    /// Exact cleanup authority for the engine-created candidate.  This is
    /// persisted with Prepared so a crashed caller cannot leak a candidate
    /// worktree or later substitute a different path/token for cleanup.
    #[serde(default)]
    pub candidate_cleanup: Option<CandidateCleanup>,
    /// Immutable binding to the create-once operation artifact manifest.
    /// Acquired has no manifest; every later phase carries this exact binding.
    #[serde(default)]
    pub artifact_manifest: Option<ArtifactManifestBinding>,
    /// Device:inode of the immutable operation proof directory. This binds
    /// every later proof read and removal to the directory created at acquire.
    #[serde(default)]
    pub proof_dir_identity: String,
    /// Device:inode of the `index.lock` this custody created, recorded so a
    /// later cleanup or recovery proves the on-disk lock is still the same
    /// file before removing it.
    pub index_lock_identity: String,
    /// Worktree the target is (or is to be) checked out in.
    pub holder: PathBuf,
    /// Private git dir of `holder` (`git rev-parse --absolute-git-dir`).
    pub git_dir: PathBuf,
    /// Snapshot of the holder index, `index.rsi-<operation_id>`.
    pub alt_index: PathBuf,
    /// Device:inode of the alternate index created at acquisition. This stays
    /// durable after promotion so recovery can distinguish it from a foreign
    /// real index.
    #[serde(default)]
    pub alt_index_identity: String,
    /// Device:inode of the holder's real index before this operation. It is
    /// the no-clobber precondition for installing the stable alternate index.
    #[serde(default)]
    pub real_index_identity: String,
    /// Create-once inode identities for the independently written phase
    /// records. Later phase records are manifest-bound hard links instead.
    #[serde(default)]
    pub acquired_phase_identity: String,
    #[serde(default)]
    pub prepared_phase_identity: String,
    /// Fixed custody marker, `rsi-integration-custody`, holding this record.
    pub marker: PathBuf,
    /// Whether `holder` is a worktree this engine created for this operation.
    pub engine_owned: bool,
    pub phase: CustodyPhase,
}

/// Exact durable reference to the operation's create-once artifact manifest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactManifestBinding {
    pub path: PathBuf,
    pub identity: String,
    /// `sha256:<lowercase hex>` of the canonical manifest bytes.
    pub digest: String,
}

/// Canonical inventory of every operation-owned file below a custody proof
/// directory that may later be written, promoted, or removed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactManifest {
    pub version: u32,
    pub operation_id: Uuid,
    pub marker: PathBuf,
    pub alt_index_identity: String,
    pub artifacts: Vec<ArtifactProof>,
}

/// One operation-owned artifact. Immutable transition records carry a digest;
/// mutable scratch files deliberately bind identity only.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactProof {
    pub kind: ArtifactKind,
    pub path: PathBuf,
    pub identity: String,
    #[serde(default)]
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    NextPrepared,
    NextApplying,
    NextApplied,
    NextStatus,
    NextVerifyIndex,
    NextRestore,
    NextRestoreBuild,
    NextApplyBuild,
    NextTmp,
}

/// Immutable identity of the candidate worktree owned by one custody.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateCleanup {
    pub operation_id: Uuid,
    pub candidate_oid: String,
    pub worktree: PathBuf,
    pub git_dir: PathBuf,
    pub marker: PathBuf,
    pub marker_token: String,
}

/// Append-only proof of a recovery-created custody lock.  Every field is
/// duplicated from the durable custody record so a restart never accepts a
/// staged inode for another target or operation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockRecoveryProof {
    pub version: u32,
    pub operation_id: Uuid,
    pub target_ref: String,
    pub expected_tip: String,
    pub candidate: Option<String>,
    pub holder: PathBuf,
    pub git_dir: PathBuf,
    pub proof_dir_identity: String,
    pub prior_lock_identity: String,
    pub replacement_lock_identity: String,
    pub phase: CustodyPhase,
    pub staged_lock: PathBuf,
    pub installed_lock: PathBuf,
    /// The two immutable proof records are reserved before either is
    /// serialized. Their inodes are part of the recovery authority, rather
    /// than being discovered again during teardown.
    #[serde(default)]
    pub prepared_proof_identity: String,
    #[serde(default)]
    pub installed_proof_identity: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// The target tip is an ancestor of the source; the candidate is the source.
    FastForward,
    /// Histories diverged; the candidate is a merge commit `(tip, source)`.
    Merge,
}

/// Proof of an engine-created candidate worktree. The identifier is private so
/// only this module can mint a handle that `discard_candidate` will honor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateHandle {
    pub worktree: PathBuf,
    id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub oid: String,
    pub kind: CandidateKind,
    /// Target tip the candidate was built on; pass it to `publish` unchanged.
    pub base_tip: String,
    pub handle: CandidateHandle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prepared {
    /// The source is already reachable from the target. Nothing to publish.
    AlreadyIntegrated,
    Candidate(Candidate),
}

/// Typed, expected refusals. None of these changed any ref or foreign tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Target is protected, malformed, or absent from the caller allowlist.
    TargetDenied,
    /// Target no longer resolves to the expected tip (`None`: it is missing).
    StaleTarget { observed: Option<String> },
    /// The source or candidate does not resolve to a commit object.
    InvalidSource,
    /// The merge conflicted at these repository-relative paths.
    Conflict { paths: Vec<String> },
    /// The expected tip is not an ancestor of the candidate.
    NotFastForward,
    /// The worktree holding the target has local state the advance would touch.
    TargetWorktreeDirty { path: PathBuf },
    /// More than one worktree claims the target branch.
    TargetCustodyAmbiguous,
    /// Another custody (or a live Git index write) already guards the holder.
    CustodyHeld { holder: PathBuf },
    /// The on-disk custody proof disagrees with the durable record (or is
    /// inconsistent with the observed refs, holder, or artifacts) and no safe
    /// checked cleanup exists. All proof is preserved for operator review.
    CustodyUncertain { reason: String },
    /// A worktree has an in-progress Git operation (rebase, merge, cherry-pick,
    /// revert, am, or bisect) that owns the target. Publish must fail closed:
    /// the operation may move the ref on completion.
    TargetOperationInProgress { path: PathBuf },
    /// The handle does not identify a worktree this engine created.
    NotEngineCandidate,
}

/// Terminal outcomes of a custody-guarded publication.
///
/// Also the outcome of reconciling an on-disk custody proof after a crash.
/// `Uncertain` is reported as `Refusal::CustodyUncertain` with every proof
/// artifact preserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publication {
    Published,
    Aborted,
}

#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    #[error("integration refused: {0:?}")]
    Refused(Refusal),
    #[error("integration git failure: {0}")]
    Git(String),
    #[error("integration invalid input: {0}")]
    InvalidInput(&'static str),
}

pub type Result<T> = std::result::Result<T, IntegrationError>;

impl From<Refusal> for IntegrationError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<IntegrationError> for DaemonError {
    fn from(error: IntegrationError) -> Self {
        match error {
            IntegrationError::Refused(_) => Self::PolicyDenied(error.to_string()),
            IntegrationError::Git(_) => Self::Process(error.to_string()),
            IntegrationError::InvalidInput(_) => Self::InvalidParam(error.to_string()),
        }
    }
}

/// Full lowercase SHA-1 or SHA-256 object name. Requiring object names instead
/// of revisions keeps every Git argument free of option or ref-syntax injection.
fn canonical_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn require_oid(value: &str, label: &'static str) -> Result<()> {
    if canonical_oid(value) {
        Ok(())
    } else {
        Err(IntegrationError::InvalidInput(label))
    }
}

/// Fail closed unless `target_ref` is a well-formed, unprotected local branch
/// ref that the caller explicitly allowlisted.
// `.lock` is Git's case-sensitive ref-name rule, not a file extension.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn authorize_target(config: &IntegrationConfig, target_ref: &str) -> Result<()> {
    let denied = || IntegrationError::Refused(Refusal::TargetDenied);
    let short = target_ref.strip_prefix(HEADS_PREFIX).ok_or_else(denied)?;
    let well_formed = !short.is_empty()
        && !short.starts_with(['-', '/', '.'])
        && !short.ends_with(['/', '.'])
        && !short.ends_with(".lock")
        && !short.contains("..")
        && !short.contains("//")
        && short
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'));
    let protected = PROTECTED_BRANCHES
        .iter()
        .any(|name| short.eq_ignore_ascii_case(name));
    let allowed = config
        .allowed_targets
        .iter()
        .any(|allowed| allowed == target_ref);
    if well_formed && !protected && allowed {
        Ok(())
    } else {
        Err(denied())
    }
}
