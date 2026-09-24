//! D00 cleanup-clamp integration tests.
//!
//! Every fixture owns a temporary SQLite database, sandbox base, and real Git
//! repository. Tests snapshot the durable row, hierarchy pointer, complete
//! sandbox tree, branch refs, and `git worktree list --porcelain` before and
//! after a cleanup entry point. A blocked call must leave those snapshots
//! byte-for-byte identical and must publish no cleanup/lifecycle success event.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rsi_common::archive_cleanup::{ArchiveCleanupDispositionV1, ArchiveCleanupErrorV1};
use rsi_common::types::{
    ContextUsageConfidence, SandboxCleanupState, SandboxKind, Session, SessionKind,
    SessionProvider, SessionStatus,
};
use rsid::bus::{DaemonEvent, EventBus};
use rsid::config::{Config, RuntimeConfig};
use rsid::sandbox::SandboxAllocator;
use rsid::session::SessionManager;
use rsid::store::Store;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;
use walkdir::WalkDir;

struct Fixture {
    manager: SessionManager,
    event_bus: Arc<EventBus>,
    _db_dir: TempDir,
    _sandbox_base: TempDir,
    sandbox_base_path: PathBuf,
    repo: TempDir,
    db_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntry {
    relative_path: PathBuf,
    kind: &'static str,
    mode: u32,
    size: u64,
    content_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitSnapshot {
    head: String,
    refs: String,
    worktrees: String,
    index: IndexSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexSnapshot {
    bytes: Vec<u8>,
    mode: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    lock_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionSnapshot {
    status: String,
    pending_archive: bool,
    sandbox_kind: Option<String>,
    sandbox_root: Option<String>,
    sandbox_branch: Option<String>,
    sandbox_cleanup_state: Option<String>,
    parent_id: Option<String>,
    lead_session_id: Option<String>,
    retry_attempt: Option<u8>,
    max_retries: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FullSnapshot {
    tree: Vec<TreeEntry>,
    git: GitSnapshot,
    session: Option<SessionSnapshot>,
    epic_lead: Option<Option<String>>,
}

fn run_git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .current_dir(directory)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output utf8")
        .trim_end()
        .to_string()
}

fn init_git_repo(dir: &Path) {
    run_git(dir, &["init", "-q", "-b", "main"]);
    run_git(dir, &["config", "user.email", "d00@example.invalid"]);
    run_git(dir, &["config", "user.name", "D00 Fixture"]);
    std::fs::write(dir.join("README.md"), "canonical\n").expect("write README");
    run_git(dir, &["add", "README.md"]);
    run_git(dir, &["commit", "-q", "-m", "initial"]);
}

fn fixture() -> Fixture {
    let db_dir = TempDir::new().expect("db tempdir");
    let sandbox_base = TempDir::new().expect("sandbox tempdir");
    let repo = TempDir::new().expect("repo tempdir");
    init_git_repo(repo.path());

    let db_path = db_dir.path().join("test.db");
    let store = Store::open(&db_path).expect("open store");
    let event_bus = Arc::new(EventBus::new(128));
    let runtime_config = RuntimeConfig::from_config(&Config::from_env());
    let sandbox_base_path = sandbox_base.path().to_path_buf();
    let manager = SessionManager::new(
        Arc::clone(&event_bus),
        store,
        false,
        db_dir.path().join("daemon.sock"),
        None,
        Vec::new(),
        runtime_config,
        sandbox_base_path.clone(),
    )
    .expect("SessionManager::new");

    Fixture {
        manager,
        event_bus,
        _db_dir: db_dir,
        _sandbox_base: sandbox_base,
        sandbox_base_path,
        repo,
        db_path,
    }
}

fn make_session(id: Uuid, working_dir: &Path, status: SessionStatus) -> Session {
    Session {
        context_fill_pct: None,
        id,
        provider: SessionProvider::Claude,
        claude_session_id: None,
        query: "D00 fixture".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: working_dir.to_path_buf(),
        git_branch: None,
        status,
        project_id: None,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        session_kind: SessionKind::Standard,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        model: None,
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        continued_from: None,
        context_usage_confidence: ContextUsageConfidence::Missing,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        rating: None,
        harness_version_hash: None,
        test_passed: None,
        clippy_passed: None,
        turn_count: None,
        retry_count: None,
        approval_wait_ms: None,
        work_time_ms: None,
        approval_started_at: None,
        sandbox_kind: None,
        sandbox_root: None,
        sandbox_branch: None,
        sandbox_cleanup_state: None,
        tag: String::new(),
        tags: Vec::new(),
        parent_id: None,
        lead_session_id: None,
        is_eval: false,
        capability_class: None,
        topology_node_id: None,
        topology_iteration: 0,
        provider_cli_version: None,
        provider_capabilities: Vec::new(),
        thinking_tokens: None,
        service_tier: None,
        cache_creation_1h_tokens: None,
        cache_creation_5m_tokens: None,
        permission_denial_count: None,
        subagent_stats_json: None,
        queued_turn_count: None,
        terminal_reason: None,
    }
}

fn insert_session(db_path: &Path, session: &Session) {
    Store::open(db_path)
        .expect("open setup store")
        .insert_session(session)
        .expect("insert session");
}

fn allocate(fix: &Fixture, session_id: Uuid) -> (PathBuf, String) {
    let allocation = SandboxAllocator::new(fix.sandbox_base_path.clone())
        .allocate(
            session_id,
            fix.repo.path(),
            SandboxKind::GitWorktree,
            "HEAD",
            None,
        )
        .expect("allocate sandbox");
    (allocation.root, allocation.branch.expect("worktree branch"))
}

fn sandbox_session(
    fix: &Fixture,
    status: SessionStatus,
    cleanup_state: SandboxCleanupState,
    with_epic_lead: bool,
) -> (Uuid, PathBuf, String, Option<Uuid>) {
    let session_id = Uuid::new_v4();
    let (root, branch) = allocate(fix, session_id);
    let epic_id = with_epic_lead.then(Uuid::new_v4);

    if let Some(epic_id) = epic_id {
        let mut epic = make_session(epic_id, fix.repo.path(), SessionStatus::Completed);
        epic.session_kind = SessionKind::Epic;
        insert_session(&fix.db_path, &epic);
    }

    let mut session = make_session(session_id, fix.repo.path(), status);
    session.sandbox_kind = Some(SandboxKind::GitWorktree);
    session.sandbox_root = Some(root.clone());
    session.sandbox_branch = Some(branch.clone());
    session.sandbox_cleanup_state = Some(cleanup_state);
    session.parent_id = epic_id;
    insert_session(&fix.db_path, &session);

    if let Some(epic_id) = epic_id {
        Store::open(&fix.db_path)
            .expect("open lead store")
            .set_lead_session(epic_id, Some(session_id))
            .expect("set epic lead");
    }

    (session_id, root, branch, epic_id)
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn snapshot_tree(root: &Path) -> Vec<TreeEntry> {
    if !root.exists() {
        return Vec::new();
    }
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .map(|entry| entry.expect("walk sandbox"))
        .map(|entry| {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(path).expect("sandbox metadata");
            let file_type = metadata.file_type();
            let (kind, content_sha256) = if file_type.is_file() {
                (
                    "file",
                    Some(hash_bytes(&std::fs::read(path).expect("read sandbox file"))),
                )
            } else if file_type.is_dir() {
                ("dir", None)
            } else if file_type.is_symlink() {
                let target = std::fs::read_link(path).expect("read sandbox symlink");
                (
                    "symlink",
                    Some(hash_bytes(target.as_os_str().as_encoded_bytes())),
                )
            } else {
                ("other", None)
            };
            TreeEntry {
                relative_path: path
                    .strip_prefix(root)
                    .expect("sandbox-relative path")
                    .to_path_buf(),
                kind,
                mode: metadata.permissions().mode(),
                size: metadata.len(),
                content_sha256,
            }
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    entries
}

fn linked_worktree_index(root: &Path) -> PathBuf {
    let path = PathBuf::from(run_git(root, &["rev-parse", "--git-path", "index"]));
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn snapshot_index(root: &Path) -> IndexSnapshot {
    let index = linked_worktree_index(root);
    let metadata = std::fs::metadata(&index).expect("linked worktree index metadata");
    IndexSnapshot {
        bytes: std::fs::read(&index).expect("linked worktree index bytes"),
        mode: metadata.mode(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        lock_exists: index.with_extension("lock").exists(),
    }
}

fn snapshot_git(repo: &Path, root: &Path) -> GitSnapshot {
    GitSnapshot {
        head: run_git(repo, &["rev-parse", "HEAD"]),
        refs: run_git(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
            ],
        ),
        worktrees: run_git(repo, &["worktree", "list", "--porcelain"]),
        index: snapshot_index(root),
    }
}

fn snapshot_session(db_path: &Path, session_id: Uuid) -> Option<SessionSnapshot> {
    let connection = rusqlite::Connection::open(db_path).expect("open snapshot db");
    connection
        .query_row(
            "SELECT status, pending_archive, sandbox_kind, sandbox_root, sandbox_branch, \
                    sandbox_cleanup_state, parent_id, lead_session_id, retry_attempt, max_retries \
             FROM sessions WHERE id = ?1",
            rusqlite::params![session_id.to_string()],
            |row| {
                Ok(SessionSnapshot {
                    status: row.get(0)?,
                    pending_archive: row.get::<_, i64>(1)? != 0,
                    sandbox_kind: row.get(2)?,
                    sandbox_root: row.get(3)?,
                    sandbox_branch: row.get(4)?,
                    sandbox_cleanup_state: row.get(5)?,
                    parent_id: row.get(6)?,
                    lead_session_id: row.get(7)?,
                    retry_attempt: row.get(8)?,
                    max_retries: row.get(9)?,
                })
            },
        )
        .optional()
        .expect("snapshot session")
}

fn snapshot_epic_lead(db_path: &Path, epic_id: Option<Uuid>) -> Option<Option<String>> {
    epic_id.map(|epic_id| {
        rusqlite::Connection::open(db_path)
            .expect("open lead snapshot db")
            .query_row(
                "SELECT lead_session_id FROM sessions WHERE id = ?1",
                rusqlite::params![epic_id.to_string()],
                |row| row.get(0),
            )
            .expect("snapshot epic lead")
    })
}

fn snapshot(fix: &Fixture, session_id: Uuid, root: &Path, epic_id: Option<Uuid>) -> FullSnapshot {
    FullSnapshot {
        tree: snapshot_tree(root),
        git: snapshot_git(fix.repo.path(), root),
        session: snapshot_session(&fix.db_path, session_id),
        epic_lead: snapshot_epic_lead(&fix.db_path, epic_id),
    }
}

fn assert_blocked<T: std::fmt::Debug>(result: rsid::error::Result<T>, reason: &str) {
    let error = result.expect_err("sandbox lifecycle action must be blocked");
    if let rsid::error::DaemonError::StructuredRpc { data, .. } = &error {
        let envelope: ArchiveCleanupErrorV1 =
            serde_json::from_value(data.clone()).expect("typed archive cleanup refusal");
        envelope
            .validate_wire()
            .expect("valid archive cleanup refusal wire shape");
        assert_eq!(envelope.safe_code.as_str(), reason);
        return;
    }
    let message = error.to_string();
    assert!(
        message.contains(reason),
        "unexpected policy denial: {message}"
    );
}

fn assert_metadata_archive_event(
    receiver: &mut tokio::sync::broadcast::Receiver<Arc<DaemonEvent>>,
    session_id: Uuid,
) {
    while let Ok(event) = receiver.try_recv() {
        if matches!(
            event.as_ref(),
            DaemonEvent::SessionArchived {
                session_id: actual,
                projection_id: None,
            } if *actual == session_id
        ) {
            return;
        }
    }
    panic!("metadata-only archive event was not observed");
}

fn assert_no_success_events(
    event_bus: &EventBus,
    receiver: &mut tokio::sync::broadcast::Receiver<Arc<DaemonEvent>>,
) {
    while let Ok(event) = receiver.try_recv() {
        assert!(
            !matches!(
                event.as_ref(),
                DaemonEvent::SandboxOrphanCleaned { .. }
                    | DaemonEvent::SessionArchived { .. }
                    | DaemonEvent::SessionDeleted { .. }
            ),
            "blocked cleanup emitted success event: {event:?}"
        );
    }
    event_bus.unsubscribe();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_archive_routes_to_cleanup_once_and_preserves_source() {
    let fix = fixture();
    let (session_id, root, branch, epic_id) = sandbox_session(
        &fix,
        SessionStatus::Completed,
        SandboxCleanupState::Live,
        true,
    );
    fix.manager
        .restore_sessions()
        .await
        .expect("restore production-shaped completed cache");
    let source_ref = format!("refs/heads/{branch}");
    let source_oid = run_git(fix.repo.path(), &["rev-parse", &source_ref]);
    let mut events = fix.event_bus.subscribe();

    let result = fix
        .manager
        .archive_session(session_id)
        .await
        .expect("eligible public archive reaches private proof service");
    result
        .validate_wire()
        .expect("archive result wire contract");
    let receipt = result.receipt.as_ref().expect("cleanup receipt");
    assert_eq!(receipt.source_branch, source_ref);
    assert_eq!(receipt.source_oid.as_str(), source_oid);
    assert!(!root.exists(), "settled cleanup removes only the worktree");
    assert_eq!(
        run_git(fix.repo.path(), &["rev-parse", &source_ref]),
        source_oid
    );
    let row = snapshot_session(&fix.db_path, session_id).expect("archived row retained");
    assert_eq!(row.status, "Archived");
    assert_eq!(row.sandbox_cleanup_state.as_deref(), Some("Purged"));
    assert!(row.sandbox_root.is_none());
    assert!(row.sandbox_branch.is_none());
    assert_eq!(snapshot_epic_lead(&fix.db_path, epic_id), Some(None));

    let projection_id = loop {
        let event = events.recv().await.expect("projected archive event");
        if let DaemonEvent::SessionArchived {
            session_id: actual,
            projection_id: Some(projection_id),
        } = event.as_ref()
        {
            assert_eq!(*actual, session_id);
            break *projection_id;
        }
    };
    let connection = rusqlite::Connection::open(&fix.db_path).expect("open projection state");
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM archive_cleanup_success_projections WHERE projection_id=?1",
                rusqlite::params![projection_id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .expect("projection cardinality"),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM archive_cleanup_projection_consumers WHERE projection_id=?1 AND delivery_state='delivered'",
                rusqlite::params![projection_id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .expect("consumer cardinality"),
        2
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT delivery_state FROM archive_cleanup_projection_consumers WHERE projection_id=?1 AND consumer_kind='memory'",
                rusqlite::params![projection_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .expect("memory consumer retry state"),
        "delivering"
    );

    let (replay_a, replay_b, replay_c) = tokio::join!(
        fix.manager.archive_session(session_id),
        fix.manager.archive_session(session_id),
        fix.manager.archive_session(session_id),
    );
    for replay in [replay_a, replay_b, replay_c] {
        assert_eq!(
            replay.expect("concurrent exact replay returns settled receipt"),
            result
        );
    }
    assert!(
        events.try_recv().is_err(),
        "concurrent replay must not publish twice"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM archive_cleanup_projection_consumers WHERE projection_id=?1 AND delivery_state='delivered'",
                rusqlite::params![projection_id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .expect("replay delivered consumer cardinality"),
        2,
        "replay must not manufacture memory delivery"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT delivery_state FROM archive_cleanup_projection_consumers WHERE projection_id=?1 AND consumer_kind='memory'",
                rusqlite::params![projection_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .expect("replay memory consumer retry state"),
        "delivering"
    );
    fix.event_bus.unsubscribe();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_delete_remains_metadata_only_and_retains_the_sandbox() {
    let fix = fixture();
    let (session_id, root, _, epic_id) = sandbox_session(
        &fix,
        SessionStatus::Completed,
        SandboxCleanupState::Live,
        true,
    );
    let before = snapshot(&fix, session_id, &root, epic_id);
    let mut events = fix.event_bus.subscribe();

    fix.manager
        .delete_session(session_id)
        .await
        .expect("delete remains the sealed metadata-only lifecycle path");

    let after = snapshot(&fix, session_id, &root, epic_id);
    assert_eq!(after.tree, before.tree);
    assert_eq!(after.git, before.git);
    let row = after.session.expect("soft-deleted row remains durable");
    assert_eq!(row.status, "Deleted");
    assert_eq!(
        row.sandbox_root,
        before.session.as_ref().unwrap().sandbox_root
    );
    assert_eq!(
        row.sandbox_branch,
        before.session.as_ref().unwrap().sandbox_branch
    );
    assert_eq!(row.sandbox_cleanup_state.as_deref(), Some("Live"));
    assert_eq!(after.epic_lead, Some(None));
    assert!(matches!(
        events.try_recv().expect("metadata delete event").as_ref(),
        DaemonEvent::SessionDeleted { session_id: actual } if *actual == session_id
    ));
    fix.event_bus.unsubscribe();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_purge_is_blocked_and_dependent_state_survives() {
    let fix = fixture();
    let (session_id, root, _, epic_id) = sandbox_session(
        &fix,
        SessionStatus::Deleted,
        SandboxCleanupState::Live,
        true,
    );
    let before = snapshot(&fix, session_id, &root, epic_id);
    let mut events = fix.event_bus.subscribe();

    assert_blocked(
        fix.manager.purge_session(session_id).await,
        "missing_independently_verified_proof",
    );

    assert_eq!(snapshot(&fix, session_id, &root, epic_id), before);
    assert_no_success_events(&fix.event_bus, &mut events);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dirty_worktree_and_untracked_content_are_preserved() {
    let fix = fixture();
    let (session_id, root, _, epic_id) = sandbox_session(
        &fix,
        SessionStatus::Completed,
        SandboxCleanupState::Live,
        true,
    );
    std::fs::write(root.join("README.md"), "dirty tracked change\n").expect("dirty tracked file");
    std::fs::write(root.join("untracked.txt"), "untracked work\n").expect("write untracked file");
    fix.manager
        .restore_sessions()
        .await
        .expect("hydrate the production completed snapshot");
    let before = snapshot(&fix, session_id, &root, epic_id);
    let mut events = fix.event_bus.subscribe();

    assert_blocked(
        fix.manager.archive_session(session_id).await,
        "worktree_dirty",
    );

    assert_eq!(snapshot(&fix, session_id, &root, epic_id), before);
    assert_no_success_events(&fix.event_bus, &mut events);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_source_ref_is_preserved_without_fallback_removal() {
    let fix = fixture();
    let (session_id, root, branch, epic_id) = sandbox_session(
        &fix,
        SessionStatus::Completed,
        SandboxCleanupState::Live,
        true,
    );
    run_git(&root, &["checkout", "--detach", "-q"]);
    run_git(
        fix.repo.path(),
        &["update-ref", "-d", &format!("refs/heads/{branch}")],
    );
    fix.manager
        .restore_sessions()
        .await
        .expect("hydrate the production completed snapshot");
    let before = snapshot(&fix, session_id, &root, epic_id);
    let mut events = fix.event_bus.subscribe();

    let result = fix
        .manager
        .archive_session(session_id)
        .await
        .expect("unrestorable source-ref shape stays on metadata-only archive");
    assert_eq!(
        result.disposition,
        ArchiveCleanupDispositionV1::NoCleanupRequired
    );
    assert!(result.receipt.is_none());
    let after = snapshot(&fix, session_id, &root, epic_id);
    assert_eq!(after.tree, before.tree);
    assert_eq!(after.git, before.git);
    let row = after
        .session
        .expect("archived source-ref row remains durable");
    assert_eq!(row.status, "Archived");
    assert_eq!(row.sandbox_cleanup_state.as_deref(), Some("Live"));
    assert_eq!(after.epic_lead, Some(None));
    assert_metadata_archive_event(&mut events, session_id);
    fix.event_bus.unsubscribe();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_source_ref_is_preserved_when_git_surfaces_index_drift() {
    let fix = fixture();
    let (session_id, root, branch, epic_id) = sandbox_session(
        &fix,
        SessionStatus::Completed,
        SandboxCleanupState::Live,
        true,
    );
    std::fs::write(fix.repo.path().join("advance.txt"), "advance\n").expect("write main change");
    run_git(fix.repo.path(), &["add", "advance.txt"]);
    run_git(fix.repo.path(), &["commit", "-q", "-m", "advance main"]);
    let main_head = run_git(fix.repo.path(), &["rev-parse", "main"]);
    run_git(
        fix.repo.path(),
        &["update-ref", &format!("refs/heads/{branch}"), &main_head],
    );
    fix.manager
        .restore_sessions()
        .await
        .expect("hydrate the production completed snapshot");
    let before = snapshot(&fix, session_id, &root, epic_id);
    let mut events = fix.event_bus.subscribe();

    assert_blocked(
        fix.manager.archive_session(session_id).await,
        "worktree_dirty",
    );

    assert_eq!(snapshot(&fix, session_id, &root, epic_id), before);
    assert_no_success_events(&fix.event_bus, &mut events);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_live_ownership_stays_on_metadata_only_archive() {
    let fix = fixture();
    let (session_id, root, branch, epic_id) = sandbox_session(
        &fix,
        SessionStatus::Completed,
        SandboxCleanupState::Live,
        true,
    );
    let second_id = Uuid::new_v4();
    let mut second = make_session(second_id, fix.repo.path(), SessionStatus::Completed);
    second.sandbox_kind = Some(SandboxKind::GitWorktree);
    second.sandbox_root = Some(root.clone());
    second.sandbox_branch = Some(branch);
    second.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
    insert_session(&fix.db_path, &second);
    let before = snapshot(&fix, session_id, &root, epic_id);
    let second_before = snapshot_session(&fix.db_path, second_id);
    let mut events = fix.event_bus.subscribe();

    let result = fix
        .manager
        .archive_session(session_id)
        .await
        .expect("shared ownership must stay on the sealed metadata-only path");
    assert_eq!(
        result.disposition,
        ArchiveCleanupDispositionV1::NoCleanupRequired
    );
    assert!(result.receipt.is_none());

    let after = snapshot(&fix, session_id, &root, epic_id);
    assert_eq!(after.tree, before.tree);
    assert_eq!(after.git, before.git);
    let row = after
        .session
        .expect("archived shared owner remains durable");
    assert_eq!(row.status, "Archived");
    assert_eq!(row.sandbox_cleanup_state.as_deref(), Some("Live"));
    assert_eq!(after.epic_lead, Some(None));
    assert_eq!(snapshot_session(&fix.db_path, second_id), second_before);
    assert_metadata_archive_event(&mut events, session_id);
    fix.event_bus.unsubscribe();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inconsistent_sandbox_tuple_stays_on_metadata_only_archive() {
    let fix = fixture();
    let session_id = Uuid::new_v4();
    let (root, _) = allocate(&fix, session_id);
    let mut session = make_session(session_id, fix.repo.path(), SessionStatus::Completed);
    session.sandbox_kind = Some(SandboxKind::GitWorktree);
    session.sandbox_root = Some(root.clone());
    session.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
    insert_session(&fix.db_path, &session);
    let before = snapshot(&fix, session_id, &root, None);
    let mut events = fix.event_bus.subscribe();

    let result = fix
        .manager
        .archive_session(session_id)
        .await
        .expect("inconsistent tuple is not selected for destructive cleanup");
    assert_eq!(
        result.disposition,
        ArchiveCleanupDispositionV1::NoCleanupRequired
    );
    assert!(result.receipt.is_none());
    let after = snapshot(&fix, session_id, &root, None);
    assert_eq!(after.tree, before.tree);
    assert_eq!(after.git, before.git);
    let row = after
        .session
        .expect("archived inconsistent row remains durable");
    assert_eq!(row.status, "Archived");
    assert_eq!(row.sandbox_cleanup_state.as_deref(), Some("Live"));
    assert_metadata_archive_event(&mut events, session_id);
    fix.event_bus.unsubscribe();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_pending_archive_uses_metadata_only_archive_and_retains_sandbox() {
    let fix = fixture();
    let (session_id, root, _, epic_id) = sandbox_session(
        &fix,
        SessionStatus::Completed,
        SandboxCleanupState::Live,
        true,
    );
    {
        let store = Store::open(&fix.db_path).expect("open pending store");
        store
            .update_pending_archive(session_id, true)
            .expect("set legacy pending marker");
    }
    let before = snapshot(&fix, session_id, &root, epic_id);
    let mut events = fix.event_bus.subscribe();

    fix.manager
        .restore_sessions()
        .await
        .expect("restore sessions");

    let after = snapshot(&fix, session_id, &root, epic_id);
    assert_eq!(after.tree, before.tree);
    assert_eq!(after.git, before.git);
    let row = after.session.expect("pending archive row remains durable");
    assert_eq!(row.status, "Archived");
    assert!(!row.pending_archive);
    assert_eq!(
        row.sandbox_root,
        before.session.as_ref().unwrap().sandbox_root
    );
    assert_eq!(
        row.sandbox_branch,
        before.session.as_ref().unwrap().sandbox_branch
    );
    assert_eq!(row.sandbox_cleanup_state.as_deref(), Some("Live"));
    assert_eq!(after.epic_lead, Some(None));
    assert_metadata_archive_event(&mut events, session_id);
    fix.event_bus.unsubscribe();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_typed_archived_and_deleted_owners_are_retained() {
    for status in [SessionStatus::Archived, SessionStatus::Deleted] {
        let fix = fixture();
        let (session_id, root, _, _) =
            sandbox_session(&fix, status, SandboxCleanupState::Live, false);
        let before = snapshot(&fix, session_id, &root, None);
        let mut events = fix.event_bus.subscribe();

        fix.manager
            .restore_sessions()
            .await
            .expect("restore sessions");

        assert_eq!(
            snapshot(&fix, session_id, &root, None),
            before,
            "startup mutated {status:?} owner"
        );
        assert_no_success_events(&fix.event_bus, &mut events);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_path_only_and_non_live_residuals_are_retained() {
    for cleanup_state in [
        None,
        Some(SandboxCleanupState::Failed),
        Some(SandboxCleanupState::Purged),
    ] {
        let fix = fixture();
        let session_id = Uuid::new_v4();
        let (stray_root, branch) = allocate(&fix, session_id);
        std::fs::write(stray_root.join("retained.txt"), "retained\n")
            .expect("write residual marker");

        if let Some(cleanup_state) = cleanup_state {
            let mut row = make_session(session_id, fix.repo.path(), SessionStatus::Archived);
            row.sandbox_kind = Some(SandboxKind::GitWorktree);
            row.sandbox_root = Some(stray_root.clone());
            row.sandbox_branch = Some(branch);
            row.sandbox_cleanup_state = Some(cleanup_state);
            insert_session(&fix.db_path, &row);
        }

        let before = snapshot(&fix, session_id, &stray_root, None);
        let mut events = fix.event_bus.subscribe();
        fix.manager
            .restore_sessions()
            .await
            .expect("restore sessions");

        let after = snapshot(&fix, session_id, &stray_root, None);
        if cleanup_state == Some(SandboxCleanupState::Purged) {
            assert_eq!(after.tree, before.tree);
            assert_eq!(after.git, before.git);
            let row = after.session.expect("purged residual row");
            assert_eq!(row.sandbox_cleanup_state.as_deref(), Some("Purged"));
            assert!(row.sandbox_root.is_none());
            assert!(row.sandbox_branch.is_none());
        } else {
            assert_eq!(
                after, before,
                "startup mutated residual with state {cleanup_state:?}"
            );
        }
        assert_no_success_events(&fix.event_bus, &mut events);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_resumable_terminal_states_preserve_sandbox() {
    for status in [
        SessionStatus::Completed,
        SessionStatus::Failed,
        SessionStatus::Interrupted,
    ] {
        let fix = fixture();
        let (session_id, root, _, _) =
            sandbox_session(&fix, status, SandboxCleanupState::Live, false);
        let before = snapshot(&fix, session_id, &root, None);
        let mut events = fix.event_bus.subscribe();

        fix.manager
            .restore_sessions()
            .await
            .expect("restore sessions");

        assert_eq!(
            snapshot(&fix, session_id, &root, None),
            before,
            "restore mutated resumable {status:?} sandbox"
        );
        assert_no_success_events(&fix.event_bus, &mut events);
    }
}

const D00_BINDING_MATRIX_INVENTORY: &[(&str, &str)] = &[
    (
        "dirty worktree",
        "dirty_worktree_and_untracked_content_are_preserved",
    ),
    (
        "unreadable worktree",
        "shared_boundary_adverse_rows_use_isolated_full_snapshots",
    ),
    (
        "missing source identity",
        "missing_source_ref_is_preserved_without_fallback_removal",
    ),
    (
        "changed source identity",
        "real_ref_drift_is_barrier_controlled_and_classifier_is_inert",
    ),
    (
        "unknown target",
        "shared_boundary_adverse_rows_use_isolated_full_snapshots",
    ),
    (
        "mismatched target",
        "shared_boundary_adverse_rows_use_isolated_full_snapshots",
    ),
    (
        "non-main unconfigured target",
        "shared_boundary_adverse_rows_use_isolated_full_snapshots",
    ),
    (
        "absent proof",
        "shared_boundary_adverse_rows_use_isolated_full_snapshots",
    ),
    (
        "mismatched proof",
        "shared_boundary_adverse_rows_use_isolated_full_snapshots",
    ),
    (
        "missing cherry-pick mapping",
        "shared_boundary_adverse_rows_use_isolated_full_snapshots",
    ),
    (
        "failed post-head verification",
        "shared_boundary_adverse_rows_use_isolated_full_snapshots",
    ),
    (
        "shared ownership",
        "shared_live_ownership_blocks_without_touching_either_owner",
    ),
    (
        "runtime ownership read failure",
        "runtime_ownership_read_failure_is_inert_in_isolated_repository",
    ),
    (
        "runtime ownership drift",
        "runtime_ownership_drift_is_compared_to_post_external_snapshot",
    ),
    (
        "inconsistent sandbox tuple",
        "inconsistent_sandbox_tuple_is_blocked_without_defaulting_to_live",
    ),
    (
        "explicit archive",
        "explicit_archive_is_blocked_before_every_mutation",
    ),
    (
        "explicit delete",
        "explicit_delete_is_blocked_before_every_mutation",
    ),
    (
        "explicit purge",
        "explicit_purge_is_blocked_and_dependent_state_survives",
    ),
    (
        "mark pending archive",
        "pending_archive_admission_is_inert_in_isolated_repository",
    ),
    (
        "terminal pending auto-archive",
        "terminal_pending_archive_preserves_cleanup_state_after_drain",
    ),
    (
        "restore-time pending archive",
        "restore_pending_archive_remains_pending_and_unarchived",
    ),
    (
        "startup typed owner",
        "startup_typed_archived_and_deleted_owners_are_retained",
    ),
    (
        "startup row re-read failure",
        "restore_row_reread_failure_uses_sweep_and_is_inert",
    ),
    (
        "startup path-only registered worktree",
        "startup_path_only_and_non_live_residuals_are_retained",
    ),
    (
        "launch abort then restart",
        "launch_abort_then_restore_path_only_is_inert",
    ),
    (
        "normal terminal states",
        "normal_resumable_terminal_states_preserve_sandbox",
    ),
    (
        "verified no target or tombstone",
        "verified_no_target_and_complete_tombstone_keep_lifecycle_behavior",
    ),
];

#[test]
fn d00_binding_matrix_inventory_names_every_isolated_evidence_row() {
    let row_names = D00_BINDING_MATRIX_INVENTORY
        .iter()
        .map(|(row, _)| *row)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(row_names.len(), D00_BINDING_MATRIX_INVENTORY.len());
    assert_eq!(D00_BINDING_MATRIX_INVENTORY.len(), 27);
    assert!(
        D00_BINDING_MATRIX_INVENTORY
            .iter()
            .all(|(row, test)| !row.is_empty() && !test.is_empty())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verified_no_target_and_complete_tombstone_keep_lifecycle_behavior() {
    let fix = fixture();

    let archive_id = Uuid::new_v4();
    insert_session(
        &fix.db_path,
        &make_session(archive_id, fix.repo.path(), SessionStatus::Completed),
    );
    fix.manager
        .archive_session(archive_id)
        .await
        .expect("archive non-sandbox row");
    assert_eq!(
        snapshot_session(&fix.db_path, archive_id)
            .expect("archived row")
            .status,
        "Archived"
    );

    let delete_id = Uuid::new_v4();
    insert_session(
        &fix.db_path,
        &make_session(delete_id, fix.repo.path(), SessionStatus::Completed),
    );
    fix.manager
        .delete_session(delete_id)
        .await
        .expect("delete non-sandbox row");
    assert_eq!(
        snapshot_session(&fix.db_path, delete_id)
            .expect("deleted row")
            .status,
        "Deleted"
    );

    let purge_id = Uuid::new_v4();
    insert_session(
        &fix.db_path,
        &make_session(purge_id, fix.repo.path(), SessionStatus::Deleted),
    );
    fix.manager
        .purge_session(purge_id)
        .await
        .expect("purge non-sandbox row");
    assert!(snapshot_session(&fix.db_path, purge_id).is_none());

    let tombstone_id = Uuid::new_v4();
    let mut tombstone = make_session(tombstone_id, fix.repo.path(), SessionStatus::Completed);
    tombstone.sandbox_kind = Some(SandboxKind::GitWorktree);
    tombstone.sandbox_cleanup_state = Some(SandboxCleanupState::Purged);
    insert_session(&fix.db_path, &tombstone);
    fix.manager
        .archive_session(tombstone_id)
        .await
        .expect("archive complete tombstone");
    let row = snapshot_session(&fix.db_path, tombstone_id).expect("tombstone row");
    assert_eq!(row.status, "Archived");
    assert_eq!(row.sandbox_cleanup_state.as_deref(), Some("Purged"));
    assert!(row.sandbox_root.is_none());
    assert!(row.sandbox_branch.is_none());
}
