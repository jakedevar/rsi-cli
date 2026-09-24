//! Master-successor sandbox recovery (Issues #620, #398).
//!
//! An `AgentReserveSuccessor` launch allocates `sandboxes/<candidate_id>` on
//! branch `rsi/<candidate_id>` before capacity admission. A refusal after that
//! point retains the root, and every later attempt for the SAME candidate id
//! then collided with its own allocation ("sandbox root already exists").
//! Reclaim is deliberately narrow: only a pristine, registered worktree of the
//! same repository, on exactly the candidate branch, whose HEAD is reachable
//! from another ref, is removed — the checks an operator makes by hand before
//! `git worktree remove` + `git branch -D`. Anything else is foreign and fails
//! closed with a typed refusal instead of being adopted or erased.

use super::{
    capture_bounded, decode_git_text, default_branch, git_command, list_worktrees_locked,
    repository_identity_path, run_git_raw, run_git_text, validate_allocation_origin,
    with_repository_mutation,
};
use crate::error::{DaemonError, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Typed refusal class: a root exists at the candidate path but cannot be
/// proven to be this candidate's untouched allocation.
pub(crate) const RETAINED_SUCCESSOR_ROOT_FOREIGN: &str = "agent_successor_retained_sandbox_foreign";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetainedSuccessorRoot {
    /// Nothing exists at the candidate path.
    Absent,
    /// The candidate's own untouched allocation was removed.
    Reclaimed,
}

fn foreign(detail: &str) -> DaemonError {
    DaemonError::PolicyDenied(format!("{RETAINED_SUCCESSOR_ROOT_FOREIGN}:{detail}"))
}

/// Remove the retained allocation for `candidate` when, and only when, it is
/// provably that candidate's untouched worktree. Callers must already hold the
/// candidate's spawn guard and have proven no durable publication exists.
pub(crate) fn reclaim_retained_successor_root(
    base_dir: &Path,
    candidate: Uuid,
    origin: &Path,
) -> Result<RetainedSuccessorRoot> {
    let proposed = base_dir.join(candidate.to_string());
    if std::fs::symlink_metadata(&proposed).is_err() {
        return Ok(RetainedSuccessorRoot::Absent);
    }
    validate_allocation_origin(origin)?;
    with_repository_mutation(origin, || reclaim_locked(&proposed, candidate, origin))
}

/// Prove `proposed` is the candidate's untouched allocation; returns its
/// canonical root and observed HEAD. Any drift is a typed foreign refusal.
fn prove_untouched_candidate_root(
    proposed: &Path,
    metadata: &std::fs::Metadata,
    candidate: Uuid,
    origin: &Path,
) -> Result<(PathBuf, String)> {
    // `symlink_metadata` does not follow links: a symlinked root is foreign.
    if !metadata.is_dir() {
        return Err(foreign("not_a_directory"));
    }
    let root = std::fs::canonicalize(proposed).map_err(|_| foreign("root_unresolvable"))?;
    let candidate_name = candidate.to_string();
    if root.file_name() != Some(OsStr::new(&candidate_name)) {
        return Err(foreign("root_identity"));
    }
    let branch = default_branch(&candidate);
    let branch_ref = format!("refs/heads/{branch}");

    let expected_common = repository_identity_path(origin)?;
    let actual_common =
        repository_identity_path(&root).map_err(|_| foreign("repository_mismatch"))?;
    if expected_common != actual_common {
        return Err(foreign("repository_mismatch"));
    }
    let registered = list_worktrees_locked(origin)?
        .into_iter()
        .find(|worktree| {
            std::fs::canonicalize(&worktree.root).is_ok_and(|registered| registered == root)
        })
        .ok_or_else(|| foreign("unregistered"))?;
    if registered.branch.as_deref() != Some(branch_ref.as_str()) {
        return Err(foreign("branch_mismatch"));
    }
    let head = registered.head.ok_or_else(|| foreign("head_missing"))?;
    let checked_out = run_git_text(&root, &["symbolic-ref", "-q", "HEAD"], "retained branch")
        .map_err(|_| foreign("branch_mismatch"))?;
    let root_head = run_git_text(
        &root,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        "retained head",
    )
    .map_err(|_| foreign("head_missing"))?;
    if checked_out != branch_ref || root_head != head {
        return Err(foreign("branch_mismatch"));
    }
    // Pristine: no tracked edit, untracked file, or ignored artifact.
    let status = run_git_text(
        &root,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored=matching",
        ],
        "retained status",
    )?;
    if !status.is_empty() {
        return Err(foreign("dirty"));
    }
    // No commit is reachable only from the candidate branch.
    let exclude = format!("--exclude={branch}");
    let unique = run_git_text(
        origin,
        &[
            "rev-list",
            "--count",
            head.as_str(),
            "--not",
            exclude.as_str(),
            "--branches",
            "--remotes",
            "--tags",
        ],
        "retained unique commits",
    )?;
    if unique != "0" {
        return Err(foreign("unique_commits"));
    }
    Ok((root, head))
}

fn reclaim_locked(
    proposed: &Path,
    candidate: Uuid,
    origin: &Path,
) -> Result<RetainedSuccessorRoot> {
    let Ok(metadata) = std::fs::symlink_metadata(proposed) else {
        return Ok(RetainedSuccessorRoot::Absent);
    };
    let (root, head) = prove_untouched_candidate_root(proposed, &metadata, candidate, origin)?;
    let branch = default_branch(&candidate);
    let branch_ref = format!("refs/heads/{branch}");
    // No `--force`: Git itself refuses to drop anything it would lose.
    let root_str = root
        .to_str()
        .ok_or_else(|| foreign("root_identity"))?
        .to_owned();
    let mut remove = git_command();
    remove
        .args(["worktree", "remove", root_str.as_str()])
        .current_dir(origin);
    let removed = capture_bounded(remove, "remove retained successor worktree")?;
    if !removed.status.success() {
        return Err(DaemonError::Process(format!(
            "retained successor worktree removal failed: {}",
            String::from_utf8_lossy(&removed.stderr).trim()
        )));
    }
    // Compare-and-delete: refuses if the branch moved since it was observed.
    let mut delete = git_command();
    delete
        .args(["update-ref", "-d", branch_ref.as_str(), head.as_str()])
        .current_dir(origin);
    let deleted = capture_bounded(delete, "delete retained successor branch")?;
    if !deleted.status.success() {
        return Err(DaemonError::Process(format!(
            "retained successor branch deletion failed: {}",
            String::from_utf8_lossy(&deleted.stderr).trim()
        )));
    }
    if std::fs::symlink_metadata(&root).is_ok() {
        return Err(DaemonError::Process(
            "retained successor worktree survived removal".into(),
        ));
    }
    tracing::info!(
        candidate_session_id = %candidate,
        root = %root.display(),
        branch = %branch,
        "Reclaimed retained master-successor sandbox"
    );
    Ok(RetainedSuccessorRoot::Reclaimed)
}

/// Source commit for a master-successor allocation. The predecessor's
/// working tree is usually the shared checkout, whose local branch lags its
/// remote-tracking upstream between operator pulls (Issue #620). Prefer the
/// already-fetched upstream only when it strictly fast-forwards local HEAD;
/// local-only or diverged work keeps the prior HEAD behavior.
pub(crate) fn freshest_successor_source(origin: &Path) -> Result<String> {
    let head = run_git_text(
        origin,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        "resolve successor source",
    )
    .map_err(|_| DaemonError::InvalidParam("sandbox source commit is unavailable".into()))?;
    #[allow(clippy::literal_string_with_formatting_args)] // Git revision syntax.
    let upstream_rev = "@{upstream}^{commit}";
    let upstream = run_git_raw(
        origin,
        &["rev-parse", "--verify", "--quiet", upstream_rev],
        "resolve successor upstream",
    )?;
    if !upstream.status.success() {
        return Ok(head);
    }
    let upstream = decode_git_text(&upstream.stdout, "resolve successor upstream")?;
    if upstream.is_empty() || upstream == head {
        return Ok(head);
    }
    let fast_forward = run_git_raw(
        origin,
        &[
            "merge-base",
            "--is-ancestor",
            head.as_str(),
            upstream.as_str(),
        ],
        "compare successor upstream",
    )?;
    Ok(if fast_forward.status.success() {
        upstream
    } else {
        head
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::allocate;
    use super::*;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "f3@example.invalid"]);
        git(dir.path(), &["config", "user.name", "F3 Fixture"]);
        std::fs::write(dir.path().join("README.md"), "fixture\n").unwrap();
        git(dir.path(), &["add", "README.md"]);
        git(dir.path(), &["commit", "-q", "-m", "base"]);
        dir
    }

    #[test]
    fn retained_successor_root_absent_is_noop() {
        let repo = repo();
        let base = tempfile::tempdir().unwrap();
        assert_eq!(
            reclaim_retained_successor_root(base.path(), Uuid::new_v4(), repo.path()).unwrap(),
            RetainedSuccessorRoot::Absent
        );
    }

    #[test]
    fn retained_successor_root_pristine_allocation_is_reclaimed_and_reallocatable() {
        let repo = repo();
        let base = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        allocate(base.path(), id, repo.path(), "HEAD", None).unwrap();
        assert_eq!(
            reclaim_retained_successor_root(base.path(), id, repo.path()).unwrap(),
            RetainedSuccessorRoot::Reclaimed
        );
        let again = allocate(base.path(), id, repo.path(), "HEAD", None).unwrap();
        assert_eq!(again.branch.as_deref(), Some(default_branch(&id).as_str()));
        assert!(again.root.join("README.md").is_file());
    }

    #[test]
    fn retained_successor_root_with_evidence_or_unique_commit_is_foreign_and_kept() {
        for case in ["untracked", "commit", "branch"] {
            let repo = repo();
            let base = tempfile::tempdir().unwrap();
            let id = Uuid::new_v4();
            let branch = (case == "branch").then(|| format!("other/{id}"));
            let allocation =
                allocate(base.path(), id, repo.path(), "HEAD", branch.as_deref()).unwrap();
            match case {
                "untracked" => {
                    std::fs::write(allocation.root.join("evidence.txt"), "keep\n").unwrap();
                }
                "commit" => {
                    std::fs::write(allocation.root.join("work.txt"), "work\n").unwrap();
                    git(&allocation.root, &["add", "work.txt"]);
                    git(&allocation.root, &["commit", "-q", "-m", "unique"]);
                }
                _ => {}
            }
            let error = reclaim_retained_successor_root(base.path(), id, repo.path())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(RETAINED_SUCCESSOR_ROOT_FOREIGN),
                "{case}: {error}"
            );
            assert!(allocation.root.join("README.md").is_file(), "{case}");
        }
    }

    #[test]
    fn freshest_successor_source_fast_forwards_to_fetched_upstream_only() {
        let upstream = repo();
        let clone = tempfile::tempdir().unwrap();
        git(
            clone.path(),
            &["clone", "-q", upstream.path().to_str().unwrap(), "."],
        );
        let stale = git(clone.path(), &["rev-parse", "HEAD"]);
        assert_eq!(freshest_successor_source(clone.path()).unwrap(), stale);

        std::fs::write(upstream.path().join("next.txt"), "next\n").unwrap();
        git(upstream.path(), &["add", "next.txt"]);
        git(upstream.path(), &["commit", "-q", "-m", "next"]);
        git(clone.path(), &["fetch", "-q", "origin"]);
        let fetched = git(clone.path(), &["rev-parse", "origin/main"]);
        assert_ne!(fetched, stale);
        assert_eq!(freshest_successor_source(clone.path()).unwrap(), fetched);

        // Local-only work is never discarded in favor of the upstream.
        git(
            clone.path(),
            &["config", "user.email", "f3@example.invalid"],
        );
        git(clone.path(), &["config", "user.name", "F3 Fixture"]);
        std::fs::write(clone.path().join("local.txt"), "local\n").unwrap();
        git(clone.path(), &["add", "local.txt"]);
        git(clone.path(), &["commit", "-q", "-m", "local"]);
        let local = git(clone.path(), &["rev-parse", "HEAD"]);
        assert_eq!(freshest_successor_source(clone.path()).unwrap(), local);
    }
}
