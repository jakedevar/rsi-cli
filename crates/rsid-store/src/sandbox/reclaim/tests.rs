#![allow(clippy::unwrap_used)]

use super::*;
use crate::sandbox::SandboxAllocator;
use crate::store::Store;
use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
use rsi_common::harness_manager::{HarnessManagerConfigV1, HarnessManagerScopeModeV1};
use rsi_common::harness_manager_v2::{
    ManagerActionStateV2, ManagerActionV2, ManagerLaunchChoiceV2, ManagerLeadFenceV2,
};
use rsi_common::manager_operator_delegation::{OperatorCallFenceV1, OperatorCallV1};
use rsi_common::types::{
    SandboxCleanupState, SandboxKind, SessionKind, SessionProvider, SessionStatus,
};
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

/// The externally-removed shape on the hub: an Archived owner whose root
/// custody was quarantined `root_missing` and whose cleanup state is Failed.
fn quarantine_root_missing_archived(f: &mut Fixture) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let tx = f.store.conn.transaction().unwrap();
    tx.execute("INSERT INTO sandbox_custody_events(event_id,custody_id,sequence,event_kind,cause,from_generation,to_generation,from_owner_session_id,to_owner_session_id,prior_state,next_state,error_code,occurred_at) VALUES(?1,?2,2,'quarantined','startup_reconciliation',1,2,?3,NULL,'live','quarantined','root_missing',?4)",rusqlite::params![Uuid::new_v4().to_string(),f.custody_id.to_string(),f.session_id.to_string(),now]).unwrap();
    tx.execute("UPDATE sandbox_custody_roots SET state='quarantined',owner_session_id=NULL,generation=2,event_sequence=2,validation_state='invalid',validated_generation=2,validated_at=?2,validation_error_code='root_missing',updated_at=?2 WHERE custody_id=?1",rusqlite::params![f.custody_id.to_string(),now]).unwrap();
    tx.execute("UPDATE sessions SET status='Archived',sandbox_cleanup_state='Failed',stop_reason='sandbox_custody:root_missing',updated_at=?2 WHERE id=?1",rusqlite::params![f.session_id.to_string(),now]).unwrap();
    tx.execute("UPDATE session_execution_projections SET execution_state='quarantined',freshness='invalid',effective_cwd=NULL,custody_generation=2,validated_at=?2,error_code='root_missing',updated_at=?2 WHERE session_id=?1",rusqlite::params![f.session_id.to_string(),now]).unwrap();
    tx.commit().unwrap();
}

/// Insert a row of a manager or successor table that the pending-consumer
/// probe reads, without building its whole lifecycle: foreign keys and CHECK
/// constraints are switched off for the one statement.
fn insert_consumer_row(f: &Fixture, sql: &str, params: &[&dyn rusqlite::ToSql]) {
    f.store
        .conn
        .execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON;")
        .unwrap();
    f.store.conn.execute(sql, params).unwrap();
    f.store
        .conn
        .execute_batch("PRAGMA ignore_check_constraints=OFF; PRAGMA foreign_keys=ON;")
        .unwrap();
}

fn insert_successor_reservation(f: &Fixture, state: &str) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let digest = format!("sha256:{}", "c".repeat(64));
    insert_consumer_row(
        f,
        "INSERT INTO agent_successor_reservations
           (reservation_id,predecessor_session_id,epic_id,candidate_session_id,caller_key_digest,
            request_json,request_fingerprint,candidate_kind,inherited_launch_json,
            expected_lead_session_id,expected_lead_generation,state,state_version,reserved_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,'{}',?5,'Task','{}',?2,1,?6,1,?7,?7)",
        &[
            &Uuid::new_v4().to_string(),
            &f.session_id.to_string(),
            &Uuid::new_v4().to_string(),
            &Uuid::new_v4().to_string(),
            &digest,
            &state,
            &now,
        ],
    );
}

fn insert_unarchived_work_fact(f: &Fixture) {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let payload = format!(r#"{{"source_session_id":"{}"}}"#, f.session_id);
    insert_consumer_row(
        f,
        "INSERT INTO harness_manager_v2_work_facts
           (project_id,kind,record_key,epic_id,work_key,row_version,payload_json,archived,
            manager_session_id,scope_version,policy_version,created_at,updated_at)
         VALUES(?1,'work','historical-delivery',?2,'historical-delivery',1,?3,0,?4,1,1,?5,?5)",
        &[
            &Uuid::new_v4().to_string(),
            &Uuid::new_v4().to_string(),
            &payload,
            &Uuid::new_v4().to_string(),
            &now,
        ],
    );
}

fn insert_queued_manager_operation(f: &Fixture) {
    insert_manager_operation(f, "queued", Some(f.session_id), &serde_json::json!({}));
}

fn insert_manager_operation(
    f: &Fixture,
    state: &str,
    target: Option<Uuid>,
    payload: &serde_json::Value,
) -> Uuid {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let id = Uuid::new_v4();
    insert_consumer_row(
        f,
        "INSERT INTO harness_manager_v2_operations
           (id,project_id,manager_session_id,scope_version,policy_version,idempotency_key,
            fingerprint,kind,payload_json,state,target_session_id,outcome_json,not_before,created_at,updated_at)
         VALUES(?1,?2,?3,1,1,'resume-lead','fingerprint','lifecycle_action',?4,?5,?6,'{}',?7,?7,?7)",
        &[
            &id.to_string(),
            &Uuid::new_v4().to_string(),
            &Uuid::new_v4().to_string(),
            &payload.to_string(),
            &state,
            &target.map(|id| id.to_string()),
            &now,
        ],
    );
    id
}

fn assert_manager_consumer(f: &Fixture, expected: bool) {
    assert_eq!(
        f.store
            .session_has_pending_consumer(&f.session_id.to_string(), f.custody_id)
            .unwrap(),
        expected,
        "probe for present root"
    );
    assert_eq!(
        f.store
            .session_has_pending_consumer_for_absent_root(&f.session_id.to_string(), f.custody_id)
            .unwrap(),
        expected,
        "probe for absent root"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn settled_successor_and_unarchived_work_fact_do_not_block_absent_root_adoption() {
    // Issue #1086 acceptance 3: on the hub 232 quarantined root_missing roots
    // were retained as `pending_consumer` only because a manager work fact
    // named the long-archived owner or a `committed` successor reservation
    // recorded its settled lineage. With nothing on disk to preserve, neither
    // pins the custody.
    let mut f = fixture();
    quarantine_root_missing_archived(&mut f);
    remove_external(&f);
    insert_successor_reservation(&f, "committed");
    insert_unarchived_work_fact(&f);
    let present = f
        .store
        .session_has_pending_consumer(&f.session_id.to_string(), f.custody_id)
        .unwrap();
    let absent = f
        .store
        .session_has_pending_consumer_for_absent_root(&f.session_id.to_string(), f.custody_id)
        .unwrap();
    assert!(
        present,
        "a root still on disk keeps both records as consumers"
    );
    assert!(!absent);
    let run_id = run(&mut f, false);
    assert_eq!(state(&f), "purged", "gate={}", only_reason(&f, run_id));
    assert_eq!(
        item_effect(&f, run_id),
        (2, "absent_adopted".into(), 1, "settled".into())
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn unsettled_successor_reservation_still_blocks_absent_root_adoption() {
    for state_name in ["reserved", "launching", "uncertain"] {
        let mut f = fixture();
        quarantine_root_missing_archived(&mut f);
        remove_external(&f);
        insert_successor_reservation(&f, state_name);
        let run_id = run(&mut f, false);
        assert_eq!(
            only_reason(&f, run_id),
            "pending_consumer",
            "reservation state {state_name}"
        );
        assert_eq!(state(&f), "quarantined", "reservation state {state_name}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn malformed_manager_operation_still_blocks_absent_root_adoption() {
    // A missing typed action/target witness stays a consumer even when the
    // root is already gone and the named session is Archived.
    let mut f = fixture();
    quarantine_root_missing_archived(&mut f);
    remove_external(&f);
    insert_queued_manager_operation(&f);
    let run_id = run(&mut f, false);
    assert_eq!(only_reason(&f, run_id), "pending_consumer");
    assert_eq!(state(&f), "quarantined");
}

#[derive(Clone, Copy, Debug)]
enum OpKind {
    Resume,
    Pause,
    Retire,
    ArchiveCall,
    UncertainCreate,
}

const INERT_KINDS: [OpKind; 5] = [
    OpKind::Resume,
    OpKind::Pause,
    OpKind::Retire,
    OpKind::ArchiveCall,
    OpKind::UncertainCreate,
];

fn lead_fence(lead: Uuid) -> ManagerLeadFenceV2 {
    ManagerLeadFenceV2 {
        lead_session_id: Some(lead),
        lead_generation: 1,
        event_sequence: 0,
        custody_generation: Some(2),
    }
}

fn operator_call(method: &str, session: Uuid) -> ManagerActionV2 {
    ManagerActionV2::OperatorCall {
        call: OperatorCallV1 {
            method: method.into(),
            params: serde_json::json!({"session_id": session}),
        },
        expected: Some(OperatorCallFenceV1 {
            session_updated_at: chrono::Utc::now(),
        }),
    }
}

fn create_session() -> ManagerActionV2 {
    ManagerActionV2::CreateSession {
        parent_id: Uuid::new_v4(),
        kind: SessionKind::Task,
        query: "Review the exact source".into(),
        launch: ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: "claude-sonnet-4-6".into(),
            effort: None,
        },
        sandbox_source: None,
    }
}

/// The action the daemon journals for `kind` against `lead`, and the state it
/// is left in (a launch that lost its executor stays `uncertain`).
fn operation_for(
    kind: OpKind,
    lead: Uuid,
    op_state: ManagerActionStateV2,
) -> (ManagerActionV2, ManagerActionStateV2) {
    let epic_id = Uuid::new_v4();
    match kind {
        OpKind::Resume => (
            ManagerActionV2::ResumeLead {
                epic_id,
                expected: lead_fence(lead),
                message: "Continue the assigned work".into(),
            },
            op_state,
        ),
        OpKind::Pause => (
            ManagerActionV2::PauseLead {
                epic_id,
                expected: lead_fence(lead),
                reason: "Stop generic auto-resume".into(),
            },
            op_state,
        ),
        OpKind::Retire => (
            ManagerActionV2::RetireLeadContinuations {
                epic_id,
                expected: lead_fence(lead),
            },
            op_state,
        ),
        OpKind::ArchiveCall => (operator_call("ArchiveSession", lead), op_state),
        OpKind::UncertainCreate => (create_session(), ManagerActionStateV2::Uncertain),
    }
}

fn manager_config() -> HarnessManagerConfigV1 {
    HarnessManagerConfigV1 {
        project_id: Uuid::new_v4(),
        manager_session_id: Uuid::new_v4(),
        current_session_id: None,
        epic_ids: Vec::new(),
        scope_mode: HarnessManagerScopeModeV1::default(),
        selected_epic_ids: None,
        group_ids: Vec::new(),
        row_version: 1,
        updated_at: chrono::Utc::now(),
    }
}

/// Journal `operation` exactly as `enqueue_manager_action` does: the operation
/// row holds `{origin, request}` and the `lifecycle_context` record holds the
/// target. Only foreign keys are off, because the fixture has no project or
/// manager session rows.
fn seed_operation(
    f: &Fixture,
    operation: ManagerActionV2,
    state: ManagerActionStateV2,
    target: Option<Uuid>,
    caller: Uuid,
) -> Uuid {
    f.store
        .conn
        .execute_batch("PRAGMA foreign_keys=OFF;")
        .unwrap();
    let id = f
        .store
        .seed_manager_action_with_context_for_test(
            &manager_config(),
            operation,
            state,
            target,
            caller,
        )
        .unwrap();
    f.store
        .conn
        .execute_batch("PRAGMA foreign_keys=ON;")
        .unwrap();
    id
}

fn seed_kind(
    f: &Fixture,
    kind: OpKind,
    op_state: ManagerActionStateV2,
    lead: Uuid,
    caller: Uuid,
) -> Uuid {
    let (operation, state) = operation_for(kind, lead, op_state);
    seed_operation(f, operation, state, Some(lead), caller)
}

/// Retire the operations seeded so far so the next case is probed alone. Rows
/// are never deleted: the notice queue retains reconciliation evidence. A
/// succeeded operation is no longer pending, which is how the daemon leaves it.
fn clear_manager_rows(f: &Fixture) {
    f.store
        .conn
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
             UPDATE harness_manager_v2_operations SET state='succeeded'
              WHERE state IN ('queued','running','uncertain');
             PRAGMA foreign_keys=ON;",
        )
        .unwrap();
}

/// Every pending-operation row and its context record, for retention checks.
fn manager_rows(f: &Fixture) -> Vec<(String, String)> {
    let mut stmt = f
        .store
        .conn
        .prepare(
            "SELECT 'operation', id||'|'||state||'|'||payload_json||'|'||row_version||'|'||COALESCE(outcome_json,'')
               FROM harness_manager_v2_operations
             UNION ALL
             SELECT 'context', record_key||'|'||row_version||'|'||payload_json
               FROM harness_manager_v2_records WHERE kind='lifecycle_context'
             ORDER BY 1,2",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn set_status(f: &Fixture, id: Uuid, status: &str) {
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status=?2 WHERE id=?1",
            rusqlite::params![id.to_string(), status],
        )
        .unwrap();
}

fn insert_other_session(f: &Fixture, status: &str) -> Uuid {
    let mut other = crate::store::tests::make_test_session();
    other.id = Uuid::new_v4();
    f.store.insert_session(&other).unwrap();
    set_status(f, other.id, status);
    other.id
}

const OP_STATES: [(&str, ManagerActionStateV2); 2] = [
    ("queued", ManagerActionStateV2::Queued),
    ("uncertain", ManagerActionStateV2::Uncertain),
];

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn stale_operations_on_archived_targets_allow_adoption_and_retain_their_evidence() {
    // Hub shape (2026-10-13): quarantined roots pinned only by stale
    // resume_lead, pause_lead, retire_lead_continuations, ArchiveSession
    // operator_call and uncertain create_session rows whose target is Archived.
    // Each row is journalled by the production writer, target in its context.
    for kind in INERT_KINDS {
        for (name, op_state) in OP_STATES {
            if matches!(kind, OpKind::UncertainCreate) && name == "queued" {
                continue;
            }
            let statuses: &[&str] = if matches!(kind, OpKind::Resume) {
                &["Archived", "Deleted"]
            } else {
                &["Archived"]
            };
            for status in statuses {
                let mut f = fixture();
                quarantine_root_missing_archived(&mut f);
                set_status(&f, f.session_id, status);
                remove_external(&f);
                seed_kind(&f, kind, op_state, f.session_id, Uuid::new_v4());
                let before = manager_rows(&f);
                assert_eq!(before.len(), 2, "operation and context for {kind:?}");
                assert_manager_consumer(&f, false);
                let run_id = run(&mut f, false);
                assert_eq!(
                    state(&f),
                    "purged",
                    "{kind:?} {name} {status} gate={}",
                    only_reason(&f, run_id)
                );
                assert_eq!(
                    item_effect(&f, run_id),
                    (2, "absent_adopted".into(), 1, "settled".into())
                );
                assert_eq!(manager_rows(&f), before, "{kind:?} evidence is retained");
                let session = f.store.get_session(f.session_id).unwrap().unwrap();
                assert_eq!(
                    session.sandbox_cleanup_state,
                    Some(SandboxCleanupState::Purged)
                );
                assert_eq!(session.sandbox_root, None);
                assert_eq!(session.sandbox_branch, None);
            }
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn archived_target_releases_the_calling_session_only_when_it_is_settled() {
    // 15 of the 16 pinned hub roots belong to the agent that issued the stale
    // operation, not to its target. The caller is attribution only.
    let f = fixture();
    let archived = insert_other_session(&f, "Archived");
    let completed = insert_other_session(&f, "Completed");
    for kind in INERT_KINDS {
        for (name, op_state) in OP_STATES {
            if matches!(kind, OpKind::UncertainCreate) && name == "queued" {
                continue;
            }
            clear_manager_rows(&f);
            seed_kind(&f, kind, op_state, archived, f.session_id);
            assert_manager_consumer(&f, false);
            clear_manager_rows(&f);
            seed_kind(&f, kind, op_state, completed, f.session_id);
            assert_manager_consumer(&f, true);
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn live_and_resumable_targets_keep_every_operation_kind_pinned() {
    let f = fixture();
    for status in [
        "Starting",
        "Running",
        "WaitingApproval",
        "Completed",
        "Failed",
        "Interrupted",
    ] {
        set_status(&f, f.session_id, status);
        for kind in INERT_KINDS {
            for (_, op_state) in OP_STATES {
                clear_manager_rows(&f);
                seed_kind(&f, kind, op_state, f.session_id, Uuid::new_v4());
                assert_manager_consumer(&f, true);
            }
        }
    }
    // A target whose session row is absent cannot be proven settled.
    let missing = Uuid::new_v4();
    for kind in INERT_KINDS {
        clear_manager_rows(&f);
        seed_kind(
            &f,
            kind,
            ManagerActionStateV2::Uncertain,
            missing,
            Uuid::new_v4(),
        );
        assert!(
            f.store
                .session_has_pending_consumer(&missing.to_string(), f.custody_id)
                .unwrap(),
            "{kind:?}"
        );
        assert!(
            f.store
                .session_has_pending_consumer_for_absent_root(&missing.to_string(), f.custody_id)
                .unwrap(),
            "{kind:?}"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn archived_targets_keep_running_or_unsettled_operations_pinned() {
    for blocker in ["pending_archive", "live", "adopted"] {
        let mut f = fixture();
        quarantine_root_missing_archived(&mut f);
        remove_external(&f);
        if blocker == "pending_archive" {
            f.store
                .conn
                .execute(
                    "UPDATE sessions SET pending_archive=1 WHERE id=?1",
                    [f.session_id.to_string()],
                )
                .unwrap();
        } else {
            let turn = crate::store::provider_turn_custody::NewProviderTurnCustody {
                invocation_id: Uuid::new_v4(),
                session_id: f.session_id,
                spool_dir: f._tmp.path().join("spool"),
                worker_scope: None,
                pid: 1,
                start_time: None,
                boot_id: Uuid::new_v4(),
            };
            let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            f.store.conn.execute(
                "INSERT INTO model_invocations
                 (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at)
                 VALUES(?1,'session.launch','session','foreground','paid','admitted','running','launch_session',?2,?3)",
                rusqlite::params![turn.invocation_id.to_string(), f.session_id.to_string(), now],
            ).unwrap();
            f.store.insert_provider_turn_custody(&turn).unwrap();
            f.store
                .conn
                .execute("UPDATE provider_turn_custody SET state=?1", [blocker])
                .unwrap();
        }
        for kind in INERT_KINDS {
            clear_manager_rows(&f);
            seed_kind(
                &f,
                kind,
                ManagerActionStateV2::Uncertain,
                f.session_id,
                Uuid::new_v4(),
            );
            assert_manager_consumer(&f, true);
        }
    }
    // A running operation may still act, whatever its target's status.
    let mut f = fixture();
    quarantine_root_missing_archived(&mut f);
    for kind in INERT_KINDS {
        clear_manager_rows(&f);
        let (operation, _) = operation_for(kind, f.session_id, ManagerActionStateV2::Running);
        seed_operation(
            &f,
            operation,
            ManagerActionStateV2::Running,
            Some(f.session_id),
            Uuid::new_v4(),
        );
        assert_manager_consumer(&f, true);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn archived_target_operations_require_consistent_durable_target_evidence() {
    let mut f = fixture();
    quarantine_root_missing_archived(&mut f);
    let me = f.session_id;
    let other = Uuid::new_v4();
    let archived_other = insert_other_session(&f, "Archived");
    let resume =
        |lead: Uuid| operation_for(OpKind::Resume, lead, ManagerActionStateV2::Uncertain).0;
    for case in [
        "valid_control",
        "no_target",
        "other_target",
        "other_expected",
        "other_context_target",
        "other_receipt_target",
        "missing_context_record",
        "malformed_context_record",
        "other_action",
        "unarchive_call",
        "queued_create",
        "drain_pending_create",
        "live_mention",
        "second_unreadable_operation",
        "payload_only_target",
        "seeded_without_context",
    ] {
        clear_manager_rows(&f);
        let mut expected_pinned = true;
        match case {
            "valid_control" => {
                seed_operation(
                    &f,
                    resume(me),
                    ManagerActionStateV2::Uncertain,
                    Some(me),
                    Uuid::new_v4(),
                );
                expected_pinned = false;
            }
            "no_target" => {
                seed_operation(
                    &f,
                    resume(me),
                    ManagerActionStateV2::Uncertain,
                    None,
                    Uuid::new_v4(),
                );
            }
            "other_target" => {
                seed_operation(
                    &f,
                    resume(me),
                    ManagerActionStateV2::Uncertain,
                    Some(archived_other),
                    Uuid::new_v4(),
                );
            }
            "other_expected" => {
                seed_operation(
                    &f,
                    resume(other),
                    ManagerActionStateV2::Uncertain,
                    Some(me),
                    Uuid::new_v4(),
                );
            }
            "other_context_target"
            | "other_receipt_target"
            | "missing_context_record"
            | "malformed_context_record" => {
                seed_operation(
                    &f,
                    resume(me),
                    ManagerActionStateV2::Uncertain,
                    Some(me),
                    Uuid::new_v4(),
                );
                let sql = match case {
                    "other_context_target" => {
                        "UPDATE harness_manager_v2_records SET payload_json=json_set(payload_json,'$.target_session_id',?1) WHERE kind='lifecycle_context'"
                    }
                    "other_receipt_target" => {
                        "UPDATE harness_manager_v2_operations SET outcome_json=json_set(outcome_json,'$.target_session_id',?1)"
                    }
                    "missing_context_record" => {
                        "UPDATE harness_manager_v2_records SET record_key='detached:'||record_key WHERE kind='lifecycle_context' AND ?1<>''"
                    }
                    _ => {
                        "UPDATE harness_manager_v2_records SET payload_json='{}' WHERE kind='lifecycle_context' AND ?1<>''"
                    }
                };
                f.store
                    .conn
                    .execute(sql, [archived_other.to_string()])
                    .unwrap();
            }
            "other_action" => {
                seed_operation(
                    &f,
                    ManagerActionV2::RetryLead {
                        epic_id: Uuid::new_v4(),
                        expected: lead_fence(me),
                        message: "Retry".into(),
                        launch: None,
                    },
                    ManagerActionStateV2::Uncertain,
                    Some(me),
                    Uuid::new_v4(),
                );
            }
            "unarchive_call" => {
                seed_operation(
                    &f,
                    operator_call("UnarchiveSession", me),
                    ManagerActionStateV2::Queued,
                    Some(me),
                    Uuid::new_v4(),
                );
            }
            "queued_create" => {
                seed_operation(
                    &f,
                    create_session(),
                    ManagerActionStateV2::Queued,
                    Some(me),
                    Uuid::new_v4(),
                );
            }
            "drain_pending_create" => {
                seed_operation(
                    &f,
                    create_session(),
                    ManagerActionStateV2::Uncertain,
                    Some(me),
                    Uuid::new_v4(),
                );
                f.store
                    .conn
                    .execute(
                        "UPDATE harness_manager_v2_operations SET outcome_json=json_set(outcome_json,'$.outcome',?1)",
                        [crate::store::manager_actions::MANAGER_LAUNCH_DRAIN_PENDING],
                    )
                    .unwrap();
            }
            "live_mention" => {
                // A live lead whose message merely names the Archived session.
                let live = insert_other_session(&f, "Running");
                let (mut operation, state) =
                    operation_for(OpKind::Resume, live, ManagerActionStateV2::Uncertain);
                if let ManagerActionV2::ResumeLead { message, .. } = &mut operation {
                    *message = format!("Previous lead {me}");
                }
                seed_operation(&f, operation, state, Some(live), Uuid::new_v4());
            }
            "second_unreadable_operation" => {
                seed_operation(
                    &f,
                    resume(me),
                    ManagerActionStateV2::Uncertain,
                    Some(me),
                    Uuid::new_v4(),
                );
                insert_queued_manager_operation(&f);
            }
            "payload_only_target" => {
                // The shape #2171 assumed and the daemon never writes: the
                // target inside the operation payload, no context record.
                let mut payload = serde_json::json!({
                    "origin": {"origin": "agent", "caller": Uuid::new_v4()},
                    "request": {
                        "fence": {"scope_version": 1, "policy_version": 1},
                        "idempotency_key": "resume-lead",
                        "operation": {
                            "action": "resume_lead", "epic_id": Uuid::new_v4(),
                            "expected": {"lead_session_id": me, "lead_generation": 1,
                                "event_sequence": 0, "custody_generation": 2},
                            "message": "Continue the assigned work"
                        }
                    }
                });
                payload["target_session_id"] = serde_json::json!(me);
                insert_manager_operation(&f, "uncertain", Some(me), &payload);
            }
            "seeded_without_context" => {
                f.store
                    .conn
                    .execute_batch("PRAGMA foreign_keys=OFF;")
                    .unwrap();
                f.store
                    .seed_manager_action_for_test(
                        &manager_config(),
                        resume(me),
                        ManagerActionStateV2::Uncertain,
                        Some(me),
                    )
                    .unwrap();
                f.store
                    .conn
                    .execute_batch("PRAGMA foreign_keys=ON;")
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert_manager_consumer(&f, expected_pinned);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn archived_target_operation_probe_fails_closed_on_query_error() {
    let mut f = fixture();
    quarantine_root_missing_archived(&mut f);
    seed_kind(
        &f,
        OpKind::Resume,
        ManagerActionStateV2::Uncertain,
        f.session_id,
        Uuid::new_v4(),
    );
    f.store
        .conn
        .execute_batch(
            "ALTER TABLE provider_turn_custody RENAME TO unavailable_provider_turn_custody",
        )
        .unwrap();
    assert_manager_consumer(&f, true);
}

fn json_keys(raw: &str) -> Vec<String> {
    let mut keys: Vec<String> = serde_json::from_str::<serde_json::Value>(raw)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn queue_written_rows_keep_the_target_in_the_context_record_and_release_an_archived_lead() {
    use crate::store::manager_actions::ManagerActionOriginV2;
    use rsi_common::harness_manager_v2::{
        AgentManagerControlRequestV2, ManagerCapabilityV2, ManagerFenceV2, ManagerOperatingModeV2,
        ManagerPolicyV2,
    };

    let store = Store::open_in_memory().unwrap();
    let (config, lead) = crate::store::manager_coordinator::tests::fixture(
        &store,
        ManagerPolicyV2 {
            mode: ManagerOperatingModeV2::Execute,
            capabilities: vec![ManagerCapabilityV2::LeadControl],
            ..Default::default()
        },
    );
    let policy_version = store
        .get_harness_manager_policy(config.project_id)
        .unwrap()
        .unwrap()
        .row_version;
    let epic = lead.parent_id.unwrap();
    let fence = ManagerFenceV2 {
        scope_version: config.row_version,
        policy_version,
    };
    // The real queue path: admission journals the row and its context record.
    let queued = store
        .enqueue_manager_action(
            ManagerActionOriginV2::Agent {
                caller: config.manager_session_id,
            },
            AgentManagerControlRequestV2::new(
                fence,
                "stale-pause".into(),
                ManagerActionV2::PauseLead {
                    epic_id: epic,
                    expected: store.manager_action_lead_fence(epic).unwrap(),
                    reason: "stop generic auto-resume".into(),
                },
            ),
        )
        .unwrap();
    // The fixture writes exactly what admission writes.
    let seeded = store
        .seed_manager_action_with_context_for_test(
            &config,
            ManagerActionV2::PauseLead {
                epic_id: epic,
                expected: store.manager_action_lead_fence(epic).unwrap(),
                reason: "stop generic auto-resume".into(),
            },
            ManagerActionStateV2::Queued,
            Some(lead.id),
            config.manager_session_id,
        )
        .unwrap();
    let payload = |id: Uuid| -> String {
        store
            .conn
            .query_row(
                "SELECT payload_json FROM harness_manager_v2_operations WHERE id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .unwrap()
    };
    let context = |id: Uuid| -> String {
        store
            .conn
            .query_row(
                "SELECT payload_json FROM harness_manager_v2_records
                  WHERE kind='lifecycle_context' AND record_key=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(
        json_keys(&payload(queued.operation_id)),
        ["origin", "request"]
    );
    assert_eq!(
        json_keys(&payload(queued.operation_id)),
        json_keys(&payload(seeded))
    );
    assert_eq!(
        json_keys(&context(queued.operation_id)),
        json_keys(&context(seeded))
    );
    let target: serde_json::Value = serde_json::from_str(&context(queued.operation_id)).unwrap();
    assert_eq!(target["target_session_id"], serde_json::json!(lead.id));

    let probe = || {
        store
            .session_has_pending_consumer_for_absent_root(&lead.id.to_string(), Uuid::new_v4())
            .unwrap()
    };
    assert!(probe(), "a Completed lead is resumable");
    store
        .conn
        .execute(
            "UPDATE sessions SET status='Archived' WHERE id=?1",
            [lead.id.to_string()],
        )
        .unwrap();
    assert!(!probe(), "an Archived lead releases its custody");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn settled_successor_does_not_adopt_a_live_owner_whose_root_is_present() {
    // The owner-successor gate and the pending-consumer probe only relax for a
    // root proven absent: a live owner with its directory present stays
    // retained (`directory_present`) and its custody is untouched.
    let mut f = fixture();
    insert_successor_reservation(&f, "committed");
    insert_unarchived_work_fact(&f);
    assert!(
        f.store
            .session_has_pending_consumer(&f.session_id.to_string(), f.custody_id)
            .unwrap()
    );
    let run_id = run(&mut f, false);
    assert_eq!(only_reason(&f, run_id), "directory_present");
    assert_eq!(state(&f), "live");
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

/// Unrelated Failed sessions whose verified ordinary projections make them
/// restart retry candidates: corpus the retry probe must not pay for.
fn insert_retry_eligible_bystanders_2354(store: &mut Store, count: usize) {
    for _ in 0..count {
        let mut other = crate::store::tests::make_test_session();
        other.id = Uuid::new_v4();
        store.insert_session(&other).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Failed' WHERE id=?1",
                [other.id.to_string()],
            )
            .unwrap();
        store.publish_startup_ordinary(other.id).unwrap();
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn retry_probe_matches_the_corpus_scan_and_ignores_unrelated_sessions_2354() {
    let mut f = fixture();
    // A Completed owner is no retry candidate by either route.
    assert!(
        !f.store
            .custody_has_retry_eligible_participant(f.custody_id)
            .unwrap()
    );
    assert!(
        !f.store
            .startup_retry_eligible_sessions()
            .unwrap()
            .contains(&f.session_id)
    );
    // A Failed owner with verified live custody is one by both routes.
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Failed',retry_attempt=1,max_retries=3 WHERE id=?1",
            [f.session_id.to_string()],
        )
        .unwrap();
    assert!(
        f.store
            .custody_has_retry_eligible_participant(f.custody_id)
            .unwrap()
    );
    assert!(
        f.store
            .startup_retry_eligible_sessions()
            .unwrap()
            .contains(&f.session_id)
    );
    // Another custody has no participant at all.
    assert!(
        !f.store
            .custody_has_retry_eligible_participant(Uuid::new_v4())
            .unwrap()
    );

    // The probe's work does not grow with unrelated Failed sessions; the
    // corpus scan's does, which is what the startup gate used to pay per
    // candidate.
    let custody_id = f.custody_id;
    let (_, clean) = crate::sandbox::startup_restore_tests::vm_ops_during(&mut f.store, |store| {
        store
            .custody_has_retry_eligible_participant(custody_id)
            .unwrap()
    });
    insert_retry_eligible_bystanders_2354(&mut f.store, 120);
    let (_, noisy) = crate::sandbox::startup_restore_tests::vm_ops_during(&mut f.store, |store| {
        store
            .custody_has_retry_eligible_participant(custody_id)
            .unwrap()
    });
    let (_, corpus) = crate::sandbox::startup_restore_tests::vm_ops_during(&mut f.store, |store| {
        store.startup_retry_eligible_sessions().unwrap().len()
    });
    assert!(
        noisy <= clean + 64,
        "the probe touched unrelated sessions: {clean} ops clean, {noisy} with 120 bystanders"
    );
    assert!(
        corpus > noisy * 5,
        "the measurement sees the corpus scan: probe {noisy} ops, scan {corpus}"
    );
}

/// Present-directory roots for owners that finished, the dominant shape on the
/// hub (288 of 310 candidates): the gate runs in full and the proof retains.
fn seed_present_roots_2354(store: &mut Store, base: &Path, repo: &Path, count: usize) {
    for index in 0..count {
        let mut owner = crate::store::tests::make_test_session();
        owner.status = SessionStatus::Completed;
        owner.working_dir = repo.to_path_buf();
        let root = base.join(owner.id.to_string());
        std::fs::create_dir_all(&root).unwrap();
        let branch = format!("rsi/reclaim-2354-{index}");
        owner.sandbox_kind = Some(SandboxKind::GitWorktree);
        owner.sandbox_root = Some(root.clone());
        owner.sandbox_branch = Some(branch.clone());
        owner.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store
            .insert_session_with_custody(
                &owner,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id: Uuid::new_v4(),
                    canonical_repo_dir: repo.to_string_lossy().into(),
                    sandbox_root: root.to_string_lossy().into(),
                    sandbox_branch: branch,
                    repository_identity: "reclaim-2354-identity".into(),
                    source_commit: "0".repeat(40),
                    cause: CustodyCause::FreshLaunch,
                }),
            )
            .unwrap();
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn adoption_run_batches_retained_rows_and_does_not_scan_the_corpus_per_candidate_2354() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("sandboxes");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    let base = std::fs::canonicalize(base).unwrap();
    let repo = std::fs::canonicalize(repo).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    seed_present_roots_2354(&mut store, &base, &repo, 24);

    let commits = Arc::new(AtomicU64::new(0));
    let handle = Arc::clone(&commits);
    store.conn.commit_hook(Some(move || {
        handle.fetch_add(1, Ordering::Relaxed);
        false
    }));
    let (run_id, clean) =
        crate::sandbox::startup_restore_tests::vm_ops_during(&mut store, |store| {
            run_absent_root_adoption(store, &base, "startup", false, 1024, None).unwrap()
        });
    store.conn.commit_hook(None::<fn() -> bool>);

    let retained: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM sandbox_reclaim_items
              WHERE run_id=?1 AND phase='retained' AND reason_code='directory_present'",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, 24, "every candidate keeps its audit row");
    // Opening the run, one batch of retained rows and closing the run; the
    // restore used to commit once per candidate on top of those two.
    assert!(
        commits.load(Ordering::Relaxed) <= 5,
        "retained rows are committed in batches, not one per candidate: {} commits",
        commits.load(Ordering::Relaxed)
    );

    // Unrelated retry-eligible sessions add nothing to a candidate's gate.
    insert_retry_eligible_bystanders_2354(&mut store, 150);
    let (_, noisy) = crate::sandbox::startup_restore_tests::vm_ops_during(&mut store, |store| {
        run_absent_root_adoption(store, &base, "startup", false, 1024, None).unwrap()
    });
    assert!(
        noisy <= clean + clean / 5,
        "the gate scaled with unrelated sessions: {clean} ops clean, {noisy} with 150 bystanders"
    );
}

fn journal_rows(f: &Fixture, run_id: Uuid) -> Vec<(String, Option<String>)> {
    let mut stmt = f
        .store
        .conn
        .prepare("SELECT class,reason_code FROM sandbox_reclaim_items WHERE run_id=?1")
        .unwrap();
    stmt.query_map([run_id.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
}

fn run_counts(f: &Fixture, run_id: Uuid) -> (i64, i64) {
    f.store
        .conn
        .query_row(
            "SELECT adopted_count,retained_count FROM sandbox_reclaim_runs WHERE run_id=?1",
            [run_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

/// #2364: a restart over unchanged candidates adds no retained rows, a changed
/// verdict adds one, adoption is always journalled, and the run's counts describe
/// its decisions rather than the rows it wrote.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn unchanged_retained_candidate_is_not_rejournalled_2364() {
    let mut f = fixture();
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Failed',retry_attempt=1,max_retries=3 WHERE id=?1",
            [f.session_id.to_string()],
        )
        .unwrap();
    remove_external(&f);

    let first = run(&mut f, false);
    assert_eq!(
        journal_rows(&f, first),
        vec![("retained".into(), Some("retry_eligible".into()))]
    );
    assert_eq!(run_counts(&f, first), (0, 1));

    // Unchanged candidate: no row, but the counts still say what was decided.
    let second = run(&mut f, false);
    assert_eq!(journal_rows(&f, second), vec![]);
    assert_eq!(run_counts(&f, second), (0, 1));
    let third = run(&mut f, false);
    assert_eq!(journal_rows(&f, third), vec![]);
    assert_eq!(run_counts(&f, third), (0, 1));

    // An operator request asks for the full report and keeps one row per candidate.
    let operator =
        run_absent_root_adoption(&mut f.store, &f.base, "operator", false, 16, None).unwrap();
    assert_eq!(
        journal_rows(&f, operator),
        vec![("retained".into(), Some("retry_eligible".into()))]
    );

    // A changed verdict adds exactly one row.
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Completed',retry_attempt=0 WHERE id=?1",
            [f.session_id.to_string()],
        )
        .unwrap();
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
    let changed = run(&mut f, false);
    assert_eq!(
        journal_rows(&f, changed),
        vec![("retained".into(), Some("live_lead".into()))]
    );
    assert_eq!(run_counts(&f, changed), (0, 1));
    let after_change = run(&mut f, false);
    assert_eq!(journal_rows(&f, after_change), vec![]);

    // Adoption is always journalled, after any number of elided retentions.
    f.store
        .conn
        .execute(
            "UPDATE sessions SET lead_session_id=NULL WHERE id=?1",
            [child.id.to_string()],
        )
        .unwrap();
    let adopted = run(&mut f, false);
    assert_eq!(state(&f), "purged");
    assert_eq!(
        journal_rows(&f, adopted),
        vec![("absent_adopted".into(), Some("external_removal".into()))]
    );
    assert_eq!(run_counts(&f, adopted), (1, 0));
}
