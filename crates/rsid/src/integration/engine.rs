//! Candidate construction, fast-forward publication, and candidate disposal.

use super::git;
use super::{
    ArtifactKind, ArtifactManifest, ArtifactManifestBinding, ArtifactProof, Candidate,
    CandidateCleanup, CandidateHandle, CandidateKind, CustodyPhase, CustodyRecord, HEADS_PREFIX,
    IntegrationConfig, IntegrationError, LockRecoveryProof, Prepared, Publication, Refusal, Result,
    authorize_target, require_oid,
};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Marker written into a candidate worktree's private Git directory. Keeping it
/// out of the working tree leaves the candidate byte-identical to its commit.
const CANDIDATE_MARKER: &str = "rsi-integration-candidate";

#[derive(Debug, Default)]
struct WorktreeEntry {
    path: PathBuf,
    branch: Option<String>,
    detached: bool,
}

/// Build the exact commit that would advance `target_ref`, in an isolated
/// engine-owned detached worktree. Moves no ref and touches no existing tree.
///
/// # Errors
///
/// `Refused` with `TargetDenied`, `StaleTarget`, `InvalidSource` or `Conflict`
/// for expected outcomes; `InvalidInput` for a non-canonical object name or an
/// unusable scratch directory; `Git` when Git itself could not be supervised.
pub async fn prepare_candidate(
    config: &IntegrationConfig,
    repo: &Path,
    target_ref: &str,
    expected_tip: &str,
    source: &str,
    scratch_dir: &Path,
) -> Result<Prepared> {
    authorize_target(config, target_ref)?;
    require_oid(
        expected_tip,
        "expected_tip must be a full lowercase object name",
    )?;
    require_oid(source, "source must be a full lowercase object name")?;
    require_target_at(config, repo, target_ref, expected_tip).await?;
    require_commit(config, repo, source).await?;

    if is_ancestor(config, repo, source, expected_tip).await? {
        return Ok(Prepared::AlreadyIntegrated);
    }
    let kind = if is_ancestor(config, repo, expected_tip, source).await? {
        CandidateKind::FastForward
    } else {
        CandidateKind::Merge
    };
    let start = match kind {
        CandidateKind::FastForward => source,
        CandidateKind::Merge => expected_tip,
    };
    let handle = create_candidate_worktree(config, repo, scratch_dir, start).await?;
    let built = match kind {
        CandidateKind::FastForward => Ok(source.to_string()),
        CandidateKind::Merge => {
            merge_source(config, &handle.worktree, target_ref, expected_tip, source).await
        }
    };
    match built {
        Ok(oid) => Ok(Prepared::Candidate(Candidate {
            oid,
            kind,
            base_tip: expected_tip.to_string(),
            handle,
        })),
        Err(error) => {
            remove_worktree(config, repo, &handle.worktree).await;
            Err(error)
        }
    }
}

/// Advance `target_ref` from exactly `expected_tip` to `candidate`, fast-forward
/// only. `Ok(())` means the target now resolves to `candidate`.
///
/// # Errors
///
/// `Refused` with `TargetDenied`, `InvalidSource`, `NotFastForward`,
/// `StaleTarget`, `TargetWorktreeDirty`, `TargetOperationInProgress` or
/// `TargetCustodyAmbiguous`; none of them moved the target. `InvalidInput` for
/// a non-canonical object name; `Git` when Git itself could not be supervised.
pub async fn publish(
    config: &IntegrationConfig,
    repo: &Path,
    target_ref: &str,
    expected_tip: &str,
    candidate: &str,
) -> Result<()> {
    authorize_target(config, target_ref)?;
    require_oid(
        expected_tip,
        "expected_tip must be a full lowercase object name",
    )?;
    require_oid(candidate, "candidate must be a full lowercase object name")?;
    require_commit(config, repo, candidate).await?;
    require_commit(config, repo, expected_tip).await?;
    if !is_ancestor(config, repo, expected_tip, candidate).await? {
        return Err(Refusal::NotFastForward.into());
    }

    let worktrees = list_worktrees(config, repo).await?;
    let target_tip = resolve_commit(config, repo, target_ref).await?;
    for entry in &worktrees {
        if worktree_holds_target_operation(config, entry, target_ref, target_tip.as_deref()).await?
        {
            return Err(Refusal::TargetOperationInProgress {
                path: entry.path.clone(),
            }
            .into());
        }
    }
    let holders: Vec<&WorktreeEntry> = worktrees
        .iter()
        .filter(|entry| entry.branch.as_deref() == Some(target_ref))
        .collect();
    match holders.as_slice() {
        [] => publish_unheld(config, repo, target_ref, expected_tip, candidate).await,
        [holder] => publish_in_place(config, &holder.path, expected_tip, candidate).await,
        _ => Err(Refusal::TargetCustodyAmbiguous.into()),
    }
}

/// Remove a candidate worktree, and only one this engine demonstrably created.
///
/// # Errors
///
/// `Refused(NotEngineCandidate)` unless the path is a registered detached
/// worktree carrying this handle's marker; the path is then left untouched.
/// `Git` when removal itself fails.
pub async fn discard_candidate(
    config: &IntegrationConfig,
    repo: &Path,
    handle: &CandidateHandle,
) -> Result<()> {
    let refused = || IntegrationError::Refused(Refusal::NotEngineCandidate);
    let wanted = tokio::fs::canonicalize(&handle.worktree)
        .await
        .map_err(|_| refused())?;
    let mut registered = None;
    for entry in list_worktrees(config, repo).await? {
        if entry.detached
            && tokio::fs::canonicalize(&entry.path)
                .await
                .is_ok_and(|path| path == wanted)
        {
            registered = Some(entry.path);
            break;
        }
    }
    let registered = registered.ok_or_else(refused)?;
    let marker = private_git_dir(config, &registered)
        .await?
        .join(CANDIDATE_MARKER);
    let recorded = tokio::fs::read_to_string(&marker)
        .await
        .map_err(|_| refused())?;
    if handle.id.is_empty() || recorded.trim() != handle.id {
        return Err(refused());
    }
    let status = git::stdout_raw(
        config,
        &registered,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await?;
    if !status.is_empty() {
        return Err(refused());
    }
    let path = path_arg(&registered)?;
    let output = git::run(config, repo, &["worktree", "remove", path]).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git::failed(&["worktree"], &output))
    }
}

async fn publish_unheld(
    config: &IntegrationConfig,
    repo: &Path,
    target_ref: &str,
    expected_tip: &str,
    candidate: &str,
) -> Result<()> {
    // `update-ref <ref> <new> <old>` is Git's atomic compare-and-swap.
    let args = [
        "update-ref",
        "-m",
        "rsi integration: fast-forward publish",
        target_ref,
        candidate,
        expected_tip,
    ];
    let output = git::run(config, repo, &args).await?;
    if output.status.success() {
        return Ok(());
    }
    let observed = resolve_commit(config, repo, target_ref).await?;
    if observed.as_deref() == Some(expected_tip) {
        Err(git::failed(&args, &output))
    } else {
        Err(Refusal::StaleTarget { observed }.into())
    }
}

async fn publish_in_place(
    config: &IntegrationConfig,
    holder: &Path,
    expected_tip: &str,
    candidate: &str,
) -> Result<()> {
    let head = git::stdout(config, holder, &["rev-parse", "--verify", "HEAD^{commit}"]).await?;
    if head != expected_tip {
        return Err(Refusal::StaleTarget {
            observed: Some(head),
        }
        .into());
    }
    let dirty = || {
        IntegrationError::Refused(Refusal::TargetWorktreeDirty {
            path: holder.to_path_buf(),
        })
    };
    // Strict cleanliness: any porcelain entry — including any untracked
    // file — means the holder is dirty and publish must refuse. This never
    // reaches `git merge` with a dirty worktree, so classification after a
    // failed merge is from the re-observed HEAD only, never from diagnostic
    // text.
    let status = git::stdout_raw(
        config,
        holder,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await?;
    if !status.is_empty() {
        return Err(dirty());
    }
    let args = ["merge", "--ff-only", "--no-stat", "-q", candidate];
    let output = git::run(config, holder, &args).await?;
    let head = git::stdout(config, holder, &["rev-parse", "--verify", "HEAD^{commit}"]).await?;
    if output.status.success() && head == candidate {
        return Ok(());
    }
    if head != expected_tip {
        return Err(Refusal::StaleTarget {
            observed: Some(head),
        }
        .into());
    }
    // The merge failed without moving HEAD. Classify only from the re-observed
    // HEAD: since the pre-check confirmed cleanliness, an unchanged HEAD after
    // a failed ff-only is a git error, not an inferred dirty state.
    Err(git::failed(&args, &output))
}

/// Whether `entry`'s worktree has an in-progress Git operation whose origin or
/// current branch is `target_ref`. A rebase, merge, cherry-pick, revert, am, or
/// bisect in progress owns the branch and may move it on completion, so publish
/// must fail closed.
///
/// # Detection
///
/// - **Rebase (merge backend)**: `rebase-merge/head-name` names the target.
/// - **Rebase (apply backend)**: `rebase-apply/head-name` names the target.
/// - **`git am`**: `rebase-apply/` exists on the target branch (am keeps the
///   worktree on its branch; `head-name` is absent).
/// - **Merge, cherry-pick, revert**: `MERGE_HEAD` / `CHERRY_PICK_HEAD` /
///   `REVERT_HEAD` exists on the target branch (these keep the worktree on its
///   branch).
/// - **Bisect**: `BISECT_LOG` exists and `BISECT_START` names the target branch
///   or records the target tip.
///
/// The remaining list-to-effect TOCTOU — an operation started between the
/// worktree scan and the ref update — is a precondition enforced by Slice 2's
/// durable single owner, not by this stateless engine.
async fn worktree_holds_target_operation(
    config: &IntegrationConfig,
    entry: &WorktreeEntry,
    target_ref: &str,
    target_tip: Option<&str>,
) -> Result<bool> {
    if tokio::fs::metadata(&entry.path).await.is_err() {
        return Ok(false);
    }
    let Ok(git_dir) = private_git_dir(config, &entry.path).await else {
        return Ok(false);
    };
    let on_branch = entry.branch.as_deref() == Some(target_ref);
    let target_short = target_ref.strip_prefix(HEADS_PREFIX).unwrap_or(target_ref);

    // Rebase (either backend): head-name records the ref being rebased.
    for dir in ["rebase-merge", "rebase-apply"] {
        if let Ok(head_name) = tokio::fs::read_to_string(git_dir.join(dir).join("head-name")).await
            && head_name.trim() == target_ref
        {
            return Ok(true);
        }
    }

    // git am: rebase-apply exists on the target branch (no head-name, stays on branch).
    if on_branch
        && tokio::fs::metadata(git_dir.join("rebase-apply"))
            .await
            .is_ok()
    {
        return Ok(true);
    }

    // Merge, cherry-pick, revert: these keep the worktree on its branch.
    if on_branch {
        for marker in ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"] {
            if tokio::fs::metadata(git_dir.join(marker)).await.is_ok() {
                return Ok(true);
            }
        }
    }

    // Bisect: detaches HEAD; BISECT_START records the branch name or starting commit.
    if tokio::fs::metadata(git_dir.join("BISECT_LOG"))
        .await
        .is_ok()
        && let Ok(start) = tokio::fs::read_to_string(git_dir.join("BISECT_START")).await
    {
        let start = start.trim();
        if start == target_short || start == target_ref {
            return Ok(true);
        }
        if let Some(tip) = target_tip
            && start == tip
        {
            return Ok(true);
        }
    }

    Ok(false)
}

async fn merge_source(
    config: &IntegrationConfig,
    worktree: &Path,
    target_ref: &str,
    expected_tip: &str,
    source: &str,
) -> Result<String> {
    let branch = target_ref.strip_prefix(HEADS_PREFIX).unwrap_or(target_ref);
    let message = format!("rsi integration: merge {source} into {branch}");
    let args = [
        "merge",
        "--no-ff",
        "--no-edit",
        "--no-stat",
        "-q",
        "-m",
        message.as_str(),
        source,
    ];
    let output = git::run(config, worktree, &args).await?;
    if !output.status.success() {
        let unmerged = git::stdout_raw(
            config,
            worktree,
            &["diff", "--name-only", "--diff-filter=U", "-z"],
        )
        .await?;
        let paths: Vec<String> = unmerged
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect();
        if paths.is_empty() {
            return Err(git::failed(&args, &output));
        }
        let _ = git::run(config, worktree, &["merge", "--abort"]).await;
        return Err(Refusal::Conflict { paths }.into());
    }
    let parents = git::stdout(
        config,
        worktree,
        &["rev-list", "--parents", "-n", "1", "HEAD"],
    )
    .await?;
    let parents: Vec<&str> = parents.split_whitespace().collect();
    match parents.as_slice() {
        [candidate, first, second] if *first == expected_tip && *second == source => {
            Ok((*candidate).to_string())
        }
        _ => Err(IntegrationError::Git(
            "merge candidate does not have exactly the parents (target tip, source)".to_string(),
        )),
    }
}

async fn create_candidate_worktree(
    config: &IntegrationConfig,
    repo: &Path,
    scratch_dir: &Path,
    start: &str,
) -> Result<CandidateHandle> {
    let scratch = tokio::fs::canonicalize(scratch_dir)
        .await
        .map_err(|_| IntegrationError::InvalidInput("scratch_dir must exist"))?;
    if !tokio::fs::metadata(&scratch)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        return Err(IntegrationError::InvalidInput(
            "scratch_dir must be a directory",
        ));
    }
    let id = Uuid::new_v4().to_string();
    let worktree = scratch.join(format!("candidate-{id}"));
    let path = path_arg(&worktree)?;
    let args = ["worktree", "add", "--detach", "--quiet", path, start];
    let output = git::run(config, repo, &args).await?;
    if !output.status.success() {
        return Err(git::failed(&args, &output));
    }
    let marked = async {
        let marker = private_git_dir(config, &worktree)
            .await?
            .join(CANDIDATE_MARKER);
        tokio::fs::write(&marker, &id).await.map_err(|error| {
            IntegrationError::Git(format!("candidate marker write failed: {error}"))
        })
    }
    .await;
    if let Err(error) = marked {
        remove_worktree(config, repo, &worktree).await;
        return Err(error);
    }
    Ok(CandidateHandle { worktree, id })
}

/// Best-effort non-destructive removal of a worktree this call created moments ago.
async fn remove_worktree(config: &IntegrationConfig, repo: &Path, worktree: &Path) {
    if let Ok(path) = path_arg(worktree) {
        let _ = git::run(config, repo, &["worktree", "remove", path]).await;
    }
}

async fn private_git_dir(config: &IntegrationConfig, worktree: &Path) -> Result<PathBuf> {
    git::stdout(config, worktree, &["rev-parse", "--absolute-git-dir"])
        .await
        .map(PathBuf::from)
}

/// Freeze all candidate cleanup identities before writing Prepared.  Later
/// terminal settlement accepts only this exact registered worktree and its
/// private marker token; a path collision or substituted marker is preserved.
async fn candidate_cleanup_record(
    config: &IntegrationConfig,
    handle: &CandidateHandle,
    candidate_oid: &str,
    operation_id: Uuid,
) -> Result<CandidateCleanup> {
    let worktree = tokio::fs::canonicalize(&handle.worktree)
        .await
        .map_err(|error| {
            IntegrationError::Git(format!("candidate worktree canonicalize failed: {error}"))
        })?;
    let git_dir = private_git_dir(config, &worktree).await?;
    Ok(CandidateCleanup {
        operation_id,
        candidate_oid: candidate_oid.to_string(),
        marker: git_dir.join(CANDIDATE_MARKER),
        marker_token: handle.id.clone(),
        worktree,
        git_dir,
    })
}

async fn preflight_candidate_cleanup(
    config: &IntegrationConfig,
    repo: &Path,
    cleanup: &CandidateCleanup,
) -> Result<()> {
    if cleanup.operation_id.is_nil() || !super::canonical_oid(&cleanup.candidate_oid) {
        return Err(uncertain("candidate cleanup record is malformed"));
    }
    let registered = list_worktrees(config, repo)
        .await?
        .into_iter()
        .find(|entry| {
            entry.detached
                && std::fs::canonicalize(&entry.path).is_ok_and(|path| path == cleanup.worktree)
        });
    let Some(entry) = registered else {
        return if !cleanup.worktree.exists()
            && !cleanup.git_dir.exists()
            && !cleanup.marker.exists()
        {
            Ok(())
        } else {
            Err(uncertain(
                "candidate worktree registration is absent or replaced",
            ))
        };
    };
    let git_dir = private_git_dir(config, &entry.path).await?;
    if git_dir != cleanup.git_dir
        || cleanup.marker != git_dir.join(CANDIDATE_MARKER)
        || tokio::fs::read_to_string(&cleanup.marker)
            .await
            .ok()
            .is_none_or(|token| token.trim() != cleanup.marker_token)
    {
        return Err(uncertain("candidate cleanup proof no longer matches"));
    }
    let head = git::stdout(config, &cleanup.worktree, &["rev-parse", "HEAD^{commit}"]).await?;
    if head != cleanup.candidate_oid {
        return Err(uncertain(
            "candidate worktree HEAD differs from durable candidate",
        ));
    }
    let status = git::stdout_raw(
        config,
        &cleanup.worktree,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await?;
    if !status.is_empty() {
        return Err(uncertain(
            "candidate worktree is not clean; refusing non-force removal",
        ));
    }
    Ok(())
}

async fn cleanup_candidate_record(
    config: &IntegrationConfig,
    repo: &Path,
    cleanup: &CandidateCleanup,
) -> Result<()> {
    preflight_candidate_cleanup(config, repo, cleanup).await?;
    // A previously removed candidate is an idempotent terminal fact. For a
    // live candidate, re-run the exact proof immediately before removal.
    if !cleanup.worktree.exists() && !cleanup.git_dir.exists() && !cleanup.marker.exists() {
        return Ok(());
    }
    preflight_candidate_cleanup(config, repo, cleanup).await?;
    let path = path_arg(&cleanup.worktree)?;
    let output = git::run(config, repo, &["worktree", "remove", path]).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git::failed(&["worktree", "remove"], &output))
    }
}

async fn require_target_at(
    config: &IntegrationConfig,
    repo: &Path,
    target_ref: &str,
    expected_tip: &str,
) -> Result<()> {
    let observed = resolve_commit(config, repo, target_ref).await?;
    if observed.as_deref() == Some(expected_tip) {
        Ok(())
    } else {
        Err(Refusal::StaleTarget { observed }.into())
    }
}

async fn require_commit(config: &IntegrationConfig, repo: &Path, oid: &str) -> Result<()> {
    match resolve_commit(config, repo, oid).await? {
        // `^{commit}` peels tags; demand the named object itself is the commit.
        Some(resolved) if resolved == oid => Ok(()),
        _ => Err(Refusal::InvalidSource.into()),
    }
}

/// `None` when the revision does not exist or does not name a commit.
async fn resolve_commit(
    config: &IntegrationConfig,
    repo: &Path,
    revision: &str,
) -> Result<Option<String>> {
    let peeled = format!("{revision}^{{commit}}");
    let args = ["rev-parse", "--verify", "--quiet", peeled.as_str()];
    let output = git::run(config, repo, &args).await?;
    if !output.status.success() {
        return Ok(None);
    }
    let resolved = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if super::canonical_oid(&resolved) {
        Ok(Some(resolved))
    } else {
        Err(git::failed(&args, &output))
    }
}

async fn is_ancestor(
    config: &IntegrationConfig,
    repo: &Path,
    ancestor: &str,
    descendant: &str,
) -> Result<bool> {
    git::predicate(
        config,
        repo,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    )
    .await
}

async fn list_worktrees(config: &IntegrationConfig, repo: &Path) -> Result<Vec<WorktreeEntry>> {
    let listing = git::stdout(config, repo, &["worktree", "list", "--porcelain"]).await?;
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in listing.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            entries.extend(current.take());
            current = Some(WorktreeEntry {
                path: PathBuf::from(path),
                ..WorktreeEntry::default()
            });
        } else if let Some(entry) = current.as_mut() {
            if let Some(branch) = line.strip_prefix("branch ") {
                entry.branch = Some(branch.to_string());
            } else if line == "detached" {
                entry.detached = true;
            }
        }
    }
    entries.extend(current);
    Ok(entries)
}

fn path_arg(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or(IntegrationError::InvalidInput("path must be valid UTF-8"))
}

// ---------------------------------------------------------------------------
// RME-S2A-002: exclusive Git custody of one repo/target.
//
// Slice 1 closed the observable worktree races but left a scan-to-effect
// window: an operator or unmanaged Git process could check the target out or
// begin a rebase between the engine's worktree scan and its ref update. This
// custody closes that window with native Git index custody. The holder's
// private git dir receives an atomically created `index.lock` (a foreign lock
// is never deleted), a snapshotted alternate index, and a fixed JSON marker
// holding the full `CustodyRecord`. While the custody is active no other Git
// process can write the holder index, so a raw checkout or rebase on the
// target is refused and the engine is the sole owner of the tree.
//
// `TargetCustody` deliberately has no `Drop` cleanup: an abandoned custody
// leaves the marker and lock on disk as proof, and every later attempt fails
// closed with `CustodyHeld`.

/// Root for operation-owned, immutable custody proof directories.
const CUSTODY_MARKER: &str = "rsi-integration-custody";
const CUSTODY_RECORD_VERSION: u32 = 1;

/// Live exclusive custody of one integration target at its expected tip.
pub struct TargetCustody {
    record: CustodyRecord,
    /// Open handle to the atomically created `index.lock`. Keeping it open
    /// pins the lock for the custody's lifetime; dropping it without aborting
    /// intentionally leaves the lock and marker in place as the custody proof.
    index_lock: std::fs::File,
    config: IntegrationConfig,
    repo: PathBuf,
}

/// The lock identity currently authorized to mutate a holder. Durable phase
/// records keep their original identity; recovery binds a replacement through
/// append-only proofs without rewriting those records.
pub(super) struct ActiveCustodyLock {
    pub(super) identity: String,
    handle: Option<std::fs::File>,
}

#[cfg_attr(test, allow(dead_code))]
pub(super) fn ensure_active_lock(record: &CustodyRecord) -> Result<ActiveCustodyLock> {
    let path = record.git_dir.join("index.lock");
    let mut recovery_prefix_exists = false;
    for name in [
        "recovery-lock-staged",
        "recovery-lock-prepared.json",
        "recovery-lock-installed.json",
    ] {
        match std::fs::symlink_metadata(record.marker.join(name)) {
            Ok(_) => {
                recovery_prefix_exists = true;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(uncertain(format!("recovery proof stat failed: {error}"))),
        }
    }
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() != 0
            {
                return Err(uncertain(
                    "present index.lock is not an empty regular custody lock",
                ));
            }
            let identity = format!("{}:{}", metadata.dev(), metadata.ino());
            if identity == record.index_lock_identity && !recovery_prefix_exists {
                return Ok(ActiveCustodyLock {
                    identity,
                    handle: None,
                });
            }
            if identity == record.index_lock_identity {
                return Err(uncertain(
                    "historical index.lock and recovery prefix coexist",
                ));
            }
            // A live replacement lock is never an invitation to start a
            // recovery protocol. It is usable only when the *complete*
            // already-published recovery chain proves this exact inode.
            validate_recovery_proof_set(record)?;
            let handle = recovery_regular_empty(&record.marker.join("recovery-lock-staged"))?;
            if file_identity(&handle).map_err(|error| {
                IntegrationError::Git(format!("recovery staged identity failed: {error}"))
            })? != identity
            {
                return Err(uncertain(
                    "present index.lock differs from the validated recovery lock",
                ));
            }
            Ok(ActiveCustodyLock {
                identity,
                handle: Some(handle),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let (identity, handle) = reacquire_missing_lock(record)?;
            Ok(ActiveCustodyLock {
                identity,
                handle: Some(handle),
            })
        }
        Err(error) => Err(IntegrationError::Git(format!(
            "active index.lock stat failed: {error}"
        ))),
    }
}

impl TargetCustody {
    /// The durable record backing this custody, exactly as written to the
    /// holder's marker file.
    #[must_use]
    pub const fn record(&self) -> &CustodyRecord {
        &self.record
    }
}

impl std::fmt::Debug for TargetCustody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The held File is not Debug; the record and repo are what identify it.
        f.debug_struct("TargetCustody")
            .field("record", &self.record)
            .field("config", &self.config)
            .field("repo", &self.repo)
            .finish_non_exhaustive()
    }
}

/// Acquire exclusive custody of `target_ref` at `expected_tip`.
///
/// A target already checked out somewhere is held in its exact existing
/// holder. An unheld target is checked out by the engine into a fresh,
/// operation-unique worktree under `scratch_dir`, which becomes the holder.
/// Either way the holder's private git dir then receives the atomically
/// created `index.lock`, the snapshotted alternate index, and the fixed
/// marker, and `validate_acquired` re-observes the whole state before the
/// custody is returned.
///
/// # Errors
///
/// `Refused(CustodyHeld)` when `index.lock` or the fixed marker already
/// exists — a foreign lock is preserved, never deleted, and an engine worktree
/// created by this call is left untouched so the foreign Git process is never
/// disturbed; `Refused(TargetDenied)`, `Refused(TargetCustodyAmbiguous)`,
/// `InvalidInput`, or `Git` for the other expected outcomes. Any failure after
/// the engine worktree was created removes it exactly, or returns the combined
/// original-plus-cleanup error.
#[allow(clippy::too_many_lines)]
pub async fn acquire_target_custody(
    config: &IntegrationConfig,
    repo: &Path,
    target_ref: &str,
    expected_tip: &str,
    scratch_dir: &Path,
    operation_id: Uuid,
) -> Result<TargetCustody> {
    authorize_target(config, target_ref)?;
    require_oid(
        expected_tip,
        "expected_tip must be a full lowercase object name",
    )?;

    let worktrees = list_worktrees(config, repo).await?;
    let holders: Vec<&WorktreeEntry> = worktrees
        .iter()
        .filter(|entry| entry.branch.as_deref() == Some(target_ref))
        .collect();
    let (holder, engine_owned) = match holders.as_slice() {
        [] => (
            create_custody_worktree(config, repo, scratch_dir, operation_id, target_ref).await?,
            true,
        ),
        [holder] => (holder.path.clone(), false),
        _ => return Err(Refusal::TargetCustodyAmbiguous.into()),
    };

    // A failed record build must not leak an engine-owned worktree: remove it
    // exactly and report the original joined with any cleanup error.
    let mut record = match build_record(
        config,
        holder.clone(),
        engine_owned,
        operation_id,
        target_ref,
        expected_tip,
    )
    .await
    {
        Ok(record) => record,
        Err(original) if engine_owned => {
            return Err(
                match remove_engine_worktree_checked(config, repo, &holder).await {
                    Ok(()) => original,
                    Err(cleanup) => IntegrationError::Git(format!(
                        "{original}; custody cleanup additionally failed: {cleanup}"
                    )),
                },
            );
        }
        Err(original) => return Err(original),
    };

    let lock_path = record.git_dir.join("index.lock");
    let lock_file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(file) => {
            // The lock file's device:inode is recorded so a later cleanup or
            // recovery proves the on-disk lock is still exactly the one this
            // custody created before touching it.
            record.index_lock_identity = match file_identity(&file) {
                Ok(identity) => identity,
                Err(error) => {
                    drop(file);
                    let original = IntegrationError::Git(format!(
                        "custody index.lock identity failed: {error}"
                    ));
                    return Err(rollback_acquire(
                        config,
                        repo,
                        &record,
                        Created {
                            lock: true,
                            ..Created::default()
                        },
                        engine_owned,
                        original,
                    )
                    .await);
                }
            };
            file
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // A foreign lock: preserve it and, for an engine-owned holder, the
            // worktree too. Fail closed; nothing owned was created yet.
            return Err(Refusal::CustodyHeld {
                holder: record.holder,
            }
            .into());
        }
        Err(error) => {
            let original =
                IntegrationError::Git(format!("custody index.lock create failed: {error}"));
            return Err(rollback_acquire(
                config,
                repo,
                &record,
                Created::default(),
                engine_owned,
                original,
            )
            .await);
        }
    };
    let mut created = Created {
        lock: true,
        ..Created::default()
    };

    record.real_index_identity = regular_file_identity(&record.git_dir.join("index"))?;
    if let Err(original) = create_alt_index(&record, &mut created) {
        drop(lock_file);
        return Err(
            rollback_artifacts(config, repo, &record, created, engine_owned, original).await,
        );
    }
    record.alt_index_identity = regular_file_identity(&record.alt_index)?;
    if let Err(original) = write_marker(&mut record, &mut created) {
        drop(lock_file);
        return Err(
            rollback_artifacts(config, repo, &record, created, engine_owned, original).await,
        );
    }
    if let Err(original) = validate_acquired(config, repo, &record).await {
        drop(lock_file);
        return Err(rollback_acquire(config, repo, &record, created, engine_owned, original).await);
    }

    Ok(TargetCustody {
        record,
        index_lock: lock_file,
        config: config.clone(),
        repo: repo.to_path_buf(),
    })
}

/// End an acquired custody exactly.
///
/// The marker is re-read and must equal the record; the alternate index and
/// owned lock are removed; and for a held target the marker is removed last,
/// or for an engine-owned holder the exact worktree is removed while the
/// marker remains.
///
/// # Errors
///
/// `InvalidInput` unless the custody is still `CustodyPhase::Acquired`; `Git`
/// when the marker no longer matches, a known-created artifact is missing, or
/// an exact removal fails. On any error nothing further is removed, so the
/// marker stays in place as the custody proof.
pub async fn abort_target_custody(custody: TargetCustody) -> Result<()> {
    if custody.record.phase != CustodyPhase::Acquired {
        return Err(IntegrationError::InvalidInput(
            "only an Acquired custody can be aborted",
        ));
    }
    let TargetCustody {
        record,
        index_lock,
        config,
        repo,
    } = custody;
    acquired_cleanup(&config, &repo, &record, index_lock).await
}

/// Compose the durable record for a holder: the canonical holder path, the
/// private git dir, and the derived alternate-index and marker paths.
async fn build_record(
    config: &IntegrationConfig,
    holder: PathBuf,
    engine_owned: bool,
    operation_id: Uuid,
    target_ref: &str,
    expected_tip: &str,
) -> Result<CustodyRecord> {
    let holder = tokio::fs::canonicalize(&holder).await.map_err(|error| {
        IntegrationError::Git(format!("custody holder canonicalize failed: {error}"))
    })?;
    let git_dir = private_git_dir(config, &holder).await?;
    let alt_index = git_dir.join(format!("index.rsi-{operation_id}"));
    let marker = git_dir.join(CUSTODY_MARKER).join(operation_id.to_string());
    Ok(CustodyRecord {
        version: CUSTODY_RECORD_VERSION,
        operation_id,
        target_ref: target_ref.to_string(),
        expected_tip: expected_tip.to_string(),
        candidate: None,
        candidate_cleanup: None,
        artifact_manifest: None,
        proof_dir_identity: String::new(),
        index_lock_identity: String::new(),
        holder,
        git_dir,
        alt_index,
        alt_index_identity: String::new(),
        real_index_identity: String::new(),
        acquired_phase_identity: String::new(),
        prepared_phase_identity: String::new(),
        marker,
        engine_owned,
        phase: CustodyPhase::Acquired,
    })
}

/// Check the target out into a fresh engine-owned worktree under `scratch_dir`.
/// The short branch name (never `--detach`) keeps HEAD a symbolic ref onto the
/// target, so the worktree registers as its holder and Git itself refuses a
/// second checkout of the same branch anywhere else.
async fn create_custody_worktree(
    config: &IntegrationConfig,
    repo: &Path,
    scratch_dir: &Path,
    operation_id: Uuid,
    target_ref: &str,
) -> Result<PathBuf> {
    let scratch = tokio::fs::canonicalize(scratch_dir)
        .await
        .map_err(|_| IntegrationError::InvalidInput("scratch_dir must exist"))?;
    if !tokio::fs::metadata(&scratch)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        return Err(IntegrationError::InvalidInput(
            "scratch_dir must be a directory",
        ));
    }
    let holder = scratch.join(format!("rsi-custody-{operation_id}"));
    let short = target_ref.strip_prefix(HEADS_PREFIX).unwrap_or(target_ref);
    let path = path_arg(&holder)?;
    let args = ["worktree", "add", "--quiet", path, short];
    let output = git::run(config, repo, &args).await?;
    if output.status.success() {
        Ok(holder)
    } else {
        Err(git::failed(&args, &output))
    }
}

/// Atomically snapshot the holder's real index to the alternate index file.
/// The snapshot is empty only when the real index does not exist. The
/// `created` flag is set the moment `create_new` succeeds, so a failure while
/// copying or syncing still leaves a partial file that rollback knows about.
fn create_alt_index(record: &CustodyRecord, created: &mut Created) -> Result<()> {
    let mut alt = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&record.alt_index)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // This call did not create the alternate index; treat it as
            // foreign and fail closed without deleting it.
            return Err(Refusal::CustodyHeld {
                holder: record.holder.clone(),
            }
            .into());
        }
        Err(error) => {
            return Err(IntegrationError::Git(format!(
                "custody alternate index create failed: {error}"
            )));
        }
    };
    created.alt = true;
    let mut source = match std::fs::File::open(record.git_dir.join("index")) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(IntegrationError::Git(format!(
                "custody index read failed: {error}"
            )));
        }
    };
    if let Some(source) = source.as_mut() {
        std::io::copy(source, &mut alt).map_err(|error| {
            IntegrationError::Git(format!("custody alternate index copy failed: {error}"))
        })?;
    }
    alt.sync_all().map_err(|error| {
        IntegrationError::Git(format!("custody alternate index sync failed: {error}"))
    })?;
    Ok(())
}

/// Atomically publish an operation-owned proof directory and its first,
/// immutable Acquired record. Existing operation paths are always foreign.
fn write_marker(record: &mut CustodyRecord, created: &mut Created) -> Result<()> {
    let root = record
        .marker
        .parent()
        .ok_or(IntegrationError::InvalidInput(
            "custody operation marker has no root",
        ))?;
    match std::fs::create_dir(root) {
        Ok(()) => sync_parent_dir(root)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(IntegrationError::Git(format!(
                "custody root create failed: {error}"
            )));
        }
    }
    match std::fs::create_dir(&record.marker) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Refusal::CustodyHeld {
                holder: record.holder.clone(),
            }
            .into());
        }
        Err(error) => {
            return Err(IntegrationError::Git(format!(
                "custody operation directory create failed: {error}"
            )));
        }
    }
    let metadata = std::fs::symlink_metadata(&record.marker).map_err(|error| {
        IntegrationError::Git(format!("custody operation directory stat failed: {error}"))
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(uncertain("custody operation proof is not a real directory"));
    }
    record.proof_dir_identity = format!("{}:{}", metadata.dev(), metadata.ino());
    created.marker = true;
    let reservation = reserve_phase_record(record)?;
    write_reserved_phase_record(record, reservation)?;
    sync_parent_dir(&record.marker)?;
    Ok(())
}

/// Re-observe the full custody state once lock, alternate index, and marker
/// are in place. Every check applies to both held and engine-owned holders,
/// and each drift class is reported as its existing typed refusal.
async fn validate_acquired(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
) -> Result<()> {
    // Rescan: exactly one target holder, at exactly the recorded holder path.
    let worktrees = list_worktrees(config, repo).await?;
    let holders: Vec<&WorktreeEntry> = worktrees
        .iter()
        .filter(|entry| entry.branch.as_deref() == Some(record.target_ref.as_str()))
        .collect();
    if holders.len() != 1 {
        return Err(Refusal::TargetCustodyAmbiguous.into());
    }
    let rescan_path = tokio::fs::canonicalize(&holders[0].path)
        .await
        .map_err(|_| IntegrationError::Refused(Refusal::TargetCustodyAmbiguous))?;
    if rescan_path != record.holder {
        return Err(Refusal::TargetCustodyAmbiguous.into());
    }

    // HEAD is a symbolic ref onto the target; a detached HEAD or a different
    // branch is stale, observed at the holder's current HEAD commit.
    match git::stdout(config, &record.holder, &["symbolic-ref", "HEAD"]).await {
        Ok(head) if head == record.target_ref => {}
        _ => {
            return Err(Refusal::StaleTarget {
                observed: resolve_holder_head(config, &record.holder).await,
            }
            .into());
        }
    }
    match resolve_holder_head(config, &record.holder).await {
        Some(oid) if oid == record.expected_tip => {}
        observed => return Err(Refusal::StaleTarget { observed }.into()),
    }

    // The ref itself still resolves to the expected tip.
    match resolve_commit(config, repo, &record.target_ref).await? {
        Some(oid) if oid == record.expected_tip => {}
        observed => return Err(Refusal::StaleTarget { observed }.into()),
    }

    // Strict porcelain, including any untracked file: the holder is clean.
    let status = git::stdout_raw(
        config,
        &record.holder,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await?;
    if !status.is_empty() {
        return Err(Refusal::TargetWorktreeDirty {
            path: record.holder.clone(),
        }
        .into());
    }

    // No in-progress Git operation owns the target in ANY worktree.
    for entry in &worktrees {
        if worktree_holds_target_operation(
            config,
            entry,
            &record.target_ref,
            Some(&record.expected_tip),
        )
        .await?
        {
            return Err(Refusal::TargetOperationInProgress {
                path: entry.path.clone(),
            }
            .into());
        }
    }
    Ok(())
}

/// Best-effort HEAD commit of a worktree, used as the observed value of a
/// `StaleTarget` refusal.
async fn resolve_holder_head(config: &IntegrationConfig, holder: &Path) -> Option<String> {
    match git::stdout(config, holder, &["rev-parse", "--verify", "HEAD^{commit}"]).await {
        Ok(oid) if super::canonical_oid(&oid) => Some(oid),
        _ => None,
    }
}

/// Which owned artifacts a failed acquisition created before it errored. The
/// flags are set the moment each `create_new` succeeds so that even a partial
/// artifact is rolled back exactly.
#[derive(Default)]
struct Created {
    lock: bool,
    alt: bool,
    marker: bool,
}

/// Checked exact rollback after a failed acquisition: remove exactly the
/// artifacts `created` records (marker last, only after recomparing it), and
/// for an engine-owned holder remove that exact worktree. The original error
/// is returned unchanged when cleanup fully succeeds, or as `original;
/// custody cleanup additionally failed: <cleanup>` when it does not.
async fn rollback_acquire(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
    created: Created,
    engine_owned: bool,
    original: IntegrationError,
) -> IntegrationError {
    match cleanup_created(config, repo, record, &created, engine_owned).await {
        Ok(()) => original,
        Err(cleanup) => IntegrationError::Git(format!(
            "{original}; custody cleanup additionally failed: {cleanup}"
        )),
    }
}

/// Rollback when an artifact step discovered a path this call did not create
/// (`CustodyHeld`): only proven-owned files are removed, the foreign path is
/// preserved, and an engine-owned worktree is never removed because its
/// private git dir holds that foreign path. Returns the original refusal when
/// the proven-owned cleanup fully succeeds.
async fn rollback_artifacts(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
    created: Created,
    engine_owned: bool,
    original: IntegrationError,
) -> IntegrationError {
    if matches!(
        original,
        IntegrationError::Refused(Refusal::CustodyHeld { .. })
    ) {
        rollback_custody_held(record, &created, original).await
    } else {
        rollback_acquire(config, repo, record, created, engine_owned, original).await
    }
}

async fn rollback_custody_held(
    record: &CustodyRecord,
    created: &Created,
    original: IntegrationError,
) -> IntegrationError {
    match cleanup_proven_only(record, created).await {
        Ok(()) => original,
        Err(cleanup) => IntegrationError::Git(format!(
            "{original}; custody cleanup additionally failed: {cleanup}"
        )),
    }
}

/// Remove exactly the artifacts this call created via `create_new`. A path
/// whose creation failed is never touched, so a foreign lock, alternate
/// index, or marker survives untouched.
async fn cleanup_proven_only(record: &CustodyRecord, created: &Created) -> Result<()> {
    if created.alt {
        remove_owned_exact(&record.alt_index, &record.alt_index_identity).await?;
    }
    if created.lock {
        remove_owned_exact(
            &record.git_dir.join("index.lock"),
            &record.index_lock_identity,
        )
        .await?;
    }
    Ok(())
}

async fn cleanup_created(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
    created: &Created,
    engine_owned: bool,
) -> Result<()> {
    // Proof discipline first, for both held and engine-owned holders: if this
    // call wrote the marker, nothing is removed until the marker is re-read
    // and matches the record exactly.
    let proof_unlinks = if created.marker {
        require_marker_matches(record).await?;
        Some(teardown_expected_unlinks(record).await?)
    } else {
        None
    };
    if engine_owned && created.marker {
        preflight_engine_worktree_removal(config, repo, &record.holder).await?;
    }
    if created.alt {
        remove_owned_exact(&record.alt_index, &record.alt_index_identity).await?;
    }
    if created.lock {
        remove_owned_exact(
            &record.git_dir.join("index.lock"),
            &record.index_lock_identity,
        )
        .await?;
    }
    if engine_owned {
        // The marker stays inside the private git dir; removing the exact
        // worktree takes it away with the git dir. If removal fails the
        // marker remains as proof.
        return remove_engine_worktree_checked(config, repo, &record.holder).await;
    }
    if created.marker {
        // Existing holder: the marker goes LAST, after the recomparison above.
        remove_preflighted_proof_directory(
            record,
            proof_unlinks.expect("marker preflight built an unlink plan"),
        )
        .await?;
    }
    Ok(())
}

/// Full cleanup of a custody that reached the `Acquired` phase (the abort
/// path). Ordered, exact, and proof-preserving:
///
/// 1. Re-read the marker and require full record equality; nothing is touched
///    on any mismatch.
/// 2. Before any mutation, validate the complete marker unlink map, alternate
///    index, active lock, and (where applicable) engine holder removal proof.
///    Existing holders then remove the alternate index, close the lock `File`,
///    remove the owned `index.lock`, then the marker LAST.
/// 3. Engine-owned holder: remove the alternate index, close and remove the
///    owned lock, then exact non-force `git worktree remove` while the marker
///    remains. On success the marker is not removed separately: the private
///    git dir that held it was removed with the worktree.
async fn acquired_cleanup(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
    index_lock: std::fs::File,
) -> Result<()> {
    require_marker_matches(record).await?;
    let proof_unlinks = teardown_expected_unlinks(record).await?;
    if regular_file_identity(&record.alt_index)? != record.alt_index_identity {
        return Err(uncertain("alternate index changed before Acquired cleanup"));
    }
    validate_lock_identity(record, &record.index_lock_identity)?;
    if record.engine_owned {
        preflight_engine_worktree_removal(config, repo, &record.holder).await?;
    }
    remove_owned_exact(&record.alt_index, &record.alt_index_identity).await?;
    drop(index_lock);
    remove_owned_exact(
        &record.git_dir.join("index.lock"),
        &record.index_lock_identity,
    )
    .await?;
    if record.engine_owned {
        return remove_engine_worktree_checked(config, repo, &record.holder).await;
    }
    remove_preflighted_proof_directory(record, proof_unlinks).await?;
    Ok(())
}

/// Remove only an exact regular inode proved by the operation. The final
/// metadata read is adjacent to unlink so pathname replacement, symlinks, and
/// non-regular collisions fail closed.
async fn remove_owned_exact(path: &Path, expected_identity: &str) -> Result<()> {
    if regular_file_identity(path)? != expected_identity {
        return Err(uncertain("owned artifact identity changed before removal"));
    }
    tokio::fs::remove_file(path).await.map_err(|error| {
        IntegrationError::Git(format!(
            "custody exact artifact removal failed ({}): {error}",
            path.display()
        ))
    })
}

/// A deletion authorized entirely by evidence read before terminal cleanup
/// starts. `canonical` is retained for immutable JSON records so an inode
/// whose bytes change after preflight is not unlinked.
struct OwnedUnlink {
    path: PathBuf,
    identity: String,
    canonical: Option<Vec<u8>>,
}

async fn remove_planned_owned(entry: &OwnedUnlink) -> Result<()> {
    if regular_file_identity(&entry.path)? != entry.identity {
        return Err(uncertain("owned artifact identity changed before removal"));
    }
    if let Some(expected) = &entry.canonical {
        let actual = tokio::fs::read(&entry.path)
            .await
            .map_err(|error| uncertain(format!("owned artifact reread failed: {error}")))?;
        if actual != *expected {
            return Err(uncertain(
                "owned immutable artifact bytes changed before removal",
            ));
        }
    }
    remove_owned_exact(&entry.path, &entry.identity).await
}

/// Build every proof-directory unlink from durable record and manifest
/// authority. This is deliberately read-only: terminal cleanup must know the
/// complete delete set before it changes even one candidate, lock, or proof.
async fn teardown_expected_unlinks(record: &CustodyRecord) -> Result<Vec<OwnedUnlink>> {
    validate_proof_dir(record)?;
    preflight_proof_directory(record).await?;
    validate_recovery_proof_set(record)?;
    let mut planned = Vec::new();
    if record.phase != CustodyPhase::Acquired {
        let manifest = read_artifact_manifest(record)?;
        for kind in artifact_kinds() {
            let entry = manifest
                .artifacts
                .iter()
                .find(|entry| entry.kind == kind)
                .ok_or_else(|| uncertain("artifact manifest entry is missing"))?;
            let path = artifact_path(record, kind);
            let actual = match std::fs::symlink_metadata(&path) {
                Ok(metadata)
                    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
                {
                    format!("{}:{}", metadata.dev(), metadata.ino())
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && matches!(
                            record.phase,
                            CustodyPhase::Applying | CustodyPhase::Applied
                        )
                        && matches!(
                            kind,
                            ArtifactKind::NextApplyBuild | ArtifactKind::NextRestoreBuild
                        ) =>
                {
                    // A build inode is consumed only after its bytes were
                    // copied into its stable manifest-owned destination.
                    continue;
                }
                _ => {
                    return Err(uncertain(
                        "manifest artifact is missing or not a regular file",
                    ));
                }
            };
            let identity = if kind == ArtifactKind::NextTmp
                && matches!(record.phase, CustodyPhase::Applying | CustodyPhase::Applied)
                && actual == record.alt_index_identity
            {
                record.alt_index_identity.clone()
            } else {
                entry.identity.clone()
            };
            if actual != identity {
                return Err(uncertain(
                    "manifest artifact identity is not phase-authorized",
                ));
            }
            let canonical = match kind {
                ArtifactKind::NextPrepared
                | ArtifactKind::NextApplying
                | ArtifactKind::NextApplied => {
                    let bytes = std::fs::read(&path).map_err(|error| {
                        uncertain(format!("immutable transition reread failed: {error}"))
                    })?;
                    Some(bytes)
                }
                _ => None,
            };
            planned.push(OwnedUnlink {
                path,
                identity,
                canonical,
            });
        }
        let binding = record
            .artifact_manifest
            .as_ref()
            .expect("non-Acquired has manifest");
        let bytes = std::fs::read(&binding.path)
            .map_err(|error| uncertain(format!("manifest reread failed: {error}")))?;
        if digest_bytes(&bytes) != binding.digest {
            return Err(uncertain(
                "manifest bytes changed during teardown preflight",
            ));
        }
        planned.push(OwnedUnlink {
            path: binding.path.clone(),
            identity: binding.identity.clone(),
            canonical: Some(bytes),
        });
    }
    for phase in [
        CustodyPhase::Acquired,
        CustodyPhase::Prepared,
        CustodyPhase::Applying,
        CustodyPhase::Applied,
    ] {
        let path = phase_record_path(record, phase);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(uncertain(format!("phase proof stat failed: {error}"))),
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(uncertain("phase proof is not a regular file"));
        }
        let identity = match phase {
            CustodyPhase::Acquired => record.acquired_phase_identity.clone(),
            CustodyPhase::Prepared => record.prepared_phase_identity.clone(),
            CustodyPhase::Applying => {
                read_artifact_manifest(record)?
                    .artifacts
                    .into_iter()
                    .find(|entry| entry.kind == ArtifactKind::NextApplying)
                    .ok_or_else(|| uncertain("manifest lacks Applying authority"))?
                    .identity
            }
            CustodyPhase::Applied => {
                read_artifact_manifest(record)?
                    .artifacts
                    .into_iter()
                    .find(|entry| entry.kind == ArtifactKind::NextApplied)
                    .ok_or_else(|| uncertain("manifest lacks Applied authority"))?
                    .identity
            }
        };
        if identity.is_empty() || format!("{}:{}", metadata.dev(), metadata.ino()) != identity {
            return Err(uncertain("phase proof identity is not durably authorized"));
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| uncertain(format!("phase proof reread failed: {error}")))?;
        let proof: CustodyRecord = serde_json::from_slice(&bytes)
            .map_err(|error| uncertain(format!("phase proof parse failed: {error}")))?;
        if serde_json::to_vec(&proof).ok().as_deref() != Some(bytes.as_slice())
            || proof.phase != phase
            || !phase_proof_belongs_to(record, &proof)
        {
            return Err(uncertain("phase proof is not canonical durable evidence"));
        }
        planned.push(OwnedUnlink {
            path,
            identity,
            canonical: Some(bytes),
        });
    }
    let staged = record.marker.join("recovery-lock-staged");
    match std::fs::symlink_metadata(&staged) {
        Ok(_) => {
            let proof = read_recovery_proof(&record.marker.join("recovery-lock-prepared.json"))?;
            planned.push(OwnedUnlink {
                path: staged,
                identity: proof.replacement_lock_identity,
                canonical: None,
            });
            for (name, identity) in [
                ("recovery-lock-prepared.json", proof.prepared_proof_identity),
                (
                    "recovery-lock-installed.json",
                    proof.installed_proof_identity,
                ),
            ] {
                let path = record.marker.join(name);
                let bytes = std::fs::read(&path)
                    .map_err(|error| uncertain(format!("recovery proof reread failed: {error}")))?;
                planned.push(OwnedUnlink {
                    path,
                    identity,
                    canonical: Some(bytes),
                });
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(uncertain(format!(
                "recovery staged lock stat failed: {error}"
            )));
        }
    }
    Ok(planned)
}

async fn remove_proof_directory(record: &CustodyRecord) -> Result<()> {
    let planned = teardown_expected_unlinks(record).await?;
    remove_preflighted_proof_directory(record, planned).await
}

/// Consume an unlink map which was fully validated before any terminal
/// mutation. Each entry still validates its durable inode and immutable bytes
/// immediately before unlinking, so a post-preflight swap remains fail-closed.
async fn remove_preflighted_proof_directory(
    record: &CustodyRecord,
    planned: Vec<OwnedUnlink>,
) -> Result<()> {
    for entry in &planned {
        remove_planned_owned(entry).await?;
    }
    match tokio::fs::remove_dir(&record.marker).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(uncertain(format!(
            "proof directory removal refused: {error}"
        ))),
    }
}

fn validate_proof_dir(record: &CustodyRecord) -> Result<()> {
    let metadata = std::fs::symlink_metadata(&record.marker)
        .map_err(|error| uncertain(format!("proof directory stat failed: {error}")))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(uncertain("proof path is not an owned real directory"));
    }
    let identity = format!("{}:{}", metadata.dev(), metadata.ino());
    if record.proof_dir_identity.is_empty() || identity != record.proof_dir_identity {
        return Err(uncertain("proof directory identity changed"));
    }
    Ok(())
}

async fn preflight_proof_directory(record: &CustodyRecord) -> Result<()> {
    let mut entries = tokio::fs::read_dir(&record.marker)
        .await
        .map_err(|error| uncertain(format!("proof directory preflight failed: {error}")))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| uncertain(format!("proof directory entry failed: {error}")))?
    {
        let path = entry.path();
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|error| uncertain(format!("proof metadata failed: {error}")))?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(uncertain("proof directory contains non-regular entry"));
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(uncertain("proof filename is not UTF-8"));
        };
        if !matches!(
            name,
            "acquired.json"
                | "prepared.json"
                | "applying.json"
                | "applied.json"
                | "next-prepared"
                | "next-applying"
                | "next-applied"
                | "next-status"
                | "next-verify-index"
                | "next-restore"
                | "next-restore-build"
                | "next-apply-build"
                | "next-tmp"
                | "artifact-manifest.json"
                | "recovery-lock-staged"
                | "recovery-lock-prepared.json"
                | "recovery-lock-installed.json"
        ) {
            return Err(uncertain("proof directory contains foreign entry"));
        }
        if record.phase == CustodyPhase::Acquired
            && matches!(
                name,
                "next-prepared"
                    | "next-applying"
                    | "next-applied"
                    | "next-status"
                    | "next-verify-index"
                    | "next-restore"
                    | "next-restore-build"
                    | "next-apply-build"
                    | "next-tmp"
                    | "artifact-manifest.json"
            )
        {
            return Err(uncertain(
                "unproved artifact prefix exists before Prepared publication",
            ));
        }
        if matches!(
            name,
            "acquired.json" | "prepared.json" | "applying.json" | "applied.json"
        ) {
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|error| uncertain(format!("phase proof read failed: {error}")))?;
            let proof: CustodyRecord = serde_json::from_slice(&bytes)
                .map_err(|error| uncertain(format!("phase proof parse failed: {error}")))?;
            let canonical = serde_json::to_vec(&proof).map_err(|error| {
                IntegrationError::Git(format!("phase proof serialize failed: {error}"))
            })?;
            if bytes != canonical
                || phase_record_path(&proof, proof.phase) != path
                || !phase_proof_belongs_to(record, &proof)
            {
                return Err(uncertain(format!(
                    "phase proof is not exact operation evidence: {name}"
                )));
            }
        }
    }
    if record.phase != CustodyPhase::Acquired {
        validate_artifact_manifest(record)?;
    }
    Ok(())
}

fn phase_proof_belongs_to(record: &CustodyRecord, proof: &CustodyRecord) -> bool {
    if proof.version != record.version
        || proof.operation_id != record.operation_id
        || proof.target_ref != record.target_ref
        || proof.expected_tip != record.expected_tip
        || proof.holder != record.holder
        || proof.git_dir != record.git_dir
        || proof.alt_index != record.alt_index
        || proof.alt_index_identity != record.alt_index_identity
        || proof.real_index_identity != record.real_index_identity
        || proof.marker != record.marker
        || proof.proof_dir_identity != record.proof_dir_identity
        || proof.engine_owned != record.engine_owned
        || proof.index_lock_identity != record.index_lock_identity
    {
        return false;
    }
    if proof.acquired_phase_identity != record.acquired_phase_identity
        || (proof.phase != CustodyPhase::Acquired
            && proof.prepared_phase_identity != record.prepared_phase_identity)
        || (proof.phase == CustodyPhase::Acquired && !proof.prepared_phase_identity.is_empty())
    {
        return false;
    }
    let manifest_ok = if proof.phase == CustodyPhase::Acquired {
        proof.artifact_manifest.is_none()
    } else {
        proof.artifact_manifest == record.artifact_manifest && proof.artifact_manifest.is_some()
    };
    if !manifest_ok {
        return false;
    }
    if proof.phase == CustodyPhase::Acquired {
        proof.candidate.is_none() && proof.candidate_cleanup.is_none()
    } else {
        proof.candidate == record.candidate && proof.candidate_cleanup == record.candidate_cleanup
    }
}

/// Validate recovery evidence as a complete immutable set. A crash may leave
/// no recovery entries, or all three entries; any partial/corrupt set is kept
/// intact and reported Uncertain rather than being "cleaned" into ambiguity.
fn validate_recovery_proof_set(record: &CustodyRecord) -> Result<()> {
    let staged = record.marker.join("recovery-lock-staged");
    let prepared = record.marker.join("recovery-lock-prepared.json");
    let installed = record.marker.join("recovery-lock-installed.json");
    let present = [&staged, &prepared, &installed]
        .into_iter()
        .map(|path| match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(uncertain(format!("recovery proof stat failed: {error}"))),
        })
        .collect::<Result<Vec<_>>>()?;
    if present.iter().all(|present| !present) {
        return Ok(());
    }
    if present.iter().any(|present| !present) {
        return Err(uncertain("recovery proof set is partial"));
    }
    let staged_file = recovery_regular_empty(&staged)?;
    let identity = file_identity(&staged_file).map_err(|error| {
        IntegrationError::Git(format!("recovery staged identity failed: {error}"))
    })?;
    drop(staged_file);
    let prepared_proof = read_recovery_proof(&prepared)?;
    let installed_proof = read_recovery_proof(&installed)?;
    if regular_file_identity(&prepared)? != prepared_proof.prepared_proof_identity
        || regular_file_identity(&installed)? != prepared_proof.installed_proof_identity
    {
        return Err(uncertain("recovery proof inode changed"));
    }
    if prepared_proof != installed_proof
        || prepared_proof.version != 1
        || prepared_proof.prepared_proof_identity.is_empty()
        || prepared_proof.installed_proof_identity.is_empty()
        || prepared_proof.operation_id != record.operation_id
        || prepared_proof.target_ref != record.target_ref
        || prepared_proof.expected_tip != record.expected_tip
        || prepared_proof.candidate != record.candidate
        || prepared_proof.holder != record.holder
        || prepared_proof.git_dir != record.git_dir
        || prepared_proof.proof_dir_identity != record.proof_dir_identity
        || prepared_proof.prior_lock_identity != record.index_lock_identity
        || prepared_proof.replacement_lock_identity != identity
        || prepared_proof.staged_lock != staged
        || prepared_proof.installed_lock != record.git_dir.join("index.lock")
        || phase_rank(prepared_proof.phase) > phase_rank(record.phase)
    {
        return Err(uncertain(
            "recovery proof does not bind this custody exactly",
        ));
    }
    Ok(())
}

fn read_recovery_proof(path: &Path) -> Result<LockRecoveryProof> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| uncertain(format!("recovery proof stat failed: {error}")))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(uncertain("recovery proof is not a regular file"));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| uncertain(format!("recovery proof read failed: {error}")))?;
    let proof: LockRecoveryProof = serde_json::from_slice(&bytes)
        .map_err(|error| uncertain(format!("recovery proof is malformed: {error}")))?;
    let canonical = serde_json::to_vec(&proof).map_err(|error| {
        IntegrationError::Git(format!("recovery proof serialize failed: {error}"))
    })?;
    if bytes != canonical {
        return Err(uncertain("recovery proof bytes are not canonical"));
    }
    Ok(proof)
}

/// Re-read the marker and require it to hold exactly this record. Anything
/// else means the custody a later reader observes is not ours, and cleanup
/// must stop without modifying the proof.
const fn phase_name(phase: CustodyPhase) -> &'static str {
    match phase {
        CustodyPhase::Acquired => "acquired.json",
        CustodyPhase::Prepared => "prepared.json",
        CustodyPhase::Applying => "applying.json",
        CustodyPhase::Applied => "applied.json",
    }
}

fn phase_record_path(record: &CustodyRecord, phase: CustodyPhase) -> PathBuf {
    record.marker.join(phase_name(phase))
}

struct PhaseRecordReservation {
    path: PathBuf,
    identity: String,
    file: std::fs::File,
}

fn reserve_phase_record(record: &mut CustodyRecord) -> Result<PhaseRecordReservation> {
    if !matches!(
        record.phase,
        CustodyPhase::Acquired | CustodyPhase::Prepared
    ) {
        return Err(IntegrationError::InvalidInput(
            "only Acquired or Prepared phase records may reserve an inode",
        ));
    }
    let path = phase_record_path(record, record.phase);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                uncertain("an immutable custody phase record already exists")
            } else {
                IntegrationError::Git(format!("custody phase create failed: {error}"))
            }
        })?;
    let identity = file_identity(&file).map_err(|error| {
        IntegrationError::Git(format!("custody phase identity failed: {error}"))
    })?;
    match record.phase {
        CustodyPhase::Acquired => record.acquired_phase_identity = identity.clone(),
        CustodyPhase::Prepared => record.prepared_phase_identity = identity.clone(),
        CustodyPhase::Applying | CustodyPhase::Applied => unreachable!(),
    }
    Ok(PhaseRecordReservation {
        path,
        identity,
        file,
    })
}

fn write_reserved_phase_record(
    record: &CustodyRecord,
    mut reservation: PhaseRecordReservation,
) -> Result<()> {
    let expected = match record.phase {
        CustodyPhase::Acquired => &record.acquired_phase_identity,
        CustodyPhase::Prepared => &record.prepared_phase_identity,
        CustodyPhase::Applying | CustodyPhase::Applied => {
            return Err(IntegrationError::InvalidInput("unbound phase reservation"));
        }
    };
    if &reservation.identity != expected
        || phase_record_path(record, record.phase) != reservation.path
        || file_identity(&reservation.file).map_err(|error| {
            IntegrationError::Git(format!("custody phase identity failed: {error}"))
        })? != *expected
    {
        return Err(uncertain("reserved phase inode changed before write"));
    }
    let bytes = serde_json::to_vec(record).map_err(|error| {
        IntegrationError::Git(format!("custody phase serialize failed: {error}"))
    })?;
    reservation
        .file
        .write_all(&bytes)
        .and_then(|()| reservation.file.sync_all())
        .map_err(|error| IntegrationError::Git(format!("custody phase write failed: {error}")))?;
    if regular_file_identity(&reservation.path)? != *expected {
        return Err(uncertain("reserved phase inode changed after write"));
    }
    sync_parent_dir(&reservation.path)
}

async fn require_marker_matches(record: &CustodyRecord) -> Result<()> {
    validate_proof_dir(record)?;
    let bytes = tokio::fs::read(phase_record_path(record, record.phase))
        .await
        .map_err(|error| IntegrationError::Git(format!("custody marker read failed: {error}")))?;
    let stored: CustodyRecord = serde_json::from_slice(&bytes)
        .map_err(|error| IntegrationError::Git(format!("custody marker parse failed: {error}")))?;
    if stored != *record {
        return Err(IntegrationError::Git(
            "custody marker does not match the acquired record; refusing to modify it".to_string(),
        ));
    }
    Ok(())
}

/// Validate the exact registration, canonical path, and cleanliness required
/// for non-force engine-holder removal. Kept separate so Acquired cleanup can
/// prove this before it removes the alternate index or drops the lock guard.
async fn preflight_engine_worktree_removal(
    config: &IntegrationConfig,
    repo: &Path,
    holder: &Path,
) -> Result<PathBuf> {
    let wanted = tokio::fs::canonicalize(holder).await.map_err(|error| {
        IntegrationError::Git(format!("engine holder canonicalize failed: {error}"))
    })?;
    let registered = list_worktrees(config, repo)
        .await?
        .into_iter()
        .any(|entry| std::fs::canonicalize(entry.path).is_ok_and(|path| path == wanted));
    if !registered {
        return Err(uncertain(
            "engine holder is no longer an exact registered worktree",
        ));
    }
    let status = git::stdout_raw(
        config,
        &wanted,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await?;
    if !status.is_empty() {
        return Err(uncertain(
            "engine holder is not clean; refusing non-force removal",
        ));
    }
    Ok(wanted)
}

/// Exact removal of an engine-created worktree, used after a failed
/// acquisition once its owned artifacts are gone. Re-run the preflight at the
/// mutation boundary to reject a registration or cleanliness race.
async fn remove_engine_worktree_checked(
    config: &IntegrationConfig,
    repo: &Path,
    holder: &Path,
) -> Result<()> {
    let wanted = preflight_engine_worktree_removal(config, repo, holder).await?;
    let args = ["worktree", "remove", path_arg(&wanted)?];
    let output = git::run(config, repo, &args).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git::failed(&args, &output))
    }
}

// ---------------------------------------------------------------------------
// RME-S2A-002 Stage B1: custody-guarded publication and record-only recovery.
//
// prepare_candidate and publish run under an already-held custody. Publish
// drives a bounded `update-ref --stdin` reference transaction whose constant
// daemon-owned hook records Applying, applies the candidate, and promotes the
// synced alternate index while the ref lock is held.  The committed callback
// is the first place which records Applied. The owned index.lock is never
// renamed: it stays as a sentinel from acquisition through cleanup, and its
// device:inode identity is recorded and re-validated before it is removed.
//
// The recovery matrix is record-only: it works from a persisted CustodyRecord
// with no open lock file. A missing marker after persisted custody is always
// Uncertain (no proof to settle from); every mismatch preserves the proof.

/// Constant daemon-owned reference-transaction hook. Nothing is interpolated
/// into these bytes: every path, ref, and OID enters only through environment
/// variables. `preparing` has no ref locks and validates only; `prepared`
/// holds the locks and is the only mutation phase.
const REFERENCE_TRANSACTION_HOOK: &str = r#"#!/bin/sh
set -eu
PATH=/usr/bin:/bin
export PATH
refuse() { echo "rsi-custody-hook: $1" >&2; exit 1; }
sync_file() { sync -d "$1" || refuse "sync failed: $1"; }
: "${RSI_CUSTODY_TARGET_REF:?required}"
: "${RSI_CUSTODY_EXPECTED_OID:?required}"
: "${RSI_CUSTODY_CANDIDATE_OID:?required}"
: "${RSI_CUSTODY_HOLDER:?required}"
: "${RSI_CUSTODY_MARKER:?required}"
: "${RSI_CUSTODY_APPLYING_RECORD:?required}"
: "${RSI_CUSTODY_APPLIED_RECORD:?required}"
: "${RSI_CUSTODY_ALT_INDEX:?required}"
: "${RSI_CUSTODY_REAL_INDEX:?required}"
: "${RSI_CUSTODY_LOCK_PATH:?required}"
: "${RSI_CUSTODY_LOCK_IDENTITY:?required}"
: "${RSI_CUSTODY_STATUS_FILE:?required}"
: "${RSI_CUSTODY_VERIFY_INDEX:?required}"
: "${RSI_CUSTODY_PREPARED_FILE:?required}"
: "${RSI_CUSTODY_APPLYING_FILE:?required}"
: "${RSI_CUSTODY_APPLIED_FILE:?required}"
: "${RSI_CUSTODY_STATUS_IDENTITY:?required}"
: "${RSI_CUSTODY_VERIFY_IDENTITY:?required}"
: "${RSI_CUSTODY_ALT_IDENTITY:?required}"
: "${RSI_CUSTODY_REAL_INDEX_IDENTITY:?required}"
: "${RSI_CUSTODY_RESTORE_IDENTITY:?required}"
: "${RSI_CUSTODY_RESTORE_BUILD:?required}"
: "${RSI_CUSTODY_APPLY_BUILD:?required}"
: "${RSI_CUSTODY_TMP_FILE:?required}"
: "${RSI_CUSTODY_TMP_IDENTITY:?required}"
git_cmd() {
    if [ -n "${RSI_CUSTODY_COMMAND_INDEX:-}" ]; then
        env -i PATH=/usr/bin:/bin HOME=/nonexistent LC_ALL=C \
            GIT_NO_REPLACE_OBJECTS=1 GIT_GRAFT_FILE=/dev/null \
            GIT_TERMINAL_PROMPT=0 GIT_EDITOR=true GIT_MERGE_AUTOEDIT=no \
            GIT_INDEX_FILE="$RSI_CUSTODY_COMMAND_INDEX" \
            git --no-optional-locks \
              -c core.fsmonitor=false -c core.hooksPath=/dev/null \
              -c commit.gpgsign=false -c rerere.enabled=false -c gc.auto=0 \
              -C "$RSI_CUSTODY_HOLDER" "$@"
    else
        env -i PATH=/usr/bin:/bin HOME=/nonexistent LC_ALL=C \
            GIT_NO_REPLACE_OBJECTS=1 GIT_GRAFT_FILE=/dev/null \
            GIT_TERMINAL_PROMPT=0 GIT_EDITOR=true GIT_MERGE_AUTOEDIT=no \
            git --no-optional-locks \
              -c core.fsmonitor=false -c core.hooksPath=/dev/null \
              -c commit.gpgsign=false -c rerere.enabled=false -c gc.auto=0 \
              -C "$RSI_CUSTODY_HOLDER" "$@"
    fi
}
validate_expected() {
    count=0
    while read -r old new name; do
        count=$((count+1))
        [ "$old" = "$RSI_CUSTODY_EXPECTED_OID" ] || refuse "stdin old oid $old"
        [ "$new" = "$RSI_CUSTODY_CANDIDATE_OID" ] || refuse "stdin new oid $new"
        [ "$name" = "$RSI_CUSTODY_TARGET_REF" ] || refuse "stdin ref $name"
    done
    [ "$count" -eq 1 ] || refuse "expected exactly one update, saw $count"
    cmp -s "$RSI_CUSTODY_PREPARED_FILE" "$RSI_CUSTODY_MARKER" || refuse "marker is not the exact prepared record"
    [ "$(git_cmd symbolic-ref HEAD)" = "$RSI_CUSTODY_TARGET_REF" ] || refuse "holder HEAD not on the target"
    [ "$(git_cmd rev-parse --verify "HEAD^{commit}")" = "$RSI_CUSTODY_EXPECTED_OID" ] || refuse "holder HEAD moved"
    [ "$(git_cmd rev-parse --verify "$RSI_CUSTODY_TARGET_REF^{commit}")" = "$RSI_CUSTODY_EXPECTED_OID" ] || refuse "target moved"
    validate_mutable_artifacts
    git_cmd status --porcelain=v1 -z --untracked-files=all > "$RSI_CUSTODY_STATUS_FILE" || refuse "holder status failed"
    validate_mutable_artifacts
    if [ -s "$RSI_CUSTODY_STATUS_FILE" ]; then
        refuse "holder is not strictly clean"
    fi
}
validate_lock() {
    [ "$(stat -c '%d:%i' "$RSI_CUSTODY_LOCK_PATH")" = "$RSI_CUSTODY_LOCK_IDENTITY" ] || refuse "index lock identity changed"
}
validate_mutable_artifacts() {
    [ "$(stat -c '%d:%i' "$RSI_CUSTODY_STATUS_FILE")" = "$RSI_CUSTODY_STATUS_IDENTITY" ] || refuse "status artifact identity changed"
    [ "$(stat -c '%d:%i' "$RSI_CUSTODY_VERIFY_INDEX")" = "$RSI_CUSTODY_VERIFY_IDENTITY" ] || refuse "verify artifact identity changed"
}
file_identity() { stat -c '%d:%i' "$1"; }
regular_identity() {
    [ -f "$1" ] && [ ! -L "$1" ] && [ "$(file_identity "$1")" = "$2" ]
}
install_restored_index() {
    # Exact crash-resume state machine: the empty manifest tmp is first
    # consumed into a hard link of the promoted alternate, then that link
    # protects the alternate inode while the real pathname is recreated from
    # the restore inode. No step overwrites an existing pathname.
    if regular_identity "$RSI_CUSTODY_TMP_FILE" "$RSI_CUSTODY_TMP_IDENTITY"; then
        [ -f "$RSI_CUSTODY_REAL_INDEX" ] && [ ! -L "$RSI_CUSTODY_REAL_INDEX" ] || refuse "real index is not a regular promoted index"
        rm "$RSI_CUSTODY_TMP_FILE" || refuse "restore tmp unlink failed"
        ln "$RSI_CUSTODY_REAL_INDEX" "$RSI_CUSTODY_TMP_FILE" || refuse "restore tmp hard-link failed"
    fi
    if regular_identity "$RSI_CUSTODY_TMP_FILE" "$RSI_CUSTODY_ALT_IDENTITY" && regular_identity "$RSI_CUSTODY_REAL_INDEX" "$RSI_CUSTODY_ALT_IDENTITY"; then
        rm "$RSI_CUSTODY_REAL_INDEX" || refuse "real alternate unlink failed"
    fi
    if regular_identity "$RSI_CUSTODY_TMP_FILE" "$RSI_CUSTODY_ALT_IDENTITY" && [ ! -e "$RSI_CUSTODY_REAL_INDEX" ]; then
        regular_identity "$RSI_CUSTODY_RESTORE_INDEX" "$RSI_CUSTODY_RESTORE_IDENTITY" || refuse "restore index identity changed"
        ln "$RSI_CUSTODY_RESTORE_INDEX" "$RSI_CUSTODY_REAL_INDEX" || refuse "restore real hard-link failed"
    fi
    regular_identity "$RSI_CUSTODY_TMP_FILE" "$RSI_CUSTODY_ALT_IDENTITY" || refuse "restore tmp identity changed"
    regular_identity "$RSI_CUSTODY_RESTORE_INDEX" "$RSI_CUSTODY_RESTORE_IDENTITY" || refuse "restore inode changed"
    regular_identity "$RSI_CUSTODY_REAL_INDEX" "$RSI_CUSTODY_RESTORE_IDENTITY" || refuse "restored real index identity changed"
    validate_lock
    sync_file "$(dirname "$RSI_CUSTODY_REAL_INDEX")"
    sync_file "$(dirname "$RSI_CUSTODY_TMP_FILE")"
}
validate_candidate() {
    [ "$(git_cmd symbolic-ref HEAD)" = "$RSI_CUSTODY_TARGET_REF" ] || refuse "holder HEAD not on target"
    [ "$(git_cmd rev-parse --verify "HEAD^{commit}")" = "$RSI_CUSTODY_CANDIDATE_OID" ] || refuse "holder HEAD not candidate"
    [ "$(git_cmd rev-parse --verify "$RSI_CUSTODY_TARGET_REF^{commit}")" = "$RSI_CUSTODY_CANDIDATE_OID" ] || refuse "target not candidate"
    validate_candidate_tree
}
validate_candidate_tree() {
    validate_mutable_artifacts
    cp "$RSI_CUSTODY_REAL_INDEX" "$RSI_CUSTODY_VERIFY_INDEX" || refuse "real index copy failed"
    validate_mutable_artifacts
    RSI_CUSTODY_COMMAND_INDEX="$RSI_CUSTODY_VERIFY_INDEX" git_cmd ls-files -u > "$RSI_CUSTODY_STATUS_FILE" || refuse "unmerged query failed"
    [ ! -s "$RSI_CUSTODY_STATUS_FILE" ] || refuse "unmerged index entries"
    [ "$(RSI_CUSTODY_COMMAND_INDEX="$RSI_CUSTODY_VERIFY_INDEX" git_cmd write-tree)" = "$(git_cmd rev-parse --verify "$RSI_CUSTODY_CANDIDATE_OID^{tree}")" ] || refuse "real index tree differs"
    RSI_CUSTODY_COMMAND_INDEX="$RSI_CUSTODY_VERIFY_INDEX" git_cmd diff --cached --quiet "$RSI_CUSTODY_CANDIDATE_OID" || refuse "cached diff differs"
    RSI_CUSTODY_COMMAND_INDEX="$RSI_CUSTODY_VERIFY_INDEX" git_cmd diff --quiet "$RSI_CUSTODY_CANDIDATE_OID" || refuse "worktree diff differs"
    RSI_CUSTODY_COMMAND_INDEX="$RSI_CUSTODY_VERIFY_INDEX" git_cmd ls-files --others --exclude-standard > "$RSI_CUSTODY_STATUS_FILE" || refuse "untracked query failed"
    validate_mutable_artifacts
    [ ! -s "$RSI_CUSTODY_STATUS_FILE" ] || refuse "holder has untracked bytes"
}
if [ "$1" = preparing ]; then
    if [ "${RSI_CUSTODY_RECOVERY:-}" = 1 ]; then
        count=0
        while read -r old new name; do
            count=$((count+1))
            [ "$old" = "$RSI_CUSTODY_EXPECTED_OID" ] || refuse "recovery old oid $old"
            [ "$new" = "$RSI_CUSTODY_EXPECTED_OID" ] || refuse "recovery new oid $new"
            [ "$name" = "$RSI_CUSTODY_TARGET_REF" ] || refuse "recovery ref $name"
        done
        [ "$count" -eq 1 ] || refuse "expected exactly one recovery update, saw $count"
        exit 0
    fi
    validate_expected
    exit 0
fi
if [ "$1" = prepared ]; then
    if [ "${RSI_CUSTODY_RECOVERY:-}" = 1 ]; then
        validate_lock
        [ "$(git_cmd symbolic-ref HEAD)" = "$RSI_CUSTODY_TARGET_REF" ] || refuse "holder HEAD not on target"
        [ "$(git_cmd rev-parse --verify "HEAD^{commit}")" = "$RSI_CUSTODY_EXPECTED_OID" ] || refuse "holder HEAD moved"
        [ "$(git_cmd rev-parse --verify "$RSI_CUSTODY_TARGET_REF^{commit}")" = "$RSI_CUSTODY_EXPECTED_OID" ] || refuse "target moved"
        validate_candidate_tree
        RSI_CUSTODY_COMMAND_INDEX="$RSI_CUSTODY_RESTORE_BUILD" git_cmd read-tree "$RSI_CUSTODY_CANDIDATE_OID" || refuse "restore index init failed"
        RSI_CUSTODY_COMMAND_INDEX="$RSI_CUSTODY_RESTORE_BUILD" git_cmd update-index --refresh || refuse "restore index refresh failed"
        RSI_CUSTODY_COMMAND_INDEX="$RSI_CUSTODY_RESTORE_BUILD" git_cmd read-tree -u -m "$RSI_CUSTODY_CANDIDATE_OID" "$RSI_CUSTODY_EXPECTED_OID" || refuse "restore apply failed"
        validate_lock
        regular_identity "$RSI_CUSTODY_RESTORE_BUILD" "$(file_identity "$RSI_CUSTODY_RESTORE_BUILD")" || refuse "restore build is not regular"
        regular_identity "$RSI_CUSTODY_RESTORE_INDEX" "$RSI_CUSTODY_RESTORE_IDENTITY" || refuse "restore inode changed before byte copy"
        cat "$RSI_CUSTODY_RESTORE_BUILD" > "$RSI_CUSTODY_RESTORE_INDEX" || refuse "restore byte copy failed"
        sync_file "$RSI_CUSTODY_RESTORE_INDEX"
        regular_identity "$RSI_CUSTODY_RESTORE_INDEX" "$RSI_CUSTODY_RESTORE_IDENTITY" || refuse "restore inode changed after byte copy"
        cmp -s "$RSI_CUSTODY_RESTORE_BUILD" "$RSI_CUSTODY_RESTORE_INDEX" || refuse "restore bytes differ from build"
        rm "$RSI_CUSTODY_RESTORE_BUILD" || refuse "restore build unlink failed"
        install_restored_index
        exit 0
    fi
    validate_expected
    validate_lock
    ln "$RSI_CUSTODY_APPLYING_FILE" "$RSI_CUSTODY_APPLYING_RECORD" || refuse "Applying proof collision"
    sync_file "$(dirname "$RSI_CUSTODY_APPLYING_RECORD")"
    RSI_CUSTODY_COMMAND_INDEX="$RSI_CUSTODY_APPLY_BUILD" git_cmd read-tree "$RSI_CUSTODY_EXPECTED_OID" || refuse "apply build init failed"
    RSI_CUSTODY_COMMAND_INDEX="$RSI_CUSTODY_APPLY_BUILD" git_cmd update-index --refresh || refuse "apply build refresh failed"
    RSI_CUSTODY_COMMAND_INDEX="$RSI_CUSTODY_APPLY_BUILD" git_cmd read-tree -u -m "$RSI_CUSTODY_EXPECTED_OID" "$RSI_CUSTODY_CANDIDATE_OID" || refuse "apply build failed"
    [ -f "$RSI_CUSTODY_APPLY_BUILD" ] && [ ! -L "$RSI_CUSTODY_APPLY_BUILD" ] || refuse "apply build is not regular"
    regular_identity "$RSI_CUSTODY_ALT_INDEX" "$RSI_CUSTODY_ALT_IDENTITY" || refuse "alternate inode changed before apply copy"
    cat "$RSI_CUSTODY_APPLY_BUILD" > "$RSI_CUSTODY_ALT_INDEX" || refuse "apply build copy failed"
    sync_file "$RSI_CUSTODY_ALT_INDEX"
    cmp -s "$RSI_CUSTODY_APPLY_BUILD" "$RSI_CUSTODY_ALT_INDEX" || refuse "alternate bytes differ from apply build"
    rm "$RSI_CUSTODY_APPLY_BUILD" || refuse "apply build unlink failed"
    sync_file "$(dirname "$RSI_CUSTODY_ALT_INDEX")"
    validate_lock
    [ "$(file_identity "$RSI_CUSTODY_REAL_INDEX")" = "$RSI_CUSTODY_REAL_INDEX_IDENTITY" ] || refuse "real index changed before install"
    rm "$RSI_CUSTODY_REAL_INDEX" || refuse "real index unlink failed"
    ln "$RSI_CUSTODY_ALT_INDEX" "$RSI_CUSTODY_REAL_INDEX" || refuse "alternate real hard-link failed"
    regular_identity "$RSI_CUSTODY_REAL_INDEX" "$RSI_CUSTODY_ALT_IDENTITY" || refuse "real index link identity changed"
    sync_file "$RSI_CUSTODY_REAL_INDEX"
    sync_file "$(dirname "$RSI_CUSTODY_REAL_INDEX")"
    validate_lock
    if [ "${RSI_TEST_ABORT_AFTER_INDEX:-}" = 1 ]; then
        refuse "injected abort after index application"
    fi
    exit 0
fi
if [ "$1" = committed ]; then
    if [ "${RSI_CUSTODY_RECOVERY:-}" = 1 ]; then
        exit 0
    fi
    validate_lock
    validate_candidate
    ln "$RSI_CUSTODY_APPLIED_FILE" "$RSI_CUSTODY_APPLIED_RECORD" || refuse "Applied proof collision"
    sync_file "$(dirname "$RSI_CUSTODY_APPLIED_RECORD")"
    exit 0
fi
# An aborted ref transaction must not alter holder state.  Recovery owns any
# verified restoration under a fresh compare-and-swap transaction.
if [ "$1" = aborted ]; then
    exit 0
fi
refuse "unknown transaction phase $1"
"#;

/// Test-only trailer appended after the mutation phase (before the final
/// `exit 0`), gated on an environment variable the daemon sets only when a
/// test requests the injected abort.
#[cfg(test)]
const REFERENCE_TRANSACTION_HOOK_TEST_TRAILER: &str = r#"
if [ "${RSI_TEST_ABORT_AFTER_INDEX:-}" = 1 ]; then
    echo "rsi-custody-hook: injected abort after index application" >&2
    exit 3
fi
"#;

/// Device:inode of an open file, the immutable identity of the index.lock
/// sentinel this custody created.
pub(super) fn file_identity(file: &std::fs::File) -> std::io::Result<String> {
    let metadata = file.metadata()?;
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
}

/// Re-validate that the on-disk `index.lock` is still exactly the file this
/// custody created. A missing lock (already removed by a partial cleanup) is
/// tolerated; a present lock with a different identity is Uncertain.
fn validate_lock_identity(record: &CustodyRecord, expected_identity: &str) -> Result<()> {
    let path = record.git_dir.join("index.lock");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() != 0
            {
                return Err(uncertain(
                    "the active custody index.lock is not an empty regular file",
                ));
            }
            let identity = format!("{}:{}", metadata.dev(), metadata.ino());
            if identity == expected_identity {
                Ok(())
            } else {
                Err(uncertain(
                    "the on-disk index.lock is not the custody's own file",
                ))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(uncertain(
            "the active custody index.lock is missing; recovery must reacquire it",
        )),
        Err(error) => Err(IntegrationError::Git(format!(
            "custody index.lock identity stat failed: {error}"
        ))),
    }
}

pub(super) fn recovery_proof(
    record: &CustodyRecord,
    staged: PathBuf,
    identity: String,
    prepared_proof_identity: String,
    installed_proof_identity: String,
) -> LockRecoveryProof {
    LockRecoveryProof {
        version: 1,
        operation_id: record.operation_id,
        target_ref: record.target_ref.clone(),
        expected_tip: record.expected_tip.clone(),
        candidate: record.candidate.clone(),
        holder: record.holder.clone(),
        git_dir: record.git_dir.clone(),
        proof_dir_identity: record.proof_dir_identity.clone(),
        prior_lock_identity: record.index_lock_identity.clone(),
        replacement_lock_identity: identity,
        phase: record.phase,
        staged_lock: staged,
        installed_lock: record.git_dir.join("index.lock"),
        prepared_proof_identity,
        installed_proof_identity,
    }
}

fn recovery_regular_empty(path: &Path) -> Result<std::fs::File> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| uncertain(format!("recovery staged lock stat failed: {error}")))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() || metadata.len() != 0 {
        return Err(uncertain(
            "recovery staged lock is not an empty regular file",
        ));
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| uncertain(format!("recovery staged lock open failed: {error}")))
}

fn write_reserved_recovery_proof(
    path: &Path,
    file: &mut std::fs::File,
    identity: &str,
    proof: &LockRecoveryProof,
) -> Result<()> {
    let canonical = serde_json::to_vec(proof).map_err(|error| {
        IntegrationError::Git(format!("recovery proof serialize failed: {error}"))
    })?;
    if file_identity(file).map_err(|error| {
        IntegrationError::Git(format!("recovery proof identity failed: {error}"))
    })? != identity
        || regular_file_identity(path)? != identity
    {
        return Err(uncertain(
            "reserved recovery proof inode changed before write",
        ));
    }
    file.write_all(&canonical)
        .and_then(|()| file.sync_all())
        .map_err(|error| IntegrationError::Git(format!("recovery proof write failed: {error}")))?;
    if regular_file_identity(path)? != identity {
        return Err(uncertain(
            "reserved recovery proof inode changed after write",
        ));
    }
    sync_parent_dir(path)
}

/// Resume every durable prefix of the same-filesystem recovery protocol. A
/// replacement lock is usable only when both immutable proofs bind the staged
/// inode and the live `index.lock` is its hard link.
fn reacquire_missing_lock(record: &CustodyRecord) -> Result<(String, std::fs::File)> {
    validate_proof_dir(record)?;
    let staged = record.marker.join("recovery-lock-staged");
    let prepared = record.marker.join("recovery-lock-prepared.json");
    let installed = record.marker.join("recovery-lock-installed.json");
    let staged_file = match std::fs::symlink_metadata(&staged) {
        Ok(_) => recovery_regular_empty(&staged)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let prepared_exists = match std::fs::symlink_metadata(&prepared) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(uncertain(format!(
                        "recovery prepared proof stat failed: {error}"
                    )));
                }
            };
            let installed_exists = match std::fs::symlink_metadata(&installed) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(uncertain(format!(
                        "recovery installed proof stat failed: {error}"
                    )));
                }
            };
            if prepared_exists || installed_exists {
                return Err(uncertain(
                    "recovery proof exists without its staged lock inode",
                ));
            }
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)
                .map_err(|error| {
                    IntegrationError::Git(format!("recovery staged lock create failed: {error}"))
                })?;
            file.sync_all().map_err(|error| {
                IntegrationError::Git(format!("recovery staged lock sync failed: {error}"))
            })?;
            sync_parent_dir(&staged)?;
            file
        }
        Err(error) => {
            return Err(uncertain(format!(
                "recovery staged lock stat failed: {error}"
            )));
        }
    };
    let identity = file_identity(&staged_file).map_err(|error| {
        IntegrationError::Git(format!("recovery lock identity failed: {error}"))
    })?;
    // The recovery records are a paired reservation.  Seeing just one empty
    // or serialized proof is an unbound crash prefix: it has no authority to
    // delete or replace anything, so preserve it and stop.
    let proof_presence = |path: &Path| match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(uncertain(format!("recovery proof stat failed: {error}"))),
    };
    let (prepared_exists, installed_exists) =
        (proof_presence(&prepared)?, proof_presence(&installed)?);
    let proof = match (prepared_exists, installed_exists) {
        (false, false) => {
            let mut prepared_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&prepared)
                .map_err(|error| {
                    IntegrationError::Git(format!(
                        "recovery prepared proof reserve failed: {error}"
                    ))
                })?;
            let prepared_identity = file_identity(&prepared_file).map_err(|error| {
                IntegrationError::Git(format!("recovery prepared proof identity failed: {error}"))
            })?;
            prepared_file.sync_all().map_err(|error| {
                IntegrationError::Git(format!("recovery prepared proof sync failed: {error}"))
            })?;
            sync_parent_dir(&prepared)?;
            let mut installed_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&installed)
                .map_err(|error| {
                    IntegrationError::Git(format!(
                        "recovery installed proof reserve failed: {error}"
                    ))
                })?;
            let installed_identity = file_identity(&installed_file).map_err(|error| {
                IntegrationError::Git(format!("recovery installed proof identity failed: {error}"))
            })?;
            installed_file.sync_all().map_err(|error| {
                IntegrationError::Git(format!("recovery installed proof sync failed: {error}"))
            })?;
            sync_parent_dir(&installed)?;
            let proof = recovery_proof(
                record,
                staged.clone(),
                identity.clone(),
                prepared_identity.clone(),
                installed_identity.clone(),
            );
            write_reserved_recovery_proof(
                &prepared,
                &mut prepared_file,
                &prepared_identity,
                &proof,
            )?;
            write_reserved_recovery_proof(
                &installed,
                &mut installed_file,
                &installed_identity,
                &proof,
            )?;
            proof
        }
        (true, true) => {
            validate_recovery_proof_set(record)?;
            read_recovery_proof(&prepared)?
        }
        _ => return Err(uncertain("recovery proof set is an unbound partial prefix")),
    };
    // This record is durable *intent*, not post-link acknowledgement: if a
    // crash happens after it lands, restart has exact evidence to perform the
    // no-clobber hard-link once. A foreign live lock remains untouched.
    match std::fs::symlink_metadata(&proof.installed_lock) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() == 0
                && format!("{}:{}", metadata.dev(), metadata.ino()) == identity => {}
        Ok(_) => return Err(uncertain("foreign index.lock appeared during recovery")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::hard_link(&staged, &proof.installed_lock).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    uncertain("foreign index.lock appeared during recovery")
                } else {
                    IntegrationError::Git(format!("recovery install failed: {error}"))
                }
            })?;
            sync_parent_dir(&proof.installed_lock)?;
        }
        Err(error) => {
            return Err(IntegrationError::Git(format!(
                "recovery lock stat failed: {error}"
            )));
        }
    }
    validate_lock_identity(record, &identity)?;
    Ok((identity, staged_file))
}

/// Sync the parent directory of `path` so a rename inside it is durable.
fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or(IntegrationError::InvalidInput(
        "custody path has no parent directory",
    ))?;
    let directory = std::fs::File::open(parent).map_err(|error| {
        IntegrationError::Git(format!(
            "custody parent directory open failed ({}): {error}",
            parent.display()
        ))
    })?;
    directory.sync_all().map_err(|error| {
        IntegrationError::Git(format!(
            "custody parent directory sync failed ({}): {error}",
            parent.display()
        ))
    })
}

fn sync_directory(path: &Path) -> Result<()> {
    let directory = std::fs::File::open(path).map_err(|error| {
        IntegrationError::Git(format!(
            "custody directory open failed ({}): {error}",
            path.display()
        ))
    })?;
    directory.sync_all().map_err(|error| {
        IntegrationError::Git(format!(
            "custody directory sync failed ({}): {error}",
            path.display()
        ))
    })
}

/// Same-operation proof file derived only from the operation phase.
fn transition_temp_path(record: &CustodyRecord, suffix: &str) -> PathBuf {
    record.marker.join(format!("next-{suffix}"))
}

/// All daemon-owned same-directory transition files for one custody.
fn transition_temp_paths(record: &CustodyRecord) -> Vec<PathBuf> {
    [
        "prepared",
        "applying",
        "applied",
        "status",
        "verify-index",
        "restore",
        "restore-build",
        "apply-build",
        "tmp",
    ]
    .iter()
    .map(|suffix| transition_temp_path(record, suffix))
    .collect()
}

const ARTIFACT_MANIFEST: &str = "artifact-manifest.json";

fn artifact_manifest_path(record: &CustodyRecord) -> PathBuf {
    record.marker.join(ARTIFACT_MANIFEST)
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn artifact_kind_name(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::NextPrepared => "prepared",
        ArtifactKind::NextApplying => "applying",
        ArtifactKind::NextApplied => "applied",
        ArtifactKind::NextStatus => "status",
        ArtifactKind::NextVerifyIndex => "verify-index",
        ArtifactKind::NextRestore => "restore",
        ArtifactKind::NextRestoreBuild => "restore-build",
        ArtifactKind::NextApplyBuild => "apply-build",
        ArtifactKind::NextTmp => "tmp",
    }
}

fn artifact_kinds() -> [ArtifactKind; 9] {
    [
        ArtifactKind::NextPrepared,
        ArtifactKind::NextApplying,
        ArtifactKind::NextApplied,
        ArtifactKind::NextStatus,
        ArtifactKind::NextVerifyIndex,
        ArtifactKind::NextRestore,
        ArtifactKind::NextRestoreBuild,
        ArtifactKind::NextApplyBuild,
        ArtifactKind::NextTmp,
    ]
}

fn artifact_path(record: &CustodyRecord, kind: ArtifactKind) -> PathBuf {
    transition_temp_path(record, artifact_kind_name(kind))
}

fn regular_file_identity(path: &Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        uncertain(format!(
            "artifact stat failed ({}): {error}",
            path.display()
        ))
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(uncertain("artifact is not a regular non-symlink file"));
    }
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
}

fn canonical_phase_payload(record: &CustodyRecord) -> Result<Vec<u8>> {
    let mut payload = record.clone();
    // The manifest binds these immutable payloads and the final phase record
    // binds the manifest. Omitting only that back-reference avoids a hash
    // fixed point while keeping all operation fields canonical.
    payload.artifact_manifest = None;
    serde_json::to_vec(&payload)
        .map_err(|error| IntegrationError::Git(format!("custody phase serialize failed: {error}")))
}

fn create_empty_artifact(path: &Path) -> Result<String> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                uncertain("operation artifact collision before manifest publication")
            } else {
                IntegrationError::Git(format!(
                    "operation artifact create failed ({}): {error}",
                    path.display()
                ))
            }
        })?;
    // A failed identity/sync leaves this unproved prefix in place. It is not
    // safe to unlink by pathname because ownership was never durably proven.
    let identity = file_identity(&file).map_err(|error| {
        IntegrationError::Git(format!("operation artifact identity failed: {error}"))
    })?;
    file.sync_all().map_err(|error| {
        IntegrationError::Git(format!("operation artifact sync failed: {error}"))
    })?;
    sync_parent_dir(path)?;
    Ok(identity)
}

fn write_owned_artifact(path: &Path, identity: &str, bytes: &[u8]) -> Result<()> {
    if regular_file_identity(path)? != identity {
        return Err(uncertain(
            "operation artifact identity changed before write",
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|error| {
            IntegrationError::Git(format!(
                "operation artifact open failed ({}): {error}",
                path.display()
            ))
        })?;
    if file_identity(&file).map_err(|error| {
        IntegrationError::Git(format!("operation artifact open identity failed: {error}"))
    })? != identity
    {
        return Err(uncertain("operation artifact was replaced during write"));
    }
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            IntegrationError::Git(format!(
                "operation artifact write failed ({}): {error}",
                path.display()
            ))
        })?;
    if regular_file_identity(path)? != identity {
        return Err(uncertain("operation artifact identity changed after write"));
    }
    sync_parent_dir(path)
}

fn prepare_artifact_manifest(record: &CustodyRecord) -> Result<ArtifactManifestBinding> {
    if record.phase != CustodyPhase::Prepared || record.artifact_manifest.is_some() {
        return Err(IntegrationError::InvalidInput(
            "artifact manifest requires an unbound Prepared record",
        ));
    }
    validate_proof_dir(record)?;
    let mut artifacts = Vec::with_capacity(9);
    for kind in artifact_kinds() {
        let path = artifact_path(record, kind);
        let identity = create_empty_artifact(&path)?;
        artifacts.push(ArtifactProof {
            kind,
            path,
            identity,
            digest: None,
        });
    }
    let alt_index_identity = regular_file_identity(&record.alt_index)?;
    let applying = advance_record(record, CustodyPhase::Applying, record.candidate.clone());
    let applied = advance_record(record, CustodyPhase::Applied, record.candidate.clone());
    for (kind, phase) in [
        (ArtifactKind::NextPrepared, record),
        (ArtifactKind::NextApplying, &applying),
        (ArtifactKind::NextApplied, &applied),
    ] {
        let entry = artifacts
            .iter_mut()
            .find(|entry| entry.kind == kind)
            .expect("fixed artifact kind exists");
        entry.digest = Some(digest_bytes(&canonical_phase_payload(phase)?));
    }
    let manifest = ArtifactManifest {
        version: 1,
        operation_id: record.operation_id,
        marker: record.marker.clone(),
        alt_index_identity,
        artifacts,
    };
    let bytes = serde_json::to_vec(&manifest).map_err(|error| {
        IntegrationError::Git(format!("artifact manifest serialize failed: {error}"))
    })?;
    let path = artifact_manifest_path(record);
    let identity = create_empty_artifact(&path)?;
    write_owned_artifact(&path, &identity, &bytes)?;
    Ok(ArtifactManifestBinding {
        path,
        identity,
        digest: digest_bytes(&bytes),
    })
}

fn read_artifact_manifest(record: &CustodyRecord) -> Result<ArtifactManifest> {
    let binding = record
        .artifact_manifest
        .as_ref()
        .ok_or_else(|| uncertain("Prepared custody lacks an artifact manifest"))?;
    let expected_path = artifact_manifest_path(record);
    if binding.path != expected_path || regular_file_identity(&binding.path)? != binding.identity {
        return Err(uncertain("artifact manifest path or identity changed"));
    }
    let bytes = std::fs::read(&binding.path).map_err(|error| {
        uncertain(format!(
            "artifact manifest read failed ({}): {error}",
            binding.path.display()
        ))
    })?;
    if digest_bytes(&bytes) != binding.digest {
        return Err(uncertain("artifact manifest digest changed"));
    }
    let manifest: ArtifactManifest = serde_json::from_slice(&bytes)
        .map_err(|error| uncertain(format!("artifact manifest is malformed: {error}")))?;
    let canonical = serde_json::to_vec(&manifest).map_err(|error| {
        IntegrationError::Git(format!("artifact manifest serialize failed: {error}"))
    })?;
    let alternate_ok = match std::fs::symlink_metadata(&record.alt_index) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            manifest.alt_index_identity == format!("{}:{}", metadata.dev(), metadata.ino())
        }
        // The prepared hook atomically promotes the owned alternate over the
        // real index. Its absence is therefore phase-appropriate only after
        // Applying has been durably reached.
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && matches!(record.phase, CustodyPhase::Applying | CustodyPhase::Applied) =>
        {
            true
        }
        _ => false,
    };
    if bytes != canonical {
        return Err(uncertain("artifact manifest bytes are not canonical"));
    }
    if manifest.version != 1
        || manifest.operation_id != record.operation_id
        || manifest.marker != record.marker
        || manifest.alt_index_identity != record.alt_index_identity
        || manifest.artifacts.len() != artifact_kinds().len()
    {
        return Err(uncertain("artifact manifest does not bind this operation"));
    }
    if !alternate_ok {
        return Err(uncertain(
            "artifact manifest alternate index identity changed",
        ));
    }
    for kind in artifact_kinds() {
        let expected_path = artifact_path(record, kind);
        let entries = manifest
            .artifacts
            .iter()
            .filter(|entry| entry.kind == kind)
            .collect::<Vec<_>>();
        let consumed_restore_artifact =
            matches!(
                kind,
                ArtifactKind::NextRestore
                    | ArtifactKind::NextRestoreBuild
                    | ArtifactKind::NextApplyBuild
            ) && matches!(record.phase, CustodyPhase::Applying | CustodyPhase::Applied)
                && matches!(
                    std::fs::symlink_metadata(&expected_path),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                );
        let temporary_is_backup = kind == ArtifactKind::NextTmp
            && matches!(record.phase, CustodyPhase::Applying | CustodyPhase::Applied)
            && regular_file_identity(&expected_path)
                .is_ok_and(|identity| identity == record.alt_index_identity);
        if entries.len() != 1
            || entries[0].path != expected_path
            || (!consumed_restore_artifact
                && !temporary_is_backup
                && regular_file_identity(&expected_path)? != entries[0].identity)
            || matches!(
                kind,
                ArtifactKind::NextPrepared | ArtifactKind::NextApplying | ArtifactKind::NextApplied
            ) != entries[0].digest.is_some()
        {
            return Err(uncertain(format!(
                "artifact manifest entry changed or is incomplete: {}",
                artifact_kind_name(kind)
            )));
        }
    }
    Ok(manifest)
}

fn validate_artifact_manifest(record: &CustodyRecord) -> Result<()> {
    if record.phase == CustodyPhase::Acquired {
        return if record.artifact_manifest.is_none() {
            Ok(())
        } else {
            Err(uncertain(
                "Acquired custody unexpectedly has artifact evidence",
            ))
        };
    }
    let manifest = read_artifact_manifest(record)?;
    let prepared = advance_record(record, CustodyPhase::Prepared, record.candidate.clone());
    let applying = advance_record(record, CustodyPhase::Applying, record.candidate.clone());
    let applied = advance_record(record, CustodyPhase::Applied, record.candidate.clone());
    let phase_records = [
        (ArtifactKind::NextPrepared, &prepared),
        (ArtifactKind::NextApplying, &applying),
        (ArtifactKind::NextApplied, &applied),
    ];
    for (kind, expected) in phase_records {
        let entry = manifest
            .artifacts
            .iter()
            .find(|entry| entry.kind == kind)
            .expect("manifest validation checked every fixed kind");
        let bytes = std::fs::read(&entry.path).map_err(|error| {
            uncertain(format!(
                "immutable transition artifact read failed: {error}"
            ))
        })?;
        let actual: CustodyRecord = serde_json::from_slice(&bytes).map_err(|error| {
            uncertain(format!("immutable transition artifact malformed: {error}"))
        })?;
        let canonical = serde_json::to_vec(&actual).map_err(|error| {
            IntegrationError::Git(format!("transition artifact serialize failed: {error}"))
        })?;
        let digest = digest_bytes(&canonical_phase_payload(&actual)?);
        if bytes != canonical || actual != *expected || entry.digest.as_deref() != Some(&digest) {
            return Err(uncertain(format!(
                "immutable transition artifact changed: {}",
                artifact_kind_name(kind)
            )));
        }
    }
    Ok(())
}

fn write_transition_artifacts(record: &CustodyRecord) -> Result<()> {
    let manifest = read_artifact_manifest(record)?;
    let applying = advance_record(record, CustodyPhase::Applying, record.candidate.clone());
    let applied = advance_record(record, CustodyPhase::Applied, record.candidate.clone());
    for (kind, proof) in [
        (ArtifactKind::NextPrepared, record),
        (ArtifactKind::NextApplying, &applying),
        (ArtifactKind::NextApplied, &applied),
    ] {
        let entry = manifest
            .artifacts
            .iter()
            .find(|entry| entry.kind == kind)
            .expect("fixed artifact kind exists");
        write_owned_artifact(
            &entry.path,
            &entry.identity,
            &serde_json::to_vec(proof).map_err(|error| {
                IntegrationError::Git(format!("transition artifact serialize failed: {error}"))
            })?,
        )?;
    }
    validate_artifact_manifest(record)?;
    sync_directory(&record.marker)
}

fn copy_into_owned_artifact(source: &Path, destination: &Path, identity: &str) -> Result<()> {
    if regular_file_identity(destination)? != identity {
        return Err(uncertain(
            "verification artifact identity changed before copy",
        ));
    }
    let mut input = std::fs::File::open(source).map_err(|error| {
        IntegrationError::Git(format!(
            "verification source open failed ({}): {error}",
            source.display()
        ))
    })?;
    let mut output = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(destination)
        .map_err(|error| {
            IntegrationError::Git(format!(
                "verification destination open failed ({}): {error}",
                destination.display()
            ))
        })?;
    if file_identity(&output).map_err(|error| {
        IntegrationError::Git(format!("verification destination identity failed: {error}"))
    })? != identity
    {
        return Err(uncertain("verification artifact was replaced during copy"));
    }
    std::io::copy(&mut input, &mut output).map_err(|error| {
        IntegrationError::Git(format!("candidate verification index copy failed: {error}"))
    })?;
    output.sync_all().map_err(|error| {
        IntegrationError::Git(format!("candidate verification index sync failed: {error}"))
    })?;
    if regular_file_identity(destination)? != identity {
        return Err(uncertain(
            "verification artifact identity changed after copy",
        ));
    }
    sync_parent_dir(destination)
}

fn advance_record(
    record: &CustodyRecord,
    phase: CustodyPhase,
    candidate: Option<String>,
) -> CustodyRecord {
    CustodyRecord {
        candidate,
        phase,
        ..record.clone()
    }
}

fn uncertain(reason: impl std::fmt::Display) -> IntegrationError {
    IntegrationError::Refused(Refusal::CustodyUncertain {
        reason: reason.to_string(),
    })
}

const fn phase_rank(phase: CustodyPhase) -> u8 {
    match phase {
        CustodyPhase::Acquired => 0,
        CustodyPhase::Prepared => 1,
        CustodyPhase::Applying => 2,
        CustodyPhase::Applied => 3,
    }
}

/// The immutable fields of an on-disk marker must equal the durable record;
/// the marker may be monotonically ahead (candidate set, phase advanced) but
/// never disagree.
fn audit_identity(durable: &CustodyRecord, on_disk: &CustodyRecord) -> Result<()> {
    let immutable = durable.version == on_disk.version
        && durable.operation_id == on_disk.operation_id
        && durable.target_ref == on_disk.target_ref
        && durable.expected_tip == on_disk.expected_tip
        && durable.holder == on_disk.holder
        && durable.git_dir == on_disk.git_dir
        && durable.alt_index == on_disk.alt_index
        && durable.alt_index_identity == on_disk.alt_index_identity
        && durable.real_index_identity == on_disk.real_index_identity
        && durable.acquired_phase_identity == on_disk.acquired_phase_identity
        && durable.prepared_phase_identity == on_disk.prepared_phase_identity
        && durable.marker == on_disk.marker
        && durable.proof_dir_identity == on_disk.proof_dir_identity
        && durable.engine_owned == on_disk.engine_owned
        && durable.index_lock_identity == on_disk.index_lock_identity
        && durable.candidate_cleanup == on_disk.candidate_cleanup;
    if !immutable {
        return Err(uncertain(
            "the on-disk marker disagrees with the durable record",
        ));
    }
    let candidate_ok = match (durable.candidate.as_ref(), on_disk.candidate.as_ref()) {
        (None, _) => true,
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    if !candidate_ok {
        return Err(uncertain("the on-disk marker disagrees on the candidate"));
    }
    let manifest_ok = match (&durable.artifact_manifest, &on_disk.artifact_manifest) {
        (None, Some(_)) if durable.phase == CustodyPhase::Acquired => true,
        (left, right) => left == right,
    };
    if !manifest_ok {
        return Err(uncertain(
            "the on-disk marker disagrees on the artifact manifest binding",
        ));
    }
    if phase_rank(on_disk.phase) < phase_rank(durable.phase) {
        return Err(uncertain("the on-disk marker is behind the durable record"));
    }
    Ok(())
}

/// Read and parse the fixed marker, or `None` when the marker is absent.
async fn read_on_disk_marker(record: &CustodyRecord) -> Result<Option<CustodyRecord>> {
    match std::fs::symlink_metadata(&record.marker) {
        Ok(_) => validate_proof_dir(record)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(uncertain(format!("proof directory stat failed: {error}"))),
    }
    let directory = match tokio::fs::read_dir(&record.marker).await {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(uncertain(format!("proof directory read failed: {error}"))),
    };
    let mut directory = directory;
    let mut found = None;
    while let Some(entry) = directory
        .next_entry()
        .await
        .map_err(|error| uncertain(format!("proof directory entry failed: {error}")))?
    {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(uncertain("proof filename is not UTF-8"));
        };
        if !matches!(
            name,
            "acquired.json" | "prepared.json" | "applying.json" | "applied.json"
        ) {
            continue;
        }
        let bytes = tokio::fs::read(entry.path())
            .await
            .map_err(|error| uncertain(format!("phase proof read failed: {error}")))?;
        let phase: CustodyRecord = serde_json::from_slice(&bytes)
            .map_err(|error| uncertain(format!("phase proof is malformed: {error}")))?;
        if phase_record_path(&phase, phase.phase) != entry.path() {
            return Err(uncertain("phase proof filename and contents disagree"));
        }
        if found
            .as_ref()
            .is_none_or(|old: &CustodyRecord| phase_rank(phase.phase) > phase_rank(old.phase))
        {
            found = Some(phase);
        }
    }
    let found = found.ok_or_else(|| uncertain("proof directory has no phase record"))?;
    // The on-disk phase may be ahead of the caller's durable snapshot (for
    // example, the hook has already promoted the alternate index). Validate
    // with that exact immutable phase rather than applying Prepared rules to
    // Applying evidence.
    preflight_proof_directory(&found).await?;
    Ok(Some(found))
}

/// Atomic same-directory marker transition: require the current marker to
/// equal `old` exactly, then stage the new record in a `create_new` temp,
/// sync the file, rename over the marker, and sync the parent directory. On
/// any failure the original marker is preserved and only the temp removed.
async fn transition_marker(old: &CustodyRecord, new: &CustodyRecord) -> Result<()> {
    require_marker_matches(old).await?;
    if new.phase != CustodyPhase::Prepared {
        return Err(IntegrationError::InvalidInput(
            "Applying and Applied records must be manifest hard links",
        ));
    }
    let mut reserved = new.clone();
    let reservation = reserve_phase_record(&mut reserved)?;
    if reserved != *new {
        return Err(uncertain("Prepared record was not reserved by its owner"));
    }
    write_reserved_phase_record(&reserved, reservation)
}

fn transaction_hook_script() -> String {
    let mut script = String::from(REFERENCE_TRANSACTION_HOOK);
    #[cfg(test)]
    script.push_str(REFERENCE_TRANSACTION_HOOK_TEST_TRAILER);
    script.push_str("exit 0\n");
    script
}

fn transaction_hook_directory() -> Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("rsi-custody-ref-hooks-")
        .tempdir()
        .map_err(|error| {
            IntegrationError::Git(format!(
                "custody reference-hook directory create failed: {error}"
            ))
        })?;
    let hook = directory.path().join("reference-transaction");
    std::fs::write(&hook, transaction_hook_script()).map_err(|error| {
        IntegrationError::Git(format!("custody reference hook write failed: {error}"))
    })?;
    let mut permissions = std::fs::metadata(&hook)
        .map_err(|error| {
            IntegrationError::Git(format!("custody reference hook metadata failed: {error}"))
        })?
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&hook, permissions).map_err(|error| {
        IntegrationError::Git(format!("custody reference hook chmod failed: {error}"))
    })?;
    Ok(directory)
}

fn reference_transaction_variables(
    record: &CustodyRecord,
    active_lock_identity: &str,
    abort_after_index: bool,
) -> Result<Vec<(String, String)>> {
    let path = |value: &Path| {
        value
            .to_str()
            .map(str::to_owned)
            .ok_or(IntegrationError::InvalidInput("custody path is not UTF-8"))
    };
    let candidate = record
        .candidate
        .as_deref()
        .ok_or(IntegrationError::InvalidInput(
            "Prepared custody has no candidate",
        ))?;
    let manifest = read_artifact_manifest(record)?;
    let artifact_identity = |kind| -> Result<String> {
        manifest
            .artifacts
            .iter()
            .find(|entry| entry.kind == kind)
            .map(|entry| entry.identity.clone())
            .ok_or_else(|| uncertain("artifact manifest lacks required hook artifact"))
    };
    let mut variables = vec![
        (
            "RSI_CUSTODY_TARGET_REF".to_string(),
            record.target_ref.clone(),
        ),
        (
            "RSI_CUSTODY_EXPECTED_OID".to_string(),
            record.expected_tip.clone(),
        ),
        (
            "RSI_CUSTODY_CANDIDATE_OID".to_string(),
            candidate.to_string(),
        ),
        ("RSI_CUSTODY_HOLDER".to_string(), path(&record.holder)?),
        (
            "RSI_CUSTODY_MARKER".to_string(),
            path(&phase_record_path(record, record.phase))?,
        ),
        (
            "RSI_CUSTODY_APPLYING_RECORD".to_string(),
            path(&phase_record_path(record, CustodyPhase::Applying))?,
        ),
        (
            "RSI_CUSTODY_APPLIED_RECORD".to_string(),
            path(&phase_record_path(record, CustodyPhase::Applied))?,
        ),
        (
            "RSI_CUSTODY_ALT_INDEX".to_string(),
            path(&record.alt_index)?,
        ),
        (
            "RSI_CUSTODY_REAL_INDEX".to_string(),
            path(&record.git_dir.join("index"))?,
        ),
        (
            "RSI_CUSTODY_LOCK_PATH".to_string(),
            path(&record.git_dir.join("index.lock"))?,
        ),
        (
            "RSI_CUSTODY_LOCK_IDENTITY".to_string(),
            active_lock_identity.to_string(),
        ),
        (
            "RSI_CUSTODY_STATUS_FILE".to_string(),
            path(&transition_temp_path(record, "status"))?,
        ),
        (
            "RSI_CUSTODY_VERIFY_INDEX".to_string(),
            path(&transition_temp_path(record, "verify-index"))?,
        ),
        (
            "RSI_CUSTODY_PREPARED_FILE".to_string(),
            path(&transition_temp_path(record, "prepared"))?,
        ),
        (
            "RSI_CUSTODY_APPLYING_FILE".to_string(),
            path(&transition_temp_path(record, "applying"))?,
        ),
        (
            "RSI_CUSTODY_APPLIED_FILE".to_string(),
            path(&transition_temp_path(record, "applied"))?,
        ),
        (
            "RSI_CUSTODY_STATUS_IDENTITY".to_string(),
            artifact_identity(ArtifactKind::NextStatus)?,
        ),
        (
            "RSI_CUSTODY_VERIFY_IDENTITY".to_string(),
            artifact_identity(ArtifactKind::NextVerifyIndex)?,
        ),
        (
            "RSI_CUSTODY_ALT_IDENTITY".to_string(),
            record.alt_index_identity.clone(),
        ),
        (
            "RSI_CUSTODY_REAL_INDEX_IDENTITY".to_string(),
            record.real_index_identity.clone(),
        ),
        (
            "RSI_CUSTODY_RESTORE_IDENTITY".to_string(),
            artifact_identity(ArtifactKind::NextRestore)?,
        ),
        (
            "RSI_CUSTODY_RESTORE_BUILD".to_string(),
            path(&transition_temp_path(record, "restore-build"))?,
        ),
        (
            "RSI_CUSTODY_APPLY_BUILD".to_string(),
            path(&transition_temp_path(record, "apply-build"))?,
        ),
        (
            "RSI_CUSTODY_TMP_FILE".to_string(),
            path(&transition_temp_path(record, "tmp"))?,
        ),
        (
            "RSI_CUSTODY_TMP_IDENTITY".to_string(),
            artifact_identity(ArtifactKind::NextTmp)?,
        ),
    ];
    if abort_after_index {
        variables.push(("RSI_TEST_ABORT_AFTER_INDEX".to_string(), "1".to_string()));
    }
    Ok(variables)
}

impl TargetCustody {
    /// Acquired custody is a precondition for prepare: the holder index.lock
    /// stays owned for the whole candidate build. On a candidate the marker is
    /// atomically rewritten to `Prepared` with `candidate = Some(oid)`.
    /// `AlreadyIntegrated` leaves the custody `Acquired` (caller can abort).
    ///
    /// # Errors
    ///
    /// `InvalidInput` unless the custody is still `Acquired` with no candidate;
    /// the plain `prepare_candidate` refusals (`TargetDenied`, `StaleTarget`,
    /// `InvalidSource`, `Conflict`); `Git` when the marker transition cannot
    /// be staged atomically, in which case the marker is preserved unchanged.
    pub async fn prepare_candidate(
        &mut self,
        source: &str,
        scratch_dir: &Path,
    ) -> Result<Prepared> {
        if self.record.phase != CustodyPhase::Acquired || self.record.candidate.is_some() {
            return Err(IntegrationError::InvalidInput(
                "prepare requires an Acquired custody",
            ));
        }
        require_marker_matches(&self.record).await?;
        let prepared = super::prepare_candidate(
            &self.config,
            &self.repo,
            &self.record.target_ref,
            &self.record.expected_tip,
            source,
            scratch_dir,
        )
        .await?;
        let Prepared::Candidate(candidate) = &prepared else {
            return Ok(prepared);
        };
        // Binding re-check directly before the marker transition.
        require_marker_matches(&self.record).await?;
        let mut next = advance_record(
            &self.record,
            CustodyPhase::Prepared,
            Some(candidate.oid.clone()),
        );
        next.candidate_cleanup = Some(
            candidate_cleanup_record(
                &self.config,
                &candidate.handle,
                &candidate.oid,
                self.record.operation_id,
            )
            .await?,
        );
        let reservation = reserve_phase_record(&mut next)?;
        // Every mutable or removable operation artifact is created and
        // identity-bound before Prepared becomes durable. A crash before that
        // publication leaves an Acquired marker plus an unproved prefix,
        // which later discovery refuses without deleting.
        let binding = prepare_artifact_manifest(&next)?;
        next.artifact_manifest = Some(binding);
        write_transition_artifacts(&next)?;
        require_marker_matches(&self.record).await?;
        write_reserved_phase_record(&next, reservation)?;
        self.record = next;
        Ok(prepared)
    }

    /// Publish the exact candidate recorded in this Prepared custody. The
    /// marker must still equal the Prepared record and the holder/ref/head/
    /// cleanliness/operation state must still be exact; the owned index.lock
    /// stays present through application, commit, proof, and cleanup.
    ///
    /// On success the target is at the candidate, the holder is consistent and
    /// cleaned, and `Published` is returned. Any update-ref failure or lost
    /// acknowledgement immediately runs the same record-only reconciliation
    /// matrix and returns its settlement (`Published`, `Aborted`, or
    /// `Refused(CustodyUncertain)` with all proof preserved).
    ///
    /// # Errors
    ///
    /// `InvalidInput` unless the custody is `Prepared` with exactly `candidate`
    /// recorded; `Refused` for the `validate_acquired` preconditions
    /// (`StaleTarget`, `TargetWorktreeDirty`, `TargetOperationInProgress`, …);
    /// `Refused(CustodyUncertain)` when the record-only matrix finds no safe
    /// settlement, leaving every proof artifact in place.
    pub async fn publish(self, candidate: &str) -> Result<Publication> {
        self.publish_inner(candidate, false, false).await
    }

    /// Test-only seam for the rejection fixtures: no process-global state, so
    /// the injected abort-after-application and the lost acknowledgement
    /// cannot cross-contaminate concurrent tests.
    #[cfg(test)]
    pub(super) async fn publish_for_test(
        self,
        candidate: &str,
        ack_loss: bool,
        abort_after_index: bool,
    ) -> Result<Publication> {
        self.publish_inner(candidate, ack_loss, abort_after_index)
            .await
    }

    async fn publish_inner(
        self,
        candidate: &str,
        #[cfg_attr(not(test), allow(unused_variables))] ack_loss: bool,
        abort_after_index: bool,
    ) -> Result<Publication> {
        if self.record.phase != CustodyPhase::Prepared {
            return Err(IntegrationError::InvalidInput(
                "publish requires a Prepared custody",
            ));
        }
        if self.record.candidate.as_deref() != Some(candidate) {
            return Err(IntegrationError::InvalidInput(
                "candidate does not match the Prepared custody record",
            ));
        }
        require_marker_matches(&self.record).await?;
        validate_acquired(&self.config, &self.repo, &self.record).await?;

        let record = self.record.clone();
        let config = self.config.clone();
        let repo = self.repo.clone();

        if let Err(original) = validate_artifact_manifest(&record) {
            drop(self);
            return settle_publication(&config, &repo, &record, original).await;
        }

        let hook_directory = transaction_hook_directory()?;
        let stdin = format!(
            "start\nupdate {} {} {}\nprepare\ncommit\n",
            record.target_ref, candidate, record.expected_tip
        );
        let variables = reference_transaction_variables(
            &record,
            &record.index_lock_identity,
            abort_after_index,
        )?;
        let outcome = git::run_reference_transaction(
            &config,
            &repo,
            &["update-ref", "--no-deref", "--stdin"],
            stdin.as_bytes(),
            hook_directory.path(),
            &variables,
        )
        .await;

        match outcome {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let error = git::failed(&["update-ref"], &output);
                drop(self);
                return settle_publication(&config, &repo, &record, error).await;
            }
            Err(error) => {
                drop(self);
                return settle_publication(&config, &repo, &record, error).await;
            }
        }

        // Make the hook's marker renames durable once the transaction
        // committed.
        if let Err(error) = sync_parent_dir(&record.marker) {
            drop(self);
            return settle_publication(&config, &repo, &record, error).await;
        }

        #[cfg(test)]
        if ack_loss {
            // Simulate the daemon dying after the ref committed and the
            // Applied marker landed but before finalization: the live object
            // is dropped and the on-disk proof is left for record-only
            // reconciliation.
            drop(self);
            return Err(IntegrationError::Git(
                "simulated acknowledgement loss after the reference transaction".to_string(),
            ));
        }

        let Self { index_lock, .. } = self;
        match finalize_publication(
            &config,
            &repo,
            &record,
            ActiveCustodyLock {
                identity: record.index_lock_identity.clone(),
                handle: Some(index_lock),
            },
            candidate,
        )
        .await
        {
            Ok(()) => Ok(Publication::Published),
            Err(original) => settle_publication(&config, &repo, &record, original).await,
        }
    }
}

/// Finalize a publication whose proof is exactly the Applied record at the
/// candidate. The prepared hook already promoted the alternate index while
/// the ref lock was held; this callback only proves that committed state and
/// cleans the exact owned artifacts.
async fn finalize_publication(
    config: &IntegrationConfig,
    repo: &Path,
    durable: &CustodyRecord,
    active_lock: ActiveCustodyLock,
    candidate: &str,
) -> Result<()> {
    let on_disk = read_on_disk_marker(durable)
        .await?
        .ok_or_else(|| uncertain("published custody has no marker"))?;
    audit_identity(durable, &on_disk)?;
    if on_disk.phase != CustodyPhase::Applied || on_disk.candidate.as_deref() != Some(candidate) {
        return Err(uncertain(
            "the marker is not the exact Applied record for the candidate",
        ));
    }
    match resolve_commit(config, repo, &on_disk.target_ref).await? {
        Some(oid) if oid == candidate => {}
        observed => {
            return Err(uncertain(format!(
                "target is not at the candidate for Applied finalization (observed {observed:?})"
            )));
        }
    }
    validate_lock_identity(&on_disk, &active_lock.identity)?;
    // The committed callback proved the exact holder/index/worktree state
    // while both locks remained held. Re-running index commands here would
    // contend with our deliberate `index.lock` sentinel.
    verify_candidate_state(config, repo, &on_disk, candidate).await?;
    cleanup_published(config, repo, &on_disk, active_lock).await
}

async fn verify_candidate_state(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
    candidate: &str,
) -> Result<()> {
    validate_artifact_manifest(record)?;
    match git::stdout(config, &record.holder, &["symbolic-ref", "HEAD"]).await {
        Ok(head) if head == record.target_ref => {}
        _ => {
            return Err(uncertain(
                "holder HEAD is not on the target after publication",
            ));
        }
    }
    match git::stdout(
        config,
        &record.holder,
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )
    .await
    {
        Ok(oid) if oid == candidate => {}
        _ => return Err(uncertain("holder HEAD is not at the candidate")),
    }
    match resolve_commit(config, repo, &record.target_ref).await? {
        Some(oid) if oid == candidate => {}
        observed => {
            return Err(uncertain(format!(
                "target is not at the candidate (observed {observed:?})"
            )));
        }
    }
    // `write-tree` takes an index lock even when it only reads. Verify a
    // synced byte-for-byte copy while custody retains the real lock.
    let manifest = read_artifact_manifest(record)?;
    let verify = manifest
        .artifacts
        .iter()
        .find(|entry| entry.kind == ArtifactKind::NextVerifyIndex)
        .expect("manifest validation checked the verification artifact");
    let verify_index = verify.path.clone();
    copy_into_owned_artifact(
        &record.git_dir.join("index"),
        &verify_index,
        &verify.identity,
    )?;
    let unmerged =
        git::stdout_raw_with_index(config, &record.holder, &["ls-files", "-u"], &verify_index)
            .await?;
    if !unmerged.is_empty() {
        return Err(uncertain(
            "holder index has unmerged entries at the candidate",
        ));
    }
    let tree = git::stdout(
        config,
        &record.holder,
        &["rev-parse", "--verify", &format!("{candidate}^{{tree}}")],
    )
    .await?;
    let index_tree =
        git::stdout_with_index(config, &record.holder, &["write-tree"], &verify_index).await?;
    if index_tree != tree {
        return Err(uncertain(
            "holder real index tree differs from the candidate",
        ));
    }
    if !git::predicate_with_index(
        config,
        &record.holder,
        &["diff", "--cached", "--quiet", candidate],
        &verify_index,
    )
    .await?
    {
        return Err(uncertain("holder cached diff differs from the candidate"));
    }
    if !git::predicate_with_index(
        config,
        &record.holder,
        &["diff", "--quiet", candidate],
        &verify_index,
    )
    .await?
    {
        return Err(uncertain("holder worktree diff differs from the candidate"));
    }
    let status = git::stdout_raw_with_index(
        config,
        &record.holder,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        &verify_index,
    )
    .await?;
    if !status.is_empty() {
        return Err(uncertain("holder is not strictly clean at the candidate"));
    }
    validate_artifact_manifest(record)?;
    Ok(())
}

/// Verify terminal candidate state after the operation proof directory has
/// already been removed. Unlike [`verify_candidate_state`], this intentionally
/// does not create a verification index below `record.marker`.
async fn verify_candidate_state_after_cleanup(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
    candidate: &str,
) -> Result<()> {
    if record.engine_owned && !record.holder.exists() && !record.git_dir.exists() {
        return match resolve_commit(config, repo, &record.target_ref).await? {
            Some(oid) if oid == candidate => Ok(()),
            _ => Err(uncertain(
                "removed engine holder target is not at candidate",
            )),
        };
    }
    if std::fs::symlink_metadata(record.git_dir.join("index.lock")).is_ok() {
        return Err(uncertain(
            "terminal verification found an active index.lock",
        ));
    }
    match git::stdout(config, &record.holder, &["symbolic-ref", "HEAD"]).await {
        Ok(head) if head == record.target_ref => {}
        _ => return Err(uncertain("terminal holder HEAD is not on the target")),
    }
    match git::stdout(
        config,
        &record.holder,
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )
    .await
    {
        Ok(oid) if oid == candidate => {}
        _ => return Err(uncertain("terminal holder HEAD is not at candidate")),
    }
    match resolve_commit(config, repo, &record.target_ref).await? {
        Some(oid) if oid == candidate => {}
        _ => return Err(uncertain("terminal target is not at candidate")),
    }
    if !git::stdout_raw(config, &record.holder, &["ls-files", "-u"])
        .await?
        .is_empty()
    {
        return Err(uncertain("terminal holder index has unmerged entries"));
    }
    let tree = git::stdout(
        config,
        &record.holder,
        &["rev-parse", "--verify", &format!("{candidate}^{{tree}}")],
    )
    .await?;
    if git::stdout(config, &record.holder, &["write-tree"]).await? != tree {
        return Err(uncertain("terminal real index tree differs from candidate"));
    }
    if !git::predicate(
        config,
        &record.holder,
        &["diff", "--cached", "--quiet", candidate],
    )
    .await?
        || !git::predicate(config, &record.holder, &["diff", "--quiet", candidate]).await?
        || !git::stdout_raw(
            config,
            &record.holder,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )
        .await?
        .is_empty()
    {
        return Err(uncertain("terminal holder is not exactly candidate-clean"));
    }
    Ok(())
}

fn terminal_custody_artifacts_absent(record: &CustodyRecord) -> Result<()> {
    for path in std::iter::once(record.git_dir.join("index.lock"))
        .chain(std::iter::once(record.alt_index.clone()))
        .chain(transition_temp_paths(record))
    {
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => return Err(uncertain("proof is absent but custody artifact remains")),
            Err(error) => return Err(uncertain(format!("terminal artifact stat failed: {error}"))),
        }
    }
    Ok(())
}

async fn verify_expected_state(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
) -> Result<()> {
    match git::stdout(config, &record.holder, &["symbolic-ref", "HEAD"]).await {
        Ok(head) if head == record.target_ref => {}
        _ => return Err(uncertain("holder HEAD is not on the target after restore")),
    }
    let expected = record.expected_tip.as_str();
    match git::stdout(
        config,
        &record.holder,
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )
    .await
    {
        Ok(oid) if oid == expected => {}
        _ => {
            return Err(uncertain(
                "holder HEAD is not at the expected tip after restore",
            ));
        }
    }
    match resolve_commit(config, repo, &record.target_ref).await? {
        Some(oid) if oid == expected => {}
        observed => {
            return Err(uncertain(format!(
                "target is not at the expected tip (observed {observed:?})"
            )));
        }
    }
    let status = git::stdout_raw(
        config,
        &record.holder,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await?;
    if !status.is_empty() {
        return Err(uncertain(
            "holder is not strictly clean at the expected tip",
        ));
    }
    Ok(())
}

async fn file_exists(path: &Path) -> Result<bool> {
    match tokio::fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(IntegrationError::Git(format!(
            "custody artifact inspect failed ({}): {error}",
            path.display()
        ))),
    }
}

/// Exact cleanup of a successfully published custody. Transition temps are
/// removed (the applying/applied files were already consumed by the hook's
/// renames), then the owned index.lock is identity-validated and removed. The
/// existing holder removes the marker LAST; the engine-owned holder removes
/// the exact worktree while the marker remains.
async fn cleanup_published(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
    mut active_lock: ActiveCustodyLock,
) -> Result<()> {
    preflight_terminal_cleanup(config, repo, record, &active_lock, true).await?;
    // Candidate cleanup is first only after every other terminal artifact
    // was proven. A failed preflight above has made no filesystem mutation.
    cleanup_terminal_candidate(config, repo, record).await?;
    if file_exists(&record.alt_index).await? {
        remove_owned_exact(&record.alt_index, &record.alt_index_identity).await?;
    }
    validate_lock_identity(record, &active_lock.identity)?;
    if let Some(handle) = active_lock.handle.take() {
        drop(handle);
    }
    let lock_path = record.git_dir.join("index.lock");
    remove_owned_exact(&lock_path, &active_lock.identity).await?;
    if record.engine_owned {
        // The private git dir holding the marker is removed with the exact
        // worktree; never remove the marker separately.
        remove_engine_worktree_checked(config, repo, &record.holder).await?;
    } else {
        require_marker_matches(record).await?;
        remove_proof_directory(record).await?;
    }
    Ok(())
}

/// Record-only abort cleanup: the marker must still hold exactly the record;
/// the alternate, transition temps, and the identity-validated index.lock are
/// removed; then the held marker is removed LAST or the exact engine worktree
/// removed while the marker remains.
pub(super) async fn abort_cleanup_record(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
    mut active_lock: ActiveCustodyLock,
) -> Result<()> {
    preflight_terminal_cleanup(
        config,
        repo,
        record,
        &active_lock,
        matches!(record.phase, CustodyPhase::Applying | CustodyPhase::Applied),
    )
    .await?;
    cleanup_terminal_candidate(config, repo, record).await?;
    if file_exists(&record.alt_index).await? {
        remove_owned_exact(&record.alt_index, &record.alt_index_identity).await?;
    }
    validate_lock_identity(record, &active_lock.identity)?;
    let lock_path = record.git_dir.join("index.lock");
    if let Some(handle) = active_lock.handle.take() {
        drop(handle);
    }
    remove_owned_exact(&lock_path, &active_lock.identity).await?;
    if record.engine_owned {
        remove_engine_worktree_checked(config, repo, &record.holder).await?;
    } else {
        require_marker_matches(record).await?;
        remove_proof_directory(record).await?;
    }
    Ok(())
}

async fn preflight_terminal_cleanup(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
    active_lock: &ActiveCustodyLock,
    promoted_alt_index: bool,
) -> Result<()> {
    require_marker_matches(record).await?;
    validate_proof_dir(record)?;
    preflight_proof_directory(record).await?;
    validate_recovery_proof_set(record)?;
    let _proof_unlinks = teardown_expected_unlinks(record).await?;
    validate_lock_identity(record, &active_lock.identity)?;
    if record.phase != CustodyPhase::Acquired {
        validate_artifact_manifest(record)?;
    }
    match std::fs::symlink_metadata(&record.alt_index) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && promoted_alt_index => {}
        Ok(metadata)
            if promoted_alt_index
                && metadata.file_type().is_file()
                && !metadata.file_type().is_symlink()
                && format!("{}:{}", metadata.dev(), metadata.ino())
                    == record.alt_index_identity => {}
        Ok(metadata)
            if !promoted_alt_index
                && metadata.file_type().is_file()
                && !metadata.file_type().is_symlink() => {}
        _ => {
            return Err(uncertain(
                "alternate index is not in its phase-authorized terminal state",
            ));
        }
    }
    match &record.candidate_cleanup {
        Some(cleanup) => preflight_candidate_cleanup(config, repo, cleanup).await?,
        None if record.candidate.is_none() => {}
        None => {
            return Err(uncertain(
                "terminal custody lacks candidate cleanup evidence",
            ));
        }
    }
    if record.engine_owned {
        let holder = std::fs::canonicalize(&record.holder)
            .map_err(|error| uncertain(format!("engine holder canonicalize failed: {error}")))?;
        if holder != record.holder {
            return Err(uncertain("engine holder identity changed"));
        }
        if !list_worktrees(config, repo)
            .await?
            .into_iter()
            .any(|entry| entry.path == record.holder)
        {
            return Err(uncertain("engine holder registration changed"));
        }
    }
    Ok(())
}

async fn cleanup_terminal_candidate(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
) -> Result<()> {
    match &record.candidate_cleanup {
        Some(cleanup) => cleanup_candidate_record(config, repo, cleanup).await,
        None if record.candidate.is_none() => Ok(()),
        None => Err(uncertain(
            "terminal candidate custody lacks a durable candidate cleanup record",
        )),
    }
}

/// Restore a holder whose worktree the hook moved to the candidate but whose
/// ref stayed at the expected tip. Only proceeds after proving the worktree
/// is exactly candidate-clean (fresh temp index initialized to the candidate
/// tree, stat-refreshed, no tracked or untracked drift), then moves it back
/// to the expected tip with `read-tree -u -m`. Any failure preserves the
/// proof and is reported as Uncertain by the caller.
async fn restore_holder(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
    active_lock: &ActiveCustodyLock,
) -> Result<()> {
    validate_artifact_manifest(record)?;
    let manifest = read_artifact_manifest(record)?;
    let restore = manifest
        .artifacts
        .iter()
        .find(|entry| entry.kind == ArtifactKind::NextRestore)
        .ok_or_else(|| uncertain("artifact manifest lacks restore index"))?;
    // The restore index is an already-published operation artifact. Truncate
    // only that revalidated inode; a crash leaves a deterministic owned
    // prefix rather than a legacy create_new file in the Git directory.
    write_owned_artifact(&restore.path, &restore.identity, b"")?;
    let hook_directory = transaction_hook_directory()?;
    let stdin = format!(
        "start\nupdate {} {} {}\nprepare\ncommit\n",
        record.target_ref, record.expected_tip, record.expected_tip
    );
    let mut variables = reference_transaction_variables(record, &active_lock.identity, false)?;
    variables.push(("RSI_CUSTODY_RECOVERY".to_string(), "1".to_string()));
    variables.push((
        "RSI_CUSTODY_RESTORE_INDEX".to_string(),
        restore
            .path
            .to_str()
            .ok_or_else(|| uncertain("restore temp index path is not UTF-8"))?
            .to_string(),
    ));
    let output = git::run_reference_transaction(
        config,
        repo,
        &["update-ref", "--no-deref", "--stdin"],
        stdin.as_bytes(),
        hook_directory.path(),
        &variables,
    )
    .await?;
    if !output.status.success() {
        return Err(uncertain(format!(
            "holder restore reference transaction failed: {}",
            git::failed(&["update-ref"], &output)
        )));
    }
    validate_artifact_manifest(record)?;

    // The recovery hook ran under an exact no-op compare-and-swap ref
    // transaction and promoted the restored alternate while both locks held.
    verify_expected_state(config, repo, record).await?;
    Ok(())
}

/// Run the reconciliation matrix after a published-custody failure or lost
/// acknowledgement: the outcome is the matrix settlement, and Uncertain
/// preserves every proof artifact.
async fn settle_publication(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
    original: IntegrationError,
) -> Result<Publication> {
    match reconcile_target_custody(config, repo, record).await {
        Ok(publication) => Ok(publication),
        Err(cleanup) => Err(uncertain(format!("{original}; reconciliation: {cleanup}"))),
    }
}

/// Record-only reconciliation of a persisted custody.
///
/// It audits the on-disk marker then settles by the exact matrix. A missing
/// marker is always Uncertain.
///
/// # Errors
///
/// Returns `CustodyUncertain` while preserving artifacts for any mismatch,
/// malformed record, or state with no safe settlement.
pub async fn reconcile_target_custody(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
) -> Result<Publication> {
    match read_on_disk_marker(record).await? {
        Some(on_disk) => reconcile_with_marker(config, repo, record, &on_disk).await,
        None => reconcile_missing_marker(config, repo, record).await,
    }
}

async fn reconcile_missing_marker(
    config: &IntegrationConfig,
    repo: &Path,
    record: &CustodyRecord,
) -> Result<Publication> {
    terminal_custody_artifacts_absent(record)?;
    let observed = resolve_commit(config, repo, &record.target_ref).await?;
    match (observed.as_deref(), record.candidate.as_deref()) {
        (Some(oid), Some(candidate)) if oid == candidate => {
            // A crash after terminal proof cleanup is recoverable only when
            // the exact holder is already demonstrably candidate-clean.
            verify_candidate_state_after_cleanup(config, repo, record, candidate).await?;
            cleanup_terminal_candidate(config, repo, record).await?;
            Ok(Publication::Published)
        }
        (Some(oid), _) if oid == record.expected_tip => {
            if record.engine_owned && !record.holder.exists() && !record.git_dir.exists() {
                // An engine holder that was removed by terminal abort has no
                // worktree left to inspect; ref and exact candidate cleanup
                // are the remaining durable evidence.
            } else {
                verify_expected_state(config, repo, record).await?;
            }
            cleanup_terminal_candidate(config, repo, record).await?;
            Ok(Publication::Aborted)
        }
        _ => Err(uncertain(
            "proof is absent and target is neither the expected tip nor exact candidate",
        )),
    }
}

async fn reconcile_with_marker(
    config: &IntegrationConfig,
    repo: &Path,
    durable: &CustodyRecord,
    on_disk: &CustodyRecord,
) -> Result<Publication> {
    audit_identity(durable, on_disk)?;
    // Re-establish operational exclusivity before observing a ref. The phase
    // proof remains immutable; recovery carries its replacement identity only
    // in the active guard and append-only recovery records.
    let active_lock = ensure_active_lock(on_disk)?;
    // Recovery can have created a replacement lock, but must never rewrite a
    // phase record. Re-read the immutable record and its complete directory
    // after exclusivity is established before observing the target ref.
    let reread = read_on_disk_marker(durable)
        .await?
        .ok_or_else(|| uncertain("custody proof disappeared during lock recovery"))?;
    audit_identity(durable, &reread)?;
    if reread != *on_disk {
        return Err(uncertain(
            "custody phase changed while recovery acquired its lock",
        ));
    }
    preflight_proof_directory(&reread).await?;
    validate_recovery_proof_set(&reread)?;
    validate_lock_identity(&reread, &active_lock.identity)?;
    let target_oid = resolve_commit(config, repo, &reread.target_ref).await?;
    let candidate = reread.candidate.as_deref();
    let expected = reread.expected_tip.as_str();
    match (target_oid.as_deref(), reread.phase, candidate) {
        (Some(oid), CustodyPhase::Applied, Some(cand)) if oid == cand => {
            finalize_publication(config, repo, &reread, active_lock, cand)
                .await
                .map(|()| Publication::Published)
                .map_err(|error| uncertain(error.to_string()))
        }
        (Some(oid), CustodyPhase::Applying, Some(cand)) if oid == cand => {
            // The ref committed but the Applied transition never landed: the
            // parent establishes Applied from the exact Applying proof, then
            // finalizes.
            verify_candidate_state(config, repo, &reread, cand)
                .await
                .map_err(|error| uncertain(error.to_string()))?;
            let applied = advance_record(&reread, CustodyPhase::Applied, Some(cand.to_string()));
            transition_marker(&reread, &applied)
                .await
                .map_err(|error| uncertain(format!("Applied transition failed: {error}")))?;
            finalize_publication(config, repo, &applied, active_lock, cand)
                .await
                .map(|()| Publication::Published)
                .map_err(|error| uncertain(error.to_string()))
        }
        (Some(oid), CustodyPhase::Acquired | CustodyPhase::Prepared, _) if oid == expected => {
            abort_cleanup_record(config, repo, &reread, active_lock)
                .await
                .map(|()| Publication::Aborted)
                .map_err(|error| uncertain(error.to_string()))
        }
        (Some(oid), CustodyPhase::Applying | CustodyPhase::Applied, Some(_)) if oid == expected => {
            // The hook applied files but the ref transaction aborted before
            // commit: restore the holder then abort-clean the custody.
            restore_holder(config, repo, &reread, &active_lock)
                .await
                .map_err(|error| uncertain(error.to_string()))?;
            abort_cleanup_record(config, repo, &reread, active_lock)
                .await
                .map(|()| Publication::Aborted)
                .map_err(|error| uncertain(error.to_string()))
        }
        _ => Err(uncertain(
            "ref/phase/candidate combination has no safe settlement",
        )),
    }
}

/// Discover the fixed custody marker for an operation id without any open lock.
///
/// The holder, Git dir, marker, alternate index, and branch must derive from
/// the repository exactly.
///
/// # Errors
///
/// Returns `CustodyUncertain` for malformed, ambiguous, or mismatched proof.
pub async fn discover_custody_record(
    config: &IntegrationConfig,
    repo: &Path,
    operation_id: Uuid,
) -> Result<Option<CustodyRecord>> {
    let worktrees = list_worktrees(config, repo).await?;
    let mut found: Option<CustodyRecord> = None;
    for entry in &worktrees {
        let Ok(git_dir) = private_git_dir(config, &entry.path).await else {
            continue;
        };
        let marker = git_dir.join(CUSTODY_MARKER).join(operation_id.to_string());
        let Ok(directory) = tokio::fs::read_dir(&marker).await else {
            continue;
        };
        let mut directory = directory;
        let mut record: Option<CustodyRecord> = None;
        while let Some(proof) = directory
            .next_entry()
            .await
            .map_err(|error| uncertain(format!("discovered proof entry failed: {error}")))?
        {
            let name = proof.file_name();
            let Some(name) = name.to_str() else {
                return Err(uncertain("discovered proof filename is not UTF-8"));
            };
            if !matches!(
                name,
                "acquired.json" | "prepared.json" | "applying.json" | "applied.json"
            ) {
                continue;
            }
            let bytes = tokio::fs::read(proof.path())
                .await
                .map_err(|error| uncertain(format!("discovered proof read failed: {error}")))?;
            let parsed: CustodyRecord = serde_json::from_slice(&bytes)
                .map_err(|error| uncertain(format!("discovered proof is malformed: {error}")))?;
            if parsed.operation_id != operation_id
                || parsed.marker != marker
                || phase_record_path(&parsed, parsed.phase) != proof.path()
            {
                return Err(uncertain(
                    "discovered proof does not bind this operation directory",
                ));
            }
            if record
                .as_ref()
                .is_none_or(|old| phase_rank(parsed.phase) > phase_rank(old.phase))
            {
                record = Some(parsed);
            }
        }
        let Some(record) = record else {
            return Err(uncertain("operation proof directory has no phase record"));
        };
        let holder = tokio::fs::canonicalize(&entry.path)
            .await
            .map_err(|_| uncertain("discovered custody holder is not canonicalizable"))?;
        let exact = holder == record.holder
            && git_dir == record.git_dir
            && marker == record.marker
            && record.alt_index == git_dir.join(format!("index.rsi-{operation_id}"))
            && entry.branch.as_deref() == Some(record.target_ref.as_str());
        if !exact {
            return Err(uncertain(format!(
                "custody marker for {operation_id} has mismatched repo-derived paths"
            )));
        }
        if found.is_some() {
            return Err(uncertain(
                "more than one custody marker matches the operation",
            ));
        }
        found = Some(record);
    }
    Ok(found)
}
