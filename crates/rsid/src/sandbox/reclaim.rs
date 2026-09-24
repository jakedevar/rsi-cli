//! Startup-time adoption of custody roots whose worktree was removed externally.
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::sandbox_reclaim::{AbsentRootCandidate, AdoptionOutcome};
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
    for candidate in candidates {
        let (branch_oid, outcome) = match store.absent_root_gate(&candidate)? {
            Some(code) => (None, AdoptionOutcome::Retained(code)),
            None => match prove_absent_root(&base, &candidate) {
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

fn prove_absent_root(
    base: &Path,
    candidate: &AbsentRootCandidate,
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
            if listed == path
                || (std::fs::canonicalize(&listed).ok().as_deref() == Some(path.as_path()))
            {
                return Err("worktree_registered");
            }
        }
    }
    let branch = if candidate.sandbox_branch.starts_with("refs/heads/") {
        candidate.sandbox_branch.clone()
    } else {
        format!("refs/heads/{}", candidate.sandbox_branch)
    };
    let shown = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["show-ref", "--verify", "--hash"])
        .arg(&branch)
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
        .args(["symbolic-ref", "-q", &branch])
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
