use super::*;
use rsi_common::agent_coordination::{AgentSpawnChildRequestV1, AgentSpawnStateV1};
use rsi_common::harness_manager::{ConfigureHarnessManagerRequestV1, HarnessManagerConfigV1};
use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};

fn replacement_manager(store: &Store, scope: &HarnessManagerConfigV1) -> Uuid {
    let mut next = store
        .get_session(scope.manager_session_id)
        .unwrap()
        .unwrap();
    next.id = Uuid::new_v4();
    store.insert_session(&next).unwrap();
    next.id
}

async fn scoped_child_request(
    manager: &mut SessionManager,
    repo: &std::path::Path,
    lead: &mut Session,
) -> crate::session::spawn_coordinator::SpawnRequest {
    lead.working_dir = repo.to_path_buf();
    lead.provider = SessionProvider::Claude;
    lead.model = Some("claude-sonnet-4-6".into());
    lead.is_eval = true;
    manager
        .store
        .lock()
        .await
        .update_session_metadata(lead)
        .unwrap();
    manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET working_dir=?2,provider='Claude',is_eval=1 WHERE id=?1",
            rusqlite::params![lead.id.to_string(), repo.to_string_lossy()],
        )
        .unwrap();
    manager
        .completed
        .write()
        .await
        .insert(lead.id, CompletedSession::for_test(lead.clone()));
    let mut receiver = manager.take_spawn_rx().unwrap();
    let outcome = manager
        .agent_control()
        .agent_spawn_child(
            lead.id,
            AgentSpawnChildRequestV1 {
                kind: SessionKind::Task,
                agent_role: None,
                provider: None,
                model: None,
                effort: None,
                query: "actual reserved scoped child".into(),
                topology_node: None,
                iteration: None,
                tags: None,
                idempotency_key: "scoped-child".into(),
            },
        )
        .await;
    let crate::session::agent_verbs::AgentSpawnChildOutcome::Accepted(receipt) = outcome else {
        panic!("child reservation rejected: {outcome:?}");
    };
    let request = receiver.recv().await.unwrap();
    assert_eq!(request.child_session_id, receipt.child_session_id);
    assert_eq!(request.spawn_request_id, receipt.spawn_request_id);
    request
}

fn scoped_fresh_config(
    scope: &HarnessManagerConfigV1,
    repo: &std::path::Path,
    id: Uuid,
) -> LaunchConfig {
    let mut config = direct_interactive_test_config(repo.to_path_buf());
    config.query = format!("scoped fresh competitor {id}");
    config.project_id = Some(scope.project_id);
    config.parent_id = Some(scope.epic_ids[0]);
    config.session_kind = Some(SessionKind::Task);
    fresh_manager_test_ids()
        .lock()
        .unwrap()
        .insert(config.query.clone(), id);
    config
}

async fn stop_scoped_test_process(manager: &SessionManager, id: Uuid) {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_v2_agent_child_and_fresh_interleave_one_shared_unpublished_slot() {
    for child_wins in [false, true] {
        let (mut manager, _dir, _sandbox) = manager();
        let repo = tempfile::tempdir().unwrap();
        init_d00_git_repo(repo.path());
        let (scope, mut lead) = crate::store::manager_resources::tests::fixture(
            &*manager.store.lock().await,
            ManagerPolicyV2 {
                max_active_sessions: 1,
                ..Default::default()
            },
        );
        let request = scoped_child_request(&mut manager, repo.path(), &mut lead).await;
        let child = request.child_session_id;
        let spawn_request = request.spawn_request_id;
        let fresh = Uuid::new_v4();
        let config = scoped_fresh_config(&scope, repo.path(), fresh);
        let child_process = install_controller_candidate_test_process(child);
        let fresh_process = install_controller_candidate_test_process(fresh);
        let (child_ready, child_resume) = install_controller_candidate_test_pause(
            child,
            ControllerCandidateTestPhase::FreshChildAfterParentValidation,
        );
        let (fresh_ready, fresh_resume) = install_controller_candidate_test_pause(
            fresh,
            ControllerCandidateTestPhase::FreshChildAfterParentValidation,
        );
        let (winner, loser) = if child_wins {
            (child, fresh)
        } else {
            (fresh, child)
        };
        let (admitted, publish) = install_controller_candidate_test_pause(
            winner,
            ControllerCandidateTestPhase::FreshChildAfterAdmission,
        );
        let (winner_resume, loser_resume) = if child_wins {
            (child_resume, fresh_resume)
        } else {
            (fresh_resume, child_resume)
        };
        let child_launch = Box::pin(manager.launch_agent_child(request));
        let fresh_launch = Box::pin(manager.launch_session(config));
        let (winner_launch, loser_launch): (
            std::pin::Pin<Box<dyn std::future::Future<Output = Result<Uuid>>>>,
            std::pin::Pin<Box<dyn std::future::Future<Output = Result<Uuid>>>>,
        ) = if child_wins {
            (child_launch, fresh_launch)
        } else {
            (fresh_launch, child_launch)
        };
        let interleave = async {
            child_ready.await.unwrap();
            fresh_ready.await.unwrap();
            winner_resume.send(()).unwrap();
            admitted.await.unwrap();
            let store = manager.store.lock().await;
            assert!(store.get_session(winner).unwrap().is_none());
            let origin = store
                .manager_v2_record(&scope, "resource_launch_origin", &winner.to_string())
                .unwrap()
                .unwrap();
            assert_eq!(origin.epic_id, Some(scope.epic_ids[0]));
            let snapshot = store.manager_v2_resource_snapshot(&scope).unwrap();
            assert_eq!(snapshot["active_sessions"], 1);
            assert_eq!(
                snapshot["active_by_provider"][if child_wins { "Claude" } else { "Local" }],
                1
            );
            assert_eq!(
                child_process.productive_start_count.load(Ordering::SeqCst),
                0
            );
            assert_eq!(
                fresh_process.productive_start_count.load(Ordering::SeqCst),
                0
            );
            drop(store);
            loser_resume.send(()).unwrap();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(30), async {
            let reject_then_publish = async {
                let (result, ()) = tokio::join!(loser_launch, interleave);
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("concurrency_capacity")
                );
                publish.send(()).unwrap();
            };
            tokio::join!(winner_launch, reject_then_publish)
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap(), winner);
        assert_eq!(
            child_process.productive_start_count.load(Ordering::SeqCst),
            usize::from(child_wins)
        );
        assert_eq!(
            fresh_process.productive_start_count.load(Ordering::SeqCst),
            usize::from(!child_wins)
        );
        let store = manager.store.lock().await;
        assert_eq!(
            store
                .get_agent_spawn_request(spawn_request)
                .unwrap()
                .unwrap()
                .state,
            if child_wins {
                AgentSpawnStateV1::Launched
            } else {
                AgentSpawnStateV1::Failed
            }
        );
        assert_eq!(
            store.manager_v2_resource_snapshot(&scope).unwrap()["active_sessions"],
            1
        );
        let admitted: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM model_invocations WHERE session_id IN (?1,?2) AND admission_status='admitted'",
            rusqlite::params![child.to_string(), fresh.to_string()], |row| row.get(0),
        ).unwrap();
        assert_eq!(admitted, 1);
        drop(store);
        drop_controller_candidate_test_process(loser);
        stop_scoped_test_process(&manager, winner).await;
    }
}

#[tokio::test]
async fn manager_v2_agent_child_rechecks_policy_scope_and_reservation_after_preflight() {
    for change in [
        "pause",
        "provider",
        "spend",
        "scope",
        "owner",
        "parent_project",
    ] {
        let (mut manager, _dir, _sandbox) = manager();
        let repo = tempfile::tempdir().unwrap();
        init_d00_git_repo(repo.path());
        let (scope, mut lead) = crate::store::manager_resources::tests::fixture(
            &*manager.store.lock().await,
            ManagerPolicyV2::default(),
        );
        if change == "spend" {
            lead.cost_usd = Some(1.0);
        }
        let request = scoped_child_request(&mut manager, repo.path(), &mut lead).await;
        let id = request.child_session_id;
        let process = install_controller_candidate_test_process(id);
        let (reached, resume) = install_controller_candidate_test_pause(
            id,
            ControllerCandidateTestPhase::FreshChildAfterParentValidation,
        );
        let mutate = async {
            reached.await.unwrap();
            let store = manager.store.lock().await;
            let code = match change {
                "scope" => {
                    store
                        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                            group_ids: Vec::new(),
                            project_id: scope.project_id,
                            session_id: replacement_manager(&store, &scope),
                            epic_ids: Some(scope.epic_ids.clone()),
                            expected_row_version: scope.row_version,
                        })
                        .unwrap();
                    "launch_scope_changed"
                }
                "owner" => {
                    store
                        .conn
                        .execute(
                            "UPDATE sessions SET lead_session_id=NULL WHERE id=?1",
                            [scope.epic_ids[0].to_string()],
                        )
                        .unwrap();
                    "spawn_reservation_changed"
                }
                "parent_project" => {
                    store
                        .conn
                        .execute(
                            "UPDATE sessions SET project_id=NULL WHERE id=?1",
                            [scope.epic_ids[0].to_string()],
                        )
                        .unwrap();
                    "spawn_reservation_changed"
                }
                other => {
                    let mut grant = store
                        .get_harness_manager_policy(scope.project_id)
                        .unwrap()
                        .unwrap();
                    let code = match other {
                        "pause" => {
                            grant.policy.paused = true;
                            "policy_paused"
                        }
                        "provider" => {
                            grant.policy.provider_limits =
                                vec![rsi_common::harness_manager_v2::ManagerProviderLimitV2 {
                                    provider: SessionProvider::Claude,
                                    max_active: 1,
                                }];
                            store
                                .update_session_status(lead.id, SessionStatus::Running)
                                .unwrap();
                            "provider_capacity"
                        }
                        "spend" => {
                            grant.policy.max_spend_usd = Some(0.5);
                            "spend_exhausted"
                        }
                        _ => unreachable!(),
                    };
                    store
                        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                            project_id: scope.project_id,
                            expected_scope_version: scope.row_version,
                            expected_policy_version: grant.row_version,
                            idempotency_key: change.into(),
                            policy: grant.policy,
                        })
                        .unwrap();
                    code
                }
            };
            resume.send(()).unwrap();
            code
        };
        let (result, expected) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(manager.launch_agent_child(request), mutate)
        })
        .await
        .unwrap();
        let error = result.unwrap_err().to_string();
        assert!(error.contains(expected), "{change}: {error}");
        assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
        let store = manager.store.lock().await;
        let admitted: i64 = store.conn.query_row(
            "SELECT COUNT(*) FROM model_invocations WHERE session_id=?1 AND admission_status='admitted'",
            [id.to_string()], |row| row.get(0),
        ).unwrap();
        assert_eq!(admitted, 0);
        drop_controller_candidate_test_process(id);
    }
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_v2_strong_successor_reserves_before_publication_without_early_baton_transfer() {
    let (manager, _dir, _sandbox) = manager();
    let repo = tempfile::tempdir().unwrap();
    init_d00_git_repo(repo.path());
    let (scope, mut lead) = crate::store::manager_resources::tests::fixture(
        &*manager.store.lock().await,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            ..Default::default()
        },
    );
    lead.working_dir = repo.path().to_path_buf();
    lead.model = Some("claude-sonnet-4-6".into());
    lead.is_eval = true;
    manager
        .store
        .lock()
        .await
        .update_session_metadata(&lead)
        .unwrap();
    manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET working_dir=?2,is_eval=1 WHERE id=?1",
            rusqlite::params![lead.id.to_string(), repo.path().to_string_lossy()],
        )
        .unwrap();
    manager
        .completed
        .write()
        .await
        .insert(lead.id, CompletedSession::for_test(lead.clone()));
    let receipt = manager
        .agent_control()
        .agent_reserve_successor(
            lead.id,
            rsi_common::agent_coordination::AgentReserveSuccessorRequestV1 {
                kind: SessionKind::Task,
                model: None,
                effort: None,
                query: "scoped strong successor".into(),
                topology_node: None,
                iteration: None,
                tags: None,
                idempotency_key: "scoped-strong".into(),
            },
        )
        .await
        .unwrap();
    let id = receipt.candidate_session_id;
    let process = install_controller_candidate_test_process(id);
    let (admitted, publish) = install_controller_candidate_test_pause(
        id,
        ControllerCandidateTestPhase::FreshChildAfterAdmission,
    );
    let inventory = crate::session::reaper::StartupReaperFixture::new();
    let _lead_inventory = inventory.scoped_runtime_reap_root(lead.id).unwrap();
    let _candidate_inventory = inventory.scoped_runtime_reap_root(id).unwrap();
    let inspect = async {
        admitted.await.unwrap();
        let store = manager.store.lock().await;
        assert!(store.get_session(id).unwrap().is_none());
        assert_eq!(
            store
                .get_session(scope.epic_ids[0])
                .unwrap()
                .unwrap()
                .lead_session_id,
            Some(lead.id)
        );
        assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
        let origin = store
            .manager_v2_record(&scope, "resource_launch_origin", &id.to_string())
            .unwrap()
            .unwrap();
        let reserved = store
            .get_agent_successor(receipt.reservation_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            origin.payload["invocation_id"],
            reserved.model_invocation_id.unwrap().to_string()
        );
        assert_eq!(
            store.manager_v2_resource_snapshot(&scope).unwrap()["active_sessions"],
            1
        );
        // Replacement of the manager revokes authority, but cannot erase an
        // already admitted unpublished reservation's selected-Epic charge.
        let revised = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: scope.project_id,
                session_id: replacement_manager(&store, &scope),
                epic_ids: Some(scope.epic_ids.clone()),
                expected_row_version: scope.row_version,
            })
            .unwrap();
        let grant = store
            .get_harness_manager_policy(scope.project_id)
            .unwrap()
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: scope.project_id,
                expected_scope_version: revised.row_version,
                expected_policy_version: grant.row_version,
                idempotency_key: "replacement-manager-cap".into(),
                policy: ManagerPolicyV2 {
                    max_active_sessions: 1,
                    ..Default::default()
                },
            })
            .unwrap();
        assert_eq!(
            store.manager_v2_resource_snapshot(&revised).unwrap()["active_sessions"],
            1
        );
        assert!(
            store
                .manager_v2_resource_gate(
                    &revised,
                    Some(scope.epic_ids[0]),
                    SessionProvider::Claude,
                    None
                )
                .unwrap_err()
                .to_string()
                .contains("concurrency_capacity")
        );
        drop(store);
        publish.send(()).unwrap();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(
            manager.reconcile_agent_successor(receipt.reservation_id),
            inspect
        )
    })
    .await
    .unwrap();
    result.unwrap();
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    let store = manager.store.lock().await;
    assert_eq!(
        store
            .get_agent_successor(receipt.reservation_id)
            .unwrap()
            .unwrap()
            .state,
        rsi_common::agent_coordination::AgentSuccessorStateV1::Committed
    );
    assert_eq!(
        store
            .get_session(scope.epic_ids[0])
            .unwrap()
            .unwrap()
            .lead_session_id,
        Some(id)
    );
    assert_eq!(
        store.get_session(id).unwrap().unwrap().continued_from,
        Some(lead.id)
    );
    assert_eq!(
        store.live_custody_for_session(id).unwrap().owner_session_id,
        id
    );
    drop(store);
    stop_scoped_test_process(&manager, id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_v2_fresh_service_launches_reserve_capacity_before_session_publication() {
    let (manager, dir, _sandbox) = manager();
    let (scope, _) = crate::store::manager_resources::tests::fixture(
        &*manager.store.lock().await,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            ..Default::default()
        },
    );
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let config = |id: Uuid| {
        let mut config = direct_interactive_test_config(dir.path().to_path_buf());
        config.query = format!("fresh scoped {id}");
        config.project_id = Some(scope.project_id);
        config.parent_id = Some(scope.epic_ids[0]);
        config.session_kind = Some(SessionKind::Task);
        fresh_manager_test_ids()
            .lock()
            .unwrap()
            .insert(config.query.clone(), id);
        config
    };
    let a = config(first);
    let b = config(second);
    let first_process = install_controller_candidate_test_process(first);
    let second_process = install_controller_candidate_test_process(second);
    let (a_validated, a_resume) = install_controller_candidate_test_pause(
        first,
        ControllerCandidateTestPhase::FreshChildAfterParentValidation,
    );
    let (b_validated, b_resume) = install_controller_candidate_test_pause(
        second,
        ControllerCandidateTestPhase::FreshChildAfterParentValidation,
    );
    let (a_admitted, a_publish) = install_controller_candidate_test_pause(
        first,
        ControllerCandidateTestPhase::FreshChildAfterAdmission,
    );
    let first_launch = manager.launch_session(a);
    let second_launch = manager.launch_session(b);
    let interleave = async {
        a_validated.await.unwrap();
        b_validated.await.unwrap();
        a_resume.send(()).unwrap();
        a_admitted.await.unwrap();
        let store = manager.store.lock().await;
        assert!(store.get_session(first).unwrap().is_none());
        assert_eq!(
            store.manager_v2_resource_snapshot(&scope).unwrap()["active_sessions"],
            1
        );
        drop(store);
        b_resume.send(()).unwrap();
        // Hold publication until the competing launch has completed admission.
    };
    tokio::pin!(first_launch);
    let (a_result, b_result) = tokio::time::timeout(Duration::from_secs(30), async {
        let b = async {
            let (outcome, ()) = tokio::join!(second_launch, interleave);
            assert!(
                outcome
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("concurrency_capacity")
            );
            a_publish.send(()).unwrap();
            outcome
        };
        tokio::join!(&mut first_launch, b)
    })
    .await
    .unwrap();
    assert_eq!(a_result.unwrap(), first);
    assert!(b_result.is_err());
    assert_eq!(
        first_process.productive_start_count.load(Ordering::SeqCst),
        1
    );
    assert_eq!(
        second_process.productive_start_count.load(Ordering::SeqCst),
        0
    );
    drop_controller_candidate_test_process(second);
    manager.interrupt_session(first).await.unwrap();
    drop_controller_candidate_test_stream(first);
    tokio::time::timeout(Duration::from_secs(10), async {
        while manager.active.read().await.contains_key(&first) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    manager.persistence.barrier().await.unwrap();
    assert!(!first_process.alive.load(Ordering::SeqCst));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if manager
                .store
                .lock()
                .await
                .manager_v2_resource_snapshot(&scope)
                .unwrap()["active_sessions"]
                == 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn manager_v2_fresh_service_pause_after_parent_validation_prevents_provider_spawn() {
    let (manager, dir, _sandbox) = manager();
    let (scope, _) = crate::store::manager_resources::tests::fixture(
        &*manager.store.lock().await,
        ManagerPolicyV2::default(),
    );
    let id = Uuid::new_v4();
    let mut config = direct_interactive_test_config(dir.path().to_path_buf());
    config.query = format!("pause scoped {id}");
    config.project_id = Some(scope.project_id);
    config.parent_id = Some(scope.epic_ids[0]);
    config.session_kind = Some(SessionKind::Task);
    fresh_manager_test_ids()
        .lock()
        .unwrap()
        .insert(config.query.clone(), id);
    let process = install_controller_candidate_test_process(id);
    let (reached, resume) = install_controller_candidate_test_pause(
        id,
        ControllerCandidateTestPhase::FreshChildAfterParentValidation,
    );
    let change = async {
        reached.await.unwrap();
        let store = manager.store.lock().await;
        let mut grant = store
            .get_harness_manager_policy(scope.project_id)
            .unwrap()
            .unwrap();
        grant.policy.paused = true;
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: scope.project_id,
                expected_scope_version: scope.row_version,
                expected_policy_version: grant.row_version,
                idempotency_key: "pause-before-admission".into(),
                policy: grant.policy,
            })
            .unwrap();
        resume.send(()).unwrap();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(manager.launch_session(config), change)
    })
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("policy_paused"));
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    assert!(
        manager
            .store
            .lock()
            .await
            .get_session(id)
            .unwrap()
            .is_none()
    );
    drop_controller_candidate_test_process(id);
}
