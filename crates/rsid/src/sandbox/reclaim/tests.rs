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
