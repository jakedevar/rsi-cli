#![allow(clippy::unwrap_used)]

use super::*;
use crate::sandbox::SandboxAllocator;
use crate::store::Store;
use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
use rsi_common::types::{SandboxCleanupState, SandboxKind, SessionStatus};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;
use uuid::Uuid;

struct Fixture {
    _tmp: TempDir,
    repo: PathBuf,
    base: PathBuf,
    session_id: Uuid,
    custody_id: Uuid,
    branch: String,
    root: PathBuf,
    store: Store,
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--initial-branch=main"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("README"), "fixture\n").unwrap();
    git(&repo, &["add", "README"]);
    git(&repo, &["commit", "-m", "initial"]);
    let source = git(&repo, &["rev-parse", "HEAD"]);
    let base = tmp.path().join("sandboxes");
    std::fs::create_dir_all(&base).unwrap();
    let session_id = Uuid::new_v4();
    let custody_id = Uuid::new_v4();
    let alloc = SandboxAllocator::new(base.clone())
        .allocate(
            session_id,
            &repo,
            SandboxKind::GitWorktree,
            &source,
            Some(&format!("rsi/reclaim-{session_id}")),
        )
        .unwrap();
    let mut session = crate::store::tests::make_test_session();
    session.id = session_id;
    session.working_dir = repo.clone();
    session.status = SessionStatus::Completed;
    session.sandbox_kind = Some(SandboxKind::GitWorktree);
    session.sandbox_root = Some(alloc.root.clone());
    session.sandbox_branch = alloc.branch.clone();
    let branch = alloc.branch.clone().unwrap();
    session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
    let common = git(&repo, &["rev-parse", "--git-common-dir"]);
    let repository_identity = if Path::new(&common).is_absolute() {
        PathBuf::from(common)
    } else {
        repo.join(common)
    };
    let binding = SessionCustodyBinding::New(NewCustodyRoot {
        custody_id,
        canonical_repo_dir: repo.canonicalize().unwrap().to_string_lossy().into(),
        sandbox_root: alloc.root.to_string_lossy().into(),
        sandbox_branch: branch.clone(),
        repository_identity: repository_identity
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into(),
        source_commit: source,
        cause: CustodyCause::FreshLaunch,
    });
    let mut store = Store::open_in_memory().unwrap();
    store
        .insert_session_with_custody(&session, binding)
        .unwrap();
    Fixture {
        _tmp: tmp,
        repo,
        base,
        session_id,
        custody_id,
        branch,
        root: alloc.root,
        store,
    }
}

fn remove_external(f: &Fixture) {
    std::fs::remove_dir_all(&f.root).unwrap();
    git(&f.repo, &["worktree", "prune", "--expire", "now"]);
}

fn run(f: &mut Fixture, dry_run: bool) -> Uuid {
    run_absent_root_adoption(&mut f.store, &f.base, "startup", dry_run, 16, None).unwrap()
}

fn state(f: &Fixture) -> String {
    f.store
        .conn
        .query_row(
            "SELECT state FROM sandbox_custody_roots WHERE custody_id=?1",
            [f.custody_id.to_string()],
            |r| r.get(0),
        )
        .unwrap()
}

fn item_effect(f: &Fixture, run_id: Uuid) -> (u64, String, i64, String) {
    f.store
        .conn
        .query_row(
            "SELECT generation,class,effectful,phase
               FROM sandbox_reclaim_items WHERE run_id=?1 AND custody_id=?2",
            rusqlite::params![run_id.to_string(), f.custody_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn absent_live_terminal_owner_is_adopted_and_linked_session_is_purged() {
    let mut f = fixture();
    let expected_branch_oid = git(&f.repo, &["rev-parse", &format!("refs/heads/{}", f.branch)]);
    remove_external(&f);
    let run = run(&mut f, false);
    assert_eq!(
        state(&f),
        "purged",
        "gate={}",
        f.store
            .conn
            .query_row("SELECT reason_code FROM sandbox_reclaim_items", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
    );
    let projection:(String,Option<String>,Option<String>,String)=f.store.conn.query_row("SELECT sandbox_cleanup_state,sandbox_root,sandbox_branch,status FROM sessions WHERE id=?1",[f.session_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(
        projection,
        ("Purged".into(), None, None, "Completed".into())
    );
    let event:(String,String,String)=f.store.conn.query_row("SELECT event_kind,cause,next_state FROM sandbox_custody_events WHERE custody_id=?1 ORDER BY sequence DESC LIMIT 1",[f.custody_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(
        event,
        ("tombstoned".into(), "purge".into(), "purged".into())
    );
    let item: (String, String, String) = f
        .store
        .conn
        .query_row(
            "SELECT class,reason_code,branch_oid FROM sandbox_reclaim_items WHERE run_id=?1",
            [run.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(item.0, "absent_adopted");
    assert_eq!(item.1, "external_removal");
    assert_eq!(item.2, expected_branch_oid);
}

fn insert_invalid_historical_session(f: &Fixture, working_dir: &Path) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    f.store
        .conn
        .execute(
            "INSERT INTO sessions(id,query,working_dir,status,created_at,updated_at)
             VALUES(?1,'historical invalid dependency row',?2,'Archived',?3,?3)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                working_dir.to_string_lossy().into_owned(),
                now
            ],
        )
        .unwrap();
}

/// Issue #1086: a historical Invalid Session row that cannot reach the absent
/// root must not block adoption; one that can reach it still does.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn unreachable_invalid_historical_session_does_not_block_absent_root_adoption() {
    let mut f = fixture();
    let elsewhere = f._tmp.path().join("historical-elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    insert_invalid_historical_session(&f, &elsewhere);
    remove_external(&f);
    run(&mut f, false);
    assert_eq!(state(&f), "purged");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn reachable_invalid_historical_session_blocks_absent_root_adoption() {
    let mut f = fixture();
    // The alias dangles once the root is gone but could be restored into it.
    let alias = f._tmp.path().join("alias-into-root");
    remove_external(&f);
    std::os::unix::fs::symlink(&f.root, &alias).unwrap();
    insert_invalid_historical_session(&f, &alias.join("work"));
    let run_id = run(&mut f, false);
    assert_eq!(state(&f), "live");
    let reason: String = f
        .store
        .conn
        .query_row(
            "SELECT reason_code FROM sandbox_reclaim_items WHERE run_id=?1",
            [run_id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "session_projection_unhealthy");
}

/// Archived Session with a verified ordinary projection whose directory was
/// then removed (hub class 1|Archived|NULL with a stale projection), plus a
/// Failed-cleanup Session whose sandbox was removed elsewhere (class
/// 2|Archived|Failed). Neither can reach the candidate root.
fn insert_stale_historical_bystanders(f: &Fixture) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let stale_dir = f._tmp.path().join("stale-elsewhere");
    std::fs::create_dir_all(&stale_dir).unwrap();
    let canonical = std::fs::canonicalize(&stale_dir).unwrap();
    let stale = Uuid::new_v4().to_string();
    f.store
        .conn
        .execute(
            "INSERT INTO sessions(id,query,working_dir,status,created_at,updated_at)
             VALUES(?1,'historical stale relevant row',?2,'Archived',?3,?3)",
            rusqlite::params![stale, stale_dir.to_string_lossy().into_owned(), now],
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE session_execution_projections
                SET execution_state='ordinary_unsandboxed',freshness='verified',
                    canonical_repo_dir=?1,effective_cwd=?1,custody_id=NULL,
                    custody_generation=NULL,validated_at=?2,error_code=NULL,updated_at=?2
              WHERE session_id=?3",
            rusqlite::params![canonical.to_string_lossy().into_owned(), now, stale],
        )
        .unwrap();
    std::fs::remove_dir_all(&stale_dir).unwrap();
    let failed = Uuid::new_v4().to_string();
    f.store
        .conn
        .execute(
            "INSERT INTO sessions(id,query,working_dir,status,sandbox_kind,sandbox_cleanup_state,
                                  sandbox_root,sandbox_branch,created_at,updated_at)
             VALUES(?1,'historical failed cleanup row',?2,'Archived','GitWorktree','Failed',?3,
                    'rsi/removed-elsewhere',?4,?4)",
            rusqlite::params![
                failed,
                f.repo.to_string_lossy().into_owned(),
                f._tmp
                    .path()
                    .join("sandboxes")
                    .join("removed")
                    .to_string_lossy()
                    .into_owned(),
                now
            ],
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn stale_historical_bystanders_do_not_block_absent_root_adoption() {
    let mut f = fixture();
    insert_stale_historical_bystanders(&f);
    remove_external(&f);
    run(&mut f, false);
    assert_eq!(state(&f), "purged");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn stale_historical_bystanders_do_not_block_quarantined_root_missing_adoption() {
    // Issue #1086 acceptance 3: a quarantined root_missing root (Session
    // cleanup Failed) moves to the terminal tombstone even though unrelated
    // historical rows hold stale projections.
    let mut f = fixture();
    insert_stale_historical_bystanders(&f);
    remove_external(&f);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let tx = f.store.conn.transaction().unwrap();
    tx.execute("INSERT INTO sandbox_custody_events(event_id,custody_id,sequence,event_kind,cause,from_generation,to_generation,from_owner_session_id,to_owner_session_id,prior_state,next_state,error_code,occurred_at) VALUES(?1,?2,2,'quarantined','startup_reconciliation',1,2,?3,NULL,'live','quarantined','root_missing',?4)",rusqlite::params![Uuid::new_v4().to_string(),f.custody_id.to_string(),f.session_id.to_string(),now]).unwrap();
    tx.execute("UPDATE sandbox_custody_roots SET state='quarantined',owner_session_id=NULL,generation=2,event_sequence=2,validation_state='invalid',validated_generation=2,validated_at=?2,validation_error_code='root_missing',updated_at=?2 WHERE custody_id=?1",rusqlite::params![f.custody_id.to_string(),now]).unwrap();
    tx.execute("UPDATE sessions SET status='Archived',sandbox_cleanup_state='Failed',stop_reason='sandbox_custody:root_missing',updated_at=?2 WHERE id=?1",rusqlite::params![f.session_id.to_string(),now]).unwrap();
    tx.execute("UPDATE session_execution_projections SET execution_state='quarantined',freshness='invalid',effective_cwd=NULL,custody_generation=2,validated_at=?2,error_code='root_missing',updated_at=?2 WHERE session_id=?1",rusqlite::params![f.session_id.to_string(),now]).unwrap();
    tx.commit().unwrap();
    run(&mut f, false);
    assert_eq!(state(&f), "purged");
    let (clean, root, branch): (String, Option<String>, Option<String>) = f
        .store
        .conn
        .query_row(
            "SELECT sandbox_cleanup_state,sandbox_root,sandbox_branch FROM sessions WHERE id=?1",
            [f.session_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((clean, root, branch), ("Purged".into(), None, None));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn absent_quarantined_root_missing_is_adopted() {
    let mut f = fixture();
    remove_external(&f);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let tx = f.store.conn.transaction().unwrap();
    tx.execute("INSERT INTO sandbox_custody_events(event_id,custody_id,sequence,event_kind,cause,from_generation,to_generation,from_owner_session_id,to_owner_session_id,prior_state,next_state,error_code,occurred_at) VALUES(?1,?2,2,'quarantined','startup_reconciliation',1,2,?3,NULL,'live','quarantined','root_missing',?4)",rusqlite::params![Uuid::new_v4().to_string(),f.custody_id.to_string(),f.session_id.to_string(),now]).unwrap();
    tx.execute("UPDATE sandbox_custody_roots SET state='quarantined',owner_session_id=NULL,generation=2,event_sequence=2,validation_state='invalid',validated_generation=2,validated_at=?2,validation_error_code='root_missing',updated_at=?2 WHERE custody_id=?1",rusqlite::params![f.custody_id.to_string(),now]).unwrap();
    tx.execute("UPDATE sessions SET status='Failed',sandbox_cleanup_state='Failed',stop_reason='sandbox_custody:root_missing',updated_at=?2 WHERE id=?1",rusqlite::params![f.session_id.to_string(),now]).unwrap();
    tx.execute("UPDATE session_execution_projections SET execution_state='quarantined',freshness='invalid',effective_cwd=NULL,custody_generation=2,validated_at=?2,error_code='root_missing',updated_at=?2 WHERE session_id=?1",rusqlite::params![f.session_id.to_string(),now]).unwrap();
    tx.commit().unwrap();
    run(&mut f, false);
    assert_eq!(
        state(&f),
        "purged",
        "gate={}",
        f.store
            .conn
            .query_row("SELECT reason_code FROM sandbox_reclaim_items", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
    );
    let (validation,generation,error):(String,i64,String)=f.store.conn.query_row("SELECT validation_state,validated_generation,validation_error_code FROM sandbox_custody_roots WHERE custody_id=?1",[f.custody_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(
        (validation, generation, error),
        ("invalid".into(), 3, "root_missing".into())
    );
    let (clean,root,branch,status):(String,Option<String>,Option<String>,String)=f.store.conn.query_row("SELECT sandbox_cleanup_state,sandbox_root,sandbox_branch,status FROM sessions WHERE id=?1",[f.session_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(
        (clean, root, branch, status),
        ("Purged".into(), None, None, "Failed".into())
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn retry_eligible_owner_is_retained_with_gate_code() {
    let mut f = fixture();
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Failed',retry_attempt=1,max_retries=3 WHERE id=?1",
            [f.session_id.to_string()],
        )
        .unwrap();
    remove_external(&f);
    let run = run(&mut f, false);
    assert_eq!(state(&f), "live");
    let item: (String, String) = f
        .store
        .conn
        .query_row(
            "SELECT class,reason_code FROM sandbox_reclaim_items WHERE run_id=?1",
            [run.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(item, ("retained".into(), "retry_eligible".into()));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn live_lead_owner_is_retained_with_gate_code() {
    let mut f = fixture();
    let mut child = crate::store::tests::make_test_session();
    child.id = Uuid::new_v4();
    child.status = SessionStatus::Completed;
    child.working_dir = f.repo.clone();
    f.store.insert_session(&child).unwrap();
    f.store
        .conn
        .execute(
            "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
            rusqlite::params![child.id.to_string(), f.session_id.to_string()],
        )
        .unwrap();
    remove_external(&f);
    let run = run(&mut f, false);
    assert_eq!(state(&f), "live");
    let reason: String = f
        .store
        .conn
        .query_row(
            "SELECT reason_code FROM sandbox_reclaim_items WHERE run_id=?1",
            [run.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "live_lead");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn dry_run_journals_without_custody_change() {
    let mut f = fixture();
    remove_external(&f);
    let run = run(&mut f, true);
    assert_eq!(state(&f), "live");
    let (dry,phase):(i64,String)=f.store.conn.query_row("SELECT r.dry_run,i.phase FROM sandbox_reclaim_runs r JOIN sandbox_reclaim_items i USING(run_id) WHERE r.run_id=?1",[run.to_string()],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(
        (dry, phase),
        (1, "settled".into()),
        "gate={}",
        f.store
            .conn
            .query_row("SELECT reason_code FROM sandbox_reclaim_items", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn present_directory_is_retained() {
    let mut f = fixture();
    let run = run(&mut f, false);
    assert_eq!(state(&f), "live");
    let reason: String = f
        .store
        .conn
        .query_row(
            "SELECT reason_code FROM sandbox_reclaim_items WHERE run_id=?1",
            [run.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "directory_present");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn retained_directory_is_adopted_at_same_generation_after_external_removal() {
    let mut f = fixture();
    let retained_run = run(&mut f, false);
    assert_eq!(state(&f), "live");
    assert_eq!(
        item_effect(&f, retained_run),
        (1, "retained".into(), 0, "retained".into())
    );

    remove_external(&f);
    let adopted_run = run(&mut f, false);
    assert_eq!(state(&f), "purged");
    assert_eq!(
        item_effect(&f, adopted_run),
        (1, "absent_adopted".into(), 1, "settled".into())
    );
    let rows: i64 = f
        .store
        .conn
        .query_row(
            "SELECT count(*) FROM sandbox_reclaim_items WHERE custody_id=?1 AND generation=1",
            [f.custody_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 2);
    assert_eq!(item_effect(&f, retained_run).2, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn retry_eligible_retention_can_be_adopted_at_same_generation_after_gate_clears() {
    let mut f = fixture();
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Failed',retry_attempt=1,max_retries=3 WHERE id=?1",
            [f.session_id.to_string()],
        )
        .unwrap();
    remove_external(&f);
    let retained_run = run(&mut f, false);
    assert_eq!(state(&f), "live");
    assert_eq!(
        item_effect(&f, retained_run),
        (1, "retained".into(), 0, "retained".into())
    );

    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Completed',retry_attempt=NULL,max_retries=NULL WHERE id=?1",
            [f.session_id.to_string()],
        )
        .unwrap();
    let adopted_run = run(&mut f, false);
    assert_eq!(state(&f), "purged");
    assert_eq!(
        item_effect(&f, adopted_run),
        (1, "absent_adopted".into(), 1, "settled".into())
    );
    assert_eq!(item_effect(&f, retained_run).2, 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn dry_run_then_real_run_adopts_at_same_generation() {
    let mut f = fixture();
    remove_external(&f);
    let dry_run = run(&mut f, true);
    assert_eq!(state(&f), "live");
    assert_eq!(
        item_effect(&f, dry_run),
        (1, "absent_adopted".into(), 0, "settled".into())
    );

    let effectful_run = run(&mut f, false);
    assert_eq!(state(&f), "purged");
    assert_eq!(
        item_effect(&f, effectful_run),
        (1, "absent_adopted".into(), 1, "settled".into())
    );
    assert_eq!(item_effect(&f, dry_run).2, 0);
}

fn only_reason(f: &Fixture, run_id: Uuid) -> String {
    f.store
        .conn
        .query_row(
            "SELECT reason_code FROM sandbox_reclaim_items WHERE run_id=?1",
            [run_id.to_string()],
            |r| r.get(0),
        )
        .unwrap()
}

fn delete_branch(f: &Fixture) {
    git(&f.repo, &["branch", "-D", &f.branch]);
}

fn candidate(f: &Fixture) -> AbsentRootCandidate {
    let mut candidates = f.store.absent_root_candidates(16).unwrap();
    assert_eq!(candidates.len(), 1);
    candidates.remove(0)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn missing_branch_is_retained_without_custody_change() {
    let mut f = fixture();
    remove_external(&f);
    delete_branch(&f);
    let run_id = run(&mut f, false);
    assert_eq!(only_reason(&f, run_id), "branch_missing");
    assert_eq!(state(&f), "live");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn registered_worktree_with_absent_directory_is_retained() {
    let mut f = fixture();
    std::fs::remove_dir_all(&f.root).unwrap();
    let run_id = run(&mut f, false);
    assert_eq!(only_reason(&f, run_id), "worktree_registered");
    assert_eq!(state(&f), "live");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn run_reads_repository_evidence_once_and_reuses_it() {
    let f = fixture();
    remove_external(&f);
    delete_branch(&f);
    let base = std::fs::canonicalize(&f.base).unwrap();
    let c = candidate(&f);
    let mut evidence = RepoEvidenceCache::default();
    assert_eq!(
        prove_absent_root(&base, &c, &mut evidence),
        Err("branch_missing")
    );
    // The branch reappears mid-run; the run keeps its one snapshot and still
    // retains, which has no custody effect. A later run sees the branch.
    git(&f.repo, &["branch", &f.branch, "main"]);
    assert_eq!(
        prove_absent_root(&base, &c, &mut evidence),
        Err("branch_missing")
    );
    assert_eq!(
        evidence.repos.len(),
        1,
        "one evidence read for one repository"
    );
    let mut next_run = RepoEvidenceCache::default();
    let oid = prove_absent_root(&base, &c, &mut next_run).unwrap();
    assert_eq!(oid, git(&f.repo, &["rev-parse", "main"]));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn stale_snapshot_never_proves_an_adoption() {
    let f = fixture();
    remove_external(&f);
    let base = std::fs::canonicalize(&f.base).unwrap();
    let c = candidate(&f);
    let repo = PathBuf::from(&c.canonical_repo_dir);
    let mut evidence = RepoEvidenceCache::default();
    // Snapshot taken while the branch exists; the branch is then deleted.
    let _ = evidence.repo(&repo);
    delete_branch(&f);
    assert_eq!(
        prove_absent_root(&base, &c, &mut evidence),
        Err("branch_missing")
    );

    // Snapshot taken before registration disappears still retains.
    let g = fixture();
    std::fs::remove_dir_all(&g.root).unwrap();
    let c = candidate(&g);
    let repo = PathBuf::from(&c.canonical_repo_dir);
    let base = std::fs::canonicalize(&g.base).unwrap();
    let mut evidence = RepoEvidenceCache::default();
    let _ = evidence.repo(&repo);
    git(&g.repo, &["worktree", "prune", "--expire", "now"]);
    assert_eq!(
        prove_absent_root(&base, &c, &mut evidence),
        Err("worktree_registered")
    );
}

/// Pre-#961 proof: every candidate re-read the listing and branch freshly.
fn prove_absent_root_always_fresh(
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
    if PathBuf::from(&candidate.sandbox_root) != path {
        return Err("sandbox_path_mismatch");
    }
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return Err("directory_present"),
        Err(_) => return Err("directory_probe_failed"),
    }
    prove_absent_root_fresh(
        Path::new(&candidate.canonical_repo_dir),
        &path,
        &branch_ref(candidate),
    )
}

/// Equivalence and timing against a copy of a real database (never the live
/// file): `RSI_RECLAIM_BENCH_DB=/tmp/rsi-copy.db
/// RSI_RECLAIM_BENCH_BASE=~/.rsi/sandboxes cargo test -p rsid --lib
/// bench_absent_root_proof_against_db_copy -- --ignored --nocapture`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
#[ignore = "needs RSI_RECLAIM_BENCH_DB and RSI_RECLAIM_BENCH_BASE"]
fn bench_absent_root_proof_against_db_copy() {
    let db = std::env::var("RSI_RECLAIM_BENCH_DB").unwrap();
    let base = std::env::var("RSI_RECLAIM_BENCH_BASE").unwrap();
    let base = std::fs::canonicalize(base).unwrap();
    let mut store = Store::open(Path::new(&db)).unwrap();
    let candidates = store.absent_root_candidates(1024).unwrap();
    let gated: Vec<_> = candidates
        .into_iter()
        .filter(|c| store.absent_root_gate(c).unwrap().is_none())
        .collect();

    let started = std::time::Instant::now();
    let fresh: Vec<_> = gated
        .iter()
        .map(|c| prove_absent_root_always_fresh(&base, c))
        .collect();
    let fresh_ms = started.elapsed().as_millis();

    let started = std::time::Instant::now();
    let mut evidence = RepoEvidenceCache::default();
    let snapshot: Vec<_> = gated
        .iter()
        .map(|c| prove_absent_root(&base, c, &mut evidence))
        .collect();
    let snapshot_ms = started.elapsed().as_millis();

    let mut reasons = std::collections::BTreeMap::<String, usize>::new();
    for (old, new) in fresh.iter().zip(&snapshot) {
        assert_eq!(old, new, "snapshot changed a proof outcome");
        *reasons
            .entry(match new {
                Ok(_) => "adoptable".into(),
                Err(code) => (*code).into(),
            })
            .or_default() += 1;
    }

    let started = std::time::Instant::now();
    run_absent_root_adoption(&mut store, &base, "startup", true, 1024, None).unwrap();
    let run_ms = started.elapsed().as_millis();
    eprintln!(
        "gated={} fresh_ms={fresh_ms} snapshot_ms={snapshot_ms} repos={} full_dry_run_ms={run_ms} reasons={reasons:?}",
        gated.len(),
        evidence.repos.len()
    );
}
