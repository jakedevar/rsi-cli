//! #578: reserved launches retain unknown roots and retry with fresh custody.
use super::*;
use rsi_common::harness_manager_v2::ManagerPolicyV2;
use rsi_common::rpc::{ProviderRateLimitSnapshot, ProviderRateLimitWindow};
use rsi_common::types::SandboxSpec;
// Reuse the daemon's existing scratch guards without linking the repository
// tool crate: that crate enables serde_json/preserve_order, which changes
// unrelated topology digests in this test binary.
macro_rules! rsi_common_manifest_dir {
    () => {
        rsi_common::manifest_dir!()
    };
}
#[allow(dead_code)]
mod git_fixture {
    include!("../../harness/tools/custody_scan_fixtures.rs");
}
use git_fixture::git;

struct World {
    manager: SessionManager,
    repo: TempDir,
    base: TempDir,
    _db: TempDir,
    source: String,
}

impl World {
    fn new() -> Self {
        let repo = git_fixture::scratch_tempdir();
        let base = disk_backed_tempdir("sandbox-retry-base-");
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "fixture@example.invalid"],
            &["config", "user.name", "Retry Fixture"],
        ] {
            git(repo.path(), args);
        }
        std::fs::write(repo.path().join("README.md"), "pinned source\n").unwrap();
        git(repo.path(), &["add", "README.md"]);
        git(repo.path(), &["commit", "-qm", "source"]);
        let source = git(repo.path(), &["rev-parse", "HEAD"]);
        let db = disk_backed_tempdir("sandbox-retry-db-");
        let store = Store::open_test_fixture(&db.path().join("rsi.db")).unwrap();
        let config = RuntimeConfig::from_config(&Config::from_env());
        config.sandbox_min_free_gib.store(0, Ordering::Relaxed);
        let manager = SessionManager::new(
            Arc::new(EventBus::new(16)),
            store,
            false,
            db.path().join("daemon.sock"),
            None,
            Vec::new(),
            config,
            base.path().to_path_buf(),
        )
        .unwrap();
        Self {
            manager,
            repo,
            base,
            _db: db,
            source,
        }
    }

    fn purpose(&self, session: Uuid) -> LaunchPurpose {
        LaunchPurpose::TopologyNode(crate::session::types::TopologyNodeLaunchContext {
            session_id: session,
            fork: crate::topology::custody::TopologyForkSource::for_test(
                self.repo.path().to_path_buf(),
                &self.source,
            ),
        })
    }

    fn config(&self, session: Uuid) -> LaunchConfig {
        let mut config = direct_interactive_test_config(self.repo.path().to_path_buf());
        config.session_kind = Some(SessionKind::Task);
        config.skip_context_pipeline = true;
        config.sandbox = Some(SandboxSpec {
            kind: Some(SandboxKind::GitWorktree),
            branch: None,
        });
        config.model_invocation_purpose = ModelInvocationPurpose::WorkflowGraphNode;
        config.model_invocation_dedup_key = Some(format!("sandbox-retry:{session}"));
        config
    }

    async fn allocation(&self, id: Uuid) -> SandboxAllocation {
        self.manager
            .allocate_launch_sandbox(&self.config(id), &self.purpose(id), id, self.repo.path())
            .await
            .unwrap()
            .0
            .unwrap()
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn reserved_launch_preserves_unverified_clean_dirty_and_foreign_roots() {
    for case in ["unverified", "clean", "dirty", "foreign"] {
        let world = World::new();
        let id = Uuid::new_v4();
        let root = world.base.path().join(id.to_string());
        let foreign = git_fixture::scratch_tempdir();
        if case == "unverified" {
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("README.md"), "unverified evidence\n").unwrap();
        } else {
            let origin = if case == "foreign" {
                git(
                    foreign.path(),
                    &["clone", "-q", world.repo.path().to_str().unwrap(), "."],
                );
                foreign.path()
            } else {
                world.repo.path()
            };
            world
                .manager
                .sandbox_allocator
                .allocate(id, origin, SandboxKind::GitWorktree, "HEAD", None)
                .unwrap();
            if case == "dirty" {
                std::fs::write(root.join("README.md"), "dirty evidence\n").unwrap();
                std::fs::write(root.join("untracked"), "keep\n").unwrap();
            }
        }
        let content = std::fs::read(root.join("README.md")).unwrap();
        std::fs::write(world.repo.path().join("README.md"), "later source\n").unwrap();
        git(world.repo.path(), &["commit", "-qam", "advance source"]);
        let registration = git(world.repo.path(), &["worktree", "list", "--porcelain"]);
        let allocation = world.allocation(id).await;
        assert_eq!(allocation.origin, world.repo.path());
        assert_eq!(
            std::fs::read_to_string(allocation.root.join("README.md")).unwrap(),
            "pinned source\n"
        );
        assert_eq!(
            crate::sandbox::git_worktree::observe_clean_head_bounded(&allocation.root)
                .unwrap()
                .1,
            world.source
        );
        assert_eq!(
            std::fs::read(root.join("README.md")).unwrap(),
            content,
            "{case}"
        );
        let roots = git(world.repo.path(), &["worktree", "list", "--porcelain"]);
        for original in registration.split("\n\n") {
            assert!(
                roots.contains(original),
                "original registrations retained: {case}"
            );
        }
        assert!(
            allocation.root != root,
            "a fresh reservation owns the retry: {case}"
        );
        if case == "dirty" {
            assert_eq!(
                std::fs::read_to_string(root.join("untracked")).unwrap(),
                "keep\n"
            );
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn provider_window_holds_allocate_nothing_then_reserved_retry_binds_once() {
    let world = World::new();
    let id = Uuid::new_v4();
    let old = world.allocation(id).await; // Retained pre-admission allocation from an earlier daemon.
    let (scope, _) = crate::store::manager_resources::tests::fixture(
        &*world.manager.store.lock().await,
        ManagerPolicyV2::default(),
    );
    let mut config = world.config(id);
    config.project_id = Some(scope.project_id);
    config.parent_id = Some(scope.epic_ids[0]);
    let now = chrono::Utc::now();
    let snapshot = |held| ProviderRateLimitSnapshot {
        provider: SessionProvider::Local,
        status: None,
        rate_limit_type: None,
        overage_status: None,
        is_using_overage: false,
        observed_at: now,
        windows: vec![ProviderRateLimitWindow {
            window_key: "test-window".into(),
            utilization: if held { 1.0 } else { 0.0 },
            resets_at_epoch: Some((now + chrono::Duration::hours(1)).timestamp()),
        }],
        stale_reason: None,
    };
    world
        .manager
        .store
        .lock()
        .await
        .upsert_provider_rate_limit_snapshot(&snapshot(true), None)
        .unwrap();
    for _ in 0..2 {
        let error = world
            .manager
            .boxed_launch_session_with_retry_admission(
                config.clone(),
                None,
                false,
                world.purpose(id),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(crate::provider_exhaustion::PROVIDER_USAGE_WINDOW_REFUSAL_CODE),
            "{error}"
        );
        assert_eq!(std::fs::read_dir(world.base.path()).unwrap().count(), 1);
    }
    world
        .manager
        .store
        .lock()
        .await
        .upsert_provider_rate_limit_snapshot(&snapshot(false), None)
        .unwrap();
    let scripted = install_controller_candidate_test_process(id);
    assert_eq!(world.manager.boxed_launch_session_with_retry_admission(
        config, None, false, world.purpose(id), None,
    ).await.unwrap(), id);
    let store = world.manager.store.lock().await;
    let session = store.get_session(id).unwrap().unwrap();
    let custody = store.live_custody_for_session(id).unwrap();
    assert_eq!(custody.owner_session_id, id);
    assert_eq!(custody.source_commit, world.source);
    assert_eq!(
        session.sandbox_root,
        Some(PathBuf::from(&custody.sandbox_root))
    );
    assert!(PathBuf::from(&custody.sandbox_root) != old.root);
    assert_eq!(
        std::fs::read_to_string(old.root.join("README.md")).unwrap(),
        "pinned source\n"
    );
    let admissions: i64 = store.conn.query_row(
        "SELECT count(*) FROM model_invocations WHERE session_id=?1 AND admission_status='admitted'",
        [id.to_string()], |row| row.get(0),
    ).unwrap();
    assert_eq!(admissions, 1);
    drop(store);
    assert!(scripted.alive.load(Ordering::SeqCst));
    h1_7g_cleanup_child(&world.manager, id).await;
    drop_controller_candidate_test_stream(id);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn manager_create_retry_keeps_reserved_identity_and_occupied_branch_policy() {
    use rsi_common::harness_manager_v2::{
        AgentManagerControlRequestV2, ManagerActionV2, ManagerCapabilityV2, ManagerFenceV2,
        ManagerLaunchChoiceV2, ManagerOperatingModeV2, ManagerSandboxSourceV1,
    };
    let world = World::new();
    let launch = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Local,
        // Use the exact test-only registry entry, as the manager launcher
        // fixtures do. An unregistered fake depends on the host's Local
        // catalog being empty and fails when it lists unrelated real models.
        model: crate::session::provider_model_validation::SCRIPTED_TEST_MODEL.into(),
        effort: None,
    };
    let (scope, _) = crate::store::manager_resources::tests::fixture(
        &*world.manager.store.lock().await,
        ManagerPolicyV2 {
            mode: ManagerOperatingModeV2::Execute,
            capabilities: vec![ManagerCapabilityV2::SessionCreate],
            max_created_sessions: 1,
            allowed_launches: vec![launch.clone()],
            ..Default::default()
        },
    );
    world
        .manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE projects SET path=?2 WHERE id=?1",
            rusqlite::params![
                scope.project_id.to_string(),
                world.repo.path().to_str().unwrap()
            ],
        )
        .unwrap();
    // Manager admission freezes the parent container's checkout, independently
    // of the project's default path.
    world
        .manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET working_dir=?2 WHERE id=?1",
            rusqlite::params![
                scope.epic_ids[0].to_string(),
                world.repo.path().to_str().unwrap()
            ],
        )
        .unwrap();
    let request = AgentManagerControlRequestV2::new(
        ManagerFenceV2 {
            scope_version: scope.row_version,
            policy_version: 1,
        },
        "sandbox-retry-manager".into(),
        ManagerActionV2::CreateSession {
            parent_id: scope.epic_ids[0],
            kind: SessionKind::Task,
            query: "reserved create retry".into(),
            launch,
            sandbox_source: Some(ManagerSandboxSourceV1::Commit(world.source.clone())),
        },
    );
    let receipt = world
        .manager
        .agent_control()
        .agent_manager_control(scope.manager_session_id, request.clone())
        .await
        .unwrap();
    let id = receipt.target_session_id.unwrap();
    let old = world.allocation(id).await;
    let claim = world
        .manager
        .store
        .lock()
        .await
        .claim_manager_action(world.manager.program_run_boot_id)
        .unwrap()
        .unwrap();
    let fork = {
        let mut source = test_session(Uuid::new_v4());
        source.working_dir = world.repo.path().to_path_buf();
        world
            .manager
            .store
            .lock()
            .await
            .insert_session(&source)
            .unwrap();
        world
            .manager
            .custody_execution_runtime()
            .prepare_manager_action_fork(&source)
            .await
            .unwrap()
    };
    let purpose = LaunchPurpose::ManagerAction(Box::new(
        crate::session::types::ManagerActionLaunchContext {
            claim: claim.clone(),
            fork,
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        },
    ));
    let mut explicit = world.config(id);
    explicit.sandbox.as_mut().unwrap().branch = old.branch.clone();
    let error = world
        .manager
        .allocate_launch_sandbox(&explicit, &purpose, id, world.repo.path())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("sandbox branch already exists"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(old.root.join("README.md")).unwrap(),
        "pinned source\n"
    );

    let scripted = install_controller_candidate_test_process(id);
    world.manager.execute_manager_action(&claim).await.unwrap();
    let replay = world
        .manager
        .agent_control()
        .agent_manager_control(scope.manager_session_id, request)
        .await
        .unwrap();
    assert_eq!(replay.operation_id, receipt.operation_id);
    assert_eq!(replay.target_session_id, Some(id));
    let store = world.manager.store.lock().await;
    let custody = store.live_custody_for_session(id).unwrap();
    assert_eq!(custody.owner_session_id, id);
    assert_eq!(custody.source_commit, world.source);
    assert!(PathBuf::from(custody.sandbox_root) != old.root);
    drop(store);
    assert!(scripted.alive.load(Ordering::SeqCst));
    h1_7g_cleanup_child(&world.manager, id).await;
    drop_controller_candidate_test_stream(id);
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn reserved_launch_preserves_symlink_and_its_target() {
    let world = World::new();
    let id = Uuid::new_v4();
    let target = disk_backed_tempdir("sandbox-retry-target-");
    std::fs::write(target.path().join("evidence"), "keep\n").unwrap();
    let link = world.base.path().join(id.to_string());
    std::os::unix::fs::symlink(target.path(), &link).unwrap();
    let allocation = world.allocation(id).await;
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(target.path().join("evidence")).unwrap(),
        "keep\n"
    );
    assert_eq!(
        crate::sandbox::git_worktree::observe_clean_head_bounded(&allocation.root)
            .unwrap()
            .1,
        world.source
    );
    assert!(allocation.root != link);
}
