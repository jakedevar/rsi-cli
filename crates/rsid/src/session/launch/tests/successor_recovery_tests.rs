use super::*;
use crate::store::successor_reservations::{
    AgentSuccessorReservation, UNPUBLISHED_SUCCESSOR_ADMISSION_DENIED,
    UNPUBLISHED_SUCCESSOR_CLEANUP,
};
use rsi_common::agent_coordination::{AgentReserveSuccessorRequestV1, AgentSuccessorStateV1};
use rsi_common::harness_manager::{ConfigureHarnessManagerRequestV1, HarnessManagerConfigV1};
use rsi_common::harness_manager_v2::{
    ConfigureHarnessManagerPolicyRequestV2, ManagerLaunchChoiceV2, ManagerPolicyV2,
    ManagerProviderLimitV2,
};
use rsi_common::model_control::ModelInvocationStatus;

struct InterruptedSuccessor {
    manager: SessionManager,
    dir: TempDir,
    _sandbox: TempDir,
    _repo: TempDir,
    scope: HarnessManagerConfigV1,
    lead: Session,
    reservation: AgentSuccessorReservation,
    process: ControllerCandidateTestProcess,
    root: PathBuf,
}

async fn interrupted_successor() -> InterruptedSuccessor {
    let (manager, dir, sandbox) = manager();
    let repo = tempfile::tempdir().unwrap();
    init_d00_git_repo(repo.path());
    let (scope, mut lead) = crate::store::manager_resources::tests::fixture(
        &*manager.store.lock().await,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            provider_limits: vec![ManagerProviderLimitV2 {
                provider: SessionProvider::Claude,
                max_active: 1,
            }],
            ..Default::default()
        },
    );
    lead.working_dir = repo.path().to_path_buf();
    lead.model = Some("claude-sonnet-4-6".into());
    lead.is_eval = true;
    {
        let store = manager.store.lock().await;
        store.update_session_metadata(&lead).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET working_dir=?2,is_eval=1 WHERE id=?1",
                rusqlite::params![lead.id.to_string(), repo.path().to_string_lossy()],
            )
            .unwrap();
    }
    manager
        .completed
        .write()
        .await
        .insert(lead.id, CompletedSession::for_test(lead.clone()));
    let receipt = manager
        .agent_control()
        .agent_reserve_successor(
            lead.id,
            AgentReserveSuccessorRequestV1 {
                kind: SessionKind::Task,
                model: None,
                effort: None,
                query: "successor interrupted before publication".into(),
                topology_node: None,
                iteration: None,
                tags: None,
                idempotency_key: "unpublished-successor".into(),
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
    let mut launch = Box::pin(manager.reconcile_agent_successor(receipt.reservation_id));
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::select! {
            result = &mut launch => panic!("launch finished before admission seam: {result:?}"),
            result = admitted => result.unwrap(),
        }
    })
    .await
    .unwrap();
    drop(launch); // Actual service future cancellation, not a forged Store row.
    drop(publish);
    let store = manager.store.lock().await;
    let reservation = store
        .get_agent_successor(receipt.reservation_id)
        .unwrap()
        .unwrap();
    assert_eq!(reservation.state, AgentSuccessorStateV1::Launching);
    assert!(store.get_session(id).unwrap().is_none());
    assert_eq!(
        store
            .load_model_invocation_record(reservation.model_invocation_id.unwrap())
            .unwrap()
            .unwrap()
            .status,
        ModelInvocationStatus::Running
    );
    assert_eq!(
        store.manager_v2_resource_snapshot(&scope).unwrap()["active_sessions"],
        1
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    let root = sandbox.path().join(id.to_string());
    assert!(root.is_dir());
    // A retained root may already contain valuable local edits. Recovery must
    // neither adopt by pathname nor erase this evidence.
    std::fs::write(root.join("retained-evidence.txt"), "keep this allocation\n").unwrap();
    drop(store);
    InterruptedSuccessor {
        manager,
        dir,
        _sandbox: sandbox,
        _repo: repo,
        scope,
        lead,
        reservation,
        process,
        root,
    }
}

async fn assert_settled(
    f: &InterruptedSuccessor,
    current_scope: &HarnessManagerConfigV1,
    current_lead: Uuid,
) {
    let store = f.manager.store.lock().await;
    let row = store
        .get_agent_successor(f.reservation.reservation_id)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, AgentSuccessorStateV1::Failed);
    assert_eq!(
        row.safe_error_class.as_deref(),
        Some(UNPUBLISHED_SUCCESSOR_CLEANUP)
    );
    assert_eq!(row.candidate_session_id, f.reservation.candidate_session_id);
    assert_eq!(row.model_invocation_id, f.reservation.model_invocation_id);
    assert_eq!(row.launch_attempt_id, f.reservation.launch_attempt_id);
    assert_eq!(
        row.expected_lead_generation,
        f.reservation.expected_lead_generation
    );
    assert_eq!(row.request, f.reservation.request);
    assert_eq!(row.request_fingerprint, f.reservation.request_fingerprint);
    assert_eq!(
        store
            .get_session(f.scope.epic_ids[0])
            .unwrap()
            .unwrap()
            .lead_session_id,
        Some(current_lead)
    );
    assert!(
        store
            .get_session(row.candidate_session_id)
            .unwrap()
            .is_none()
    );
    let invocation = store
        .load_model_invocation_record(row.model_invocation_id.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(invocation.status, ModelInvocationStatus::Failed);
    assert_eq!(
        invocation.usage.confidence,
        ModelUsageConfidence::Unavailable
    );
    assert_eq!(invocation.usage.estimated_cost_usd, None);
    let count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM model_invocations WHERE session_id=?1",
            [row.candidate_session_id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "stable invocation is never replaced by retry");
    let resources = store.manager_v2_resource_snapshot(current_scope).unwrap();
    assert_eq!(resources["active_sessions"], 0);
    assert!(
        resources["unknown_spend_observations"].as_u64().unwrap() >= 1,
        "the exact invocation's unavailable cost remains unknown, including across reopen"
    );
    assert_eq!(f.process.productive_start_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read_to_string(f.root.join("retained-evidence.txt")).unwrap(),
        "keep this allocation\n"
    );
    assert!(f.root.join(".git").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_v2_successor_service_replay_at_cap_one_settles_retained_unpublished_root() {
    let f = interrupted_successor().await;
    let inventory = crate::session::reaper::StartupReaperFixture::new();
    let _lease = inventory
        .scoped_runtime_reap_root(f.reservation.candidate_session_id)
        .unwrap();
    let result = f
        .manager
        .reconcile_agent_successor(f.reservation.reservation_id)
        .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains(UNPUBLISHED_SUCCESSOR_CLEANUP)
    );
    assert_settled(&f, &f.scope, f.lead.id).await;
    f.manager
        .reconcile_agent_successor(f.reservation.reservation_id)
        .await
        .unwrap();
    assert_settled(&f, &f.scope, f.lead.id).await;
    drop_controller_candidate_test_process(f.reservation.candidate_session_id);

    // The released slot admits real new service work at the unchanged caps.
    let fresh = Uuid::new_v4();
    let mut config = direct_interactive_test_config(f._repo.path().to_path_buf());
    config.query = format!("work after settled successor {fresh}");
    config.project_id = Some(f.scope.project_id);
    config.parent_id = Some(f.scope.epic_ids[0]);
    config.session_kind = Some(SessionKind::Task);
    fresh_manager_test_ids()
        .lock()
        .unwrap()
        .insert(config.query.clone(), fresh);
    let process = install_controller_candidate_test_process(fresh);
    assert_eq!(f.manager.launch_session(config).await.unwrap(), fresh);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    f.manager.interrupt_session(fresh).await.unwrap();
    drop_controller_candidate_test_stream(fresh);
    tokio::time::timeout(Duration::from_secs(10), async {
        while f.manager.active.read().await.contains_key(&fresh) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    f.manager.persistence.barrier().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_v2_successor_service_replay_denied_admission_settles_without_relaunch() {
    let f = interrupted_successor().await;
    let invocation_id = f.reservation.model_invocation_id.unwrap();
    {
        let store = f.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE model_invocations
                 SET admission_status='denied',status='denied',
                     error_class='orchestration_escalation_denied',
                     completed_at=created_at
                 WHERE id=?1",
                [invocation_id.to_string()],
            )
            .unwrap();
        let uncertain = store
            .settle_agent_successor_uncertain(
                f.reservation.reservation_id,
                f.reservation.state_version,
                f.reservation.launch_attempt_id.unwrap(),
                Uuid::new_v4(),
                "candidate launch returned after model admission denial",
                "agent_successor_launch_uncertain",
            )
            .unwrap();
        assert_eq!(
            uncertain.state,
            rsi_common::agent_coordination::AgentSuccessorStateV1::Uncertain
        );
    }

    let error = f
        .manager
        .reconcile_agent_successor(f.reservation.reservation_id)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains(UNPUBLISHED_SUCCESSOR_ADMISSION_DENIED),
        "{error}"
    );
    {
        let store = f.manager.store.lock().await;
        let reservation = store
            .get_agent_successor(f.reservation.reservation_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            reservation.state,
            rsi_common::agent_coordination::AgentSuccessorStateV1::Failed
        );
        assert_eq!(
            reservation.safe_error_class.as_deref(),
            Some(UNPUBLISHED_SUCCESSOR_ADMISSION_DENIED)
        );
        let invocation = store
            .load_model_invocation_record(invocation_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            invocation.admission_status,
            rsi_common::model_control::AdmissionStatus::Denied
        );
        assert_eq!(invocation.status, ModelInvocationStatus::Denied);
        assert_eq!(
            invocation.error_class.as_deref(),
            Some("orchestration_escalation_denied")
        );
        let count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM model_invocations WHERE session_id=?1",
                [f.reservation.candidate_session_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "denial recovery never mints a replacement invocation"
        );
        assert!(
            store
                .get_session(f.reservation.candidate_session_id)
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(f.process.productive_start_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read_to_string(f.root.join("retained-evidence.txt")).unwrap(),
        "keep this allocation\n"
    );

    f.manager
        .reconcile_agent_successor(f.reservation.reservation_id)
        .await
        .unwrap();
    drop_controller_candidate_test_process(f.reservation.candidate_session_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_v2_successor_service_replay_policy_or_authority_change_never_relaunches() {
    for change in ["pause", "provider", "spend", "scope", "lead"] {
        let f = interrupted_successor().await;
        let mut current_scope = f.scope.clone();
        let mut current_lead = f.lead.id;
        {
            let store = f.manager.store.lock().await;
            match change {
                "scope" => {
                    let mut manager = store
                        .get_session(f.scope.manager_session_id)
                        .unwrap()
                        .unwrap();
                    manager.id = Uuid::new_v4();
                    store.insert_session(&manager).unwrap();
                    current_scope = store
                        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                            group_ids: Vec::new(),
                            project_id: f.scope.project_id,
                            session_id: manager.id,
                            epic_ids: Some(f.scope.epic_ids.clone()),
                            expected_row_version: f.scope.row_version,
                        })
                        .unwrap();
                }
                "lead" => {
                    let mut lead = f.lead.clone();
                    lead.id = Uuid::new_v4();
                    store.insert_session(&lead).unwrap();
                    assert!(
                        store
                            .set_lead_session(f.scope.epic_ids[0], Some(lead.id))
                            .unwrap_err()
                            .to_string()
                            .contains("agent_successor_epic_lead_locked")
                    );
                    // Normal lead edits are fenced by the strong reservation.
                    // Model an out-of-band repair/authority drift in this
                    // disposable DB, as existing provider-effect CAS tests do.
                    store
                        .conn
                        .execute(
                            "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
                            rusqlite::params![f.scope.epic_ids[0].to_string(), lead.id.to_string()],
                        )
                        .unwrap();
                    current_lead = lead.id;
                }
                _ => {
                    let mut grant = store
                        .get_harness_manager_policy(f.scope.project_id)
                        .unwrap()
                        .unwrap();
                    match change {
                        "pause" => grant.policy.paused = true,
                        "provider" => {
                            grant.policy.allowed_launches = vec![ManagerLaunchChoiceV2 {
                                provider: SessionProvider::Codex,
                                model: "gpt-5.5".into(),
                                effort: None,
                            }]
                        }
                        "spend" => grant.policy.max_spend_usd = Some(0.01),
                        _ => unreachable!(),
                    }
                    store
                        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                            project_id: f.scope.project_id,
                            expected_scope_version: f.scope.row_version,
                            expected_policy_version: grant.row_version,
                            idempotency_key: change.into(),
                            policy: grant.policy,
                        })
                        .unwrap();
                }
            }
        }
        let inventory = crate::session::reaper::StartupReaperFixture::new();
        let _lease = inventory
            .scoped_runtime_reap_root(f.reservation.candidate_session_id)
            .unwrap();
        let error = f
            .manager
            .reconcile_agent_successor(f.reservation.reservation_id)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(UNPUBLISHED_SUCCESSOR_CLEANUP),
            "{change}: {error}"
        );
        assert_settled(&f, &current_scope, current_lead).await;
        drop_controller_candidate_test_process(f.reservation.candidate_session_id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_v2_successor_service_cleanup_failure_and_reopen_retain_capacity_until_proven() {
    for failure in ["before_reap", "reaper", "ledger", "after_settlement"] {
        let f = interrupted_successor().await;
        let id = f.reservation.candidate_session_id;
        let inventory = crate::session::reaper::StartupReaperFixture::new();
        let mut orphan = inventory.spawn_runtime_session(id);
        let lease = inventory.scoped_runtime_reap_root(id).unwrap();
        if failure == "reaper" {
            crate::session::reaper::fail_runtime_orphan_reap_for_test(id);
        } else if failure == "ledger" {
            f.manager.store.lock().await.conn.execute_batch(
                "CREATE TEMP TRIGGER reject_successor_settlement BEFORE UPDATE ON model_invocations
                 WHEN NEW.status='failed' BEGIN SELECT RAISE(ABORT,'injected successor settlement failure'); END;"
            ).unwrap();
        }
        if matches!(failure, "before_reap" | "after_settlement") {
            let (reached, resume) = install_controller_candidate_test_pause(
                id,
                if failure == "before_reap" {
                    ControllerCandidateTestPhase::SuccessorUnpublishedBeforeReap
                } else {
                    ControllerCandidateTestPhase::SuccessorUnpublishedAfterSettlement
                },
            );
            let mut reconcile = Box::pin(
                f.manager
                    .reconcile_agent_successor(f.reservation.reservation_id),
            );
            tokio::time::timeout(Duration::from_secs(30), async {
                tokio::select! {
                    result = &mut reconcile => panic!("settled before seam: {result:?}"),
                    result = reached => result.unwrap(),
                }
            })
            .await
            .unwrap();
            drop(reconcile);
            drop(resume);
        } else {
            let error = f
                .manager
                .reconcile_agent_successor(f.reservation.reservation_id)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains(if failure == "reaper" {
                    "injected runtime orphan reap failure"
                } else {
                    "injected successor settlement failure"
                }),
                "{error}"
            );
        }
        if matches!(failure, "before_reap" | "reaper") {
            orphan.assert_alive("failed inventory keeps capacity and live orphan");
        } else {
            orphan.wait_signalled("exact orphan settled before ledger release");
        }
        drop(lease);
        {
            let store = f.manager.store.lock().await;
            let invocation = store
                .load_model_invocation_record(f.reservation.model_invocation_id.unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(
                invocation.status,
                if failure == "after_settlement" {
                    ModelInvocationStatus::Failed
                } else {
                    ModelInvocationStatus::CancellationRequested
                }
            );
            assert_eq!(
                store.manager_v2_resource_snapshot(&f.scope).unwrap()["active_sessions"],
                if failure == "after_settlement" { 0 } else { 1 }
            );
            assert_eq!(
                invocation.error_class.as_deref(),
                Some(UNPUBLISHED_SUCCESSOR_CLEANUP)
            );
        }
        f.manager.persistence.barrier().await.unwrap();
        *f.manager.store.lock().await = Store::open(&f.dir.path().join("rsi.db")).unwrap();
        let _lease = inventory.scoped_runtime_reap_root(id).unwrap();
        let error = f
            .manager
            .reconcile_agent_successor(f.reservation.reservation_id)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(UNPUBLISHED_SUCCESSOR_CLEANUP),
            "{error}"
        );
        if matches!(failure, "before_reap" | "reaper") {
            orphan.wait_signalled("reopen resumes the durable exact cleanup claim");
        }
        assert_settled(&f, &f.scope, f.lead.id).await;
        drop_controller_candidate_test_process(id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_v2_successor_service_cleanup_rejects_changed_admission_identity() {
    let f = interrupted_successor().await;
    f.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE model_invocations SET request_fingerprint='changed' WHERE id=?1",
            [f.reservation.model_invocation_id.unwrap().to_string()],
        )
        .unwrap();
    let error = f
        .manager
        .reconcile_agent_successor(f.reservation.reservation_id)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("cleanup_admission_changed"),
        "{error}"
    );
    let store = f.manager.store.lock().await;
    assert_eq!(
        store.manager_v2_resource_snapshot(&f.scope).unwrap()["active_sessions"],
        1
    );
    assert_eq!(
        store
            .load_model_invocation_record(f.reservation.model_invocation_id.unwrap())
            .unwrap()
            .unwrap()
            .status,
        ModelInvocationStatus::Running
    );
    assert_eq!(f.process.productive_start_count.load(Ordering::SeqCst), 0);
    assert!(f.root.is_dir());
    drop_controller_candidate_test_process(f.reservation.candidate_session_id);
}

// ---- Issues #620 / #398: uncertain master successors never livelock ----

struct UncertainSuccessor {
    manager: SessionManager,
    dir: TempDir,
    sandbox: TempDir,
    repo: TempDir,
    scope: HarnessManagerConfigV1,
    lead: Session,
    reservation_id: Uuid,
    candidate: Uuid,
    root: PathBuf,
}

async fn successor_lead_fixture() -> (
    SessionManager,
    TempDir,
    TempDir,
    TempDir,
    HarnessManagerConfigV1,
    Session,
) {
    let (manager, dir, sandbox) = manager();
    let repo = tempfile::tempdir().unwrap();
    init_d00_git_repo(repo.path());
    let (scope, mut lead) = crate::store::manager_resources::tests::fixture(
        &*manager.store.lock().await,
        ManagerPolicyV2 {
            max_active_sessions: 1,
            provider_limits: vec![ManagerProviderLimitV2 {
                provider: SessionProvider::Claude,
                max_active: 1,
            }],
            ..Default::default()
        },
    );
    lead.working_dir = repo.path().to_path_buf();
    lead.model = Some("claude-sonnet-4-6".into());
    lead.is_eval = true;
    {
        let store = manager.store.lock().await;
        store.update_session_metadata(&lead).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET working_dir=?2,is_eval=1 WHERE id=?1",
                rusqlite::params![lead.id.to_string(), repo.path().to_string_lossy()],
            )
            .unwrap();
    }
    manager
        .completed
        .write()
        .await
        .insert(lead.id, CompletedSession::for_test(lead.clone()));
    (manager, dir, sandbox, repo, scope, lead)
}

async fn reserve(manager: &SessionManager, lead: Uuid, key: &str) -> (Uuid, Uuid) {
    let receipt = manager
        .agent_control()
        .agent_reserve_successor(
            lead,
            AgentReserveSuccessorRequestV1 {
                kind: SessionKind::Task,
                model: None,
                effort: None,
                query: format!("successor {key}"),
                topology_node: None,
                iteration: None,
                tags: None,
                idempotency_key: key.into(),
            },
        )
        .await
        .unwrap();
    (receipt.reservation_id, receipt.candidate_session_id)
}

/// The persisted #620 state: a claimed attempt settled `Uncertain` after the
/// first launch allocated `sandboxes/<candidate>` and was refused before
/// admission, so no candidate row or invocation exists.
async fn uncertain_successor_with_retained_root(branch: Option<&str>) -> UncertainSuccessor {
    let (manager, dir, sandbox, repo, scope, lead) = successor_lead_fixture().await;
    let (reservation_id, candidate) = reserve(&manager, lead.id, "uncertain-retained").await;
    {
        use crate::store::successor_reservations::{
            AgentSuccessorLaunchIds, ClaimAgentSuccessorOutcome,
        };
        let store = manager.store.lock().await;
        let reserved = store.get_agent_successor(reservation_id).unwrap().unwrap();
        let ClaimAgentSuccessorOutcome::Claimed(claimed) = store
            .claim_agent_successor_launch(
                reservation_id,
                reserved.state_version,
                AgentSuccessorLaunchIds {
                    launch_attempt_id: Uuid::new_v4(),
                    model_invocation_id: Uuid::new_v4(),
                    transition_id: Uuid::new_v4(),
                },
            )
            .unwrap()
        else {
            panic!("successor claim was not ready");
        };
        store
            .settle_agent_successor_uncertain(
                reservation_id,
                claimed.state_version,
                claimed.launch_attempt_id.unwrap(),
                Uuid::new_v4(),
                "candidate launch returned before establishment could be proven",
                "agent_successor_launch_uncertain",
            )
            .unwrap();
    }
    let allocation = manager
        .sandbox_allocator
        .allocate(
            candidate,
            &repo.path().canonicalize().unwrap(),
            SandboxKind::GitWorktree,
            "HEAD",
            branch,
        )
        .unwrap();
    assert_eq!(
        allocation.root.file_name().unwrap(),
        candidate.to_string().as_str()
    );
    UncertainSuccessor {
        manager,
        dir,
        sandbox,
        repo,
        scope,
        lead,
        reservation_id,
        candidate,
        root: allocation.root,
    }
}

async fn successor_row(manager: &SessionManager, id: Uuid) -> AgentSuccessorReservation {
    manager
        .store
        .lock()
        .await
        .get_agent_successor(id)
        .unwrap()
        .unwrap()
}

/// Positive unlock proof: the Epic accepts a lead write again and the
/// predecessor is still its lead.
async fn assert_epic_unlocked_with_lead(manager: &SessionManager, epic: Uuid, lead: Uuid) {
    let store = manager.store.lock().await;
    assert_eq!(
        store.get_session(epic).unwrap().unwrap().lead_session_id,
        Some(lead)
    );
    store.set_lead_session(epic, Some(lead)).unwrap();
}

async fn stop_candidate(manager: &SessionManager, id: Uuid) {
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

async fn assert_committed_once(
    manager: &SessionManager,
    f_reservation: Uuid,
    candidate: Uuid,
    epic: Uuid,
    lead: Uuid,
    root: &std::path::Path,
    process: &ControllerCandidateTestProcess,
) {
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    let row = successor_row(manager, f_reservation).await;
    assert_eq!(row.state, AgentSuccessorStateV1::Committed);
    assert_eq!(
        row.candidate_session_id, candidate,
        "same candidate identity"
    );
    let store = manager.store.lock().await;
    assert_eq!(
        store.get_session(epic).unwrap().unwrap().lead_session_id,
        Some(candidate)
    );
    let session = store.get_session(candidate).unwrap().unwrap();
    assert_eq!(session.continued_from, Some(lead));
    assert_eq!(session.sandbox_root.as_deref(), Some(root));
    assert_eq!(
        session.sandbox_branch.as_deref(),
        Some(format!("rsi/{candidate}").as_str())
    );
    let successors: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE continued_from=?1",
            [lead.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(successors, 1, "exactly one successor row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uncertain_successor_with_retained_candidate_sandbox_relaunches_once() {
    let f = uncertain_successor_with_retained_root(None).await;
    let process = install_controller_candidate_test_process(f.candidate);
    let inventory = crate::session::reaper::StartupReaperFixture::new();
    let _lead = inventory.scoped_runtime_reap_root(f.lead.id).unwrap();
    let _candidate = inventory.scoped_runtime_reap_root(f.candidate).unwrap();
    for pass in 0..3 {
        f.manager
            .reconcile_agent_successor(f.reservation_id)
            .await
            .unwrap_or_else(|error| panic!("reconcile pass {pass}: {error}"));
    }
    assert_committed_once(
        &f.manager,
        f.reservation_id,
        f.candidate,
        f.scope.epic_ids[0],
        f.lead.id,
        &f.root,
        &process,
    )
    .await;
    stop_candidate(&f.manager, f.candidate).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uncertain_successor_with_foreign_or_dirty_root_settles_failed_and_unlocks_epic() {
    for case in ["dirty", "foreign_branch"] {
        let branch = format!("other/{}", Uuid::new_v4());
        let f = uncertain_successor_with_retained_root(
            (case == "foreign_branch").then_some(branch.as_str()),
        )
        .await;
        if case == "dirty" {
            std::fs::write(f.root.join("evidence.txt"), "keep\n").unwrap();
        }
        let process = install_controller_candidate_test_process(f.candidate);
        let error = f
            .manager
            .reconcile_agent_successor(f.reservation_id)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(crate::sandbox::git_worktree::RETAINED_SUCCESSOR_ROOT_FOREIGN),
            "{case}: {error}"
        );
        let row = successor_row(&f.manager, f.reservation_id).await;
        assert_eq!(row.state, AgentSuccessorStateV1::Failed, "{case}");
        assert_eq!(
            row.safe_error_class.as_deref(),
            Some(crate::sandbox::git_worktree::RETAINED_SUCCESSOR_ROOT_FOREIGN),
            "{case}"
        );
        assert_epic_unlocked_with_lead(&f.manager, f.scope.epic_ids[0], f.lead.id).await;
        // The retired candidate is terminal: later passes are quiet no-ops.
        f.manager
            .reconcile_agent_successor(f.reservation_id)
            .await
            .unwrap();
        assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
        assert!(f.root.join("README.md").is_file(), "{case}: root retained");
        if case == "dirty" {
            assert_eq!(
                std::fs::read_to_string(f.root.join("evidence.txt")).unwrap(),
                "keep\n"
            );
        }
        drop_controller_candidate_test_process(f.candidate);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uncertain_successor_after_capacity_refusal_settles_failed_without_retry_loop() {
    let (manager, _dir, sandbox, repo, scope, lead) = successor_lead_fixture().await;
    let (reservation_id, candidate) = reserve(&manager, lead.id, "capacity-refusal").await;
    {
        let store = manager.store.lock().await;
        let tx = rusqlite::Transaction::new_unchecked(
            &store.conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .unwrap();
        let stamp = crate::store::harness_manager_v2::now();
        // The live resource quota is 1,024 combined origin/spend rows.
        for n in 0..512_u128 {
            let id = Uuid::from_u128(0x61500000000000000000000000000000 + n).to_string();
            for (kind, payload) in [
                (
                    "resource_launch_origin",
                    serde_json::json!({ "provider": lead.provider }),
                ),
                (
                    "resource_spend",
                    serde_json::json!({ "known_floor_usd": 0.0, "zero_origin": true }),
                ),
            ] {
                store
                    .conn
                    .execute(
                        "INSERT INTO harness_manager_v2_records(project_id,manager_session_id,scope_version,kind,record_key,row_version,payload_json,archived,created_at,updated_at)
                         VALUES(?1,?2,?3,?4,?5,1,?6,0,?7,?7)",
                        rusqlite::params![
                            scope.project_id.to_string(),
                            scope.manager_session_id.to_string(),
                            scope.row_version,
                            kind,
                            id,
                            payload.to_string(),
                            stamp
                        ],
                    )
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let process = install_controller_candidate_test_process(candidate);
    let first = manager
        .reconcile_agent_successor(reservation_id)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        first.contains("manager_v2_resource_record_limit"),
        "{first}"
    );
    for _ in 0..2 {
        manager
            .reconcile_agent_successor(reservation_id)
            .await
            .unwrap();
    }
    let row = successor_row(&manager, reservation_id).await;
    assert_eq!(row.state, AgentSuccessorStateV1::Failed);
    assert_eq!(
        row.safe_error_class.as_deref(),
        Some("manager_v2_resource_record_limit")
    );
    assert_eq!(row.candidate_session_id, candidate);
    assert_epic_unlocked_with_lead(&manager, scope.epic_ids[0], lead.id).await;
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    // The refused attempt's own untouched allocation was reclaimed.
    let root = sandbox.path().join(candidate.to_string());
    assert!(std::fs::symlink_metadata(&root).is_err());
    let branch = Command::new("git")
        .args(["branch", "--list", &format!("rsi/{candidate}")])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(branch.status.success());
    assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), "");
    drop_controller_candidate_test_process(candidate);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cleanup_admission_drift_settles_failed_and_unlocks_epic() {
    let f = interrupted_successor().await;
    let invocation_id = f.reservation.model_invocation_id.unwrap();
    {
        let store = f.manager.store.lock().await;
        store
            .settle_agent_successor_uncertain(
                f.reservation.reservation_id,
                f.reservation.state_version,
                f.reservation.launch_attempt_id.unwrap(),
                Uuid::new_v4(),
                "candidate launch returned before establishment could be proven",
                "agent_successor_launch_uncertain",
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE model_invocations SET request_fingerprint='changed' WHERE id=?1",
                [invocation_id.to_string()],
            )
            .unwrap();
    }
    for pass in 0..3 {
        let result = f
            .manager
            .reconcile_agent_successor(f.reservation.reservation_id)
            .await;
        if pass == 0 {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("cleanup_admission_changed"), "{error}");
        } else {
            result.unwrap();
        }
    }
    let row = successor_row(&f.manager, f.reservation.reservation_id).await;
    assert_eq!(row.state, AgentSuccessorStateV1::Failed);
    assert_eq!(
        row.safe_error_class.as_deref(),
        Some("agent_successor_cleanup_admission_changed")
    );
    assert_eq!(row.candidate_session_id, f.reservation.candidate_session_id);
    assert_epic_unlocked_with_lead(&f.manager, f.scope.epic_ids[0], f.lead.id).await;
    // An unproven admission keeps its capacity and evidence; no relaunch.
    let store = f.manager.store.lock().await;
    assert_eq!(
        store
            .load_model_invocation_record(invocation_id)
            .unwrap()
            .unwrap()
            .status,
        ModelInvocationStatus::Running
    );
    drop(store);
    assert_eq!(f.process.productive_start_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read_to_string(f.root.join("retained-evidence.txt")).unwrap(),
        "keep this allocation\n"
    );
    drop_controller_candidate_test_process(f.reservation.candidate_session_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uncertain_successor_retryable_failure_is_bounded_in_process() {
    let f = uncertain_successor_with_retained_root(None).await;
    // A runtime witness of the candidate is a retryable refusal.
    let mut ghost = f.lead.clone();
    ghost.id = f.candidate;
    f.manager
        .completed
        .write()
        .await
        .insert(f.candidate, CompletedSession::for_test(ghost));
    for attempt in
        1..=crate::session::launch::successor_recovery::AGENT_SUCCESSOR_UNCERTAIN_RETRY_LIMIT
    {
        let error = f
            .manager
            .reconcile_agent_successor(f.reservation_id)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("runtime_witness_exists"), "{error}");
        let expected = if attempt
            < crate::session::launch::successor_recovery::AGENT_SUCCESSOR_UNCERTAIN_RETRY_LIMIT
        {
            AgentSuccessorStateV1::Uncertain
        } else {
            AgentSuccessorStateV1::Failed
        };
        assert_eq!(
            successor_row(&f.manager, f.reservation_id).await.state,
            expected
        );
    }
    let row = successor_row(&f.manager, f.reservation_id).await;
    assert_eq!(
        row.safe_error_class.as_deref(),
        Some(crate::session::launch::successor_recovery::AGENT_SUCCESSOR_RECOVERY_EXHAUSTED)
    );
    assert_epic_unlocked_with_lead(&f.manager, f.scope.epic_ids[0], f.lead.id).await;
    f.manager
        .reconcile_agent_successor(f.reservation_id)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uncertain_successor_with_retained_root_converges_after_restart() {
    let f = uncertain_successor_with_retained_root(None).await;
    let UncertainSuccessor {
        manager,
        dir,
        sandbox,
        repo: _repo,
        scope,
        lead,
        reservation_id,
        candidate,
        root,
    } = f;
    manager.persistence.barrier().await.unwrap();
    drop(manager); // Simulated daemon restart: only durable state survives.
    let restarted = manager_for_disk_fixture(dir.path(), sandbox.path());
    restarted
        .completed
        .write()
        .await
        .insert(lead.id, CompletedSession::for_test(lead.clone()));
    assert_eq!(
        successor_row(&restarted, reservation_id).await.state,
        AgentSuccessorStateV1::Uncertain
    );
    let process = install_controller_candidate_test_process(candidate);
    let inventory = crate::session::reaper::StartupReaperFixture::new();
    let _lead = inventory.scoped_runtime_reap_root(lead.id).unwrap();
    let _candidate = inventory.scoped_runtime_reap_root(candidate).unwrap();
    for _ in 0..2 {
        restarted
            .reconcile_agent_successor(reservation_id)
            .await
            .unwrap();
    }
    assert_committed_once(
        &restarted,
        reservation_id,
        candidate,
        scope.epic_ids[0],
        lead.id,
        &root,
        &process,
    )
    .await;
    stop_candidate(&restarted, candidate).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uncertain_successor_past_durable_deadline_settles_exhausted_after_restart() {
    let f = uncertain_successor_with_retained_root(None).await;
    f.manager.persistence.barrier().await.unwrap();
    let UncertainSuccessor {
        manager,
        dir,
        sandbox,
        scope,
        lead,
        reservation_id,
        candidate,
        ..
    } = f;
    drop(manager);
    let restarted = manager_for_disk_fixture(dir.path(), sandbox.path());
    // Downtime longer than the durable deadline: the fresh process has no
    // in-memory attempts, yet the first failed relaunch settles.
    crate::session::launch::successor_recovery::advance_uncertain_clock_for_test(
        reservation_id,
        crate::session::launch::successor_recovery::AGENT_SUCCESSOR_UNCERTAIN_DEADLINE_SECS + 60,
    );
    let mut ghost = lead.clone();
    ghost.id = candidate;
    restarted
        .completed
        .write()
        .await
        .insert(candidate, CompletedSession::for_test(ghost));
    let error = restarted
        .reconcile_agent_successor(reservation_id)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("runtime_witness_exists"), "{error}");
    let row = successor_row(&restarted, reservation_id).await;
    assert_eq!(row.state, AgentSuccessorStateV1::Failed);
    assert_eq!(
        row.safe_error_class.as_deref(),
        Some(crate::session::launch::successor_recovery::AGENT_SUCCESSOR_RECOVERY_EXHAUSTED)
    );
    assert_epic_unlocked_with_lead(&restarted, scope.epic_ids[0], lead.id).await;
}
