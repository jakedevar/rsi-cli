//! Bounded purge of archived sandboxes whose work is provably in Git.
use super::reaper;
use crate::error::{DaemonError, Result};
use crate::sandbox::git_worktree::{self, DirectRefObservation};
use crate::store::custody_lock_order::BlockingStoreLockExt;
use crate::store::sandbox_custody;
use crate::store::sandbox_reclaim::ArchivedSandboxPurgeCandidate;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const ARCHIVED_SANDBOX_PURGE_INTERVAL: Duration = Duration::from_secs(600);
const ARCHIVED_SANDBOX_PURGE_MAX_PER_PASS: usize = 32;
const UNLANDED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ArchivedSandboxPurgeCounts {
    pub no_output: u64,
    pub integrated: u64,
    pub preserved: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArchivedSandboxPurgeCursor {
    pub updated_at: String,
    pub session_id: Uuid,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ArchivedSandboxPurgeReport {
    pub purged: ArchivedSandboxPurgeCounts,
    pub retained: BTreeMap<String, u64>,
    pub bytes_freed: u64,
    pub dry_run: bool,
    pub candidates_examined: u64,
    pub next_cursor: Option<ArchivedSandboxPurgeCursor>,
}

#[derive(Debug, Clone, Copy)]
enum PurgedKind {
    NoOutput,
    Integrated,
    Preserved,
}

#[derive(Debug, Clone)]
enum CandidateOutcome {
    Purged(PurgedKind, u64),
    Retained(&'static str),
}

enum PreservationResult {
    Success,
    Failed,
    Conflict,
}

impl ArchivedSandboxPurgeReport {
    fn retain(&mut self, code: &'static str) {
        *self.retained.entry(code.to_string()).or_default() += 1;
    }

    const fn purged(&mut self, kind: PurgedKind, bytes: u64) {
        match kind {
            PurgedKind::NoOutput => self.purged.no_output += 1,
            PurgedKind::Integrated => self.purged.integrated += 1,
            PurgedKind::Preserved => self.purged.preserved += 1,
        }
        self.bytes_freed = self.bytes_freed.saturating_add(bytes);
    }
}

impl super::SessionManager {
    pub(crate) async fn run_archived_sandbox_purge(
        &self,
        dry_run: bool,
        max_count: usize,
        after: Option<ArchivedSandboxPurgeCursor>,
    ) -> Result<ArchivedSandboxPurgeReport> {
        let candidates = {
            let store = self.store.lock().await;
            store.archived_sandbox_purge_candidates(
                after.map(|cursor| (cursor.updated_at, cursor.session_id)),
                max_count,
            )?
        };
        let mut report = ArchivedSandboxPurgeReport {
            dry_run,
            ..ArchivedSandboxPurgeReport::default()
        };
        report.candidates_examined = u64::try_from(candidates.len())
            .map_err(|_| DaemonError::Store("candidate count overflowed".into()))?;
        if candidates.len() == max_count
            && let Some(last) = candidates.last()
        {
            report.next_cursor = Some(ArchivedSandboxPurgeCursor {
                updated_at: last.updated_at.clone(),
                session_id: last.session_id,
            });
        }
        for candidate in candidates {
            if self.active.read().await.contains_key(&candidate.session_id) {
                report.retain("active_session");
                continue;
            }
            let store = Arc::clone(&self.store);
            let active = Arc::clone(&self.active);
            let sandbox_base = self.sandbox_allocator.base_dir().to_path_buf();
            let session_candidate = candidate.clone();
            // Tests replace live /proc (host processes such as io_uring users
            // make it nondeterministic) with the archive-cleanup synthetic
            // inventory registered for this Session.
            #[cfg(test)]
            let test_holder_proc = super::archive_cleanup::archive_cleanup_test_holder_proc(
                candidate.session_id,
                &sandbox_base,
            );
            let outcome = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                if let Some((proc_root, uid)) = test_holder_proc {
                    return reaper::with_quarantine_holder_test_proc(&proc_root, uid, || {
                        process_candidate(
                            &store,
                            &active,
                            &sandbox_base,
                            &session_candidate,
                            dry_run,
                        )
                    });
                }
                process_candidate(&store, &active, &sandbox_base, &session_candidate, dry_run)
            })
            .await
            .map_err(|_| DaemonError::Process("archived sandbox purge worker panicked".into()))?;
            match outcome {
                Ok(CandidateOutcome::Purged(kind, bytes)) => report.purged(kind, bytes),
                Ok(CandidateOutcome::Retained(code)) => report.retain(code),
                Err(error) => {
                    tracing::warn!(
                        session_id = %candidate.session_id,
                        error = %error,
                        "Archived sandbox purge retained a candidate whose proof failed"
                    );
                    report.retain("proof_failed");
                }
            }
        }
        Ok(report)
    }

    pub async fn run_archived_sandbox_purge_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(ARCHIVED_SANDBOX_PURGE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut cursor = None;
        loop {
            interval.tick().await;
            if !self
                .runtime_config
                .archived_sandbox_purge_enabled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                continue;
            }
            match self
                .run_archived_sandbox_purge(
                    false,
                    ARCHIVED_SANDBOX_PURGE_MAX_PER_PASS,
                    cursor.clone(),
                )
                .await
            {
                Ok(report) => {
                    cursor.clone_from(&report.next_cursor);
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "Archived sandbox purge pass deferred"
                    );
                }
            }
        }
    }
}

// One linear proof -> classify -> effect -> finalize sequence; splitting it
// would scatter the fences that must stay in this order.
#[allow(clippy::too_many_lines)]
fn process_candidate(
    store: &Arc<tokio::sync::Mutex<crate::store::Store>>,
    active: &Arc<tokio::sync::RwLock<HashMap<Uuid, super::types::TrackedSession>>>,
    sandbox_base: &Path,
    candidate: &ArchivedSandboxPurgeCandidate,
    dry_run: bool,
) -> Result<CandidateOutcome> {
    if active.blocking_read().contains_key(&candidate.session_id) {
        return Ok(CandidateOutcome::Retained("active_session"));
    }
    reaper::prove_archive_cleanup_has_no_provider_processes(&[candidate.session_id])?;
    let root = PathBuf::from(&candidate.sandbox_root);
    let origin = PathBuf::from(&candidate.canonical_repo_dir);
    let source_ref = format!("refs/heads/{}", candidate.sandbox_branch);
    let root_exists = root.exists();
    prove_exact_allocation_path(sandbox_base, &root, candidate, root_exists)?;
    let _root_guard = sandbox_custody::lock_custody_root(candidate.custody_id);
    // The slow, read-only proofs (worktree observation, quarantine tree proof,
    // and the /proc-wide holder scan) run before the repository mutation lock:
    // `git_worktree::allocate` takes the same lock, so holding it across a slow
    // proof would starve every sandbox allocation in this repository (#1123).
    // The locked sections below only do cheap Git reads and re-validate that
    // the proved tree is unchanged before removal.
    let unlocked_proof = if root_exists {
        let observation = git_worktree::observe_worktree_ignoring_ignored_locked(&origin, &root)?;
        if !observation.clean {
            return Ok(CandidateOutcome::Retained("worktree_dirty"));
        }
        let tree = git_worktree::prove_quarantine_tree_safe(&root)?;
        #[cfg(test)]
        proof_test_hook::run(&root, proof_test_hook::Stage::BeforeHolderProof);
        reaper::prove_quarantine_has_no_untrusted_same_uid_holders(&tree)?;
        #[cfg(test)]
        proof_test_hook::run(&root, proof_test_hook::Stage::AfterProofs);
        Some(tree)
    } else {
        None
    };
    let tip = git_worktree::with_repository_mutation(&origin, || {
        if root_exists {
            let tip = branch_tip(&origin, &source_ref)?;
            git_worktree::prove_registered_worktree_exact_ignoring_ignored_locked(
                &origin,
                &root,
                &source_ref,
                &tip,
            )?;
            Ok(tip)
        } else {
            if std::fs::symlink_metadata(&root).is_ok()
                || !git_worktree::prove_worktree_unregistered_locked(&origin, &root)?
            {
                return Err(DaemonError::Process(
                    "absent archived sandbox recovery has root or registration residue".into(),
                ));
            }
            match git_worktree::observe_direct_ref_locked(&origin, &source_ref)? {
                DirectRefObservation::Commit(tip) => Ok(tip),
                DirectRefObservation::Missing if candidate.source_commit.len() == 40 => {
                    Ok(candidate.source_commit.clone())
                }
                _ => Err(DaemonError::Process(
                    "absent archived sandbox has no provable branch tip".into(),
                )),
            }
        }
    })?;
    {
        let store = store.blocking_lock_checked()?;
        let dependencies = if root_exists {
            store
                .source_worktree_targeted_dependencies(candidate.custody_id, candidate.generation)?
        } else {
            store.source_worktree_targeted_dependencies_for_absent_root(
                candidate.custody_id,
                candidate.generation,
            )?
        };
        if !dependencies.complete {
            return Ok(CandidateOutcome::Retained(
                dependencies.reason.unwrap_or("dependency_incomplete"),
            ));
        }
        if dependencies.scheduled_dependency_count != 0
            || dependencies.session_path_dependency_count != 0
        {
            return Ok(CandidateOutcome::Retained("external_dependency"));
        }
        if !store.archived_sandbox_purge_candidate_is_current(candidate)? {
            return Ok(CandidateOutcome::Retained("candidate_changed"));
        }
        if store
            .session_has_pending_consumer(&candidate.session_id.to_string(), candidate.custody_id)?
        {
            return Ok(CandidateOutcome::Retained("pending_consumer"));
        }
    }
    let Some(kind) = classify_tip(&origin, candidate, &tip)? else {
        return Ok(CandidateOutcome::Retained("unlanded_recent"));
    };
    if dry_run {
        return Ok(CandidateOutcome::Purged(
            kind,
            purge_bytes(root_exists, &root)?,
        ));
    }
    if matches!(kind, PurgedKind::Preserved) {
        match preserve_tip(&origin, candidate, &tip)? {
            PreservationResult::Success => {}
            PreservationResult::Failed => {
                return Ok(CandidateOutcome::Retained("preservation_failed"));
            }
            PreservationResult::Conflict => {
                return Ok(CandidateOutcome::Retained("preservation_ref_conflict"));
            }
        }
    }
    let root_bytes = purge_bytes(root_exists, &root)?;
    let removed = git_worktree::with_repository_mutation(&origin, || {
        if let Some(tree) = &unlocked_proof {
            // The unlocked proof is only as fresh as this check: a tree that
            // changed since (a late writer, a new holder's output) is retained
            // for the next pass instead of removed on a stale proof.
            if !git_worktree::quarantine_tree_matches(tree)? {
                return Ok::<bool, DaemonError>(false);
            }
            // The holder proof above ran outside the lock and is stale by now:
            // a task that took a cwd or fd inside the tree since is missed by
            // the digest check. Re-prove, with its own short wall-clock budget
            // so the locked section stays bounded, immediately before the
            // destructive step; a holder or a timeout retains the candidate.
            reaper::reprove_quarantine_has_no_untrusted_same_uid_holders_within(
                &origin,
                tree,
                reaper::QUARANTINE_HOLDER_LOCKED_REPROOF_BUDGET,
            )?;
            git_worktree::remove_live_worktree_non_force_locked(&origin, &root, &source_ref, &tip)?;
        }
        if git_worktree::observe_direct_ref_locked(&origin, &source_ref)?
            == DirectRefObservation::Commit(tip.clone())
        {
            git_worktree::delete_ref_compare_locked(&origin, &source_ref, &tip)?;
        }
        Ok(true)
    })?;
    if !removed {
        return Ok(CandidateOutcome::Retained("worktree_changed"));
    }
    {
        let mut store = store.blocking_lock_checked()?;
        store.finalize_archived_sandbox_purge(candidate)?;
    }
    Ok(CandidateOutcome::Purged(kind, root_bytes))
}

fn branch_tip(origin: &Path, source_ref: &str) -> Result<String> {
    match git_worktree::observe_direct_ref_locked(origin, source_ref)? {
        DirectRefObservation::Commit(tip) => Ok(tip),
        _ => Err(DaemonError::Process(
            "archived sandbox branch is not an exact commit ref".into(),
        )),
    }
}

fn prove_exact_allocation_path(
    sandbox_base: &Path,
    root: &Path,
    candidate: &ArchivedSandboxPurgeCandidate,
    root_exists: bool,
) -> Result<()> {
    let canonical_base = std::fs::canonicalize(sandbox_base)
        .map_err(|_| DaemonError::Process("sandbox base is unavailable".into()))?;
    let observed_root = if root_exists {
        std::fs::canonicalize(root)
            .map_err(|_| DaemonError::Process("sandbox root is unavailable".into()))?
    } else {
        root.to_path_buf()
    };
    let allocation_name = candidate.allocation_id.to_string();
    if observed_root.parent() != Some(canonical_base.as_path())
        || observed_root.file_name().and_then(|value| value.to_str())
            != Some(allocation_name.as_str())
        || std::fs::symlink_metadata(root).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(DaemonError::Process(
            "sandbox root is not the exact private allocation path".into(),
        ));
    }
    Ok(())
}

fn classify_tip(
    origin: &Path,
    candidate: &ArchivedSandboxPurgeCandidate,
    tip: &str,
) -> Result<Option<PurgedKind>> {
    if tip == candidate.source_commit {
        return Ok(Some(PurgedKind::NoOutput));
    }
    let target = git_worktree::observe_repository_target_locked(origin)?;
    if git_worktree::is_ancestor_locked(origin, tip, &target.target_oid)? {
        return Ok(Some(PurgedKind::Integrated));
    }
    if let Some(upstream_ref) = git_worktree::observe_configured_upstream_ref_locked(origin)?
        && let DirectRefObservation::Commit(upstream_oid) =
            git_worktree::observe_direct_ref_locked(origin, &upstream_ref)?
        && git_worktree::is_ancestor_locked(origin, tip, &upstream_oid)?
    {
        return Ok(Some(PurgedKind::Integrated));
    }
    let updated_at = DateTime::parse_from_rfc3339(&candidate.updated_at)
        .map_err(|_| DaemonError::Store("archived Session timestamp is invalid".into()))?
        .with_timezone(&Utc);
    let retention = chrono::Duration::from_std(UNLANDED_RETENTION)
        .map_err(|_| DaemonError::Store("retention duration is invalid".into()))?;
    if Utc::now().signed_duration_since(updated_at) < retention {
        return Ok(None);
    }
    Ok(Some(PurgedKind::Preserved))
}

fn preserve_tip(
    origin: &Path,
    candidate: &ArchivedSandboxPurgeCandidate,
    tip: &str,
) -> Result<PreservationResult> {
    let reference = format!("refs/rsi/abandoned/{}", candidate.session_id);
    let push_refspec = format!("{tip}:{reference}");
    let push = run_preservation_git(origin, &["push", "origin", &push_refspec])?;
    if push.status.success() {
        return read_back_preserved_tip(origin, tip, &reference);
    }
    match read_back_preserved_tip(origin, tip, &reference)? {
        PreservationResult::Conflict => Ok(PreservationResult::Conflict),
        _ => Ok(PreservationResult::Failed),
    }
}

fn run_preservation_git(origin: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("timeout")
        .current_dir(origin)
        .arg("--kill-after=5")
        .arg("120")
        .arg("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=15",
        )
        .args(args)
        .output()
        .map_err(|error| DaemonError::Process(format!("Git preservation command failed: {error}")))
}

fn observed_reference_tip(output: &std::process::Output) -> Option<String> {
    if !output.status.success() {
        return None;
    }
    let observed = String::from_utf8_lossy(&output.stdout);
    let tip = observed.split_whitespace().next()?.to_string();
    if tip.is_empty() { None } else { Some(tip) }
}

fn read_back_preserved_tip(
    origin: &Path,
    tip: &str,
    reference: &str,
) -> Result<PreservationResult> {
    let remote = run_preservation_git(origin, &["ls-remote", "origin", reference])?;
    match observed_reference_tip(&remote) {
        Some(observed_tip) if observed_tip == tip => Ok(PreservationResult::Success),
        Some(_) => Ok(PreservationResult::Conflict),
        None => Ok(PreservationResult::Failed),
    }
}

fn purge_bytes(root_exists: bool, root: &Path) -> Result<u64> {
    if root_exists {
        directory_size(root)
    } else {
        Ok(0)
    }
}

fn directory_size(root: &Path) -> Result<u64> {
    let mut bytes = 0_u64;
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry
            .map_err(|error| DaemonError::Process(format!("sandbox size proof failed: {error}")))?;
        if !entry.file_type().is_symlink()
            && let Some(metadata) = entry.metadata().ok()
        {
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok(bytes)
}

/// Test seam: lets a test park or mutate the unlocked proof section of
/// `process_candidate`, keyed by sandbox root so parallel tests do not collide.
#[cfg(test)]
mod proof_test_hook {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Stage {
        BeforeHolderProof,
        AfterProofs,
    }

    type Hook = Arc<dyn Fn(Stage) + Send + Sync>;

    static HOOKS: Mutex<Option<HashMap<PathBuf, Hook>>> = Mutex::new(None);

    pub(super) fn install(root: &Path, hook: impl Fn(Stage) + Send + Sync + 'static) {
        HOOKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(HashMap::new)
            .insert(root.to_path_buf(), Arc::new(hook));
    }

    pub(super) fn run(root: &Path, stage: Stage) {
        let hook = HOOKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|hooks| hooks.get(root).cloned());
        if let Some(hook) = hook {
            hook(stage);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::config::{Config, RuntimeConfig};
    use crate::sandbox::SandboxAllocator;
    use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
    use rsi_common::types::{SandboxCleanupState, SandboxKind, SessionKind, SessionStatus};
    use rsid_store::test_support::make_test_session;
    use std::process::Command;

    struct PurgeFixture {
        _temp: tempfile::TempDir,
        store: Arc<std::sync::Mutex<crate::store::Store>>,
        database: PathBuf,
        repository: PathBuf,
        bare_origin: PathBuf,
        sandbox_base: PathBuf,
        root: PathBuf,
        source_ref: String,
        session_id: Uuid,
        source_commit: String,
    }

    struct ExtraCandidate {
        session_id: Uuid,
        root: PathBuf,
    }

    impl PurgeFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("fixture root");
            let repository = temp.path().join("repository");
            let bare_origin = temp.path().join("origin.git");
            let sandbox_base = temp.path().join("sandboxes");
            std::fs::create_dir(&repository).expect("create repository");
            std::fs::create_dir(&bare_origin).expect("create bare origin");
            std::fs::create_dir(&sandbox_base).expect("create sandbox base");
            git(&repository, &["init", "-q", "-b", "rolling"]);
            git(&bare_origin, &["init", "-q", "--bare", "-b", "rolling"]);
            git(&repository, &["config", "user.email", "purge@example.test"]);
            git(&repository, &["config", "user.name", "Purge Fixture"]);
            std::fs::write(repository.join("tracked"), "base\n").expect("write base");
            std::fs::write(repository.join(".gitignore"), "target/\n").expect("write gitignore");
            git(&repository, &["add", "tracked", ".gitignore"]);
            git(&repository, &["commit", "-qm", "base"]);
            git(
                &repository,
                &[
                    "remote",
                    "add",
                    "origin",
                    bare_origin.display().to_string().as_str(),
                ],
            );
            git(&repository, &["push", "-q", "-u", "origin", "rolling"]);
            let source_commit = git(&repository, &["rev-parse", "HEAD"]);
            let allocation_id = Uuid::new_v4();
            let allocation = SandboxAllocator::new(sandbox_base.clone())
                .allocate(
                    allocation_id,
                    &repository,
                    SandboxKind::GitWorktree,
                    &source_commit,
                    None,
                )
                .expect("allocate worktree");
            let branch = allocation.branch.clone().expect("allocation branch");
            let common_dir = git(
                &repository,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            );
            let common_dir = std::fs::canonicalize(common_dir).expect("canonical common dir");
            let session_id = Uuid::new_v4();
            let mut session = make_test_session();
            session.id = session_id;
            session.project_id = None;
            session.session_kind = SessionKind::Task;
            session.status = SessionStatus::Archived;
            session.working_dir = repository.clone();
            session.git_branch = Some("rolling".into());
            session.sandbox_kind = Some(SandboxKind::GitWorktree);
            session.sandbox_root = Some(allocation.root.clone());
            session.sandbox_branch = Some(branch.clone());
            session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            session.updated_at = Utc::now();
            let database = temp.path().join("purge.db");
            let mut store = crate::store::Store::open(&database).expect("open store");
            store
                .insert_session_with_custody(
                    &session,
                    SessionCustodyBinding::New(NewCustodyRoot {
                        custody_id: Uuid::new_v4(),
                        canonical_repo_dir: repository.display().to_string(),
                        sandbox_root: allocation.root.display().to_string(),
                        sandbox_branch: branch.clone(),
                        repository_identity: common_dir.display().to_string(),
                        source_commit: source_commit.clone(),
                        cause: CustodyCause::FreshLaunch,
                    }),
                )
                .expect("insert archived Session");
            Self {
                _temp: temp,
                store: Arc::new(std::sync::Mutex::new(store)),
                database,
                repository,
                bare_origin,
                sandbox_base,
                root: allocation.root,
                source_ref: format!("refs/heads/{branch}"),
                session_id,
                source_commit,
            }
        }

        fn manager(&self) -> super::super::SessionManager {
            let config = Config::from_env();
            let runtime = RuntimeConfig::from_config(&config);
            runtime
                .sandbox_min_free_gib
                .store(0, std::sync::atomic::Ordering::Relaxed);
            super::super::SessionManager::new(
                Arc::new(EventBus::new(1024)),
                crate::store::Store::open(&self.database).expect("reopen store"),
                false,
                self._temp
                    .path()
                    .join(format!("purge-{}.sock", Uuid::new_v4())),
                None,
                Vec::new(),
                runtime,
                self.sandbox_base.clone(),
            )
            .expect("create manager")
        }

        async fn run(&self, dry_run: bool) -> ArchivedSandboxPurgeReport {
            let manager = self.manager();
            let _holder = manager.install_archive_cleanup_test_holder_proc(self.session_id);
            manager
                .run_archived_sandbox_purge(dry_run, 32, None)
                .await
                .expect("purge pass")
        }

        fn commit_unique_output(&self) -> String {
            std::fs::write(self.root.join("tracked"), "unique output\n").expect("write output");
            git(&self.root, &["add", "tracked"]);
            git(&self.root, &["commit", "-qm", "unique output"]);
            git(&self.root, &["rev-parse", "HEAD"])
        }

        fn age_session(&self, hours: i64) {
            self.set_session_updated_at(self.session_id, hours);
        }

        fn set_session_updated_at(&self, session_id: Uuid, hours: i64) {
            let updated_at = (Utc::now() - chrono::Duration::hours(hours))
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            self.store
                .lock()
                .expect("lock store")
                .conn
                .execute(
                    "UPDATE sessions SET updated_at=?2 WHERE id=?1",
                    rusqlite::params![session_id.to_string(), updated_at],
                )
                .expect("age Session");
        }

        fn add_candidate(&self, age_hours: i64) -> ExtraCandidate {
            let allocation_id = Uuid::new_v4();
            let allocation = SandboxAllocator::new(self.sandbox_base.clone())
                .allocate(
                    allocation_id,
                    &self.repository,
                    SandboxKind::GitWorktree,
                    &self.source_commit,
                    None,
                )
                .expect("allocate extra candidate worktree");
            let branch = allocation.branch.clone().expect("extra candidate branch");
            let common_dir = git(
                &self.repository,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            );
            let common_dir = std::fs::canonicalize(common_dir).expect("canonical common dir");
            let session_id = Uuid::new_v4();
            let mut session = make_test_session();
            session.id = session_id;
            session.project_id = None;
            session.session_kind = SessionKind::Task;
            session.status = SessionStatus::Archived;
            session.working_dir = self.repository.clone();
            session.git_branch = Some("rolling".into());
            session.sandbox_kind = Some(SandboxKind::GitWorktree);
            session.sandbox_root = Some(allocation.root.clone());
            session.sandbox_branch = Some(branch.clone());
            session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            session.updated_at = Utc::now() - chrono::Duration::hours(age_hours);
            self.store
                .lock()
                .expect("lock store")
                .insert_session_with_custody(
                    &session,
                    SessionCustodyBinding::New(NewCustodyRoot {
                        custody_id: Uuid::new_v4(),
                        canonical_repo_dir: self.repository.display().to_string(),
                        sandbox_root: allocation.root.display().to_string(),
                        sandbox_branch: branch,
                        repository_identity: common_dir.display().to_string(),
                        source_commit: self.source_commit.clone(),
                        cause: CustodyCause::FreshLaunch,
                    }),
                )
                .expect("insert extra archived Session");
            ExtraCandidate {
                session_id,
                root: allocation.root,
            }
        }

        fn session_row(&self) -> (String, Option<String>, Option<String>) {
            self.store
                .lock()
                .expect("lock store")
                .conn
                .query_row(
                    "SELECT sandbox_cleanup_state,sandbox_root,sandbox_branch FROM sessions WHERE id=?1",
                    [self.session_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("read Session")
        }
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        assert!(
            cwd.exists(),
            "fixture Git directory is unavailable: {cwd:?}"
        );
        let output = Command::new("/usr/bin/git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("run fixture Git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout)
            .expect("Git output is UTF-8")
            .trim()
            .to_string()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn integrated_sandbox_is_purged_and_tombstoned() {
        let fixture = PurgeFixture::new();
        std::fs::write(fixture.root.join("tracked"), "integrated\n").expect("write output");
        git(&fixture.root, &["add", "tracked"]);
        git(&fixture.root, &["commit", "-qm", "integrated output"]);
        git(
            &fixture.repository,
            &[
                "merge",
                "--ff-only",
                fixture.source_ref.trim_start_matches("refs/heads/"),
            ],
        );
        git(&fixture.repository, &["push", "-q", "origin", "rolling"]);
        let report = fixture.run(false).await;
        assert_eq!(report.purged.integrated, 1);
        assert_eq!(report.candidates_examined, 1);
        assert!(!fixture.root.exists());
        assert_eq!(
            git(&fixture.repository, &["for-each-ref", &fixture.source_ref]),
            ""
        );
        assert_eq!(fixture.session_row(), ("Purged".into(), None, None));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn upstream_integration_purges_despite_stale_local_rolling() {
        let fixture = PurgeFixture::new();
        let tip = fixture.commit_unique_output();
        git(
            &fixture.repository,
            &["push", "-q", "origin", &format!("{tip}:refs/heads/rolling")],
        );
        let report = fixture.run(false).await;
        assert_eq!(report.purged.integrated, 1);
        assert!(!fixture.root.exists());
        assert_eq!(fixture.session_row().0, "Purged");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn recent_unlanded_sandbox_is_retained_without_git_effects() {
        let fixture = PurgeFixture::new();
        fixture.commit_unique_output();
        let report = fixture.run(false).await;
        assert_eq!(report.retained["unlanded_recent"], 1);
        assert!(fixture.root.exists());
        assert!(std::fs::metadata(fixture.root.join("tracked")).is_ok());
        assert_eq!(fixture.session_row().0, "Live");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn stale_unlanded_sandbox_is_preserved_then_purged() {
        let fixture = PurgeFixture::new();
        let tip = fixture.commit_unique_output();
        fixture.age_session(25);
        let report = fixture.run(false).await;
        assert_eq!(report.purged.preserved, 1);
        let reference = format!("refs/rsi/abandoned/{}", fixture.session_id);
        let observed = git(&fixture.bare_origin, &["show-ref", &reference]);
        assert!(observed.starts_with(&tip));
        assert!(!fixture.root.exists());
        assert_eq!(fixture.session_row().0, "Purged");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn unreachable_preservation_origin_retains_sandbox() {
        let fixture = PurgeFixture::new();
        fixture.commit_unique_output();
        fixture.age_session(25);
        git(&fixture.repository, &["remote", "remove", "origin"]);
        let report = fixture.run(false).await;
        assert_eq!(report.retained["preservation_failed"], 1);
        assert!(fixture.root.exists());
        let branch_refs = git(&fixture.repository, &["for-each-ref", &fixture.source_ref]);
        assert!(
            !branch_refs.is_empty(),
            "branch refs were removed: {branch_refs:?}"
        );
        assert_eq!(fixture.session_row().0, "Live");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn dirty_sandbox_is_retained() {
        let fixture = PurgeFixture::new();
        std::fs::write(fixture.root.join("untracked"), "user work\n").expect("write untracked");
        let report = fixture.run(false).await;
        assert_eq!(report.retained["worktree_dirty"], 1);
        assert!(fixture.root.join("untracked").exists());
        assert_eq!(fixture.session_row().0, "Live");
    }

    /// #1123: a purge candidate parked inside its holder proof must not hold
    /// the repository mutation lock, so a concurrent repository mutation (a
    /// sandbox allocation) is never delayed by a slow proof.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn parked_holder_proof_does_not_block_repository_mutation() {
        let fixture = PurgeFixture::new();
        let (parked_tx, parked_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let parked_tx = std::sync::Mutex::new(parked_tx);
        let release_rx = std::sync::Mutex::new(release_rx);
        proof_test_hook::install(&fixture.root, move |stage| {
            if stage != proof_test_hook::Stage::BeforeHolderProof {
                return;
            }
            parked_tx.lock().expect("parked sender").send(()).ok();
            let _ = release_rx
                .lock()
                .expect("release receiver")
                .recv_timeout(Duration::from_secs(20));
        });
        let origin = fixture.repository.clone();
        let observer = tokio::task::spawn_blocking(move || {
            parked_rx
                .recv_timeout(Duration::from_secs(20))
                .expect("purge reached its holder proof");
            let started = std::time::Instant::now();
            git_worktree::with_repository_mutation(&origin, || Ok::<(), DaemonError>(()))
                .expect("repository mutation");
            let waited = started.elapsed();
            release_tx.send(()).ok();
            waited
        });
        let (report, waited) = tokio::join!(fixture.run(false), observer);
        let waited = waited.expect("observer task");
        assert!(
            waited < Duration::from_secs(5),
            "repository mutation waited {waited:?} behind a parked holder proof"
        );
        assert_eq!(report.purged.no_output, 1);
        assert!(!fixture.root.exists());
    }

    /// #1123: the unlocked proof is re-validated under the lock; a tree that
    /// changed in between is retained for the next pass, not removed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn tree_changed_after_unlocked_proof_is_retained() {
        let fixture = PurgeFixture::new();
        let root = fixture.root.clone();
        proof_test_hook::install(&fixture.root, move |stage| {
            if stage != proof_test_hook::Stage::AfterProofs {
                return;
            }
            std::fs::create_dir_all(root.join("target")).expect("create ignored dir");
            std::fs::write(root.join("target").join("late"), "late writer\n")
                .expect("write late file");
        });
        let report = fixture.run(false).await;
        assert_eq!(report.retained["worktree_changed"], 1);
        assert!(fixture.root.join("target").join("late").exists());
        assert_eq!(fixture.session_row().0, "Live");
        assert_ne!(
            git(&fixture.repository, &["for-each-ref", &fixture.source_ref]),
            ""
        );
    }

    /// #1141 (#1123 F10): a holder acquired after the unlocked proof leaves the
    /// tree digest unchanged, so only the final holder re-proof under the lock
    /// can see it. The candidate is retained and nothing is removed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn holder_acquired_after_unlocked_proof_is_retained() {
        let fixture = PurgeFixture::new();
        let manager = fixture.manager();
        let holder = Arc::new(manager.install_archive_cleanup_test_holder_proc(fixture.session_id));
        let root = fixture.root.clone();
        let late_holder = Arc::clone(&holder);
        proof_test_hook::install(&fixture.root, move |stage| {
            if stage != proof_test_hook::Stage::AfterProofs {
                return;
            }
            // A task opens a file inside the tree: no tree content changes.
            late_holder.add_fd_holder(&root.join("tracked"));
        });
        let report = manager
            .run_archived_sandbox_purge(false, 32, None)
            .await
            .expect("purge pass");
        assert_eq!(report.retained["proof_failed"], 1, "{report:?}");
        assert!(fixture.root.join("tracked").exists());
        assert_eq!(fixture.session_row().0, "Live");
        assert_ne!(
            git(&fixture.repository, &["for-each-ref", &fixture.source_ref]),
            ""
        );
    }

    /// #1141: the locked holder re-proof is supervised. A proof stalled in the
    /// kernel (a hung mount) times out inside its budget, the candidate is
    /// retained, a competing repository mutation makes progress meanwhile,
    /// nothing is deleted, and a second pass while the helper is still blocked
    /// is refused at once instead of stranding another thread.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn stalled_locked_holder_reproof_times_out_without_holding_the_repository_lock() {
        let fixture = PurgeFixture::new();
        let manager = fixture.manager();
        let _holder = manager.install_archive_cleanup_test_holder_proc(fixture.session_id);
        let (parked_tx, parked_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let parked_tx = std::sync::Mutex::new(parked_tx);
        let release_rx = std::sync::Mutex::new(release_rx);
        reaper::locked_reproof_test_hook::install(&fixture.root, move || {
            parked_tx.lock().expect("parked sender").send(()).ok();
            let _ = release_rx
                .lock()
                .expect("release receiver")
                .recv_timeout(Duration::from_secs(30));
        });
        let origin = fixture.repository.clone();
        let observer = tokio::task::spawn_blocking(move || {
            parked_rx
                .recv_timeout(Duration::from_secs(20))
                .expect("the locked re-proof helper parked");
            let started = std::time::Instant::now();
            git_worktree::with_repository_mutation(&origin, || Ok::<(), DaemonError>(()))
                .expect("repository mutation");
            started.elapsed()
        });
        let started = std::time::Instant::now();
        let (report, waited) = tokio::join!(
            manager.run_archived_sandbox_purge(false, 32, None),
            observer
        );
        let report = report.expect("purge pass");
        let waited = waited.expect("observer task");
        assert_eq!(report.retained["proof_failed"], 1, "{report:?}");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the purge pass was not bounded by the re-proof budget"
        );
        assert!(
            waited <= reaper::QUARANTINE_HOLDER_LOCKED_REPROOF_BUDGET + Duration::from_secs(5),
            "repository mutation waited {waited:?} behind a stalled re-proof"
        );
        // Nothing was deleted or finalized.
        assert!(fixture.root.join("tracked").exists());
        assert_eq!(fixture.session_row().0, "Live");
        assert_ne!(
            git(&fixture.repository, &["for-each-ref", &fixture.source_ref]),
            ""
        );
        // The helper is still blocked: a second pass is refused immediately.
        let second_started = std::time::Instant::now();
        let second = manager
            .run_archived_sandbox_purge(false, 32, None)
            .await
            .expect("second purge pass");
        assert_eq!(second.retained["proof_failed"], 1, "{second:?}");
        assert!(second_started.elapsed() < reaper::QUARANTINE_HOLDER_LOCKED_REPROOF_BUDGET);
        assert!(fixture.root.join("tracked").exists());
        release_tx.send(()).ok();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn ignored_target_cache_is_disposable() {
        let fixture = PurgeFixture::new();
        std::fs::create_dir_all(fixture.root.join("target")).expect("create target");
        std::fs::write(fixture.root.join("target").join("artifact"), "bytes\n")
            .expect("write artifact");
        let report = fixture.run(false).await;
        assert_eq!(report.purged.no_output, 1);
        assert!(!fixture.root.exists());
        assert_eq!(fixture.session_row().0, "Purged");
    }

    /// A purge-eligible archived sandbox must be retained while the owner
    /// session still has a durable consumer (a live manager scope here), even
    /// when every other purge gate would let it go.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn pending_consumer_is_retained() {
        let fixture = PurgeFixture::new();
        std::fs::write(fixture.root.join("tracked"), "integrated\n").expect("write output");
        git(&fixture.root, &["add", "tracked"]);
        git(&fixture.root, &["commit", "-qm", "integrated output"]);
        git(
            &fixture.repository,
            &[
                "merge",
                "--ff-only",
                fixture.source_ref.trim_start_matches("refs/heads/"),
            ],
        );
        git(&fixture.repository, &["push", "-q", "origin", "rolling"]);
        let store = fixture.store.lock().expect("lock store");
        let project_id = Uuid::new_v4().to_string();
        let stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO projects (id,name,path,description,color,created_at,updated_at)
                 VALUES (?1,?2,NULL,NULL,NULL,?3,?3)",
                rusqlite::params![project_id, format!("pending-consumer-{project_id}"), stamp],
            )
            .expect("insert project");
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_scopes (project_id,manager_session_id,epic_ids_json,row_version,updated_at)
                 VALUES (?1,?2,'[]',1,?3)",
                rusqlite::params![project_id, fixture.session_id.to_string(), stamp],
            )
            .expect("insert manager scope");
        drop(store);
        let report = fixture.run(false).await;
        assert_eq!(report.retained["pending_consumer"], 1);
        assert_eq!(report.purged.integrated, 0);
        assert!(fixture.root.exists());
        assert!(fixture.root.join("tracked").exists());
        assert_eq!(fixture.session_row().0, "Live");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn live_epic_lead_is_not_a_candidate() {
        let fixture = PurgeFixture::new();
        let mut epic = make_test_session();
        epic.id = Uuid::new_v4();
        epic.project_id = None;
        epic.session_kind = SessionKind::Epic;
        epic.status = SessionStatus::Running;
        epic.lead_session_id = Some(fixture.session_id);
        epic.working_dir = fixture.repository.clone();
        fixture
            .store
            .lock()
            .expect("lock store")
            .insert_session(&epic)
            .expect("insert Epic");
        let report = fixture.run(false).await;
        assert_eq!(report.candidates_examined, 0);
        assert_eq!(report.retained.len(), 0);
        assert!(fixture.root.exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn non_archived_statuses_are_not_candidates() {
        for status in ["Completed", "Interrupted", "Running"] {
            let fixture = PurgeFixture::new();
            fixture
                .store
                .lock()
                .expect("lock store")
                .conn
                .execute(
                    "UPDATE sessions SET status=?2 WHERE id=?1",
                    rusqlite::params![fixture.session_id.to_string(), status],
                )
                .expect("update status");
            let report = fixture.run(false).await;
            assert_eq!(report.candidates_examined, 0, "{status}");
            assert!(fixture.root.exists(), "{status}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn removed_root_recovery_finalizes_on_next_pass() {
        let fixture = PurgeFixture::new();
        git(
            &fixture.repository,
            &[
                "worktree",
                "remove",
                fixture.root.display().to_string().as_str(),
            ],
        );
        let report = fixture.run(false).await;
        assert_eq!(report.purged.no_output + report.purged.integrated, 1);
        assert_eq!(fixture.session_row().0, "Purged");
        assert_eq!(
            git(&fixture.repository, &["for-each-ref", &fixture.source_ref]),
            ""
        );
    }

    /// Insert a historical terminal Session whose dependency row is Invalid
    /// (its execution projection was never authenticated) at `working_dir`.
    fn insert_invalid_historical_session(fixture: &PurgeFixture, working_dir: &Path) {
        let stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        fixture
            .store
            .lock()
            .expect("lock store")
            .conn
            .execute(
                "INSERT INTO sessions(id,query,working_dir,status,created_at,updated_at)
                 VALUES(?1,'historical invalid dependency row',?2,'Archived',?3,?3)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    working_dir.display().to_string(),
                    stamp
                ],
            )
            .expect("insert Invalid historical Session");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn unreachable_invalid_historical_session_does_not_block_purge() {
        // Issue #1086: a historical Invalid Session row elsewhere must not
        // refuse every candidate.
        let fixture = PurgeFixture::new();
        let elsewhere = fixture._temp.path().join("historical-elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("create unrelated directory");
        insert_invalid_historical_session(&fixture, &elsewhere);
        let report = fixture.run(false).await;
        assert_eq!(report.retained.get("session_projection_unhealthy"), None);
        assert_eq!(report.purged.no_output + report.purged.integrated, 1);
        assert!(!fixture.root.exists());
        assert_eq!(fixture.session_row().0, "Purged");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn unreachable_invalid_historical_session_does_not_block_absent_root_purge() {
        let fixture = PurgeFixture::new();
        git(
            &fixture.repository,
            &[
                "worktree",
                "remove",
                fixture.root.display().to_string().as_str(),
            ],
        );
        let elsewhere = fixture._temp.path().join("historical-elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("create unrelated directory");
        insert_invalid_historical_session(&fixture, &elsewhere);
        let report = fixture.run(false).await;
        assert_eq!(report.retained.get("session_projection_unhealthy"), None);
        assert_eq!(report.purged.no_output + report.purged.integrated, 1);
        assert_eq!(fixture.session_row().0, "Purged");
    }

    /// Archived Session with a verified ordinary projection whose working
    /// directory was then removed: a Relevant row with a stale projection.
    fn insert_stale_relevant_historical_session(fixture: &PurgeFixture, working_dir: &Path) {
        std::fs::create_dir_all(working_dir).expect("create stale Session directory");
        let canonical = std::fs::canonicalize(working_dir).expect("canonicalize stale directory");
        let id = Uuid::new_v4().to_string();
        let stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let store = fixture.store.lock().expect("lock store");
        store
            .conn
            .execute(
                "INSERT INTO sessions(id,query,working_dir,status,created_at,updated_at)
                 VALUES(?1,'historical stale relevant row',?2,'Archived',?3,?3)",
                rusqlite::params![id, working_dir.display().to_string(), stamp],
            )
            .expect("insert stale Relevant Session");
        store
            .conn
            .execute(
                "UPDATE session_execution_projections
                    SET execution_state='ordinary_unsandboxed',freshness='verified',
                        canonical_repo_dir=?1,effective_cwd=?1,custody_id=NULL,
                        custody_generation=NULL,validated_at=?2,error_code=NULL,updated_at=?2
                  WHERE session_id=?3",
                rusqlite::params![canonical.display().to_string(), stamp, id],
            )
            .expect("verify stale Relevant projection");
        drop(store);
        std::fs::remove_dir_all(working_dir).expect("remove stale Session directory");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn unreachable_stale_relevant_historical_session_does_not_block_purge() {
        // Issue #1086 acceptance 3: archived Sessions whose cached projection
        // went stale refused every candidate on the hub.
        let fixture = PurgeFixture::new();
        let elsewhere = fixture._temp.path().join("stale-elsewhere");
        insert_stale_relevant_historical_session(&fixture, &elsewhere);
        let report = fixture.run(false).await;
        assert_eq!(report.retained.get("session_projection_unhealthy"), None);
        assert_eq!(report.purged.no_output + report.purged.integrated, 1);
        assert!(!fixture.root.exists());
        assert_eq!(fixture.session_row().0, "Purged");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn reachable_stale_relevant_historical_session_retains_purge_candidate() {
        let fixture = PurgeFixture::new();
        insert_stale_relevant_historical_session(&fixture, &fixture.root.join("nested-stale"));
        let report = fixture.run(false).await;
        assert_eq!(report.retained["session_projection_unhealthy"], 1);
        assert_eq!(report.purged.no_output + report.purged.integrated, 0);
        assert!(fixture.root.exists());
        assert_eq!(fixture.session_row().0, "Live");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn reachable_invalid_historical_session_retains_purge_candidate() {
        let fixture = PurgeFixture::new();
        let alias = fixture._temp.path().join("alias-into-candidate");
        std::os::unix::fs::symlink(&fixture.root, &alias).expect("create inward alias");
        insert_invalid_historical_session(&fixture, &alias);
        let report = fixture.run(false).await;
        assert_eq!(report.retained["session_projection_unhealthy"], 1);
        assert_eq!(report.purged.no_output + report.purged.integrated, 0);
        assert!(fixture.root.exists());
        assert_eq!(fixture.session_row().0, "Live");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn dry_run_reports_bytes_without_effects() {
        let fixture = PurgeFixture::new();
        std::fs::create_dir_all(fixture.root.join("target")).expect("create target");
        std::fs::write(fixture.root.join("target").join("artifact"), "bytes\n")
            .expect("write artifact");
        let report = fixture.run(true).await;
        assert!(report.dry_run);
        assert_eq!(report.purged.no_output, 1);
        assert!(report.bytes_freed > 0);
        assert!(fixture.root.exists());
        let branch_refs = git(&fixture.repository, &["for-each-ref", &fixture.source_ref]);
        assert!(
            !branch_refs.is_empty(),
            "branch refs were removed: {branch_refs:?}"
        );
        assert_eq!(fixture.session_row().0, "Live");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[tokio::test]
    async fn retained_head_does_not_starve_later_candidates() {
        let fixture = PurgeFixture::new();
        std::fs::write(fixture.root.join("untracked"), "dirty\n").expect("write dirty file");
        fixture.set_session_updated_at(fixture.session_id, 3);
        let first_clean = fixture.add_candidate(2);
        let second_clean = fixture.add_candidate(1);
        let manager = fixture.manager();
        let _holders = [
            manager.install_archive_cleanup_test_holder_proc(fixture.session_id),
            manager.install_archive_cleanup_test_holder_proc(first_clean.session_id),
            manager.install_archive_cleanup_test_holder_proc(second_clean.session_id),
        ];

        let mut cursor = None;
        let first_report = manager
            .run_archived_sandbox_purge(false, 1, cursor.clone())
            .await
            .expect("first pass");
        assert_eq!(first_report.candidates_examined, 1);
        assert_eq!(first_report.retained.get("worktree_dirty"), Some(&1));
        assert!(first_report.next_cursor.is_some());
        cursor = first_report.next_cursor.clone();

        let second_report = manager
            .run_archived_sandbox_purge(false, 1, cursor.clone())
            .await
            .expect("second pass");
        assert_eq!(second_report.purged.no_output, 1);
        assert!(second_report.next_cursor.is_some());
        cursor = second_report.next_cursor.clone();

        let third_report = manager
            .run_archived_sandbox_purge(false, 1, cursor.clone())
            .await
            .expect("third pass");
        assert_eq!(third_report.purged.no_output, 1);
        assert!(third_report.next_cursor.is_some());

        assert!(fixture.root.exists());
        assert_eq!(fixture.session_row().0, "Live");
        assert!(!first_clean.root.exists());
        assert!(!second_clean.root.exists());
        let purged_count: i64 = fixture
            .store
            .lock()
            .expect("lock store")
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE id IN (?1,?2) AND sandbox_cleanup_state='Purged'",
                rusqlite::params![
                    first_clean.session_id.to_string(),
                    second_clean.session_id.to_string()
                ],
                |row| row.get(0),
            )
            .expect("read purged candidates");
        assert_eq!(purged_count, 2);
    }
}
