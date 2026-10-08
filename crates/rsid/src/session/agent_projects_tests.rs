//! `AgentCreateProject` / `AgentUpdateProject` tests (#1626 slice 1): they
//! assert the stated intent (who may register and edit projects, where, and
//! that nothing is ever deleted).

use super::*;
use crate::session::agent_verbs::tests::{control_handle_with_store, test_session};
use rsi_common::agent_control_schema::AgentControlVerbV1 as Verb;
use rsi_common::agent_projects::{
    PROJECT_HARNESS_PROTECTED, PROJECT_HAS_LIVE_SESSIONS, PROJECT_NAME_TAKEN,
    PROJECT_NOT_AUTHORIZED, PROJECT_NOT_IN_SCOPE, PROJECT_PATH_INVALID, PROJECT_PATH_TAKEN,
};
use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::harness_manager_v2::{
    ConfigureHarnessManagerPolicyRequestV2, ManagerLaunchChoiceV2, ManagerOperatingModeV2,
    ManagerPolicyV2,
};
use rsi_common::types::{SessionKind, SessionProvider, SessionStatus};

type SharedStore = Arc<tokio::sync::Mutex<Store>>;

struct Fixture {
    control: AgentControlHandle,
    store: SharedStore,
    index: Arc<RwLock<ProjectIndex>>,
    root: tempfile::TempDir,
    /// Project A: its project manager is `pm`.
    a: Uuid,
    /// Project B: inside the portfolio seat's grant, no manager.
    b: Uuid,
    /// Project C: outside every grant.
    c: Uuid,
    pm: Uuid,
    seat: Uuid,
    worker: Uuid,
}

fn execute() -> ManagerPolicyV2 {
    ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        max_created_sessions: 8,
        ..ManagerPolicyV2::default()
    }
}

fn add_project(store: &Store, name: &str, path: Option<PathBuf>) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    store
        .insert_project(&Project {
            id,
            name: name.into(),
            path,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    id
}

fn add_session(store: &Store, project: Option<Uuid>, status: SessionStatus) -> Uuid {
    let id = Uuid::new_v4();
    let mut row = test_session(id, PathBuf::from("/tmp/agent-projects"));
    row.project_id = project;
    row.session_kind = SessionKind::Standard;
    row.status = status;
    store.insert_session(&row).unwrap();
    id
}

fn set_pm_policy(store: &Store, project: Uuid, policy: ManagerPolicyV2) {
    let config = store.get_harness_manager(project).unwrap().unwrap();
    let policy_version = store
        .get_harness_manager_policy(project)
        .unwrap()
        .map_or(0, |grant| grant.row_version);
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: config.row_version,
            expected_policy_version: policy_version,
            idempotency_key: Uuid::new_v4().to_string(),
            policy,
        })
        .unwrap();
}

fn grant_seat(
    store: &Store,
    seat: Uuid,
    projects: Vec<Uuid>,
    policy: ManagerPolicyV2,
    version: i64,
) {
    store
        .configure_global_manager(
            &ConfigureGlobalManagerRequestV1 {
                session_id: seat,
                project_ids: projects,
                allowed_launches: vec![ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-opus-5-5".into(),
                    effort: Some("high".into()),
                }],
                project_policy: policy,
                expected_grant_version: version,
                idempotency_key: format!("grant-{version}"),
            },
            "operator:test",
        )
        .unwrap();
}

async fn fixture() -> Fixture {
    fixture_with(true, false).await
}

/// `with_roots`: the temp root is the only configured workspace root;
/// otherwise none is configured and the temp root stands in for the daemon
/// user's home directory. `with_harness`: `<root>/harness` is the harness
/// root.
async fn fixture_with(with_roots: bool, with_harness: bool) -> Fixture {
    let (control, store) = control_handle_with_store();
    let root = tempfile::tempdir().unwrap();
    let canonical_root = root.path().canonicalize().unwrap();
    let index = Arc::new(RwLock::new(ProjectIndex::new(Vec::new())));
    let control = control.with_project_admin(ProjectAdminContext {
        workspace_roots: if with_roots {
            vec![canonical_root.clone()]
        } else {
            Vec::new()
        },
        home_dir: (!with_roots).then(|| canonical_root.clone()),
        harness_root: with_harness.then(|| canonical_root.join("harness")),
        project_index: Arc::clone(&index),
        workflow_config_cache: Arc::new(RwLock::new(
            crate::project_workflow::ProjectWorkflowCache::new(),
        )),
    });
    let guard = store.lock().await;
    let dir_a = canonical_root.join("a");
    std::fs::create_dir(&dir_a).unwrap();
    let a = add_project(&guard, "Alpha", Some(dir_a));
    let b = add_project(&guard, "Beta", None);
    let c = add_project(&guard, "Gamma", None);
    let pm = add_session(&guard, Some(a), SessionStatus::Running);
    let worker = add_session(&guard, Some(a), SessionStatus::Running);
    let seat = add_session(&guard, None, SessionStatus::Running);
    guard
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: a,
            session_id: pm,
            epic_ids: None,
            group_ids: vec![],
            expected_row_version: 0,
        })
        .unwrap();
    set_pm_policy(&guard, a, execute());
    grant_seat(&guard, seat, vec![a, b], execute(), 0);
    drop(guard);
    Fixture {
        control,
        store,
        index,
        root,
        a,
        b,
        c,
        pm,
        seat,
        worker,
    }
}

fn create(name: &str, path: &Path) -> AgentCreateProjectRequestV1 {
    AgentCreateProjectRequestV1 {
        name: name.into(),
        path: path.to_string_lossy().into_owned(),
        description: None,
        color: None,
    }
}

fn update(project_id: Uuid) -> AgentUpdateProjectRequestV1 {
    AgentUpdateProjectRequestV1 {
        project_id,
        name: None,
        path: None,
        description: None,
        color: None,
    }
}

fn code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn subdir(fixture: &Fixture, name: &str) -> PathBuf {
    let path = fixture.root.path().canonicalize().unwrap().join(name);
    std::fs::create_dir_all(&path).unwrap();
    path
}

async fn project_count(store: &SharedStore) -> usize {
    store.lock().await.load_projects().unwrap().len()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn project_manager_and_portfolio_seat_each_register_a_project() {
    let f = fixture().await;
    let before = project_count(&f.store).await;
    let dir = subdir(&f, "pm-made");
    let made = f
        .control
        .agent_create_project(f.pm, create("PM made", &dir))
        .await
        .unwrap();
    assert!(!made.deduplicated);
    assert_eq!(made.path.as_deref(), dir.to_str());
    let stored = f.store.lock().await.get_project(made.project_id).unwrap();
    assert_eq!(stored.map(|p| p.name), Some("PM made".to_string()));

    let seat_dir = subdir(&f, "seat-made");
    let seat_made = f
        .control
        .agent_create_project(f.seat, create("Seat made", &seat_dir))
        .await
        .unwrap();
    assert_eq!(project_count(&f.store).await, before + 2);
    // The running daemon's launch-time index sees both directories at once.
    let index = f.index.read().await;
    assert_eq!(index.find_project_for_path(&dir), Some(made.project_id));
    assert_eq!(
        index.find_project_for_path(&seat_dir),
        Some(seat_made.project_id)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_replay_of_a_project_outside_coverage_is_a_name_conflict() {
    let f = fixture().await;
    let dir = subdir(&f, "replay");
    f.control
        .agent_create_project(f.seat, create("Replay", &dir))
        .await
        .unwrap();
    // The new project is not in the creator's coverage, so a replay is the
    // plain name conflict and carries no project fields.
    assert_eq!(
        code(
            f.control
                .agent_create_project(f.seat, create("Replay", &dir))
                .await
                .unwrap_err()
        ),
        PROJECT_NAME_TAKEN
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn only_an_execute_mode_unpaused_manager_seat_may_create() {
    let f = fixture().await;
    let before = project_count(&f.store).await;
    let dir = subdir(&f, "refused");
    // A worker holds no manager seat.
    assert_eq!(
        code(
            f.control
                .agent_create_project(f.worker, create("W", &dir))
                .await
                .unwrap_err()
        ),
        PROJECT_NOT_AUTHORIZED
    );
    // A project manager outside Execute mode, or paused, is refused.
    for edit in [
        (ManagerOperatingModeV2::Status, false),
        (ManagerOperatingModeV2::Execute, true),
    ] {
        let mut policy = execute();
        policy.mode = edit.0;
        policy.paused = edit.1;
        set_pm_policy(&*f.store.lock().await, f.a, policy);
        assert_eq!(
            code(
                f.control
                    .agent_create_project(f.pm, create("PM", &dir))
                    .await
                    .unwrap_err()
            ),
            PROJECT_NOT_AUTHORIZED
        );
    }
    // So is a portfolio seat in Status mode.
    let mut status = execute();
    status.mode = ManagerOperatingModeV2::Status;
    grant_seat(&*f.store.lock().await, f.seat, vec![f.a, f.b], status, 1);
    assert_eq!(
        code(
            f.control
                .agent_create_project(f.seat, create("Seat", &dir))
                .await
                .unwrap_err()
        ),
        PROJECT_NOT_AUTHORIZED
    );
    assert_eq!(project_count(&f.store).await, before);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn create_refuses_bad_directories_and_taken_names_and_paths() {
    let f = fixture().await;
    let root = f.root.path().canonicalize().unwrap();
    let file = root.join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let outside = tempfile::tempdir().unwrap();
    for bad in [
        root.join("missing"),
        file,
        outside.path().canonicalize().unwrap(),
    ] {
        assert_eq!(
            code(
                f.control
                    .agent_create_project(f.pm, create("Bad", &bad))
                    .await
                    .unwrap_err()
            ),
            PROJECT_PATH_INVALID,
            "{}",
            bad.display()
        );
    }
    let dir = subdir(&f, "fresh");
    assert_eq!(
        code(
            f.control
                .agent_create_project(f.pm, create("Alpha", &dir))
                .await
                .unwrap_err()
        ),
        PROJECT_NAME_TAKEN
    );
    // Project A already registers its directory under another name.
    assert_eq!(
        code(
            f.control
                .agent_create_project(f.pm, create("Other name", &root.join("a")))
                .await
                .unwrap_err()
        ),
        PROJECT_PATH_TAKEN
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn update_is_limited_to_the_callers_coverage() {
    let f = fixture().await;
    // A project manager edits its own project.
    let mut rename = update(f.a);
    rename.name = Some("Alpha renamed".into());
    rename.color = Some("#112233".into());
    let renamed = f.control.agent_update_project(f.pm, rename).await.unwrap();
    assert_eq!(renamed.name, "Alpha renamed");
    assert_eq!(renamed.color, "#112233");
    // ... but not another project, nor one that does not exist.
    for target in [f.b, f.c, Uuid::new_v4()] {
        let mut request = update(target);
        request.description = Some("nope".into());
        assert_eq!(
            code(
                f.control
                    .agent_update_project(f.pm, request)
                    .await
                    .unwrap_err()
            ),
            PROJECT_NOT_IN_SCOPE
        );
    }
    // A portfolio seat edits projects of its grant, not outside it.
    let mut request = update(f.b);
    request.description = Some("Beta, edited".into());
    let edited = f
        .control
        .agent_update_project(f.seat, request)
        .await
        .unwrap();
    assert_eq!(edited.description.as_deref(), Some("Beta, edited"));
    let mut outside = update(f.c);
    outside.name = Some("Gamma renamed".into());
    assert_eq!(
        code(
            f.control
                .agent_update_project(f.seat, outside)
                .await
                .unwrap_err()
        ),
        PROJECT_NOT_IN_SCOPE
    );
    // A rename onto another project's name is refused.
    let mut clash = update(f.b);
    clash.name = Some("Gamma".into());
    assert_eq!(
        code(
            f.control
                .agent_update_project(f.seat, clash)
                .await
                .unwrap_err()
        ),
        PROJECT_NAME_TAKEN
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_path_change_waits_for_live_sessions_and_refreshes_the_index() {
    let f = fixture().await;
    let moved = subdir(&f, "moved");
    let mut request = update(f.a);
    request.path = Some(moved.to_string_lossy().into_owned());
    // The project manager and a worker of A are Running.
    assert_eq!(
        code(
            f.control
                .agent_update_project(f.pm, request.clone())
                .await
                .unwrap_err()
        ),
        PROJECT_HAS_LIVE_SESSIONS
    );
    f.store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET status='Completed' WHERE project_id=?1",
            [f.a.to_string()],
        )
        .unwrap();
    let updated = f.control.agent_update_project(f.pm, request).await.unwrap();
    assert_eq!(updated.path.as_deref(), moved.to_str());
    assert_eq!(
        f.index.read().await.find_project_for_path(&moved),
        Some(f.a)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn the_catalog_lists_the_project_verbs_for_manager_seats_only() {
    let f = fixture().await;
    let guard = f.store.lock().await;
    for seat in [f.pm, f.seat] {
        let verbs = guard.agent_authority_projection(seat).unwrap().verbs;
        assert!(verbs.contains(&Verb::CreateProject), "{seat}");
        assert!(verbs.contains(&Verb::UpdateProject), "{seat}");
    }
    let worker = guard.agent_authority_projection(f.worker).unwrap().verbs;
    assert!(!worker.contains(&Verb::CreateProject));
    assert!(!worker.contains(&Verb::UpdateProject));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_handle_without_project_caches_refuses_instead_of_going_stale() {
    let f = fixture().await;
    let (bare, _) = control_handle_with_store();
    let dir = subdir(&f, "bare");
    assert_eq!(
        code(
            bare.agent_create_project(f.pm, create("Bare", &dir))
                .await
                .unwrap_err()
        ),
        rsi_common::agent_projects::PROJECT_ADMIN_UNAVAILABLE
    );
}

fn admin_in_catalog(guard: &Store, session: Uuid) -> bool {
    let verbs = guard.agent_authority_projection(session).unwrap().verbs;
    assert_eq!(
        verbs.contains(&Verb::CreateProject),
        verbs.contains(&Verb::UpdateProject),
        "{session}"
    );
    verbs.contains(&Verb::CreateProject)
}

fn mode_policy(mode: ManagerOperatingModeV2, paused: bool) -> ManagerPolicyV2 {
    let mut policy = execute();
    policy.mode = mode;
    policy.paused = paused;
    policy
}

/// The daemon's answer to "may this session update `project`?": `Ok` when it
/// is permitted, otherwise the refusal code.
async fn may_update(f: &Fixture, session: Uuid, project: Uuid) -> std::result::Result<(), String> {
    let mut request = update(project);
    request.description = Some("touched".into());
    f.control
        .agent_update_project(session, request)
        .await
        .map(drop)
        .map_err(code)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn the_catalog_omits_both_verbs_for_status_mode_and_paused_seats() {
    let f = fixture().await;
    for (mode, paused) in [
        (ManagerOperatingModeV2::Status, false),
        (ManagerOperatingModeV2::Execute, true),
    ] {
        set_pm_policy(&*f.store.lock().await, f.a, mode_policy(mode, paused));
        {
            let guard = f.store.lock().await;
            let version = guard
                .portfolio_seat_grant(f.seat)
                .unwrap()
                .unwrap()
                .grant_version;
            grant_seat(
                &guard,
                f.seat,
                vec![f.a, f.b],
                mode_policy(mode, paused),
                version,
            );
        }
        let guard = f.store.lock().await;
        assert!(!admin_in_catalog(&guard, f.pm), "pm {mode:?} {paused}");
        assert!(!admin_in_catalog(&guard, f.seat), "seat {mode:?} {paused}");
        drop(guard);
        assert_eq!(
            may_update(&f, f.pm, f.a).await,
            Err(PROJECT_NOT_AUTHORIZED.into())
        );
        assert_eq!(
            may_update(&f, f.seat, f.b).await,
            Err(PROJECT_NOT_AUTHORIZED.into())
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_revoked_manager_grant_is_refused_and_leaves_the_catalog() {
    let f = fixture().await;
    {
        let guard = f.store.lock().await;
        assert!(admin_in_catalog(&guard, f.pm));
        // Re-scoping the seat strands its policy grant at the old scope
        // version: the grant reads as revoked.
        let config = guard.get_harness_manager(f.a).unwrap().unwrap();
        guard
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: f.a,
                session_id: f.pm,
                epic_ids: Some(vec![]),
                group_ids: vec![],
                expected_row_version: config.row_version,
            })
            .unwrap();
        assert!(
            guard
                .get_harness_manager_policy(f.a)
                .unwrap()
                .unwrap()
                .revoked
        );
        assert!(!admin_in_catalog(&guard, f.pm));
    }
    assert_eq!(
        may_update(&f, f.pm, f.a).await,
        Err(PROJECT_NOT_AUTHORIZED.into())
    );
    let dir = subdir(&f, "revoked");
    assert_eq!(
        code(
            f.control
                .agent_create_project(f.pm, create("Revoked", &dir))
                .await
                .unwrap_err()
        ),
        PROJECT_NOT_AUTHORIZED
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_revoked_portfolio_grant_is_refused_and_leaves_the_catalog() {
    let f = fixture().await;
    {
        let guard = f.store.lock().await;
        assert!(admin_in_catalog(&guard, f.seat));
        let version = guard
            .portfolio_seat_grant(f.seat)
            .unwrap()
            .unwrap()
            .grant_version;
        guard
            .revoke_global_manager(&rsi_common::global_manager::RevokeGlobalManagerRequestV1 {
                expected_grant_version: version,
                idempotency_key: "revoke".into(),
            })
            .unwrap();
        assert!(!admin_in_catalog(&guard, f.seat));
    }
    assert_eq!(
        may_update(&f, f.seat, f.b).await,
        Err(PROJECT_NOT_AUTHORIZED.into())
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn an_issue_bound_worker_is_refused_and_the_catalog_omits_the_verbs() {
    let f = fixture().await;
    {
        let guard = f.store.lock().await;
        let issue = guard
            .create_issue(&rsi_common::types::NewIssue {
                project_id: f.a,
                title: "bound".into(),
                body: String::new(),
                priority: None,
                labels: vec![],
                created_by_session_id: None,
                assignee: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
            })
            .unwrap();
        let payload = serde_json::json!({
            "issue_binding": {"issue_id": issue.id.to_string(), "display_number": 1}
        });
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        guard
            .conn
            .execute(
                "INSERT INTO harness_manager_v2_operations(id,project_id,manager_session_id,
                    scope_version,policy_version,idempotency_key,fingerprint,kind,payload_json,
                    state,target_session_id,outcome_json,not_before,created_at,updated_at)
                 VALUES(?1,?2,?3,1,1,?1,'fixture',?4,?5,'succeeded',?6,'{}',?7,?7,?7)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    f.a.to_string(),
                    f.pm.to_string(),
                    "lifecycle_action",
                    payload.to_string(),
                    f.worker.to_string(),
                    now
                ],
            )
            .unwrap();
        // The binding is live: the worker may read and append to its Issue.
        let verbs = guard.agent_authority_projection(f.worker).unwrap().verbs;
        assert!(verbs.contains(&Verb::UpdateIssue));
        assert!(!admin_in_catalog(&guard, f.worker));
    }
    assert_eq!(
        may_update(&f, f.worker, f.a).await,
        Err(PROJECT_NOT_AUTHORIZED.into())
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_dual_seat_session_acts_on_the_union_of_its_permitting_seats() {
    let f = fixture().await;
    // The project manager of A also sits on a portfolio node covering B.
    grant_seat(&*f.store.lock().await, f.pm, vec![f.b], execute(), 1);
    assert_eq!(may_update(&f, f.pm, f.a).await, Ok(()), "own project");
    assert_eq!(may_update(&f, f.pm, f.b).await, Ok(()), "granted project");
    assert_eq!(
        may_update(&f, f.pm, f.c).await,
        Err(PROJECT_NOT_IN_SCOPE.into())
    );
    assert!(admin_in_catalog(&*f.store.lock().await, f.pm));

    // A Status-mode portfolio grant does not block the project-manager seat.
    let version = f
        .store
        .lock()
        .await
        .portfolio_seat_grant(f.pm)
        .unwrap()
        .unwrap()
        .grant_version;
    grant_seat(
        &*f.store.lock().await,
        f.pm,
        vec![f.b],
        mode_policy(ManagerOperatingModeV2::Status, false),
        version,
    );
    assert!(admin_in_catalog(&*f.store.lock().await, f.pm));
    assert_eq!(may_update(&f, f.pm, f.a).await, Ok(()), "own project");
    assert_eq!(
        may_update(&f, f.pm, f.b).await,
        Err(PROJECT_NOT_IN_SCOPE.into()),
        "the Status seat contributes no coverage"
    );

    // A paused project-manager seat does not block the portfolio seat, and
    // that grant need not cover the PM's own project.
    let version = f
        .store
        .lock()
        .await
        .portfolio_seat_grant(f.pm)
        .unwrap()
        .unwrap()
        .grant_version;
    grant_seat(&*f.store.lock().await, f.pm, vec![f.b], execute(), version);
    set_pm_policy(
        &*f.store.lock().await,
        f.a,
        mode_policy(ManagerOperatingModeV2::Execute, true),
    );
    assert!(admin_in_catalog(&*f.store.lock().await, f.pm));
    assert_eq!(may_update(&f, f.pm, f.b).await, Ok(()), "granted project");
    assert_eq!(
        may_update(&f, f.pm, f.a).await,
        Err(PROJECT_NOT_IN_SCOPE.into()),
        "the paused seat contributes no coverage"
    );
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn path_aliases_are_canonicalised_before_the_containment_check() {
    let f = fixture().await;
    let root = f.root.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_dir = outside.path().canonicalize().unwrap();
    let real = subdir(&f, "real");
    // A symlink inside the root that points inside resolves to its target.
    let inward = root.join("inward");
    std::os::unix::fs::symlink(&real, &inward).unwrap();
    let made = f
        .control
        .agent_create_project(f.pm, create("Inward", &inward))
        .await
        .unwrap();
    assert_eq!(made.path.as_deref(), real.to_str());
    // A symlink inside the root that points out of it is refused.
    let escape = root.join("escape");
    std::os::unix::fs::symlink(&outside_dir, &escape).unwrap();
    // A symlink outside the root that points in is accepted and stored as
    // its canonical target.
    let alias = outside_dir.join("alias");
    let target = subdir(&f, "target");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let aliased = f
        .control
        .agent_create_project(f.pm, create("Aliased", &alias))
        .await
        .unwrap();
    assert_eq!(aliased.path.as_deref(), target.to_str());
    // `..` that climbs out of the root, and a trailing slash.
    let climb = root.join("a").join("..").join("..");
    let slash = format!("{}/", subdir(&f, "slash").display());
    for bad in [escape.clone(), climb, outside_dir.join("..")] {
        assert_eq!(
            code(
                f.control
                    .agent_create_project(f.pm, create("Escape", &bad))
                    .await
                    .unwrap_err()
            ),
            PROJECT_PATH_INVALID,
            "{}",
            bad.display()
        );
    }
    let request = AgentCreateProjectRequestV1 {
        name: "Slash".into(),
        path: slash.clone(),
        description: None,
        color: None,
    };
    let slashed = f.control.agent_create_project(f.pm, request).await.unwrap();
    assert_eq!(slashed.path.as_deref(), slash.trim_end_matches('/').into());
    let dotted = root.join("a").join("..").join("slash");
    assert_eq!(
        code(
            f.control
                .agent_create_project(f.pm, create("Dotted", &dotted))
                .await
                .unwrap_err()
        ),
        PROJECT_PATH_TAKEN,
        "`..` resolves to the registered directory"
    );
    // The same aliases are canonicalised on update.
    let mut moved = update(f.a);
    moved.path = Some(escape.to_string_lossy().into_owned());
    assert_eq!(
        code(
            f.control
                .agent_update_project(f.pm, moved)
                .await
                .unwrap_err()
        ),
        PROJECT_PATH_INVALID
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn with_no_workspace_roots_only_strict_descendants_of_home_outside_the_data_dir_are_accepted()
{
    let f = fixture_with(false, false).await;
    let home = f.root.path().canonicalize().unwrap();
    let data_dir = home.join(".rsi");
    std::fs::create_dir_all(data_dir.join("sandboxes")).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    for (n, bad) in [
        home.clone(),
        data_dir.clone(),
        data_dir.join("sandboxes"),
        PathBuf::from(std::path::MAIN_SEPARATOR_STR),
        elsewhere.path().canonicalize().unwrap(),
        home.join("."),
        home.join("a").join(".."),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            code(
                f.control
                    .agent_create_project(f.pm, create(&format!("Bad {n}"), &bad))
                    .await
                    .unwrap_err()
            ),
            PROJECT_PATH_INVALID,
            "{}",
            bad.display()
        );
    }
    let ok = subdir(&f, "work/repo");
    let made = f
        .control
        .agent_create_project(f.pm, create("Home descendant", &ok))
        .await
        .unwrap();
    assert_eq!(made.path.as_deref(), ok.to_str());
    // An update applies the same rule.
    let mut moved = update(f.a);
    moved.path = Some(data_dir.to_string_lossy().into_owned());
    assert_eq!(
        code(
            f.control
                .agent_update_project(f.pm, moved)
                .await
                .unwrap_err()
        ),
        PROJECT_PATH_INVALID
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn configured_workspace_roots_keep_their_containment_behaviour() {
    let f = fixture().await;
    // The root itself is inside the root; no home-directory rule applies.
    let root = f.root.path().canonicalize().unwrap();
    let made = f
        .control
        .agent_create_project(f.pm, create("Root itself", &root))
        .await
        .unwrap();
    assert_eq!(made.path.as_deref(), root.to_str());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn the_harness_root_and_its_project_are_protected_from_agents() {
    let f = fixture_with(true, true).await;
    let root = f.root.path().canonicalize().unwrap();
    let harness_dir = subdir(&f, "harness");
    let inside = subdir(&f, "harness/crate");
    let harness = add_project(&*f.store.lock().await, "Harness", Some(harness_dir.clone()));
    let version = f
        .store
        .lock()
        .await
        .portfolio_seat_grant(f.seat)
        .unwrap()
        .unwrap()
        .grant_version;
    grant_seat(
        &*f.store.lock().await,
        f.seat,
        vec![f.a, f.b, harness],
        execute(),
        version,
    );
    let before = project_count(&f.store).await;
    // Create at, inside, or around the harness root.
    for (n, path) in [harness_dir.clone(), inside, root.clone()]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            code(
                f.control
                    .agent_create_project(f.seat, create(&format!("Shadow {n}"), &path))
                    .await
                    .unwrap_err()
            ),
            PROJECT_HARNESS_PROTECTED,
            "{}",
            path.display()
        );
    }
    assert_eq!(project_count(&f.store).await, before);
    // Repointing the harness project is operator-only, even for a seat whose
    // grant covers it.
    let elsewhere = subdir(&f, "elsewhere");
    let mut moved = update(harness);
    moved.path = Some(elsewhere.to_string_lossy().into_owned());
    assert_eq!(
        code(
            f.control
                .agent_update_project(f.seat, moved)
                .await
                .unwrap_err()
        ),
        PROJECT_HARNESS_PROTECTED
    );
    // Nor may another project move onto the harness root.
    let mut onto = update(f.b);
    onto.path = Some(harness_dir.to_string_lossy().into_owned());
    assert_eq!(
        code(
            f.control
                .agent_update_project(f.seat, onto)
                .await
                .unwrap_err()
        ),
        PROJECT_HARNESS_PROTECTED
    );
    let stored = f.store.lock().await.get_project(harness).unwrap().unwrap();
    assert_eq!(stored.path.as_deref(), Some(harness_dir.as_path()));
    // Other fields of the harness project, and unrelated projects, still edit.
    let mut rename = update(harness);
    rename.description = Some("kept".into());
    assert!(f.control.agent_update_project(f.seat, rename).await.is_ok());
    let fine = subdir(&f, "unrelated");
    assert!(
        f.control
            .agent_create_project(f.seat, create("Unrelated", &fine))
            .await
            .is_ok()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_replay_outside_the_callers_coverage_discloses_nothing() {
    let f = fixture().await;
    let hidden_dir = subdir(&f, "hidden");
    let hidden = add_project(&*f.store.lock().await, "Hidden", Some(hidden_dir.clone()));
    // Project C and the new project are outside the PM's and seat's coverage.
    for caller in [f.pm, f.seat] {
        assert_eq!(
            code(
                f.control
                    .agent_create_project(caller, create("Hidden", &hidden_dir))
                    .await
                    .unwrap_err()
            ),
            PROJECT_NAME_TAKEN
        );
    }
    // Inside coverage the replay still returns the project.
    let a_dir = f
        .store
        .lock()
        .await
        .get_project(f.a)
        .unwrap()
        .unwrap()
        .path
        .unwrap();
    for caller in [f.pm, f.seat] {
        let again = f
            .control
            .agent_create_project(caller, create("Alpha", &a_dir))
            .await
            .unwrap();
        assert!(again.deduplicated);
        assert_eq!(again.project_id, f.a);
    }
    assert_ne!(hidden, f.a);
}

fn drain_project_created(
    events: &mut tokio::sync::broadcast::Receiver<Arc<crate::bus::DaemonEvent>>,
) -> Vec<(Uuid, String, String, Uuid)> {
    let mut found = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let crate::bus::DaemonEvent::ProjectCreated {
            project_id,
            name,
            source,
            created_by_session_id,
        } = event.as_ref()
        {
            found.push((
                *project_id,
                name.clone(),
                source.clone(),
                *created_by_session_id,
            ));
        }
    }
    found
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn an_agent_create_publishes_one_project_created_event_naming_its_creator() {
    let f = fixture().await;
    let mut events = f.control.event_bus.subscribe();
    let dir = subdir(&f, "announced");
    let made = f
        .control
        .agent_create_project(f.seat, create("Announced", &dir))
        .await
        .unwrap();
    assert_eq!(
        drain_project_created(&mut events),
        vec![(
            made.project_id,
            "Announced".to_string(),
            "agent".to_string(),
            f.seat
        )]
    );
    // The bus view carries the same payload the TUI parses.
    let bus: rsi_common::rpc::BusEvent = crate::bus::DaemonEvent::ProjectCreated {
        project_id: made.project_id,
        name: "Announced".into(),
        source: "agent".into(),
        created_by_session_id: f.seat,
    }
    .into();
    assert_eq!(
        bus.event_type,
        rsi_common::agent_projects::PROJECT_CREATED_EVENT
    );
    let parsed: rsi_common::agent_projects::ProjectCreatedEventV1 =
        serde_json::from_value(bus.data).unwrap();
    assert_eq!(parsed.created_by_session_id, f.seat);
    assert_eq!(parsed.project_id, made.project_id);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_repeated_or_refused_create_publishes_no_project_created_event() {
    let f = fixture().await;
    let dir = subdir(&f, "quiet");
    f.control
        .agent_create_project(f.seat, create("Quiet", &dir))
        .await
        .unwrap();
    let mut events = f.control.event_bus.subscribe();
    // #1635: a new project is outside its creator's coverage, so the repeat
    // is refused (not deduplicated) and announces nothing.
    assert_eq!(
        code(
            f.control
                .agent_create_project(f.seat, create("Quiet", &dir))
                .await
                .unwrap_err()
        ),
        PROJECT_NAME_TAKEN
    );
    let _ = f
        .control
        .agent_create_project(f.worker, create("Worker", &subdir(&f, "w")))
        .await
        .unwrap_err();
    assert_eq!(drain_project_created(&mut events), Vec::new());
}

/// N1 (#1642): `~/.rsi` may be a symlink to another directory under home; the
/// state it points at is still the daemon's own and is never registrable.
#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_symlinked_data_dir_keeps_its_target_unregistrable_without_roots() {
    let f = fixture_with(false, false).await;
    let home = f.root.path().canonicalize().unwrap();
    let state = home.join("elsewhere-state");
    std::fs::create_dir_all(state.join("sandboxes")).unwrap();
    std::os::unix::fs::symlink(&state, home.join(".rsi")).unwrap();
    for (n, bad) in [
        state.clone(),
        state.join("sandboxes"),
        home.join(".rsi"),
        home.join(".rsi").join("sandboxes"),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            code(
                f.control
                    .agent_create_project(f.pm, create(&format!("Linked {n}"), &bad))
                    .await
                    .unwrap_err()
            ),
            PROJECT_PATH_INVALID,
            "{}",
            bad.display()
        );
    }
    // A sibling of the target is an ordinary home descendant.
    let ok = subdir(&f, "elsewhere-state-sibling");
    f.control
        .agent_create_project(f.pm, create("Sibling", &ok))
        .await
        .unwrap();
}
