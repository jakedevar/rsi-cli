//! Startup-time adoption of custody roots whose worktree was removed externally.
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::sandbox_reclaim::{AbsentRootCandidate, AdoptionOutcome};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

/// Run a bounded absent-root adoption pass. Filesystem evidence is collected
/// before each store transition; ambiguous evidence is retained with a code.
pub fn run_absent_root_adoption(
    store: &mut Store,
    sandbox_base: &Path,
    trigger: &str,
    dry_run: bool,
    max_count: usize,
    manager_operation_id: Option<Uuid>,
) -> Result<Uuid> {
    if !matches!(
        trigger,
        "startup" | "pressure" | "periodic" | "operator" | "manager"
    ) {
        return Err(DaemonError::InvalidParam(
            "invalid sandbox reclaim trigger".into(),
        ));
    }
    if max_count == 0 {
        return Err(DaemonError::InvalidParam(
            "max_count must be positive".into(),
        ));
    }
    let base = std::fs::canonicalize(sandbox_base)
        .map_err(|error| DaemonError::Process(format!("canonicalize sandbox base: {error}")))?;
    let (run_id, created) =
        store.begin_absent_adoption_run(trigger, dry_run, max_count, manager_operation_id)?;
    if !created {
        return Ok(run_id);
    }
    let candidates = store.absent_root_candidates(max_count)?;
    let mut evidence = RepoEvidenceCache::default();
    for candidate in candidates {
        let (branch_oid, outcome) = match store.absent_root_gate(&candidate)? {
            Some(code) => (None, AdoptionOutcome::Retained(code)),
            None => match prove_absent_root(&base, &candidate, &mut evidence) {
                Ok(oid) => {
                    // Recheck custody and every database gate immediately before commit.
                    match store.absent_root_dependency_gate(&candidate)? {
                        Some(code) => (Some(oid), AdoptionOutcome::Retained(code)),
                        None => match store.absent_root_gate(&candidate)? {
                            Some(code) => (Some(oid), AdoptionOutcome::Retained(code)),
                            None => (Some(oid), AdoptionOutcome::Adopted),
                        },
                    }
                }
                Err(code) => (None, AdoptionOutcome::Retained(code)),
            },
        };
        store.record_absent_adoption_item(
            run_id,
            &candidate,
            branch_oid.as_deref(),
            outcome,
            dry_run,
        )?;
    }
    store.finish_absent_adoption_run(run_id, None)?;
    Ok(run_id)
}

/// Repository evidence read once per adoption run (#961).
///
/// Every startup re-examines every retained candidate, and most of them are
/// permanently retained (`branch_missing`: directory, registration and branch
/// all gone). Reading `git worktree list` and `git show-ref` per candidate cost
/// ~51 ms each, 18 s for 351 such roots, all before `request_ready`. A run now
/// reads each repository's worktree listing and branch refs once.
///
/// The snapshot only ever selects a retain outcome, which has no custody
/// effect. A candidate the snapshot cannot retain still runs the full fresh
/// proof, so an adoption never rests on snapshot evidence.
#[derive(Default)]
struct RepoEvidenceCache {
    repos: HashMap<PathBuf, RepoEvidence>,
}

struct RepoEvidence {
    /// Listed worktree paths, raw and canonicalized; `None` when the listing
    /// could not be read.
    registered: Option<HashSet<PathBuf>>,
    /// Full names of every branch ref; `None` when refs could not be read.
    branches: Option<HashSet<String>>,
}

impl RepoEvidenceCache {
    fn repo(&mut self, repo: &Path) -> &RepoEvidence {
        self.repos
            .entry(repo.to_path_buf())
            .or_insert_with(|| RepoEvidence::read(repo))
    }
}

impl RepoEvidence {
    fn read(repo: &Path) -> Self {
        let registered = git_stdout(repo, &["worktree", "list", "--porcelain"]).map(|text| {
            let mut paths = HashSet::new();
            for value in text
                .lines()
                .filter_map(|line| line.strip_prefix("worktree "))
            {
                let listed = PathBuf::from(value);
                if let Ok(canonical) = std::fs::canonicalize(&listed) {
                    paths.insert(canonical);
                }
                paths.insert(listed);
            }
            paths
        });
        let branches = git_stdout(
            repo,
            &["for-each-ref", "--format=%(refname)", "refs/heads/"],
        )
        .map(|text| text.lines().map(str::to_owned).collect());
        Self {
            registered,
            branches,
        }
    }

    /// The retain code this snapshot proves, in the fresh proof's order.
    fn retain_code(&self, path: &Path, branch: &str) -> Option<&'static str> {
        let registered = self.registered.as_ref()?;
        if registered.contains(path) {
            return Some("worktree_registered");
        }
        match &self.branches {
            Some(branches) if !branches.contains(branch) => Some("branch_missing"),
            _ => None,
        }
    }
}

fn git_stdout(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn branch_ref(candidate: &AbsentRootCandidate) -> String {
    if candidate.sandbox_branch.starts_with("refs/heads/") {
        candidate.sandbox_branch.clone()
    } else {
        format!("refs/heads/{}", candidate.sandbox_branch)
    }
}

fn prove_absent_root(
    base: &Path,
    candidate: &AbsentRootCandidate,
    evidence: &mut RepoEvidenceCache,
) -> std::result::Result<String, &'static str> {
    let allocation = Uuid::parse_str(
        candidate
            .allocation_id
            .as_deref()
            .ok_or("allocation_id_missing")?,
    )
    .map_err(|_| "allocation_id_invalid")?;
    let path = base.join(allocation.to_string());
    let stored = PathBuf::from(&candidate.sandbox_root);
    if stored != path {
        return Err("sandbox_path_mismatch");
    }
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return Err("directory_present"),
        Err(_) => return Err("directory_probe_failed"),
    }
    let repo = Path::new(&candidate.canonical_repo_dir);
    let branch = branch_ref(candidate);
    if let Some(code) = evidence.repo(repo).retain_code(&path, &branch) {
        return Err(code);
    }
    prove_absent_root_fresh(repo, &path, &branch)
}

/// Fresh registration and branch proof. Runs only for candidates the run's
/// snapshot could not retain.
fn prove_absent_root_fresh(
    repo: &Path,
    path: &Path,
    branch: &str,
) -> std::result::Result<String, &'static str> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|_| "worktree_list_failed")?;
    if !output.status.success() {
        return Err("worktree_list_failed");
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|_| "worktree_list_invalid")?;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("worktree ") {
            let listed = PathBuf::from(value);
            if listed == path || (std::fs::canonicalize(&listed).ok().as_deref() == Some(path)) {
                return Err("worktree_registered");
            }
        }
    }
    let shown = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["show-ref", "--verify", "--hash"])
        .arg(branch)
        .output()
        .map_err(|_| "branch_probe_failed")?;
    if !shown.status.success() {
        return Err("branch_missing");
    }
    let oid = std::str::from_utf8(&shown.stdout)
        .map_err(|_| "branch_probe_invalid")?
        .trim()
        .to_owned();
    if oid.len() != 40 && oid.len() != 64 {
        return Err("branch_not_direct_commit");
    }
    let symbolic = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["symbolic-ref", "-q", branch])
        .output()
        .map_err(|_| "branch_probe_failed")?;
    if symbolic.status.success() {
        return Err("branch_not_direct_commit");
    }
    let kind = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["cat-file", "-t", &oid])
        .output()
        .map_err(|_| "branch_probe_failed")?;
    if !kind.status.success()
        || std::str::from_utf8(&kind.stdout).ok().map(str::trim) != Some("commit")
    {
        return Err("branch_not_direct_commit");
    }
    Ok(oid)
}

#[cfg(test)]
mod tests;
