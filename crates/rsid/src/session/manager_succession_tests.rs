// Isolated production Store/service fixtures. Scripted processes are explicit;
// AppServer tests below execute a local protocol script, never a paid model.
use crate::store::manager_successions::{ManagerRootState, ManagerRootSuccession};
use rsi_common::harness_manager::*;
use rsi_common::harness_manager_v2::*;

struct RootWorld {
    manager: SessionManager,
    dir: TempDir,
    _sandbox: TempDir,
    project: Uuid,
    owner: Uuid,
    handoff: ManagerCommittedHandoffV2,
}
impl RootWorld {
    async fn new() -> Self {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_test_writer()
            .try_init();
        let (mut manager, dir, sandbox) = manager();
        manager.context_rotation_enabled = true;
        init_d00_git_repo(dir.path());
        std::fs::write(
            dir.path().join("handoff.md"),
            "Continue the committed manager work.\n",
        )
        .unwrap();
        h1_7g_git(dir.path(), &["add", "handoff.md"]);
        h1_7g_git(dir.path(), &["commit", "-qm", "committed handoff"]);
        let handoff = ManagerCommittedHandoffV2 {
            source_commit: h1_7g_git(dir.path(), &["rev-parse", "HEAD"]),
            relative_path: "handoff.md".into(),
            blob_oid: h1_7g_git(dir.path(), &["rev-parse", "HEAD:handoff.md"]),
        };
        let project = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let mut source = test_session(owner);
        source.session_kind = SessionKind::Standard;
        source.parent_id = None;
        source.project_id = Some(project);
        source.working_dir = dir.path().to_path_buf();
        source.model = Some("gpt-6-astra".into());
        source.effort = Some("high".into());
        source.max_retries = Some(0);
        source.is_eval = false;
        source.rotation_disabled_at = Some(chrono::Utc::now());
        source.title = Some("Root operator manager".into());
        source.description = Some("Manager work retained across models".into());
        source.active_task = Some("Keep the original manager obligation".into());
        source.tags = vec!["manager".into(), "retained".into()];
        let now = chrono::Utc::now();
        {
            let store = manager.store.lock().await;
            store
                .insert_project(&Project {
                    id: project,
                    name: "Root runtime project".into(),
                    path: Some(dir.path().to_path_buf()),
                    description: None,
                    color: Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
            store.insert_session(&source).unwrap();
            store
                .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                    project_id: project,
                    session_id: owner,
                    epic_ids: None,
                    group_ids: vec![],
                    expected_row_version: 0,
                })
                .unwrap();
            store
                .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                    project_id: project,
                    expected_scope_version: 1,
                    expected_policy_version: 0,
                    idempotency_key: "root-grant".into(),
                    policy: ManagerPolicyV2 {
                        mode: ManagerOperatingModeV2::Execute,
                        capabilities: vec![ManagerCapabilityV2::SelfSuccession],
                        max_active_sessions: 1,
                        provider_limits: vec![ManagerProviderLimitV2 {
                            provider: SessionProvider::Codex,
                            max_active: 1,
                        }],
                        max_created_sessions: 4,
                        max_recovery_attempts: 0,
                        ..Default::default()
                    },
                })
                .unwrap();
        }
        manager
            .update_session_tags(owner, source.tags.clone())
            .await
            .unwrap();
        Self {
            manager,
            dir,
            _sandbox: sandbox,
            project,
            owner,
            handoff,
        }
    }
    async fn request(
        &self,
        owner: Uuid,
        key: &str,
        provider: SessionProvider,
        model: &str,
        effort: Option<&str>,
    ) -> AgentManagerControlRequestV2 {
        let store = self.manager.store.lock().await;
        let config = store.get_harness_manager(self.project).unwrap().unwrap();
        let policy = store
            .get_harness_manager_policy(self.project)
            .unwrap()
            .unwrap();
        let expected = match store.manager_succession_preflight(&config).unwrap() {
            ManagerSuccessionPreflightV2::Eligible { observation } => {
                assert_eq!(observation.logical_manager_session_id, self.owner);
                assert_eq!(observation.current_session_id, owner);
                observation.expected
            }
            ManagerSuccessionPreflightV2::Ineligible { denial, .. } => {
                panic!("root manager preflight was denied: {denial:?}")
            }
        };
        AgentManagerControlRequestV2 {
            fence: ManagerFenceV2 {
                scope_version: config.row_version,
                policy_version: policy.row_version,
            },
            idempotency_key: key.into(),
            operation: ManagerActionV2::SucceedManager {
                expected,
                launch: ManagerLaunchChoiceV2 {
                    provider,
                    model: model.into(),
                    effort: effort.map(str::to_string),
                },
                handoff: self.handoff.clone(),
            },
        }
    }
    async fn enqueue(
        &self,
        owner: Uuid,
        request: AgentManagerControlRequestV2,
    ) -> (ManagerActionReceiptV2, ManagerRootSuccession) {
        let receipt = self
            .manager
            .agent_control()
            .agent_manager_control(owner, request)
            .await
            .unwrap();
        let root = self
            .manager
            .store
            .lock()
            .await
            .manager_succession(receipt.operation_id)
            .unwrap()
            .unwrap();
        (receipt, root)
    }
    async fn terminal(&self, owner: Uuid) {
        self.manager
            .store
            .lock()
            .await
            .update_session_status(owner, SessionStatus::Completed)
            .unwrap();
    }
    async fn claim(&self) -> crate::store::manager_actions::ManagerActionClaimV2 {
        self.manager
            .store
            .lock()
            .await
            .claim_manager_action(self.manager.program_run_boot_id)
            .unwrap()
            .unwrap()
    }
    async fn policy(&self, edit: impl FnOnce(&mut ManagerPolicyV2)) {
        let store = self.manager.store.lock().await;
        let config = store.get_harness_manager(self.project).unwrap().unwrap();
        let mut grant = store
            .get_harness_manager_policy(self.project)
            .unwrap()
            .unwrap();
        edit(&mut grant.policy);
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: self.project,
                expected_scope_version: config.row_version,
                expected_policy_version: grant.row_version,
                idempotency_key: format!("edit-{}", grant.row_version),
                policy: grant.policy,
            })
            .unwrap();
    }
    async fn stop_scripted(&self, id: Uuid, process: &ControllerCandidateTestProcess) {
        let mut events = self.manager.event_bus.subscribe();
        process.alive.store(false, Ordering::SeqCst);
        send_controller_candidate_test_event(id, serde_json::from_value(serde_json::json!({"type":"result","subtype":"success","result":"Committed manager handoff complete","is_error":false,"total_cost_usd":0.01,"num_turns":1})).unwrap()).await;
        drop_controller_candidate_test_stream(id);
        let settled = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let active = self.manager.active.read().await.contains_key(&id);
                let durable = {
                    let store = self.manager.store.lock().await;
                    let terminal = store.get_session(id).unwrap().is_some_and(|s| {
                        matches!(
                            s.status,
                            SessionStatus::Completed
                                | SessionStatus::Failed
                                | SessionStatus::Interrupted
                        )
                    });
                    let ledger = store
                        .session_model_invocation_id(id)
                        .unwrap()
                        .and_then(|inv| store.load_model_invocation_record(inv).unwrap())
                        .is_none_or(|inv| {
                            inv.raw_status != "running"
                                && inv.raw_status != "cancellation_requested"
                        });
                    terminal && ledger
                };
                if !active && durable {
                    break;
                }
                let _ = events.recv().await;
            }
        })
        .await;
        self.manager.event_bus.unsubscribe();
        settled.expect("actual monitor metadata and invocation settlement");
    }
}

async fn assert_drain_reserved_without_effect(
    w: &RootWorld,
    original: &ManagerRootSuccession,
    process: &ControllerCandidateTestProcess,
) -> i64 {
    let store = w.manager.store.lock().await;
    let operation = store
        .manager_action_operation(original.operation_id)
        .unwrap()
        .unwrap();
    let root = store
        .manager_succession(original.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        operation.receipt.state,
        ManagerActionStateV2::Queued,
        "a still-draining predecessor must retain the queued handoff"
    );
    assert_eq!(root.state, ManagerRootState::Reserved);
    assert_eq!(
        operation.receipt.outcome.as_deref(),
        Some("awaiting_predecessor_settlement")
    );
    assert_eq!((operation.claim_boot_id, root.claim_boot_id), (None, None));
    assert_eq!(
        (
            root.candidate_session_id,
            root.launch_attempt_id,
            root.model_invocation_id
        ),
        (
            original.candidate_session_id,
            original.launch_attempt_id,
            original.model_invocation_id
        )
    );
    assert_eq!(
        serde_json::to_value(&root.frozen).unwrap(),
        serde_json::to_value(&original.frozen).unwrap()
    );
    assert_eq!(root.authority_epoch, original.authority_epoch);
    assert!(!root.admission_recorded && !root.effect_claimed && !operation.effect_started);
    assert!(
        store
            .load_model_invocation_record(root.model_invocation_id)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_session(root.candidate_session_id)
            .unwrap()
            .is_none()
    );
    let charge: (i64, i64, i64) = store.conn.query_row(
        "SELECT count(*),sum(creation_quantity),sum(recovery_quantity) FROM manager_root_successions", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).unwrap();
    assert_eq!(charge, (1, 1, 0));
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    assert_eq!(process.interrupt_count.load(Ordering::SeqCst), 0);
    let config = store.get_harness_manager(w.project).unwrap().unwrap();
    assert_eq!(config.current_session_id, Some(w.owner));
    assert_eq!(
        store
            .get_harness_manager_policy(w.project)
            .unwrap()
            .unwrap()
            .policy
            .max_recovery_attempts,
        0
    );
    root.row_version
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_drain_deferral_repeats_then_establishes_same_occurrence() {
    let w = RootWorld::new().await;
    let request = w
        .request(
            w.owner,
            "drain-eventual",
            SessionProvider::Codex,
            "gpt-6-astra",
            None,
        )
        .await;
    let (_, root) = w.enqueue(w.owner, request).await;
    let mut source = w
        .manager
        .store
        .lock()
        .await
        .get_session(w.owner)
        .unwrap()
        .unwrap();
    source.status = SessionStatus::Running;
    w.manager
        .active
        .write()
        .await
        .insert(w.owner, TrackedSession::new_for_test(source));
    // Seed the reviewed durable-terminal / retained-active boundary. This does
    // not assert that ordinary monitor completion naturally orders it this way.
    w.terminal(w.owner).await;
    let process = install_controller_candidate_test_process(root.candidate_session_id);
    let mut version = root.row_version;
    for _ in 0..3 {
        let count = w.manager.reconcile_manager_actions_once().await.unwrap();
        assert!(
            (1..=4).contains(&count),
            "bounded reconciler made {count} visits"
        );
        let next = assert_drain_reserved_without_effect(&w, &root, &process).await;
        assert_eq!(next, version + 2 * count as i64);
        version = next;
        assert!(w.manager.active.read().await.contains_key(&w.owner));
        assert!(
            w.manager
                .active
                .read()
                .await
                .get(&root.candidate_session_id)
                .is_none()
        );
    }
    w.manager.active.write().await.remove(&w.owner);
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(10),
            w.manager.reconcile_manager_actions_once()
        )
        .await
        .unwrap()
        .unwrap(),
        1
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    {
        let store = w.manager.store.lock().await;
        let current = store
            .manager_succession(root.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(current.state, ManagerRootState::Committed);
        assert_eq!(
            (
                current.candidate_session_id,
                current.launch_attempt_id,
                current.model_invocation_id
            ),
            (
                root.candidate_session_id,
                root.launch_attempt_id,
                root.model_invocation_id
            )
        );
        assert_eq!(
            store
                .manager_action_operation(root.operation_id)
                .unwrap()
                .unwrap()
                .receipt
                .state,
            ManagerActionStateV2::Succeeded
        );
        assert_eq!(
            store
                .get_harness_manager(w.project)
                .unwrap()
                .unwrap()
                .current_session_id,
            Some(root.candidate_session_id)
        );
        assert_eq!(
            store.get_session(w.owner).unwrap().unwrap().status,
            SessionStatus::Archived
        );
        let invocation_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM model_invocations WHERE session_id=?1",
                [root.candidate_session_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(invocation_count, 1);
        let charge: (i64, i64) = store.conn.query_row("SELECT sum(creation_quantity),sum(recovery_quantity) FROM manager_root_successions", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(charge, (1, 0));
    }
    assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 0);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    w.stop_scripted(root.candidate_session_id, &process).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_drain_deferral_rechecks_policy_after_active_owner_leaves() {
    let w = RootWorld::new().await;
    let request = w
        .request(
            w.owner,
            "drain-policy",
            SessionProvider::Codex,
            "gpt-6-astra",
            None,
        )
        .await;
    let (_, root) = w.enqueue(w.owner, request).await;
    let source = w
        .manager
        .store
        .lock()
        .await
        .get_session(w.owner)
        .unwrap()
        .unwrap();
    w.manager
        .active
        .write()
        .await
        .insert(w.owner, TrackedSession::new_for_test(source));
    w.terminal(w.owner).await;
    let process = install_controller_candidate_test_process(root.candidate_session_id);
    w.manager.reconcile_manager_actions_once().await.unwrap();
    assert_drain_reserved_without_effect(&w, &root, &process).await;
    w.manager.active.write().await.remove(&w.owner);
    w.policy(|p| p.paused = true).await;
    assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 1);
    let store = w.manager.store.lock().await;
    let operation = store
        .manager_action_operation(root.operation_id)
        .unwrap()
        .unwrap();
    let after = store
        .manager_succession(root.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(operation.receipt.state, ManagerActionStateV2::Revoked);
    assert_eq!(
        operation.receipt.outcome.as_deref(),
        Some("manager_v2_policy_changed")
    );
    assert_eq!(after.state, ManagerRootState::Revoked);
    assert_eq!(
        (
            after.candidate_session_id,
            after.launch_attempt_id,
            after.model_invocation_id
        ),
        (
            root.candidate_session_id,
            root.launch_attempt_id,
            root.model_invocation_id
        )
    );
    assert!(!after.admission_recorded && !after.effect_claimed);
    assert!(
        store
            .load_model_invocation_record(root.model_invocation_id)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_session(root.candidate_session_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .get_harness_manager(w.project)
            .unwrap()
            .unwrap()
            .current_session_id,
        Some(w.owner)
    );
    let charge: (i64, i64) = store
        .conn
        .query_row(
            "SELECT sum(creation_quantity),sum(recovery_quantity) FROM manager_root_successions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(charge, (1, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_downgrade_escalation_preserves_dirty_custody_mail_and_policy() {
    for (downgrade_model, downgrade_effort) in [("gpt-6-astra", Some("medium")), ("gpt-5.5", None)]
    {
        let w = RootWorld::new().await;
        let mail = {
            let store = w.manager.store.lock().await;
            let mut group = test_session(Uuid::new_v4());
            group.session_kind = SessionKind::Group;
            group.parent_id = None;
            group.project_id = Some(w.project);
            group.status = SessionStatus::Completed;
            store.insert_session(&group).unwrap();
            let mut epic = group.clone();
            epic.id = Uuid::new_v4();
            epic.session_kind = SessionKind::Epic;
            epic.parent_id = Some(group.id);
            store.insert_session(&epic).unwrap();
            let mut lead = epic.clone();
            lead.id = Uuid::new_v4();
            lead.session_kind = SessionKind::Feature;
            lead.parent_id = Some(epic.id);
            store.insert_session(&lead).unwrap();
            store.set_lead_session(epic.id, Some(lead.id)).unwrap();
            store
                .manager_send(
                    w.owner,
                    &AgentManagerSendRequestV1 {
                        epic_id: epic.id,
                        message: "Pending exact manager exchange".into(),
                        idempotency_key: "root-mail".into(),
                    },
                )
                .unwrap()
        };
        std::fs::write(
            w.dir.path().join("handoff.md"),
            "dirty operator edits retained",
        )
        .unwrap();
        std::fs::write(w.dir.path().join("untracked"), "operator scratch").unwrap();
        let req = w
            .request(
                w.owner,
                "downgrade",
                SessionProvider::Codex,
                downgrade_model,
                downgrade_effort,
            )
            .await;
        let (receipt, root) = w.enqueue(w.owner, req.clone()).await;
        assert_eq!(receipt.state, ManagerActionStateV2::Queued);
        let (replay, replay_root) = w.enqueue(w.owner, req).await;
        assert!(replay.deduplicated);
        assert_eq!(replay_root.candidate_session_id, root.candidate_session_id);
        assert!(
            w.manager
                .store
                .lock()
                .await
                .claim_manager_action(w.manager.program_run_boot_id)
                .unwrap()
                .is_none()
        );
        w.terminal(w.owner).await;
        let script = install_controller_candidate_test_process(root.candidate_session_id);
        let claim = w.claim().await;
        if let Err(error) = w.manager.execute_manager_action(&claim).await {
            panic!(
                "root execute {error:?}, receipt {:?}",
                w.manager
                    .store
                    .lock()
                    .await
                    .manager_action_operation(root.operation_id)
                    .unwrap()
                    .map(|op| op.receipt)
            );
        }
        let first = {
            let store = w.manager.store.lock().await;
            let config = store.get_harness_manager(w.project).unwrap().unwrap();
            assert_eq!(config.manager_session_id, w.owner);
            assert_eq!(config.current_session_id, Some(root.candidate_session_id));
            assert_eq!(config.row_version, 1);
            let policy = store
                .get_harness_manager_policy(w.project)
                .unwrap()
                .unwrap();
            assert_eq!(policy.row_version, 1);
            assert_eq!(policy.policy.max_recovery_attempts, 0);
            let candidate = store
                .get_session(root.candidate_session_id)
                .unwrap()
                .unwrap();
            assert_eq!(candidate.model.as_deref(), Some(downgrade_model));
            assert_eq!(candidate.effort.as_deref(), downgrade_effort);
            assert_eq!(candidate.rotation_depth, 1);
            assert_eq!(candidate.title.as_deref(), Some("Root operator manager"));
            assert_eq!(
                candidate.description.as_deref(),
                Some("Manager work retained across models")
            );
            assert_eq!(
                candidate.active_task.as_deref(),
                Some("Keep the original manager obligation")
            );
            assert_eq!(candidate.tags, vec!["manager", "retained"]);
            assert_eq!(
                candidate.rotation_disabled_at,
                store
                    .get_session(w.owner)
                    .unwrap()
                    .unwrap()
                    .rotation_disabled_at
            );
            let custody = store.live_custody_for_session(candidate.id).unwrap();
            assert_eq!(custody.owner_session_id, candidate.id);
            assert_eq!(custody.allocation_session_id, candidate.id);
            assert_eq!(custody.generation, 1);
            assert_eq!(custody.source_commit, w.handoff.source_commit);
            let inv = store
                .load_model_invocation_record(root.model_invocation_id)
                .unwrap()
                .unwrap();
            assert_eq!(inv.purpose, ModelInvocationPurpose::SessionRotateChild);
            let inbox = store
                .manager_inbox(
                    candidate.id,
                    &AgentManagerInboxRequestV1 {
                        request_id: Some(mail.message_id),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(inbox.messages[0].message, "Pending exact manager exchange");
            assert_eq!(inbox.messages[0].sender_session_id, w.owner);
            let resources = store.manager_v2_resource_snapshot(&config).unwrap();
            assert_eq!(resources["active_sessions"], 1);
            assert!(resources["unknown_spend_observations"].as_u64().unwrap() > 0);
            candidate
        };
        assert_eq!(script.productive_start_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read_to_string(w.dir.path().join("handoff.md")).unwrap(),
            "dirty operator edits retained"
        );
        assert_eq!(
            std::fs::read_to_string(w.dir.path().join("untracked")).unwrap(),
            "operator scratch"
        );
        let old_root = first.sandbox_root.clone().unwrap();
        std::fs::write(old_root.join("local-dirty"), "retained on escalation").unwrap();
        let request = w
            .request(
                first.id,
                "escalate",
                SessionProvider::Codex,
                "gpt-6-astra",
                Some("high"),
            )
            .await;
        let (_, second) = w.enqueue(first.id, request).await;
        w.stop_scripted(first.id, &script).await;
        let script2 = install_controller_candidate_test_process(second.candidate_session_id);
        let next_claim = {
            let store = w.manager.store.lock().await;
            store
                .claim_manager_action(w.manager.program_run_boot_id)
                .unwrap()
                .unwrap_or_else(|| {
                    panic!(
                        "missing escalation claim; predecessor {:?}, receipt {:?}",
                        store.get_session(first.id).unwrap().map(|s| s.status),
                        store
                            .manager_action_operation(second.operation_id)
                            .unwrap()
                            .map(|op| op.receipt)
                    )
                })
        };
        w.manager.execute_manager_action(&next_claim).await.unwrap();
        let store = w.manager.store.lock().await;
        let new = store
            .get_session(second.candidate_session_id)
            .unwrap()
            .unwrap();
        assert_eq!(new.rotation_depth, 2);
        assert_eq!(new.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(new.effort.as_deref(), Some("high"));
        assert_ne!(new.sandbox_root, first.sandbox_root);
        assert_ne!(new.sandbox_branch, first.sandbox_branch);
        assert_eq!(
            store
                .live_custody_for_session(first.id)
                .unwrap()
                .owner_session_id,
            first.id
        );
        assert_eq!(
            store.get_session(first.id).unwrap().unwrap().status,
            SessionStatus::Archived
        );
        assert_eq!(
            store
                .get_harness_manager(w.project)
                .unwrap()
                .unwrap()
                .manager_session_id,
            w.owner
        );
        drop(store);
        assert_eq!(
            std::fs::read_to_string(old_root.join("local-dirty")).unwrap(),
            "retained on escalation"
        );
        w.stop_scripted(new.id, &script2).await;
        let reopened = Store::open(&w.dir.path().join("rsi.db")).unwrap();
        assert_eq!(
            reopened
                .get_harness_manager(w.project)
                .unwrap()
                .unwrap()
                .current_session_id,
            Some(new.id)
        );
        assert_eq!(
            reopened
                .manager_root_resource_origins(w.project, None, 256)
                .unwrap()
                .len(),
            3
        );
    }
}

#[tokio::test]
async fn manager_root_runtime_committed_blob_refuses_mismatch_symlink_and_bounds() {
    let w = RootWorld::new().await;
    for case in 0..4 {
        let mut req = w
            .request(
                w.owner,
                &format!("invalid-{case}"),
                SessionProvider::Codex,
                "gpt-6-astra",
                None,
            )
            .await;
        if let ManagerActionV2::SucceedManager { handoff, .. } = &mut req.operation {
            match case {
                0 => handoff.source_commit = "a".repeat(40),
                1 => handoff.blob_oid = "b".repeat(40),
                2 => handoff.relative_path = "../handoff.md".into(),
                _ => handoff.relative_path = "a".repeat(1025),
            }
        }
        assert!(
            w.manager
                .agent_control()
                .agent_manager_control(w.owner, req)
                .await
                .is_err()
        );
    }
    std::os::unix::fs::symlink("handoff.md", w.dir.path().join("link")).unwrap();
    std::fs::write(w.dir.path().join("large"), vec![b'x'; 256 * 1024 + 1]).unwrap();
    std::fs::write(w.dir.path().join("binary"), [0xff, 0xfe]).unwrap();
    h1_7g_git(w.dir.path(), &["add", "link", "large", "binary"]);
    h1_7g_git(w.dir.path(), &["commit", "-qm", "invalid handoff fixtures"]);
    for path in ["link", "large", "binary"] {
        let handoff = ManagerCommittedHandoffV2 {
            source_commit: h1_7g_git(w.dir.path(), &["rev-parse", "HEAD"]),
            relative_path: path.into(),
            blob_oid: h1_7g_git(w.dir.path(), &["rev-parse", &format!("HEAD:{path}")]),
        };
        assert!(
            crate::sandbox::git_worktree::read_manager_handoff(w.dir.path(), &handoff).is_err()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_policy_changes_before_and_after_effect_stop_exact_candidate() {
    for (phase, expected_starts) in [
        (
            ControllerCandidateTestPhase::FreshChildAfterParentValidation,
            0,
        ),
        (ControllerCandidateTestPhase::FreshChildAfterAdmission, 0),
        (
            ControllerCandidateTestPhase::SuccessorAfterSyncProviderStart,
            1,
        ),
    ] {
        let w = RootWorld::new().await;
        let req = w
            .request(
                w.owner,
                "pause-gap",
                SessionProvider::Codex,
                "gpt-6-astra",
                None,
            )
            .await;
        let (_, root) = w.enqueue(w.owner, req).await;
        w.terminal(w.owner).await;
        let scripted = install_controller_candidate_test_process(root.candidate_session_id);
        let (reached, resume) =
            install_controller_candidate_test_pause(root.candidate_session_id, phase);
        let mut task = Box::pin(w.manager.reconcile_manager_actions_once());
        tokio::select! { result = &mut task => panic!("effect did not reach boundary: {result:?}"), result = reached => result.unwrap() }
        w.policy(|p| p.paused = true).await;
        resume.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            scripted.productive_start_count.load(Ordering::SeqCst),
            expected_starts
        );
        let store = w.manager.store.lock().await;
        let current = store
            .manager_succession(root.operation_id)
            .unwrap()
            .unwrap();
        assert!(matches!(
            current.state,
            ManagerRootState::Failed | ManagerRootState::Blocked | ManagerRootState::Revoked
        ));
        assert_eq!(
            store
                .get_harness_manager(w.project)
                .unwrap()
                .unwrap()
                .current_session_id,
            Some(w.owner)
        );
        let snapshot = store
            .manager_v2_resource_snapshot(&store.get_harness_manager(w.project).unwrap().unwrap())
            .unwrap();
        assert_eq!(snapshot["active_sessions"], 0);
        if let Some(inv) = store
            .load_model_invocation_record(root.model_invocation_id)
            .unwrap()
        {
            assert_ne!(inv.raw_status, "running");
        }
        drop(store);
        if expected_starts == 1 {
            assert!(!scripted.alive.load(Ordering::SeqCst));
        }
        drop_controller_candidate_test_process(root.candidate_session_id);
        drop_controller_candidate_test_stream(root.candidate_session_id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_current_zero_caps_choice_global_disable_and_unknown_spend_refuse() {
    for case in 0..10 {
        let mut w = RootWorld::new().await;
        match case {
            0 => w.policy(|p| p.max_created_sessions = 0).await,
            1 | 8 => {
                if case == 8 {
                    // Isolate the provider ceiling from the shared ceiling.
                    w.policy(|p| p.max_active_sessions = 2).await;
                }
                let store = w.manager.store.lock().await;
                let mut group = test_session(Uuid::new_v4());
                group.project_id = Some(w.project);
                group.session_kind = SessionKind::Group;
                group.parent_id = None;
                group.status = SessionStatus::Completed;
                store.insert_session(&group).unwrap();
                let mut epic = group.clone();
                epic.id = Uuid::new_v4();
                epic.session_kind = SessionKind::Epic;
                epic.parent_id = Some(group.id);
                store.insert_session(&epic).unwrap();
                let mut live = epic.clone();
                live.id = Uuid::new_v4();
                live.parent_id = Some(epic.id);
                live.session_kind = SessionKind::Feature;
                live.status = SessionStatus::Running;
                store.insert_session(&live).unwrap();
            }
            2 => w.policy(|p| p.max_spend_usd = Some(100.0)).await,
            3 => {
                w.policy(|p| {
                    p.allowed_launches = vec![ManagerLaunchChoiceV2 {
                        provider: SessionProvider::Codex,
                        model: "gpt-6-astra".into(),
                        effort: Some("high".into()),
                    }]
                })
                .await
            }
            4 => w.manager.context_rotation_enabled = false,
            5 => {
                w.policy(|p| {
                    p.allowed_launches = vec![ManagerLaunchChoiceV2 {
                        provider: SessionProvider::Codex,
                        model: "gpt-6-astra".into(),
                        effort: Some("high".into()),
                    }]
                })
                .await
            }
            9 => crate::session::reaper::fail_runtime_orphan_reap_for_test(w.owner),
            _ => {}
        }
        let request = w
            .request(
                w.owner,
                "refuse",
                match case {
                    6 => SessionProvider::Local,
                    7 => SessionProvider::Harness,
                    _ => SessionProvider::Codex,
                },
                "gpt-6-astra",
                match case {
                    5 => Some("ultra"),
                    6 | 7 => Some("medium"),
                    _ => None,
                },
            )
            .await;
        let admitted = w
            .manager
            .agent_control()
            .agent_manager_control(w.owner, request)
            .await;
        if let Ok(receipt) = admitted {
            let root = w
                .manager
                .store
                .lock()
                .await
                .manager_succession(receipt.operation_id)
                .unwrap()
                .unwrap();
            let scripted = install_controller_candidate_test_process(root.candidate_session_id);
            w.terminal(w.owner).await;
            w.manager.reconcile_manager_actions_once().await.unwrap();
            assert_eq!(
                scripted.productive_start_count.load(Ordering::SeqCst),
                0,
                "refused succession must not start a candidate (case {case})"
            );
            let store = w.manager.store.lock().await;
            let row = store
                .manager_succession(root.operation_id)
                .unwrap()
                .unwrap();
            assert!(matches!(
                row.state,
                ManagerRootState::Blocked | ManagerRootState::Revoked | ManagerRootState::Failed
            ));
            if case == 9 {
                let action = store
                    .manager_action_operation(root.operation_id)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    action.receipt.outcome.as_deref(),
                    Some("manager_succession_predecessor_unsettled")
                );
                assert!(!row.admission_recorded);
                assert!(!row.effect_claimed);
            }
            assert_eq!(
                store
                    .get_harness_manager(w.project)
                    .unwrap()
                    .unwrap()
                    .current_session_id,
                Some(w.owner)
            );
            drop_controller_candidate_test_process(root.candidate_session_id);
        } else {
            assert!(
                case == 0 || case == 3 || case == 5,
                "zero creation or restricted choice rejects before queue"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_deferred_establishment_rechecks_pause_and_cleans_writer() {
    for case in 0..3 {
        let rejected = case != 0;
        let w = RootWorld::new().await;
        let request = w
            .request(
                w.owner,
                "deferred",
                SessionProvider::CodexAppServer,
                "gpt-6-astra",
                None,
            )
            .await;
        let (_, root) = w.enqueue(w.owner, request).await;
        w.terminal(w.owner).await;
        let binary = write_d03_fake_codex_app_server(w.dir.path());
        let pid_file = w.dir.path().join("provider.pid");
        let script = std::fs::read_to_string(&binary).unwrap();
        let (header, body) = script.split_once('\n').unwrap();
        std::fs::write(
            &binary,
            format!(
                "{header}\nprintf '%s\\n' \"$$\" > '{}'\n{body}",
                pid_file.display()
            ),
        )
        .unwrap();
        install_controller_candidate_test_app_server_binary(root.candidate_session_id, binary);
        let control = SuccessorDeferredProviderTestControl {
            productive_start_count: Arc::new(AtomicUsize::new(0)),
            fail_after_start: false,
        };
        successor_deferred_provider_test_controls()
            .lock()
            .unwrap()
            .insert(root.candidate_session_id, control.clone());
        let (reached, resume) = install_controller_candidate_test_pause(
            root.candidate_session_id,
            ControllerCandidateTestPhase::SuccessorAfterDeferredProviderStart,
        );
        let mut task = Box::pin(w.manager.reconcile_manager_actions_once());
        tokio::select! { result = &mut task => panic!("deferred boundary not reached: {result:?}, receipt {:?}", w.manager.store.lock().await.manager_action_operation(root.operation_id).unwrap().map(|op| op.receipt)), result = reached => result.unwrap() }
        {
            let store = w.manager.store.lock().await;
            assert_eq!(
                store
                    .get_harness_manager(w.project)
                    .unwrap()
                    .unwrap()
                    .current_session_id,
                Some(w.owner)
            );
            let pending = store
                .manager_succession(root.operation_id)
                .unwrap()
                .unwrap();
            assert_eq!(pending.state, ManagerRootState::Executing);
            assert!(pending.effect_claimed);
            assert!(
                store
                    .manager_succession_observation(root.candidate_session_id)
                    .is_err()
            );
        }
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        #[cfg(target_os = "linux")]
        assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
        if case == 1 {
            w.policy(|p| p.paused = true).await;
        } else if case == 2 {
            // Replay the daemon's startup journal fence while the real launch
            // task still owns its guards. Cleanup must defer to that owner,
            // whose subsequent publication then fails the stale boot claim.
            w.manager
                .store
                .lock()
                .await
                .recover_manager_actions_startup(Uuid::new_v4())
                .unwrap();
            assert_eq!(
                w.manager
                    .reconcile_manager_succession_cleanup()
                    .await
                    .unwrap(),
                0
            );
            let reopened = Store::open(&w.dir.path().join("rsi.db")).unwrap();
            let pending = reopened
                .manager_succession(root.operation_id)
                .unwrap()
                .unwrap();
            assert_eq!(pending.state, ManagerRootState::CleanupRequired);
            assert!(pending.effect_claimed);
            assert_eq!(
                reopened
                    .manager_v2_resource_snapshot(
                        &reopened.get_harness_manager(w.project).unwrap().unwrap()
                    )
                    .unwrap()["active_sessions"],
                1
            );
            assert_eq!(
                reopened
                    .manager_action_operation(root.operation_id)
                    .unwrap()
                    .unwrap()
                    .receipt
                    .state,
                ManagerActionStateV2::Uncertain
            );
        }
        resume.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(control.productive_start_count.load(Ordering::SeqCst), 1);
        let store = w.manager.store.lock().await;
        let root_after = store
            .manager_succession(root.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            root_after.state,
            if rejected {
                ManagerRootState::Failed
            } else {
                ManagerRootState::Committed
            }
        );
        assert_eq!(
            store
                .get_harness_manager(w.project)
                .unwrap()
                .unwrap()
                .current_session_id,
            Some(if rejected {
                w.owner
            } else {
                root.candidate_session_id
            })
        );
        drop(store);
        if rejected {
            assert!(
                w.manager
                    .active
                    .read()
                    .await
                    .get(&root.candidate_session_id)
                    .is_none()
            );
            #[cfg(target_os = "linux")]
            assert!(
                !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "checked cleanup reaped the exact real provider"
            );
            let reopened = Store::open(&w.dir.path().join("rsi.db")).unwrap();
            let settled = reopened
                .manager_succession(root.operation_id)
                .unwrap()
                .unwrap();
            assert_eq!(settled.state, ManagerRootState::Failed);
            assert_eq!(settled.model_invocation_id, root.model_invocation_id);
            assert_eq!(
                reopened
                    .load_model_invocation_record(root.model_invocation_id)
                    .unwrap()
                    .unwrap()
                    .usage
                    .estimated_cost_usd,
                None
            );
            assert_eq!(
                reopened
                    .manager_v2_resource_snapshot(
                        &reopened.get_harness_manager(w.project).unwrap().unwrap()
                    )
                    .unwrap()["active_sessions"],
                0
            );
        } else {
            w.manager
                .interrupt_session(root.candidate_session_id)
                .await
                .unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_unpublished_admission_reopen_settles_without_resend_or_spend_reset() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    for revise in [false, true] {
        let w = RootWorld::new().await;
        let request = w
            .request(
                w.owner,
                "interrupted-admission",
                SessionProvider::Codex,
                "gpt-6-astra",
                None,
            )
            .await;
        let (_, root) = w.enqueue(w.owner, request).await;
        w.terminal(w.owner).await;
        let script = install_controller_candidate_test_process(root.candidate_session_id);
        let (reached, resume) = install_controller_candidate_test_pause(
            root.candidate_session_id,
            ControllerCandidateTestPhase::FreshChildAfterAdmission,
        );
        let mut task = Box::pin(w.manager.reconcile_manager_actions_once());
        tokio::select! { result = &mut task => panic!("admission boundary not reached: {result:?}"), result = reached => result.unwrap() }
        {
            let store = w.manager.store.lock().await;
            assert!(
                store
                    .get_session(root.candidate_session_id)
                    .unwrap()
                    .is_none()
            );
            let origin = store
                .manager_root_resource_origins(w.project, None, 256)
                .unwrap();
            assert_eq!(origin.len(), 2);
            let snapshot = store
                .manager_v2_resource_snapshot(
                    &store.get_harness_manager(w.project).unwrap().unwrap(),
                )
                .unwrap();
            assert_eq!(snapshot["active_sessions"], 1);
            store
                .record_manager_root_spend_floor(w.owner, 7.25)
                .unwrap();
            if revise {
                store
                    .request_model_invocation_cancellation(
                        root.model_invocation_id,
                        "interrupted before publication",
                        "test",
                    )
                    .unwrap();
                let current = store.get_harness_manager(w.project).unwrap().unwrap();
                store
                    .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                        project_id: w.project,
                        session_id: w.owner,
                        epic_ids: Some(vec![]),
                        group_ids: vec![],
                        expected_row_version: current.row_version,
                    })
                    .unwrap();
                let current = store.get_harness_manager(w.project).unwrap().unwrap();
                let retained = store.manager_v2_resource_snapshot(&current).unwrap();
                assert_eq!(retained["active_sessions"], 1);
                assert!(retained["known_spend_usd"].as_f64().unwrap() >= 7.25);
            }
        }
        drop(task);
        drop(resume);
        let retained = w
            ._sandbox
            .path()
            .join(root.candidate_session_id.to_string());
        assert!(retained.is_dir());
        let reopened = Store::open(&w.dir.path().join("rsi.db")).unwrap();
        let runtime_config = RuntimeConfig::from_config(&Config::from_env());
        let restarted = SessionManager::new(
            Arc::new(EventBus::new(16)),
            reopened,
            true,
            w.dir.path().join("restart.sock"),
            None,
            vec![],
            runtime_config,
            w._sandbox.path().to_path_buf(),
        )
        .unwrap();
        // Same startup journal entry point as the daemon, followed by the real
        // process-first bounded recovery service; no fake settlement receipt.
        restarted
            .store
            .lock()
            .await
            .recover_manager_actions_startup(restarted.program_run_boot_id)
            .unwrap();
        assert_eq!(
            restarted
                .reconcile_manager_succession_cleanup()
                .await
                .unwrap(),
            1
        );
        let store = restarted.store.lock().await;
        let after = store
            .manager_succession(root.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(after.state, ManagerRootState::Failed);
        assert_eq!(after.model_invocation_id, root.model_invocation_id);
        assert!(!after.effect_claimed);
        let row = store
            .load_model_invocation_record(root.model_invocation_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.raw_status, "failed");
        assert_eq!(row.cancellation_requested_at.is_some(), revise);
        assert_eq!(row.usage.estimated_cost_usd, None);
        let config = store.get_harness_manager(w.project).unwrap().unwrap();
        let snapshot = store.manager_v2_resource_snapshot(&config).unwrap();
        assert_eq!(snapshot["active_sessions"], 0);
        assert!(snapshot["known_spend_usd"].as_f64().unwrap() >= 7.25);
        assert!(snapshot["unknown_spend_observations"].as_u64().unwrap() > 0);
        assert_eq!(config.current_session_id, Some(w.owner));
        assert_eq!(script.productive_start_count.load(Ordering::SeqCst), 0);
        assert!(retained.is_dir());
        drop(store);
        restarted.reconcile_manager_actions_once().await.unwrap();
        assert_eq!(script.productive_start_count.load(Ordering::SeqCst), 0);
        drop_controller_candidate_test_process(root.candidate_session_id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_live_source_scope_epoch_custody_human_recovery_fences() {
    for case in 0..6 {
        let w = RootWorld::new().await;
        let req = w
            .request(
                w.owner,
                "live-fence",
                SessionProvider::Codex,
                "gpt-6-astra",
                None,
            )
            .await;
        let (_, root) = w.enqueue(w.owner, req).await;
        w.terminal(w.owner).await;
        let script = install_controller_candidate_test_process(root.candidate_session_id);
        let (reached, resume) = install_controller_candidate_test_pause(
            root.candidate_session_id,
            ControllerCandidateTestPhase::SuccessorAfterDurablePreEffect,
        );
        let mut task = Box::pin(w.manager.reconcile_manager_actions_once());
        tokio::select! { result = &mut task => panic!("pre-effect not reached, case {case}: {result:?}, {:?}", w.manager.store.lock().await.manager_action_operation(root.operation_id).unwrap().map(|op| op.receipt)), result = reached => result.unwrap() }
        let candidate_root = w
            .manager
            .store
            .lock()
            .await
            .get_session(root.candidate_session_id)
            .unwrap()
            .unwrap()
            .sandbox_root
            .unwrap();
        match case {
            0 => {
                w.manager.store.lock().await.configure_harness_manager(&ConfigureHarnessManagerRequestV1 { project_id: w.project, session_id: w.owner, epic_ids: Some(vec![]), group_ids: vec![], expected_row_version: 1 }).unwrap();
            }
            1 => {
                // Exact persisted epoch race; normal rotation/restore epoch hooks
                // are exercised by the separately run root Store regression.
                w.manager.store.lock().await.conn.execute("UPDATE manager_authority_epochs SET epoch=epoch+1 WHERE project_id=?1", [w.project.to_string()]).unwrap();
            }
            2 => {
                std::fs::write(w.dir.path().join("README.md"), "new committed source\n").unwrap();
                h1_7g_git(w.dir.path(), &["add", "README.md"]); h1_7g_git(w.dir.path(), &["commit", "-qm", "source advances while admission waits"]);
            }
            3 => std::fs::rename(candidate_root.join(".git"), candidate_root.join("retained-git-entry")).unwrap(),
            4 => w.manager.store.lock().await.update_session_pending_question_json(w.owner, Some(r#"{"questions":[{"question":"Choose the next action","header":"Operator","options":[],"multi_select":false}]}"#)).unwrap(),
            _ => {
                let key = crate::store::daemon_settings::c5_autofile_pending_key(w.owner);
                w.manager.store.lock().await.set_daemon_setting(&key, "pending exact predecessor recovery").unwrap();
            }
        }
        resume.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(script.productive_start_count.load(Ordering::SeqCst), 0);
        let store = w.manager.store.lock().await;
        let after = store
            .manager_succession(root.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(after.state, ManagerRootState::Failed, "case {case}");
        assert!(!after.effect_claimed);
        assert_eq!(
            store
                .load_model_invocation_record(root.model_invocation_id)
                .unwrap()
                .unwrap()
                .raw_status,
            "failed"
        );
        assert_eq!(
            store
                .get_harness_manager(w.project)
                .unwrap()
                .unwrap()
                .current_session_id,
            Some(w.owner)
        );
        drop(store);
        if case == 3 {
            std::fs::rename(
                candidate_root.join("retained-git-entry"),
                candidate_root.join(".git"),
            )
            .unwrap();
        }
        assert!(candidate_root.exists());
        drop_controller_candidate_test_process(root.candidate_session_id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_pause_during_initialize_prevents_productive_wire_request() {
    let w = RootWorld::new().await;
    let request = w
        .request(
            w.owner,
            "handshake-pause",
            SessionProvider::CodexAppServer,
            "gpt-6-astra",
            None,
        )
        .await;
    let (_, root) = w.enqueue(w.owner, request).await;
    w.terminal(w.owner).await;
    let binary = write_d03_fake_codex_app_server(w.dir.path());
    let wire = w.dir.path().join("wire.log");
    let script = std::fs::read_to_string(&binary).unwrap().replace(
        "    case \"$request\" in",
        &format!(
            "    printf '%s\\n' \"$request\" >> '{}'\n    case \"$request\" in",
            wire.display()
        ),
    );
    std::fs::write(&binary, script).unwrap();
    install_controller_candidate_test_app_server_binary(root.candidate_session_id, binary);
    let (reached, resume) = install_controller_candidate_test_pause(
        root.candidate_session_id,
        ControllerCandidateTestPhase::ManagerAfterDeferredInitialize,
    );
    let mut task = Box::pin(w.manager.reconcile_manager_actions_once());
    tokio::select! { result = &mut task => panic!("initialize boundary not reached: {result:?}"), result = reached => result.unwrap() }
    w.policy(|p| p.paused = true).await;
    resume.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    let methods: Vec<String> = std::fs::read_to_string(&wire)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["method"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(methods.first().map(String::as_str), Some("initialize"));
    assert!(
        methods
            .iter()
            .all(|m| m == "initialize" || m == "notifications/initialized"),
        "only handshake control messages precede the refused productive request"
    );
    let store = w.manager.store.lock().await;
    assert_eq!(
        store
            .manager_succession(root.operation_id)
            .unwrap()
            .unwrap()
            .state,
        ManagerRootState::Failed
    );
    assert_eq!(
        store
            .manager_v2_resource_snapshot(&store.get_harness_manager(w.project).unwrap().unwrap())
            .unwrap()["active_sessions"],
        0
    );
    assert_eq!(
        store
            .get_harness_manager(w.project)
            .unwrap()
            .unwrap()
            .current_session_id,
        Some(w.owner)
    );
    assert!(
        w.manager
            .active
            .read()
            .await
            .get(&root.candidate_session_id)
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_failed_checked_stop_completes_when_notice_transport_fails() {
    let w = RootWorld::new().await;
    let request = w
        .request(
            w.owner,
            "checked-stop",
            SessionProvider::Codex,
            "gpt-6-astra",
            None,
        )
        .await;
    let (_, root) = w.enqueue(w.owner, request).await;
    w.terminal(w.owner).await;
    let scripted = install_controller_candidate_test_process(root.candidate_session_id);
    let (reached, resume) = install_controller_candidate_test_pause(
        root.candidate_session_id,
        ControllerCandidateTestPhase::ManagerBeforePublication,
    );
    let mut task = Box::pin(w.manager.reconcile_manager_actions_once());
    tokio::select! { result = &mut task => panic!("publication boundary not reached: {result:?}"), result = reached => result.unwrap() }
    {
        let mut active = w.manager.active.write().await;
        let tracked = active.get_mut(&root.candidate_session_id).unwrap();
        let Some(ProviderProcess::Scripted(process)) = tracked.process.as_mut() else {
            panic!("scripted checked-stop seam")
        };
        process.kill_fails = true;
    }
    w.policy(|p| p.paused = true).await;
    resume.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    {
        let store = w.manager.store.lock().await;
        assert_eq!(
            store
                .manager_succession(root.operation_id)
                .unwrap()
                .unwrap()
                .state,
            ManagerRootState::CleanupRequired
        );
        assert_eq!(
            store
                .load_model_invocation_record(root.model_invocation_id)
                .unwrap()
                .unwrap()
                .raw_status,
            "running"
        );
        assert_eq!(
            store
                .manager_v2_resource_snapshot(
                    &store.get_harness_manager(w.project).unwrap().unwrap()
                )
                .unwrap()["active_sessions"],
            1
        );
    }
    assert!(scripted.alive.load(Ordering::SeqCst));
    {
        let mut active = w.manager.active.write().await;
        let tracked = active.get_mut(&root.candidate_session_id).unwrap();
        let Some(ProviderProcess::Scripted(process)) = tracked.process.as_mut() else {
            panic!("scripted checked-stop seam")
        };
        process.kill_fails = false;
    }
    let final_receipt_version = {
        let store = w.manager.store.lock().await;
        let current = store
            .manager_action_operation(root.operation_id)
            .unwrap()
            .unwrap()
            .receipt;
        assert_eq!(current.state, ManagerActionStateV2::Uncertain);
        let final_receipt_version = current.row_version + 1;
        store
            .conn
            .execute_batch(&format!(
                "CREATE TEMP TRIGGER fail_manager_action_notice_transport
                 BEFORE UPDATE OF attention_signature ON harness_manager_watches
                 WHEN NEW.route_kind='manager_action'
                   AND NEW.attention_signature='{final_receipt_version}'
                 BEGIN
                   SELECT RAISE(ABORT,'test transient manager action notice transport failure');
                 END;"
            ))
            .unwrap();
        final_receipt_version
    };
    let candidate_token = "checked-stop-candidate-token".to_string();
    w.manager
        .register_agent_token(candidate_token.clone(), root.candidate_session_id)
        .await;
    assert_eq!(
        w.manager.resolve_agent_token(&candidate_token).await,
        Some(root.candidate_session_id)
    );
    let settled = tokio::time::timeout(
        Duration::from_secs(5),
        w.manager.reconcile_manager_succession_cleanup(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(settled, 1);
    assert!(!scripted.alive.load(Ordering::SeqCst));
    assert!(
        w.manager
            .active
            .read()
            .await
            .get(&root.candidate_session_id)
            .is_none()
    );
    assert_eq!(
        w.manager
            .completed
            .read()
            .await
            .get(&root.candidate_session_id)
            .unwrap()
            .session
            .status,
        SessionStatus::Failed
    );
    assert!(
        w.manager
            .resolve_agent_token(&candidate_token)
            .await
            .is_none()
    );
    {
        let store = w.manager.store.lock().await;
        assert_eq!(
            store
                .manager_v2_resource_snapshot(
                    &store.get_harness_manager(w.project).unwrap().unwrap(),
                )
                .unwrap()["active_sessions"],
            0
        );
        assert_eq!(
            store
                .get_harness_manager(w.project)
                .unwrap()
                .unwrap()
                .current_session_id,
            Some(w.owner)
        );
        assert_eq!(
            store
                .manager_succession(root.operation_id)
                .unwrap()
                .unwrap()
                .state,
            ManagerRootState::Failed
        );
        let pending_final_version: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_action_notice_queue
                 WHERE operation_id=?1 AND operation_row_version=?2
                   AND reconciled_at IS NULL",
                rusqlite::params![root.operation_id.to_string(), final_receipt_version],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending_final_version, 1);
        let final_notice_count: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_notices
                 WHERE kind='action_result' AND subject_id=?1 AND subject_version=?2",
                rusqlite::params![
                    root.operation_id.to_string(),
                    final_receipt_version.to_string()
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(final_notice_count, 0);
        store
            .conn
            .execute_batch("DROP TRIGGER fail_manager_action_notice_transport")
            .unwrap();
        let config = store.get_harness_manager(w.project).unwrap().unwrap();
        store.manager_v2_reconcile_notices(&config).unwrap();
    }
    let store = w.manager.store.lock().await;
    let action_notices: Vec<_> = store
        .manager_inbox(w.owner, &Default::default())
        .unwrap()
        .notices
        .into_iter()
        .filter(|notice| {
            notice.kind == "action_result" && notice.subject_id == root.operation_id.to_string()
        })
        .collect();
    assert_eq!(action_notices.len(), 2);
    assert_eq!(action_notices[0].state["state"], "uncertain");
    assert_eq!(action_notices[1].state["state"], "failed");
    assert_eq!(
        action_notices[1].subject_version,
        final_receipt_version.to_string()
    );
    let pending_action_versions: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_action_notice_queue
             WHERE operation_id=?1 AND reconciled_at IS NULL",
            [root.operation_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending_action_versions, 0);
    assert_eq!(scripted.productive_start_count.load(Ordering::SeqCst), 1);
    drop_controller_candidate_test_stream(root.candidate_session_id);
}
