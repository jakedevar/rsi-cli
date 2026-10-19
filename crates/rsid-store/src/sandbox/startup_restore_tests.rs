//! Startup custody restore: a settled store is read, never rewritten, and the
//! work grows linearly with the group count (#2354). Kept out of
//! `store/tests.rs` on purpose: a helper added there reads as a change to the
//! released-migration slow tier's own source and would run it in full.

#![allow(clippy::unwrap_used)]

use super::custody::CustodyService;
use crate::store::Store;
use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
use crate::store::tests::make_test_session;
use rsi_common::types::{SandboxCleanupState, SandboxKind, SessionStatus};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Everything startup reconciliation writes, minus timestamps, plus the order
/// in which custody events were written.
fn startup_custody_dump_2354(store: &Store) -> Vec<String> {
    let mut dump = Vec::new();
    for sql in [
        "SELECT 'root|'||custody_id||'|'||state||'|'||COALESCE(owner_session_id,'')||'|'||generation||'|'||
                event_sequence||'|'||validation_state||'|'||COALESCE(validated_generation,'')||'|'||
                COALESCE(validation_error_code,'')
           FROM sandbox_custody_roots ORDER BY custody_id",
        "SELECT 'event|'||custody_id||'|'||sequence||'|'||event_kind||'|'||cause||'|'||
                COALESCE(error_code,'')||'|'||COALESCE(from_generation,'')||'|'||to_generation
           FROM sandbox_custody_events ORDER BY rowid",
        "SELECT 'projection|'||session_id||'|'||execution_state||'|'||freshness||'|'||
                COALESCE(effective_cwd,'')||'|'||COALESCE(custody_id,'')||'|'||
                COALESCE(custody_generation,'')||'|'||COALESCE(error_code,'')
           FROM session_execution_projections ORDER BY session_id",
        "SELECT 'session|'||id||'|'||status||'|'||COALESCE(stop_reason,'')
           FROM sessions ORDER BY id",
    ] {
        let mut statement = store.conn.prepare(sql).unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        dump.extend(rows);
    }
    dump
}

/// SQLite virtual-machine instructions executed on `store`'s connection while
/// `work` runs. For fixed statements and data this is deterministic, so it
/// measures how much a pass does (and how that grows with the data) without a
/// wall-clock assertion (#2354).
pub(crate) fn vm_ops_during<R>(store: &mut Store, work: impl FnOnce(&mut Store) -> R) -> (R, u64) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let counter = Arc::new(AtomicU64::new(0));
    let handle = Arc::clone(&counter);
    store.conn.progress_handler(
        1,
        Some(move || {
            handle.fetch_add(1, Ordering::Relaxed);
            false
        }),
    );
    let result = work(store);
    store.conn.progress_handler(1, None::<fn() -> bool>);
    (result, counter.load(Ordering::Relaxed))
}

/// The directories a git-free startup corpus needs: a sandbox base and the
/// repository directory its sessions name. Neither is a git repository: the
/// shapes below never reach a git probe.
fn startup_dirs_2354(parent: &Path) -> (PathBuf, PathBuf) {
    let base = parent.join("sandboxes");
    let repo = parent.join("repo");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    (
        std::fs::canonicalize(base).unwrap(),
        std::fs::canonicalize(repo).unwrap(),
    )
}

/// `groups` startup custody groups cycling through four shapes that need no
/// git: an ordinary session, a rootless Purged tombstone, a rootless cleanup
/// failure, and a live-owner root whose directory is gone (it quarantines as
/// `root_missing` and takes the terminal path on later boots). Returns each
/// session with the projection state it settles to.
fn seed_startup_corpus_2354(
    store: &mut Store,
    sandbox_base: &Path,
    repo: &Path,
    groups: usize,
) -> Vec<(Uuid, &'static str)> {
    let mut seeded = Vec::new();
    for index in 0..groups {
        let mut session = make_test_session();
        session.status = SessionStatus::Completed;
        session.working_dir = repo.to_path_buf();
        match index % 4 {
            0 => {
                store.insert_session(&session).unwrap();
                seeded.push((session.id, "ordinary_unsandboxed"));
            }
            1 => {
                session.sandbox_kind = Some(SandboxKind::GitWorktree);
                session.sandbox_cleanup_state = Some(SandboxCleanupState::Purged);
                store.insert_session(&session).unwrap();
                seeded.push((session.id, "historical_purged"));
            }
            2 => {
                session.sandbox_kind = Some(SandboxKind::GitWorktree);
                session.sandbox_cleanup_state = Some(SandboxCleanupState::Failed);
                store.insert_session(&session).unwrap();
                seeded.push((session.id, "historical_cleanup_failed"));
            }
            _ => {
                let root = sandbox_base.join(session.id.to_string());
                let branch = format!("rsi/startup-2354-{index}");
                session.sandbox_kind = Some(SandboxKind::GitWorktree);
                session.sandbox_root = Some(root.clone());
                session.sandbox_branch = Some(branch.clone());
                session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
                store
                    .insert_session_with_custody(
                        &session,
                        SessionCustodyBinding::New(NewCustodyRoot {
                            custody_id: Uuid::new_v4(),
                            canonical_repo_dir: repo.display().to_string(),
                            sandbox_root: root.display().to_string(),
                            sandbox_branch: branch,
                            repository_identity: "startup-2354-identity".to_owned(),
                            source_commit: "0".repeat(40),
                            cause: CustodyCause::FreshLaunch,
                        }),
                    )
                    .unwrap();
                seeded.push((session.id, "quarantined"));
            }
        }
    }
    seeded
}

/// Run startup reconciliation until a pass leaves the observable state alone
/// (a live root with a missing directory needs two boots to reach its terminal
/// projection), and return that settled dump.
fn settle_startup_2354(store: &mut Store, sandbox_base: &Path) -> Vec<String> {
    let mut previous = Vec::new();
    for _ in 0..6 {
        CustodyService::reconcile_startup_with_pool(store, sandbox_base, 1).unwrap();
        let dump = startup_custody_dump_2354(store);
        if dump == previous {
            return dump;
        }
        previous = dump;
    }
    panic!("startup custody reconciliation did not settle in six boots");
}

fn projection_state_2354(store: &Store, session_id: Uuid) -> String {
    store
        .conn
        .query_row(
            "SELECT execution_state FROM session_execution_projections WHERE session_id=?1",
            [session_id.to_string()],
            |row| row.get(0),
        )
        .unwrap()
}

/// A settled startup publishes the same verdicts again without writing a
/// row: the restore used to rewrite every projection of every session on
/// every boot (about 9,000 commits on the hub, #2354).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn settled_startup_reconcile_writes_nothing_2354() {
    let tempdir = tempfile::tempdir().unwrap();
    let (base, repo) = startup_dirs_2354(tempdir.path());
    let mut store = Store::open_in_memory().unwrap();
    let seeded = seed_startup_corpus_2354(&mut store, &base, &repo, 24);

    let settled = settle_startup_2354(&mut store, &base);
    for (session_id, expected) in &seeded {
        assert_eq!(
            projection_state_2354(&store, *session_id),
            *expected,
            "{session_id}"
        );
    }

    let changes_before = store.conn.total_changes();
    CustodyService::reconcile_startup_with_pool(&mut store, &base, 1).unwrap();
    assert_eq!(
        store.conn.total_changes(),
        changes_before,
        "a settled startup rewrites no row"
    );
    assert_eq!(startup_custody_dump_2354(&store), settled);
}

/// Skipping an unchanged publication must never hide drift: a projection that
/// no longer holds the verdict is republished, to exactly the settled state.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn drifted_projections_are_republished_by_startup_2354() {
    let tempdir = tempfile::tempdir().unwrap();
    let (base, repo) = startup_dirs_2354(tempdir.path());
    let mut store = Store::open_in_memory().unwrap();
    let seeded = seed_startup_corpus_2354(&mut store, &base, &repo, 16);
    let settled = settle_startup_2354(&mut store, &base);

    // One drifted projection per published shape, each satisfying the
    // projection table's CHECK constraints.
    let drift = [
        "freshness='unverified', effective_cwd=NULL, validated_at=NULL, error_code=NULL",
        "execution_state='invalid', freshness='invalid', error_code='tuple_incomplete'",
        "freshness='unverified', validated_at=NULL",
        "execution_state='invalid'",
    ];
    for ((session_id, _), assignment) in seeded.iter().take(4).zip(drift) {
        store
            .conn
            .execute(
                &format!(
                    "UPDATE session_execution_projections SET {assignment} WHERE session_id=?1"
                ),
                [session_id.to_string()],
            )
            .unwrap();
    }
    assert_ne!(startup_custody_dump_2354(&store), settled);

    let changes_before = store.conn.total_changes();
    CustodyService::reconcile_startup_with_pool(&mut store, &base, 1).unwrap();
    assert!(
        store.conn.total_changes() >= changes_before + 4,
        "every drifted projection is rewritten"
    );
    assert_eq!(startup_custody_dump_2354(&store), settled);
}

/// The settled pass does work proportional to the group count. This is the
/// benchmark-style bound (#2354): a per-group scan of the whole corpus would
/// grow about 16 times for 4 times the groups, not 4.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn settled_startup_reconcile_work_is_linear_in_groups_2354() {
    let settled_pass_ops = |groups: usize| -> u64 {
        let tempdir = tempfile::tempdir().unwrap();
        let (base, repo) = startup_dirs_2354(tempdir.path());
        let mut store = Store::open_in_memory().unwrap();
        seed_startup_corpus_2354(&mut store, &base, &repo, groups);
        settle_startup_2354(&mut store, &base);
        let (result, ops) = vm_ops_during(&mut store, |store| {
            CustodyService::reconcile_startup_with_pool(store, &base, 1)
        });
        result.unwrap();
        ops
    };

    let small = settled_pass_ops(16);
    let large = settled_pass_ops(64);
    assert!(small > 0 && large > 0, "the pass executed statements");
    assert!(
        large * 10 <= small * 48,
        "4x the groups must cost at most 4.8x the work: {small} ops for 16 groups, {large} for 64"
    );
}

/// The whole-corpus and per-custody retry-eligibility statements are two
/// literals; the per-custody one must stay the whole-corpus one plus exactly
/// the custody narrowing, so the startup gate keeps the verdict of the scan.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
#[test]
fn retry_eligibility_sql_variants_stay_in_step_2354() {
    use crate::store::sandbox_custody::{
        STARTUP_RETRY_ELIGIBLE_BY_CUSTODY_SQL, STARTUP_RETRY_ELIGIBLE_SQL,
    };

    assert_eq!(
        STARTUP_RETRY_ELIGIBLE_BY_CUSTODY_SQL,
        format!("{STARTUP_RETRY_ELIGIBLE_SQL}\n AND s.sandbox_custody_id=?1")
    );
}
