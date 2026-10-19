//! Startup-time adoption of custody roots whose worktree was removed externally.
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::sandbox_reclaim::{AbsentRootCandidate, AdoptionOutcome};
use crate::store::source_worktree_v120::DependencyProofWork;
use rsid_core::pass_cost::{StageCosts, ThreadMeter};
use std::collections::{BTreeMap, HashMap, HashSet};
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
    let meter = ThreadMeter::start();
    let mut costs = AdoptionPassCosts::default();
    let candidates = store.absent_root_candidates(max_count)?;
    costs.candidates = candidates.len() as u64;
    let mut evidence = RepoEvidenceCache::default();
    // Retained outcomes only insert an audit row, so they are written one
    // transaction per batch instead of one commit each: about 300 commits per
    // restore on the hub (#2354). An adopted outcome commits its custody
    // transition with its own row, so pending retained rows go first and the
    // rows land in the order a one-commit-per-item run would have written.
    let mut retained: Vec<(AbsentRootCandidate, Option<String>, &'static str)> = Vec::new();
    // Restarts and periodic passes journal a retained candidate only when its
    // verdict changed (#2364); an operator or manager request asks for the full
    // report, so it keeps one row per candidate. The run's counts come from the
    // decisions made here, not from the rows written.
    let changed_only = !matches!(trigger, "operator" | "manager");
    let (mut adopted_count, mut retained_count) = (0usize, 0usize);
    for candidate in candidates {
        let (branch_oid, outcome) = match judge_absent_root_candidate(
            store,
            &base,
            &candidate,
            &mut evidence,
            &mut costs,
        ) {
            Ok(judged) => judged,
            Err(error) => {
                // Keep what the earlier candidates already earned, as the
                // per-item path did before returning the error.
                let _ =
                    store.record_absent_adoption_retained_batch(run_id, &retained, changed_only);
                return Err(error);
            }
        };
        match outcome {
            AdoptionOutcome::Retained(code) => {
                retained_count += 1;
                *costs.retained.entry(code).or_default() += 1;
                retained.push((candidate, branch_oid, code));
                if retained.len() >= RETAINED_BATCH {
                    store.record_absent_adoption_retained_batch(run_id, &retained, changed_only)?;
                    retained.clear();
                }
            }
            AdoptionOutcome::Adopted => {
                adopted_count += 1;
                store.record_absent_adoption_retained_batch(run_id, &retained, changed_only)?;
                retained.clear();
                store.record_absent_adoption_item(
                    run_id,
                    &candidate,
                    branch_oid.as_deref(),
                    outcome,
                    dry_run,
                )?;
            }
        }
    }
    store.record_absent_adoption_retained_batch(run_id, &retained, changed_only)?;
    store.finish_absent_adoption_run(run_id, None, adopted_count, retained_count)?;
    costs.adopted = adopted_count as u64;
    costs.log(trigger, dry_run, &meter.finish());
    Ok(run_id)
}

/// What one absent-root adoption pass did and cost (hub #2940): logged once
/// per pass, never per candidate. Evidence only; no verdict reads it.
#[derive(Default)]
struct AdoptionPassCosts {
    candidates: u64,
    adopted: u64,
    retained: BTreeMap<&'static str, u64>,
    /// Candidates that reached the dependency proof.
    dependency_proofs: u64,
    dependency_work: DependencyProofWork,
    stages: StageCosts,
}

impl AdoptionPassCosts {
    fn log(&self, trigger: &str, dry_run: bool, pass: &rsid_core::pass_cost::Cost) {
        // A pass with nothing to judge costs a query: keep it out of the log.
        if self.candidates == 0 {
            return;
        }
        tracing::info!(
            trigger,
            dry_run,
            candidates = self.candidates,
            adopted = self.adopted,
            retained = ?self.retained,
            dependency_proofs = self.dependency_proofs,
            dependency_enabled_jobs_validated = self.dependency_work.enabled_jobs_validated,
            dependency_relevant_sessions_validated =
                self.dependency_work.relevant_sessions_validated,
            dependency_indexed_job_rows = self.dependency_work.indexed_job_rows_loaded,
            dependency_indexed_session_rows = self.dependency_work.indexed_session_rows_loaded,
            dependency_path_canonicalizations = self.dependency_work.path_canonicalizations,
            wall_ms = pass.wall_ms(),
            thread_cpu_ms = pass.cpu_ms(),
            read_kib = pass.read_kib(),
            stage_cpu_wall_runs = %self.stages.summary(),
            "Sandbox absent-root adoption pass finished"
        );
    }
}

/// Retained audit rows written per transaction.
const RETAINED_BATCH: usize = 256;

/// The verdict for one candidate: its branch tip (when proven) and whether it
/// is adopted or retained with a reason code. Filesystem evidence is collected
/// before the database gate is rechecked, immediately before the commit.
fn judge_absent_root_candidate(
    store: &Store,
    base: &Path,
    candidate: &AbsentRootCandidate,
    evidence: &mut RepoEvidenceCache,
    costs: &mut AdoptionPassCosts,
) -> Result<(Option<String>, AdoptionOutcome)> {
    let gate = costs
        .stages
        .stage("database_gate", || store.absent_root_gate(candidate))?;
    if let Some(code) = gate {
        return Ok((None, AdoptionOutcome::Retained(code)));
    }
    let proved = costs
        .stages
        .stage("git_proof", || prove_absent_root(base, candidate, evidence));
    let oid = match proved {
        Ok(oid) => oid,
        Err(code) => return Ok((None, AdoptionOutcome::Retained(code))),
    };
    // Recheck custody and every database gate immediately before commit.
    costs.dependency_proofs += 1;
    let (dependency_verdict, work) = costs.stages.stage("dependency_proof", || {
        store.absent_root_dependency_gate_with_work(candidate)
    })?;
    costs.dependency_work.add(work);
    if let Some(code) = dependency_verdict {
        return Ok((Some(oid), AdoptionOutcome::Retained(code)));
    }
    let recheck = costs
        .stages
        .stage("database_gate", || store.absent_root_gate(candidate))?;
    Ok(match recheck {
        Some(code) => (Some(oid), AdoptionOutcome::Retained(code)),
        None => (Some(oid), AdoptionOutcome::Adopted),
    })
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
