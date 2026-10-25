// #2750: `ReseatHarnessManager` end to end on a production Store and manager,
// with the successor's provider process scripted (never a paid model).
use super::*;
use crate::issue_tracker::poller::SessionLauncher;
use crate::sandbox::custody::CustodyExecutionRuntime;
use crate::session::harness::tools::schedule_wake::{
    ScheduleWakeRequest, build_agent_scheduled_job,
};
use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot, SessionCustodyBinding};
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
use rsi_common::manager_reseat::{
    RESEAT_SEAT_LIVE, RESEAT_SEAT_NOT_FAILED, ReseatHarnessManagerParams, successor_brief,
};
use rsi_common::satellite::SatelliteUuidV1;
use rsi_common::satellite_dispatch::{
    SatelliteDeliverOutcomeV1, SatelliteDeliverRequestV1, SatelliteInboundPolicyV1,
    SatelliteScopeKindV1,
};

async fn stop_reseat_successor(manager: &SessionManager, id: Uuid) {
    manager.interrupt_session(id).await.unwrap();
    drop_controller_candidate_test_stream(id);
    tokio::time::timeout(Duration::from_secs(10), async {
        while manager.active.read().await.contains_key(&id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    manager.persistence.barrier().await.unwrap();
}

struct ReseatWorld {
    manager: Arc<SessionManager>,
    _dir: TempDir,
    _sandbox: TempDir,
    _repo: TempDir,
    project: Uuid,
    seat: Uuid,
    hub: Uuid,
}

impl ReseatWorld {
    /// A project whose appointed manager is a Codex session in `status`, with
    /// a confirmed policy grant and the satellite's inbound scope pinning the
    /// seat's session id.
    async fn new(status: SessionStatus) -> Self {
        Self::build(status, false).await
    }

    /// [`Self::new`] whose seat was launched with a real git-worktree sandbox
    /// and holds the live, verified custody root of it (the aggregate a
    /// production sandboxed launch binds), so the successor allocates its own
    /// (#2956).
    async fn sandboxed(status: SessionStatus) -> Self {
        Self::build(status, true).await
    }

    async fn build(status: SessionStatus, sandboxed: bool) -> Self {
        let (manager, dir, sandbox) = manager();
        let repo = tempfile::tempdir().unwrap();
        init_d00_git_repo(repo.path());
        let (project, seat, hub) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let now = chrono::Utc::now();
        {
            let mut store = manager.store.lock().await;
            store
                .insert_project(&rsi_common::types::Project {
                    id: project,
                    name: "Reseat project".into(),
                    path: Some(repo.path().to_path_buf()),
                    description: None,
                    color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
            let mut codex = test_session(seat);
            codex.session_kind = SessionKind::Standard;
            codex.parent_id = None;
            codex.project_id = Some(project);
            codex.working_dir = repo.path().to_path_buf();
            codex.provider = SessionProvider::Codex;
            codex.model = Some("gpt-6-astra".into());
            codex.status = status;
            codex.title = Some("Laptop project manager".into());
            if sandboxed {
                let allocation = manager
                    .sandbox_allocator
                    .allocate(seat, repo.path(), SandboxKind::GitWorktree, "HEAD", None)
                    .expect("allocate the seat's real worktree");
                let branch = allocation.branch.clone().expect("a worktree branch");
                let identity = std::fs::canonicalize(
                    git_stdout(
                        &allocation.root,
                        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                    )
                    .trim(),
                )
                .expect("canonical git common dir");
                let source_commit = git_stdout(&allocation.root, &["rev-parse", "HEAD"])
                    .trim()
                    .to_owned();
                codex.sandbox_kind = Some(SandboxKind::GitWorktree);
                codex.sandbox_root = Some(allocation.root.clone());
                codex.sandbox_branch = Some(branch.clone());
                codex.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
                codex.git_branch = Some(branch.clone());
                store
                    .insert_session_with_custody(
                        &codex,
                        SessionCustodyBinding::New(NewCustodyRoot {
                            custody_id: Uuid::new_v4(),
                            canonical_repo_dir: repo.path().display().to_string(),
                            sandbox_root: allocation.root.display().to_string(),
                            sandbox_branch: branch,
                            repository_identity: identity.display().to_string(),
                            source_commit,
                            cause: CustodyCause::FreshLaunch,
                        }),
                    )
                    .expect("the seat's launch binds its custody root");
            } else {
                store.insert_session(&codex).unwrap();
            }
            store
                .conn
                .execute(
                    "INSERT INTO session_tags(session_id,tag) VALUES(?1,'manager')",
                    [seat.to_string()],
                )
                .unwrap();
            let scope = store
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    project_id: project,
                    session_id: seat,
                    epic_ids: None,
                    group_ids: Vec::new(),
                    expected_row_version: 0,
                })
                .unwrap();
            store
                .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                    project_id: project,
                    expected_scope_version: scope.row_version,
                    expected_policy_version: 0,
                    idempotency_key: "grant-before-outage".into(),
                    policy: ManagerPolicyV2 {
                        max_created_sessions: 4,
                        max_active_sessions: 2,
                        ..Default::default()
                    },
                })
                .unwrap();
            store
                .put_satellite_inbound_policy(&SatelliteInboundPolicyV1 {
                    allowed_hub_installations: vec![SatelliteUuidV1(hub)],
                    scope_roots: vec![SatelliteUuidV1(seat)],
                    ..Default::default()
                })
                .unwrap();
        }
        Self {
            manager: Arc::new(manager),
            _dir: dir,
            _sandbox: sandbox,
            _repo: repo,
            project,
            seat,
            hub,
        }
    }

    fn params(&self, provider_unavailable: bool) -> ReseatHarnessManagerParams {
        ReseatHarnessManagerParams {
            project_id: self.project,
            provider: SessionProvider::Claude,
            model: None,
            effort: None,
            reason: "codex login expired (401)".into(),
            provider_unavailable,
        }
    }

    /// Script the successor's provider process under a known session id.
    fn script_successor(
        &self,
        params: &ReseatHarnessManagerParams,
    ) -> (Uuid, ControllerCandidateTestProcess) {
        let successor = Uuid::new_v4();
        let query = successor_brief(self.seat, SessionProvider::Codex, &params.reason);
        fresh_manager_test_ids()
            .lock()
            .unwrap()
            .insert(query, successor);
        (
            successor,
            install_controller_candidate_test_process(successor),
        )
    }
}

/// #169: a seat launched without a sandbox in a git-backed project root (the
/// psychic-square seat: appointed with no sandbox, its root a git work tree) is
/// reseated into a git-worktree sandbox of its own. Exactly one writer holds a
/// tree afterwards: the successor owns the one live custody root, the stopped
/// seat holds none, and the tree is not the project's shared root.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sandboxless_seat_of_a_git_backed_project_is_reseated_into_its_own_sandbox() {
    let world = ReseatWorld::new(SessionStatus::Failed).await;
    let params = world.params(false);
    let (successor, _process) = world.script_successor(&params);
    let result = world
        .manager
        .reseat_harness_manager(params)
        .await
        .expect("a failed sandbox-less seat is reseated");
    assert_eq!(result.successor_session_id, successor);

    let own_root = live_root_facts(&world.manager, successor).await;
    assert_eq!(own_root.owner, successor);
    assert_eq!(own_root.allocation_session, successor);
    assert_eq!(own_root.generation, 1);
    let store = world.manager.store.lock().await;
    let new_seat = store.get_session(successor).unwrap().unwrap();
    assert_eq!(new_seat.sandbox_kind, Some(SandboxKind::GitWorktree));
    assert_eq!(
        new_seat.sandbox_cleanup_state,
        Some(SandboxCleanupState::Live)
    );
    let tree = new_seat
        .sandbox_root
        .clone()
        .expect("the successor has a tree");
    assert_eq!(std::path::PathBuf::from(&own_root.sandbox_root), tree);
    assert!(
        tree.starts_with(world.manager.sandbox_allocator.base_dir()),
        "the tree is a sandbox, not the project root: {}",
        tree.display()
    );
    let predecessor = store.get_session(world.seat).unwrap().unwrap();
    assert_eq!(predecessor.sandbox_root, None);
    assert!(
        store.live_custody_for_session(world.seat).is_err(),
        "the stopped seat holds no tree"
    );
    let live_roots: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sandbox_custody_roots
              WHERE owner_session_id IN (?1, ?2) AND state='live'",
            [world.seat.to_string(), successor.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(live_roots, 1, "one writer: the successor's own tree");
    drop(store);
    stop_reseat_successor(&world.manager, successor).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_codex_manager_is_reseated_onto_claude_and_hub_dispatch_follows() {
    let world = ReseatWorld::new(SessionStatus::Failed).await;
    let params = world.params(false);
    let (successor, _process) = world.script_successor(&params);

    let result = world
        .manager
        .reseat_harness_manager(params)
        .await
        .expect("a failed seat is reseated");
    assert_eq!(result.successor_session_id, successor);
    assert_eq!(result.predecessor_session_id, world.seat);
    assert_eq!(result.predecessor_provider, SessionProvider::Codex);
    assert_eq!(result.provider, SessionProvider::Claude);
    assert_eq!(result.reason, "codex login expired (401)");
    assert!(
        result.policy_version.is_some(),
        "the grant was carried over"
    );

    let store = world.manager.store.lock().await;
    // The successor continues the failed seat on the new provider and keeps
    // its identity: working dir, role metadata and tags.
    let new_seat = store.get_session(successor).unwrap().unwrap();
    assert_eq!(new_seat.provider, SessionProvider::Claude);
    assert_eq!(new_seat.continued_from, Some(world.seat));
    assert_eq!(new_seat.project_id, Some(world.project));
    assert_eq!(new_seat.parent_id, None);
    assert_eq!(new_seat.title.as_deref(), Some("Laptop project manager"));
    let tags: Vec<String> = store
        .conn
        .prepare("SELECT tag FROM session_tags WHERE session_id=?1")
        .unwrap()
        .query_map([successor.to_string()], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(tags, vec!["manager".to_string()]);
    // The appointment and its policy belong to the successor, unrevoked.
    let config = store.get_harness_manager(world.project).unwrap().unwrap();
    assert_eq!(config.current_session_id, Some(successor));
    let policy = store
        .get_harness_manager_policy(world.project)
        .unwrap()
        .unwrap();
    assert!(!policy.revoked);
    assert_eq!(policy.policy.max_created_sessions, 4);
    assert_eq!(policy.policy.max_active_sessions, 2);
    // The reason is on the replaced seat's transcript.
    let noted: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM conversation_events WHERE session_id=?1 AND event_type='System'
               AND metadata LIKE '%manager_reseat%' AND content LIKE '%codex login expired%'",
            [world.seat.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(noted, 1);
    // The satellite's inbound root still names the failed seat's id and
    // resolves to the Claude successor: hub dispatch keeps working.
    assert_eq!(
        store
            .resolve_satellite_scope_entry(SatelliteScopeKindV1::Session, world.seat)
            .unwrap()
            .seat(),
        Some(successor)
    );
    let scope = store.inbound_scope().unwrap();
    assert!(scope.allows_hub(world.hub));
    let hit = store
        .inbound_scope_hit(&scope, world.seat, None)
        .unwrap()
        .expect("the declared address is in scope");
    assert_eq!(
        store
            .inbound_scope_delivery_seat(&scope, hit, world.seat)
            .unwrap(),
        Some(successor)
    );
    drop(store);

    // A retry meets the healthy successor and launches nothing more.
    let again = world
        .manager
        .reseat_harness_manager(world.params(true))
        .await
        .unwrap_err();
    assert!(again.to_string().contains(RESEAT_SEAT_LIVE), "{again}");
    stop_reseat_successor(&world.manager, successor).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_manager_needs_the_provider_down_assertion_to_be_reseated() {
    let world = ReseatWorld::new(SessionStatus::Completed).await;
    let refused = world
        .manager
        .reseat_harness_manager(world.params(false))
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains(RESEAT_SEAT_NOT_FAILED),
        "{refused}"
    );
    // Nothing launched and nothing moved.
    let store = world.manager.store.lock().await;
    let config = store.get_harness_manager(world.project).unwrap().unwrap();
    assert_eq!(config.current_session_id, Some(world.seat));
    let launched: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE continued_from=?1",
            [world.seat.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(launched, 0);
    drop(store);

    let params = world.params(true);
    let (successor, _process) = world.script_successor(&params);
    let result = world.manager.reseat_harness_manager(params).await.unwrap();
    assert_eq!(result.successor_session_id, successor);
    stop_reseat_successor(&world.manager, successor).await;
}

/// The facts of one custody root a continuation must never disturb.
#[derive(Debug, PartialEq, Eq)]
struct RootFacts {
    custody_id: Uuid,
    owner: Uuid,
    allocation_session: Uuid,
    allocation_id: Uuid,
    generation: u64,
    sandbox_root: String,
}

async fn live_root_facts(manager: &SessionManager, session: Uuid) -> RootFacts {
    let root = manager
        .store
        .lock()
        .await
        .live_custody_for_session(session)
        .expect("the session holds a live, verified custody root");
    RootFacts {
        custody_id: root.custody_id,
        owner: root.owner_session_id,
        allocation_session: root.allocation_session_id,
        allocation_id: root.allocation_id,
        generation: root.generation,
        sandbox_root: root.sandbox_root,
    }
}

/// Move the seat's sandbox custody to a later rotation of the seat through the
/// rotation path production uses (`prepare_rotation_successor`, a reserved
/// rotation row, `bind_rotation_successor`: an exact same-root Transfer), then
/// settle that rotation `Archived`, as in the incident where the seat's root
/// stayed live under a later rotation. Returns the rotation.
async fn move_seat_custody_to_a_later_rotation(world: &ReseatWorld) -> Uuid {
    let seat = world
        .manager
        .store
        .lock()
        .await
        .get_session(world.seat)
        .unwrap()
        .unwrap();
    let runtime = world.manager.custody_execution_runtime();
    let candidate = runtime
        .prepare_rotation_successor(&seat)
        .await
        .expect("the completed seat's live root authenticates for rotation");
    let mut rotation = test_session(Uuid::new_v4());
    rotation.status = SessionStatus::Starting;
    rotation.parent_id = None;
    rotation.project_id = Some(world.project);
    rotation.working_dir = seat.working_dir.clone();
    rotation.provider = SessionProvider::Claude;
    rotation.continued_from = Some(world.seat);
    rotation.rotation_depth = 1;
    rotation.query = "later rotation of the seat".into();
    CustodyExecutionRuntime::apply_rotation_successor_tuple(&candidate, &seat, &mut rotation)
        .expect("a live seat rotates onto its own root");
    let invocation_id = Uuid::new_v4();
    {
        let mut store = world.manager.store.lock().await;
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO model_invocations (
                     id, purpose, invocation_kind, foreground, paid_risk,
                     admission_status, status, trigger_source, session_id,
                     policy_snapshot_json, created_at, started_at
                 ) VALUES (
                     ?1, 'session.rotate.child', 'session_lifecycle', 'foreground', 'paid_capable',
                     'admitted', 'running', 'reseat_custody_test', ?2,
                     '{}', ?3, ?3
                 )",
                rusqlite::params![invocation_id.to_string(), rotation.id.to_string(), now],
            )
            .expect("insert the rotation's invocation");
        store
            .insert_reserved_rotation_session_with_invocation(
                &rotation,
                invocation_id,
                "reseat-later-rotation",
            )
            .expect("reserve the later rotation");
    }
    if runtime
        .bind_rotation_successor(candidate, &seat, &rotation)
        .await
        .is_err()
    {
        panic!("the later rotation takes the seat's custody by transfer");
    }
    world
        .manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET status='Archived' WHERE id=?1",
            [rotation.id.to_string()],
        )
        .unwrap();
    rotation.id
}

/// A scripted provider process reports no session id; a resumable session has
/// one (the fence tests do the same).
async fn give_provider_session(manager: &SessionManager, id: Uuid) {
    let provider_session = format!("provider-{id}");
    manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET claude_session_id=?1 WHERE id=?2",
            rusqlite::params![provider_session, id.to_string()],
        )
        .unwrap();
    if let Some(cached) = manager.completed.write().await.get_mut(&id) {
        cached.session.claude_session_id = Some(provider_session);
    }
}

/// Complete the scripted turn of `id`: a `result` event, the stream closing and
/// the monitor settling the row `Completed` with nothing left active and its
/// completed-map entry published. The row turns `Completed` before the entry
/// exists; `give_provider_session` patches the entry, and an entry published
/// after that patch would hold no provider session id, so the later resume
/// wake would refuse with "no session ID was captured" (#614).
async fn finish_scripted_turn(
    manager: &SessionManager,
    id: Uuid,
    process: &ControllerCandidateTestProcess,
) {
    process
        .alive
        .store(false, std::sync::atomic::Ordering::SeqCst);
    send_controller_candidate_test_event(
        id,
        serde_json::from_value(serde_json::json!({
            "type": "result",
            "subtype": "success",
            "result": "Committed manager handoff complete",
            "is_error": false,
            "total_cost_usd": 0.01,
            "num_turns": 1
        }))
        .unwrap(),
    )
    .await;
    drop_controller_candidate_test_stream(id);
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let active = manager.active.read().await.contains_key(&id);
            let published = manager.completed.read().await.contains_key(&id);
            let status = manager
                .store
                .lock()
                .await
                .get_session(id)
                .unwrap()
                .map(|session| session.status);
            if !active && published && status == Some(SessionStatus::Completed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the scripted turn completes the session");
    manager.persistence.barrier().await.unwrap();
}

async fn transcript_has(manager: &SessionManager, id: Uuid, text: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let found = manager
                .store
                .lock()
                .await
                .load_events(id)
                .unwrap_or_default()
                .iter()
                .any(|event| event.content.contains(text));
            if found {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

/// #2956 / #2985: the seat's sandbox custody root is live and owned by a LATER
/// rotation of the seat (the incident's shape). A reseat still gives the
/// successor a sandbox AND a live custody root it owns, bound in its launch
/// transaction. The successor's first turn completes, then a scheduled
/// `mode:resume` wake and a hub message to the manager seat each reach it with
/// no `sandbox_custody:*` refusal, and neither disturbs the successor's root
/// nor the later rotation's.
///
/// Not covered: `AgentSendMessage` agent mail (it queues for the target and
/// never calls the continuation itself; the satellite hub-message path above is
/// the manager-message route that failed with `continuation_failed` in the
/// incident) and a real provider process (the turns are scripted).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reseated_sandboxed_seat_gets_a_live_custody_root_it_owns() {
    let world = ReseatWorld::sandboxed(SessionStatus::Completed).await;
    let rotation = move_seat_custody_to_a_later_rotation(&world).await;
    let rotation_root = live_root_facts(&world.manager, rotation).await;
    assert_eq!(rotation_root.owner, rotation);
    assert_eq!(rotation_root.allocation_session, world.seat);
    assert_eq!(rotation_root.generation, 2, "the transfer advanced it");
    assert!(
        world
            .manager
            .store
            .lock()
            .await
            .live_custody_for_session(world.seat)
            .is_err(),
        "the seat no longer owns its root: a later rotation does"
    );

    let params = world.params(true);
    let (successor, first_turn) = world.script_successor(&params);
    let result = world
        .manager
        .reseat_harness_manager(params)
        .await
        .expect("a parked sandboxed seat is reseated");
    assert_eq!(result.successor_session_id, successor);

    let own_root = live_root_facts(&world.manager, successor).await;
    {
        let store = world.manager.store.lock().await;
        let new_seat = store.get_session(successor).unwrap().unwrap();
        assert_eq!(new_seat.continued_from, Some(world.seat));
        assert_eq!(new_seat.sandbox_kind, Some(SandboxKind::GitWorktree));
        assert_eq!(
            Some(std::path::PathBuf::from(&own_root.sandbox_root)),
            new_seat.sandbox_root
        );
        let predecessor = store.get_session(world.seat).unwrap().unwrap();
        assert_ne!(predecessor.sandbox_root, new_seat.sandbox_root);
    }
    assert_eq!(own_root.owner, successor);
    assert_eq!(own_root.allocation_session, successor);
    assert_eq!(own_root.generation, 1);
    assert_ne!(own_root.custody_id, rotation_root.custody_id);

    // The successor's first turn completes.
    finish_scripted_turn(&world.manager, successor, &first_turn).await;
    assert_eq!(live_root_facts(&world.manager, successor).await, own_root);

    // (a) A scheduled `mode:resume` wake for the successor fires and resumes it.
    const WAKE: &str = "scheduled wake for the reseated seat";
    let job = build_agent_scheduled_job(ScheduleWakeRequest {
        message: WAKE.into(),
        in_seconds: Some(60),
        at: None,
        name: None,
        every_seconds: None,
        mode: Some("resume".into()),
        working_dir: world._repo.path().to_path_buf(),
        provider: Some(SessionProvider::Claude),
        model: Some("reseat-scripted-provider".into()),
        project_id: None,
        origin_session_id: Some(successor),
        watch_session_id: None,
    })
    .expect("a resume wake for the successor");
    world
        .manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&job)
        .unwrap();
    give_provider_session(&world.manager, successor).await;
    let wake_turn = install_controller_candidate_test_process(successor);
    let launcher: Arc<dyn SessionLauncher> = world.manager.clone();
    crate::scheduler::fire_job_for_test(
        &world.manager.store,
        world.manager.event_bus(),
        &launcher,
        &job,
    )
    .await;
    assert!(
        transcript_has(&world.manager, successor, WAKE).await,
        "the wake text reached the successor"
    );
    assert_eq!(
        wake_turn
            .productive_start_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the wake resumed the successor's provider once"
    );
    {
        let store = world.manager.store.lock().await;
        let settled = store.get_scheduled_job(&job.id).unwrap().unwrap();
        assert!(
            settled.last_fired_at.is_some(),
            "the delivered one-shot is stamped, not retained with a refusal"
        );
        assert_eq!(store.continuation_retry(job.id).unwrap(), None);
    }
    assert_eq!(live_root_facts(&world.manager, successor).await, own_root);
    finish_scripted_turn(&world.manager, successor, &wake_turn).await;

    // (b) A hub message addressed to the seat's declared id is delivered to the
    // current appointed manager, the successor, with its own turn.
    const MESSAGE: &str = "please rebase the reseated seat";
    give_provider_session(&world.manager, successor).await;
    let message_turn = install_controller_candidate_test_process(successor);
    let delivered = crate::satellite::delivery::deliver_hub_message(
        &world.manager,
        SatelliteDeliverRequestV1 {
            wire_version: rsi_common::satellite::SATELLITE_WIRE_VERSION_V1,
            message_id: Uuid::new_v4(),
            hub_installation_id: SatelliteUuidV1(world.hub),
            sender_label: "hub manager".into(),
            sender_session_id: Uuid::new_v4(),
            remote_session_id: SatelliteUuidV1(world.seat),
            scope_project_id: None,
            message: MESSAGE.into(),
        },
    )
    .await
    .expect("the hub message reaches the reseated seat with no custody refusal");
    assert_eq!(delivered.outcome, SatelliteDeliverOutcomeV1::Delivered);
    assert!(
        transcript_has(&world.manager, successor, MESSAGE).await,
        "the message text reached the successor"
    );
    assert_eq!(
        message_turn
            .productive_start_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the message resumed the successor's provider once"
    );
    finish_scripted_turn(&world.manager, successor, &message_turn).await;

    // Neither continuation touched either root.
    assert_eq!(live_root_facts(&world.manager, successor).await, own_root);
    assert_eq!(
        live_root_facts(&world.manager, rotation).await,
        rotation_root
    );
    let continued = {
        let mut store = world.manager.store.lock().await;
        let new_seat = store.get_session(successor).unwrap().unwrap();
        CustodyService::authorize_live(
            &new_seat,
            &mut store,
            world.manager.sandbox_allocator.base_dir(),
            rsi_common::types::SandboxCustodyTransitionV1::Continue,
        )
        .map(|_| ())
    };
    assert!(continued.is_ok(), "{:?}", continued.err());
}
