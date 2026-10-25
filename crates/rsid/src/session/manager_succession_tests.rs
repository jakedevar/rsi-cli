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
        Self::with_rotation_enabled(true).await
    }
    async fn with_rotation_enabled(enabled: bool) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_test_writer()
            .try_init();
        let (mut manager, dir, sandbox) = manager();
        manager.context_rotation_enabled = enabled;
        manager
            .runtime_config
            .update_field("context_rotation_enabled", &serde_json::json!(enabled))
            .unwrap();
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
            // These succession fixtures exercise metered budget floors and unknown usage.
            store
                .set_daemon_setting("claude_billing_mode", "metered_api")
                .unwrap();
            store
                .set_daemon_setting("codex_billing_mode", "metered_api")
                .unwrap();
            crate::store::daemon_settings::persist_runtime_config_field(
                &store,
                &manager.runtime_config,
                "context_rotation_enabled",
            )
            .unwrap();
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
                        allowed_launches: vec![
                            ManagerLaunchChoiceV2 {
                                provider: SessionProvider::Codex,
                                model: "gpt-6-astra".into(),
                                effort: None,
                            },
                            ManagerLaunchChoiceV2 {
                                provider: SessionProvider::Codex,
                                model: "gpt-6-astra".into(),
                                effort: Some("high".into()),
                            },
                            ManagerLaunchChoiceV2 {
                                provider: SessionProvider::Codex,
                                model: "gpt-6-astra".into(),
                                effort: Some("medium".into()),
                            },
                            ManagerLaunchChoiceV2 {
                                provider: SessionProvider::Codex,
                                model: "gpt-5.5".into(),
                                effort: None,
                            },
                            ManagerLaunchChoiceV2 {
                                provider: SessionProvider::CodexAppServer,
                                model: "gpt-6-astra".into(),
                                effort: None,
                            },
                            // These two cases isolate the provider backstop,
                            // after launch-policy admission has succeeded.
                            ManagerLaunchChoiceV2 {
                                provider: SessionProvider::Local,
                                model: "gpt-6-astra".into(),
                                effort: Some("medium".into()),
                            },
                            ManagerLaunchChoiceV2 {
                                provider: SessionProvider::Harness,
                                model: "gpt-6-astra".into(),
                                effort: Some("medium".into()),
                            },
                        ],
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
    async fn set_live_rotation_enabled(&self, enabled: bool) {
        // Match UpdateDaemonConfig's live-first, persistence-second ordering.
        self.manager
            .runtime_config
            .update_field("context_rotation_enabled", &serde_json::json!(enabled))
            .unwrap();
        let store = self.manager.store.lock().await;
        crate::store::daemon_settings::persist_runtime_config_field(
            &store,
            &self.manager.runtime_config,
            "context_rotation_enabled",
        )
        .unwrap();
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
            ..AgentManagerControlRequestV2::new(
                ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: policy.row_version,
                },
                key.into(),
                ManagerActionV2::SucceedManager {
                    expected,
                    launch: ManagerLaunchChoiceV2 {
                        provider,
                        model: model.into(),
                        effort: effort.map(str::to_string),
                    },
                    handoff: self.handoff.clone(),
                },
            )
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

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_live_rotation_enable_after_disabled_startup_establishes_once() {
    // The deadline bounds the scenario, not fixture construction: building the
    // Store is CPU-bound and contends on SQLite's process-wide allocator mutex
    // when many tests start together (#1095).
    let w = RootWorld::with_rotation_enabled(false).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        // Overview readiness names the denial the admission below gives.
        {
            let store = w.manager.store.lock().await;
            let config = store.get_harness_manager(w.project).unwrap().unwrap();
            match store.manager_succession_preflight(&config).unwrap() {
                ManagerSuccessionPreflightV2::Ineligible { denial, .. } => {
                    assert_eq!(denial.reason, ManagerSuccessionDenialReasonV2::ContextRotationDisabled);
                    assert_eq!(
                        denial.required_action,
                        ManagerSuccessionRequiredActionV2::OperatorEnableContextRotation
                    );
                }
                other => panic!("disabled rotation must be ineligible: {other:?}"),
            }
            // Persisted-enabled with the live choice off: the request fence is
            // observable, and the live recheck alone must refuse it early.
            store
                .conn
                .execute(
                    "UPDATE daemon_settings SET value='true' WHERE key='context_rotation_enabled'",
                    [],
                )
                .unwrap();
        }
        let request = w
            .request(
                w.owner,
                "live-enable",
                SessionProvider::Codex,
                "gpt-6-astra",
                None,
            )
            .await;
        let epoch = w
            .manager
            .store
            .lock()
            .await
            .manager_authority_epoch(w.project)
            .unwrap();
        for persisted in ["true", "false"] {
            w.manager
                .store
                .lock()
                .await
                .conn
                .execute(
                    "UPDATE daemon_settings SET value=?1 WHERE key='context_rotation_enabled'",
                    [persisted],
                )
                .unwrap();
            let disabled = w
                .manager
                .agent_control()
                .agent_manager_control(w.owner, request.clone())
                .await
                .unwrap_err();
            assert_eq!(
                crate::session::manager_actions::safe_action_error(&disabled),
                "manager_succession_rotation_disabled"
            );
            let store = w.manager.store.lock().await;
            let reservations: i64 = store
                .conn
                .query_row("SELECT count(*) FROM manager_root_successions", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(reservations, 0);
            assert_eq!(store.manager_authority_epoch(w.project).unwrap(), epoch);
            assert_eq!(
                store.get_harness_manager(w.project).unwrap().unwrap().current_session_id,
                Some(w.owner)
            );
        }

        w.set_live_rotation_enabled(true).await;
        assert!(!w.manager.context_rotation_enabled);
        let (receipt, root) = w.enqueue(w.owner, request.clone()).await;
        assert_eq!(receipt.state, ManagerActionStateV2::Queued);
        let (replay, replay_root) = w.enqueue(w.owner, request).await;
        assert!(replay.deduplicated);
        assert_eq!(replay.operation_id, receipt.operation_id);
        assert_eq!(replay_root.candidate_session_id, root.candidate_session_id);
        w.terminal(w.owner).await;
        let process = install_controller_candidate_test_process(root.candidate_session_id);
        assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 1);
        assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 0);
        assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
        {
            let store = w.manager.store.lock().await;
            let current = store.manager_succession(root.operation_id).unwrap().unwrap();
            assert_eq!(current.state, ManagerRootState::Committed);
            assert!(current.admission_recorded && current.effect_claimed);
            assert_eq!(current.candidate_session_id, root.candidate_session_id);
            assert_eq!(current.model_invocation_id, root.model_invocation_id);
            assert_eq!(current.published_epoch, Some(epoch + 1));
            assert_eq!(store.manager_authority_epoch(w.project).unwrap(), epoch + 1);
            assert_eq!(
                store.manager_action_operation(root.operation_id).unwrap().unwrap().receipt.state,
                ManagerActionStateV2::Succeeded
            );
            assert_eq!(
                store.get_harness_manager(w.project).unwrap().unwrap().current_session_id,
                Some(root.candidate_session_id)
            );
            let source = store.get_session(w.owner).unwrap().unwrap();
            let candidate = store.get_session(root.candidate_session_id).unwrap().unwrap();
            assert_eq!(source.status, SessionStatus::Archived);
            assert_eq!(candidate.continued_from, Some(w.owner));
            assert_eq!(candidate.rotation_disabled_at, source.rotation_disabled_at);
            let invocations: i64 = store.conn.query_row(
                "SELECT count(*) FROM model_invocations WHERE session_id=?1",
                [root.candidate_session_id.to_string()], |r| r.get(0),
            ).unwrap();
            assert_eq!(invocations, 1);
            let charge: (i64, i64, i64) = store.conn.query_row(
                "SELECT count(*),sum(creation_quantity),sum(recovery_quantity) FROM manager_root_successions",
                [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ).unwrap();
            assert_eq!(charge, (1, 1, 0));
        }
        w.stop_scripted(root.candidate_session_id, &process).await;
    })
    .await
    .expect("live rotation enable must settle within the local fixture deadline");
}

impl RootWorld {
    /// The Overview `manager_control` row exactly as an agent reads it.
    async fn overview_control_row(&self) -> serde_json::Value {
        let page = self
            .manager
            .agent_control()
            .agent_manager_inspect(self.owner, AgentManagerInspectRequestV2::default())
            .await
            .unwrap();
        page.rows
            .into_iter()
            .find(|row| row["type"] == "manager_control")
            .expect("Overview carries the manager_control row")
    }
    async fn forget_persisted_rotation_setting(&self) {
        self.manager
            .store
            .lock()
            .await
            .conn
            .execute(
                "DELETE FROM daemon_settings WHERE key='context_rotation_enabled'",
                [],
            )
            .unwrap();
    }
    async fn wake_state(&self) -> (i64, i64) {
        self.manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT count(*), coalesce(sum(enabled),0) FROM scheduled_jobs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_overview_denies_live_disabled_rotation_even_with_no_persisted_row() {
    let w = RootWorld::with_rotation_enabled(false).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        let expected_denial = serde_json::json!({
            "reason":"context_rotation_disabled",
            "required_action":"operator_enable_context_rotation"
        });
        let epoch = w.manager.store.lock().await.manager_authority_epoch(w.project).unwrap();
        let wakes = w.wake_state().await;
        // Persisted-absent + live-off, then persisted-true + live-off: the Store
        // alone says eligible, the live overlay must agree with admission.
        for persisted in [None, Some("true")] {
            match persisted {
                None => w.forget_persisted_rotation_setting().await,
                Some(value) => {
                    w.manager
                        .store
                        .lock()
                        .await
                        .conn
                        .execute(
                            "INSERT OR REPLACE INTO daemon_settings(key,value,updated_at) VALUES('context_rotation_enabled',?1,'2026-10-15T00:00:00.000000000Z')",
                            [value],
                        )
                        .unwrap();
                }
            }
            {
                let store = w.manager.store.lock().await;
                let config = store.get_harness_manager(w.project).unwrap().unwrap();
                assert!(matches!(
                    store.manager_succession_preflight(&config).unwrap(),
                    ManagerSuccessionPreflightV2::Eligible { .. }
                ));
            }
            let row = w.overview_control_row().await;
            assert_eq!(row["eligibility"], "ineligible");
            assert_eq!(row["denial"], expected_denial);
            assert_eq!(row["logical_manager_session_id"], serde_json::json!(w.owner));
            assert_eq!(row["current_session_id"], serde_json::json!(w.owner));
            assert!(row.get("expected").is_none() && row.get("unresolved_operation_id").is_none());
            // Admission gives the refusal the row advertised, with nothing reserved.
            let request = w
                .request(w.owner, "overlay-refuse", SessionProvider::Codex, "gpt-6-astra", None)
                .await;
            let refusal = w
                .manager
                .agent_control()
                .agent_manager_control(w.owner, request)
                .await
                .unwrap_err();
            assert_eq!(
                crate::session::manager_actions::safe_action_error(&refusal),
                "manager_succession_rotation_disabled"
            );
            let store = w.manager.store.lock().await;
            let reservations: i64 = store
                .conn
                .query_row("SELECT count(*) FROM manager_root_successions", [], |r| r.get(0))
                .unwrap();
            assert_eq!(reservations, 0);
            assert_eq!(store.manager_authority_epoch(w.project).unwrap(), epoch);
            assert_eq!(
                store.get_harness_manager(w.project).unwrap().unwrap().current_session_id,
                Some(w.owner)
            );
            drop(store);
            assert_eq!(w.wake_state().await, wakes);
        }
        // Live choice back on: readiness is eligible again with its fences and
        // normal admission queues exactly one occurrence.
        w.set_live_rotation_enabled(true).await;
        let row = w.overview_control_row().await;
        assert_eq!(row["eligibility"], "eligible");
        assert_eq!(row["expected"]["authority_epoch"], serde_json::json!(epoch));
        let request = w
            .request(w.owner, "overlay-enabled", SessionProvider::Codex, "gpt-6-astra", None)
            .await;
        let (receipt, _root) = w.enqueue(w.owner, request).await;
        assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    })
    .await
    .expect("overview overlay scenario must settle within the local fixture deadline");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_queued_succession_stays_visible_and_replays_with_rotation_off() {
    let w = RootWorld::new().await;
    tokio::time::timeout(Duration::from_secs(30), async {
        let request = w
            .request(
                w.owner,
                "lost-receipt",
                SessionProvider::Codex,
                "gpt-6-astra",
                None,
            )
            .await;
        let (receipt, root) = w.enqueue(w.owner, request.clone()).await;
        assert_eq!(receipt.state, ManagerActionStateV2::Queued);
        // Live off only (persisted still on), then live and persisted off.
        for persisted_too in [false, true] {
            if persisted_too {
                w.set_live_rotation_enabled(false).await;
            } else {
                w.manager
                    .runtime_config
                    .update_field("context_rotation_enabled", &serde_json::json!(false))
                    .unwrap();
            }
            // (a) the identical retry gets the stored receipt, not a refusal.
            let (replay, replay_root) = w.enqueue(w.owner, request.clone()).await;
            assert!(replay.deduplicated);
            assert_eq!(replay.operation_id, receipt.operation_id);
            assert_eq!(replay_root.candidate_session_id, root.candidate_session_id);
            // (b) Overview still points at the in-flight occurrence.
            let row = w.overview_control_row().await;
            assert_eq!(row["eligibility"], "eligible");
            assert_eq!(
                row["unresolved_operation_id"],
                serde_json::json!(receipt.operation_id)
            );
            // A different key is a new occurrence and is still refused.
            let other = w
                .request(
                    w.owner,
                    "new-occurrence",
                    SessionProvider::Codex,
                    "gpt-6-astra",
                    None,
                )
                .await;
            let refusal = w
                .manager
                .agent_control()
                .agent_manager_control(w.owner, other)
                .await
                .unwrap_err();
            assert_eq!(
                crate::session::manager_actions::safe_action_error(&refusal),
                "manager_succession_rotation_disabled"
            );
            let reservations: i64 = w
                .manager
                .store
                .lock()
                .await
                .conn
                .query_row("SELECT count(*) FROM manager_root_successions", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(reservations, 1);
        }
    })
    .await
    .expect("queued succession replay scenario must settle within the local fixture deadline");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_live_rotation_disable_after_admission_refuses_without_effect() {
    // Fixtures are built before the scenario deadline starts (see the enable
    // test above, #1095).
    let worlds = [RootWorld::new().await, RootWorld::new().await];
    tokio::time::timeout(Duration::from_secs(30), async {
        // First isolate the live guard before write-through persistence catches
        // up; then cover the completed operator update with both values false.
        for (persist_disabled, w) in [false, true].into_iter().zip(worlds) {
            let request = w
                .request(
                    w.owner,
                    "live-disable",
                    SessionProvider::Codex,
                    "gpt-6-astra",
                    None,
                )
                .await;
            let (receipt, root) = w.enqueue(w.owner, request).await;
            assert_eq!(receipt.state, ManagerActionStateV2::Queued);
            assert!(w.manager.context_rotation_enabled);
            if persist_disabled {
                w.set_live_rotation_enabled(false).await;
            } else {
                w.manager.runtime_config
                    .update_field("context_rotation_enabled", &serde_json::json!(false))
                    .unwrap();
            }
            w.terminal(w.owner).await;
            let process = install_controller_candidate_test_process(root.candidate_session_id);
            assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 1);
            assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 0);
            assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
            assert_eq!(process.interrupt_count.load(Ordering::SeqCst), 0);
            {
                let store = w.manager.store.lock().await;
                let current = store.manager_succession(root.operation_id).unwrap().unwrap();
                let action = store.manager_action_operation(root.operation_id).unwrap().unwrap();
                assert_eq!(current.state, ManagerRootState::Blocked);
                assert_eq!(action.receipt.state, ManagerActionStateV2::Blocked);
                assert_eq!(action.receipt.outcome.as_deref(), Some("manager_succession_rotation_disabled"));
                assert!(!current.admission_recorded && !current.effect_claimed && !action.effect_started);
                assert_eq!(current.published_epoch, None);
                assert_eq!(current.candidate_session_id, root.candidate_session_id);
                assert_eq!(current.model_invocation_id, root.model_invocation_id);
                assert!(store.get_session(root.candidate_session_id).unwrap().is_none());
                assert!(store.load_model_invocation_record(root.model_invocation_id).unwrap().is_none());
                let config = store.get_harness_manager(w.project).unwrap().unwrap();
                assert_eq!(config.current_session_id, Some(w.owner));
                assert_eq!(store.manager_authority_epoch(w.project).unwrap(), root.authority_epoch);
                assert_eq!(store.get_session(w.owner).unwrap().unwrap().status, SessionStatus::Completed);
                let charge: (i64, i64, i64) = store.conn.query_row(
                    "SELECT count(*),sum(creation_quantity),sum(recovery_quantity) FROM manager_root_successions",
                    [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                ).unwrap();
                assert_eq!(charge, (1, 1, 0));
            }
            drop_controller_candidate_test_process(root.candidate_session_id);
        }
    })
    .await
    .expect("live rotation disable must settle within the local fixture deadline");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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

/// #235: a succession's seat sessions fill no worker slot of the seat's own
/// cap; they are listed apart in the resources view (`listed` of them).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn assert_seat_listed_not_counted(resources: &serde_json::Value, listed: usize) {
    assert_eq!(resources["active_sessions"], 0, "{resources}");
    assert_eq!(
        resources["excluded_manager_seat_ids"]
            .as_array()
            .map(Vec::len),
        Some(listed),
        "{resources}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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
                        informational: false,
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
            assert_seat_listed_not_counted(&resources, 1);
            assert_eq!(
                resources["excluded_manager_seat_ids"][0],
                candidate.id.to_string()
            );
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
        let reopened = Store::open_test_fixture(&w.dir.path().join("rsi.db")).unwrap();
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

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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
            crate::sandbox::git_worktree::read_manager_handoff(w.dir.path(), &handoff, None)
                .is_err()
        );
    }
}

/// hub #3778: a seat that commits once more after `succeed_manager` was
/// accepted still settles while HEAD descends from `source_commit` and the
/// handoff blob is unchanged there. A changed blob, a deleted handoff and a
/// HEAD that does not descend from `source_commit` keep refusing
/// `manager_succession_handoff_head_changed`; the bytes returned are always the
/// accepted blob.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn settlement_accepts_a_descended_head_with_the_handoff_blob_unchanged_3778() {
    let w = RootWorld::new().await;
    let dir = w.dir.path();
    let read = |handoff: &ManagerCommittedHandoffV2| {
        crate::sandbox::git_worktree::read_manager_handoff(dir, handoff, None)
    };
    let refusal = |handoff: &ManagerCommittedHandoffV2| {
        crate::session::manager_actions::safe_action_error(&read(handoff).unwrap_err())
    };
    let accepted = read(&w.handoff).unwrap();
    assert_eq!(accepted, "Continue the committed manager work.\n");

    // A sibling of the accepted commit: same tree, but HEAD does not descend
    // from it once HEAD moves elsewhere.
    let sibling = h1_7g_git(
        dir,
        &[
            "commit-tree",
            "HEAD^{tree}",
            "-p",
            &w.handoff.source_commit,
            "-m",
            "sibling",
        ],
    );

    // A later commit that leaves the handoff file alone (the seat log line).
    std::fs::write(dir.join("seat.log"), "one more line\n").unwrap();
    h1_7g_git(dir, &["add", "seat.log"]);
    h1_7g_git(dir, &["commit", "-qm", "seat log"]);
    assert_ne!(
        h1_7g_git(dir, &["rev-parse", "HEAD"]),
        w.handoff.source_commit
    );
    assert_eq!(read(&w.handoff).unwrap(), accepted);

    // A source that HEAD does not descend from is refused, blob unchanged.
    let sideways = ManagerCommittedHandoffV2 {
        source_commit: sibling,
        ..w.handoff.clone()
    };
    assert_eq!(
        refusal(&sideways),
        "manager_succession_handoff_head_changed"
    );

    // A changed handoff blob at the descended HEAD is refused.
    std::fs::write(dir.join("handoff.md"), "Rewritten after acceptance.\n").unwrap();
    h1_7g_git(dir, &["commit", "-qam", "rewrite handoff"]);
    assert_eq!(
        refusal(&w.handoff),
        "manager_succession_handoff_head_changed"
    );

    // So is a deleted handoff.
    h1_7g_git(dir, &["rm", "-q", "handoff.md"]);
    h1_7g_git(dir, &["commit", "-qm", "delete handoff"]);
    assert_eq!(
        refusal(&w.handoff),
        "manager_succession_handoff_head_changed"
    );
}

/// hub #3778 / #641: the queued notice is cleared in the settled receipt.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_succession_receipt_clears_commit_nothing_after_settlement_641() {
    let w = RootWorld::new().await;
    let request = w
        .request(
            w.owner,
            "commit-nothing-3778",
            SessionProvider::Codex,
            "gpt-6-astra",
            None,
        )
        .await;
    let (receipt, root) = w.enqueue(w.owner, request).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    assert!(
        receipt
            .outcome_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("commit nothing after this"))
    );
    w.terminal(w.owner).await;
    let process = install_controller_candidate_test_process(root.candidate_session_id);
    assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 1);
    {
        let store = w.manager.store.lock().await;
        let settled = store
            .manager_action_operation(root.operation_id)
            .unwrap()
            .unwrap()
            .receipt;
        assert_eq!(settled.state, ManagerActionStateV2::Succeeded);
        assert_eq!(settled.outcome_detail, None);
    }
    w.stop_scripted(root.candidate_session_id, &process).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_current_zero_caps_choice_global_disable_and_unknown_spend_refuse() {
    // Cases 1 and 8 (the shared and provider worker caps) moved to
    // `manager_root_succession_at_full_worker_capacity_is_admitted_241`:
    // a succession is no longer refused for worker capacity (#241).
    for case in [0, 2, 3, 4, 5, 6, 7, 9] {
        let w = RootWorld::new().await;
        match case {
            0 => w.policy(|p| p.max_created_sessions = 0).await,
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
            4 => w
                .manager
                .runtime_config
                .context_rotation_enabled
                .store(false, Ordering::Relaxed),
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
            // Case 4: the live rotation choice is off, so admission refuses
            // before any reservation (#155) instead of blocking at the effect.
            assert!(
                case == 0 || case == 3 || case == 4 || case == 5,
                "zero creation, restricted choice or disabled rotation rejects before queue (case {case})"
            );
        }
    }
}

/// #241: a manager whose workers fill its cap (N for both the shared and the
/// Codex provider cap) still hands its seat on. The candidate is admitted and
/// started, and the resources view lists it as a seat, not a worker.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_succession_at_full_worker_capacity_is_admitted_241() {
    const N: u16 = 2;
    let w = RootWorld::new().await;
    w.policy(|p| {
        p.max_active_sessions = N;
        p.provider_limits = vec![ManagerProviderLimitV2 {
            provider: SessionProvider::Codex,
            max_active: N,
        }];
    })
    .await;
    let mut workers = {
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
        (0..N)
            .map(|_| {
                let mut live = epic.clone();
                live.id = Uuid::new_v4();
                live.parent_id = Some(epic.id);
                live.session_kind = SessionKind::Feature;
                live.provider = SessionProvider::Codex;
                live.status = SessionStatus::Running;
                store.insert_session(&live).unwrap();
                live.id.to_string()
            })
            .collect::<Vec<_>>()
    };
    workers.sort();
    {
        // The cap is full: one more worker is refused.
        let store = w.manager.store.lock().await;
        let config = store.get_harness_manager(w.project).unwrap().unwrap();
        let resources = store.manager_v2_resource_snapshot(&config).unwrap();
        assert_eq!(resources["active_sessions"], N, "{resources}");
        let refusal = store
            .manager_v2_resource_gate(&config, None, SessionProvider::Codex, None)
            .unwrap_err();
        assert!(
            refusal
                .to_string()
                .contains("manager_v2_concurrency_capacity"),
            "{refusal}"
        );
    }
    let request = w
        .request(
            w.owner,
            "succeed-at-cap",
            SessionProvider::Codex,
            "gpt-6-astra",
            None,
        )
        .await;
    let (receipt, root) = w.enqueue(w.owner, request).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    w.terminal(w.owner).await;
    let script = install_controller_candidate_test_process(root.candidate_session_id);
    let claim = w.claim().await;
    if let Err(error) = w.manager.execute_manager_action(&claim).await {
        panic!(
            "succession at full worker capacity refused: {error:?}, receipt {:?}",
            w.manager
                .store
                .lock()
                .await
                .manager_action_operation(root.operation_id)
                .unwrap()
                .map(|op| op.receipt)
        );
    }
    {
        let store = w.manager.store.lock().await;
        let config = store.get_harness_manager(w.project).unwrap().unwrap();
        assert_eq!(config.current_session_id, Some(root.candidate_session_id));
        let resources = store.manager_v2_resource_snapshot(&config).unwrap();
        assert_eq!(resources["active_sessions"], N, "{resources}");
        let mut counted = resources["counted_session_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        counted.sort();
        assert_eq!(counted, workers, "{resources}");
        assert_eq!(
            resources["excluded_manager_seat_ids"],
            serde_json::json!([root.candidate_session_id.to_string()]),
            "{resources}"
        );
    }
    assert_eq!(script.productive_start_count.load(Ordering::SeqCst), 1);
    drop_controller_candidate_test_process(root.candidate_session_id);
    drop_controller_candidate_test_stream(root.candidate_session_id);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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
        #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
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
            let reopened = Store::open_test_fixture(&w.dir.path().join("rsi.db")).unwrap();
            let pending = reopened
                .manager_succession(root.operation_id)
                .unwrap()
                .unwrap();
            assert_eq!(pending.state, ManagerRootState::CleanupRequired);
            assert!(pending.effect_claimed);
            assert_seat_listed_not_counted(
                &reopened
                    .manager_v2_resource_snapshot(
                        &reopened.get_harness_manager(w.project).unwrap().unwrap(),
                    )
                    .unwrap(),
                1,
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

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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
            assert_seat_listed_not_counted(&snapshot, 1);
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
                assert_seat_listed_not_counted(&retained, 1);
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
        let reopened = Store::open_test_fixture(&w.dir.path().join("rsi.db")).unwrap();
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

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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
                // hub #3778: only a source advance that changes the accepted handoff blob fences the successor; an unrelated later commit is accepted (see settlement_accepts_a_descended_head_with_the_handoff_blob_unchanged_3778).
                std::fs::write(w.dir.path().join("handoff.md"), "handoff rewritten while admission waits\n").unwrap();
                h1_7g_git(w.dir.path(), &["add", "handoff.md"]); h1_7g_git(w.dir.path(), &["commit", "-qm", "source handoff changes while admission waits"]);
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

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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
            .all(|m| m == "initialize" || m == "initialized"),
        "only handshake control messages precede the refused productive request: {methods:?}"
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

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
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
    let (receipt, root) = w.enqueue(w.owner, request).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    assert!(
        receipt
            .outcome_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("commit nothing after this"))
    );
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
        assert_seat_listed_not_counted(
            &store
                .manager_v2_resource_snapshot(
                    &store.get_harness_manager(w.project).unwrap().unwrap(),
                )
                .unwrap(),
            1,
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
        assert_eq!(current.outcome_detail, None);
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
    let candidate_token =
        "ac7b77935f90e1bd6fccb40d941b6bda088d2b3b3f59a148439f4e2a698843a6".to_string();
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
        let settled = store
            .manager_action_operation(root.operation_id)
            .unwrap()
            .unwrap()
            .receipt;
        assert_eq!(settled.state, ManagerActionStateV2::Failed);
        assert_eq!(settled.outcome_detail, None);
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

impl RootWorld {
    /// #2667: one settled turn of `owner`, made the session's current
    /// invocation (what a resume for a decision-class notice leaves behind).
    async fn settled_turn(&self) -> Uuid {
        let store = self.manager.store.lock().await;
        let id = Uuid::new_v4();
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store.conn.execute(
            "INSERT INTO model_invocations
             (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at)
             VALUES(?1,'session.launch','session','foreground','paid','admitted','succeeded','launch_session',?2,?3)",
            rusqlite::params![id.to_string(), self.owner.to_string(), at],
        ).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET model_invocation_id=?1 WHERE id=?2",
                rusqlite::params![id.to_string(), self.owner.to_string()],
            )
            .unwrap();
        drop(store);
        tokio::time::sleep(Duration::from_millis(3)).await;
        id
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_decision_notice_turn_after_queueing_still_establishes_2667() {
    let w = RootWorld::new().await;
    let frozen_turn = w.settled_turn().await;
    let request = w
        .request(
            w.owner,
            "notice-turn",
            SessionProvider::Codex,
            "gpt-6-astra",
            None,
        )
        .await;
    let (_, root) = w.enqueue(w.owner, request).await;
    assert_eq!(root.frozen.predecessor_invocation_id, Some(frozen_turn));
    // The decision-class notice resumed the predecessor after the handoff was
    // queued: it took a turn and settled again before the daemon ran the action.
    let notice_turn = w.settled_turn().await;
    w.terminal(w.owner).await;
    let process = install_controller_candidate_test_process(root.candidate_session_id);
    assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 1);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    {
        let store = w.manager.store.lock().await;
        let current = store
            .manager_succession(root.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(current.state, ManagerRootState::Committed);
        // The candidate still descends from the invocation that wrote the handoff.
        assert_eq!(current.frozen.predecessor_invocation_id, Some(frozen_turn));
        assert_ne!(Some(notice_turn), current.frozen.predecessor_invocation_id);
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
        assert!(store.manager_succession_refusal(w.owner).unwrap().is_none());
    }
    w.stop_scripted(root.candidate_session_id, &process).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_finally_refused_succession_wakes_predecessor_and_shows_code_2667() {
    let w = RootWorld::new().await;
    w.settled_turn().await;
    let request = w
        .request(
            w.owner,
            "turn-flood",
            SessionProvider::Codex,
            "gpt-6-astra",
            None,
        )
        .await;
    let (_, root) = w.enqueue(w.owner, request).await;
    // A predecessor that keeps taking turns is not handing the seat over: past
    // the bound the succession is refused, and the seat is told why.
    for _ in 0..6 {
        w.settled_turn().await;
    }
    w.terminal(w.owner).await;
    let process = install_controller_candidate_test_process(root.candidate_session_id);
    assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 1);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    {
        let store = w.manager.store.lock().await;
        let current = store
            .manager_succession(root.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(current.state, ManagerRootState::Blocked);
        let action = store
            .manager_action_operation(root.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(action.receipt.state, ManagerActionStateV2::Blocked);
        assert_eq!(
            action.receipt.outcome.as_deref(),
            Some("manager_succession_predecessor_changed")
        );
        // The predecessor seat is woken with the typed reason.
        let notices: Vec<_> = store
            .manager_inbox(w.owner, &Default::default())
            .unwrap()
            .notices
            .into_iter()
            .filter(|n| n.kind == "action_result" && n.subject_id == root.operation_id.to_string())
            .collect();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].state["state"], "blocked");
        assert_eq!(
            notices[0].state["outcome"],
            "manager_succession_predecessor_changed"
        );
        assert_eq!(
            store
                .manager_succession_refusal(w.owner)
                .unwrap()
                .as_deref(),
            Some("manager_succession_predecessor_changed")
        );
    }
    // Both overviews are built from the seat views.
    let seat = w
        .manager
        .agent_control()
        .seat_view(Some(w.owner))
        .await
        .unwrap();
    assert_eq!(
        seat.succession_refusal.as_deref(),
        Some("manager_succession_predecessor_changed")
    );
    drop_controller_candidate_test_process(root.candidate_session_id);
}

/// #2747: the #2667 state (a finished seat whose succession was finally refused)
/// is a `pm_seat_succession_blocked` health condition naming the project, so a
/// manager's armed `health_anomaly` wake hears of it with no model turn.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_root_runtime_refused_succession_on_a_finished_seat_is_a_health_anomaly_2747() {
    use crate::health_anomaly::{Inputs, Scope, Sustain, collect_store_facts, evaluate};
    use rsi_common::health_anomaly::{HealthAnomalySpec, KIND_PM_SEAT_BLOCKED};

    let w = RootWorld::new().await;
    w.settled_turn().await;
    let request = w
        .request(
            w.owner,
            "turn-flood",
            SessionProvider::Codex,
            "gpt-6-astra",
            None,
        )
        .await;
    let (_, root) = w.enqueue(w.owner, request).await;
    // Before the refusal the succession is only queued: nothing to report.
    {
        let store = w.manager.store.lock().await;
        let facts = collect_store_facts(
            &store,
            &Scope::portfolio(vec![w.project]),
            chrono::Utc::now(),
        )
        .unwrap();
        assert!(facts.seats.is_empty(), "{:?}", facts.seats);
    }
    for _ in 0..6 {
        w.settled_turn().await;
    }
    w.terminal(w.owner).await;
    let process = install_controller_candidate_test_process(root.candidate_session_id);
    assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 1);
    {
        let store = w.manager.store.lock().await;
        let now = chrono::Utc::now();
        let facts = collect_store_facts(&store, &Scope::portfolio(vec![w.project]), now).unwrap();
        assert_eq!(facts.seats.len(), 1, "{:?}", facts.seats);
        assert_eq!(facts.seats[0].project_id, w.project);
        assert_eq!(
            facts.seats[0].refusal,
            "manager_succession_predecessor_changed"
        );
        let found = evaluate(
            &Inputs {
                store: facts,
                ..Inputs::default()
            },
            &Default::default(),
            &HealthAnomalySpec::default(),
            &mut Sustain::default(),
            now,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, KIND_PM_SEAT_BLOCKED);
        assert_eq!(found[0].subject, w.project.to_string());
        // Another project's wake does not hear of it.
        let other =
            collect_store_facts(&store, &Scope::project(uuid::Uuid::new_v4()), now).unwrap();
        assert!(other.seats.is_empty());
    }
    let _ = process;
    drop_controller_candidate_test_process(root.candidate_session_id);
}

/// #640: without a private ref, an unchanged handoff in a shared-HEAD ancestor
/// is accepted and the successor forks from that exact source commit.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sandboxless_seat_accepts_a_shared_head_ancestor_without_a_private_ref_640() {
    let w = RootWorld::new().await;
    let dir = w.dir.path();
    std::fs::write(dir.join("shared.log"), "Later shared-root work.\n").unwrap();
    h1_7g_git(dir, &["add", "shared.log"]);
    h1_7g_git(dir, &["commit", "-qm", "advance shared HEAD"]);
    let shared_head = h1_7g_git(dir, &["rev-parse", "HEAD"]);
    assert_eq!(
        h1_7g_git(dir, &["rev-parse", "HEAD^"]),
        w.handoff.source_commit
    );
    let anchor = crate::sandbox::git_worktree::manager_handoff_ref(w.owner);
    assert_eq!(
        crate::sandbox::git_worktree::read_manager_handoff(dir, &w.handoff, Some(&anchor)).unwrap(),
        "Continue the committed manager work.\n"
    );
    let request = w
        .request(
            w.owner,
            "shared-ancestor-640",
            SessionProvider::Codex,
            "gpt-6-astra",
            None,
        )
        .await;
    let (receipt, succession) = w.enqueue(w.owner, request).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    w.terminal(w.owner).await;
    let process = install_controller_candidate_test_process(succession.candidate_session_id);
    assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 1);
    {
        let store = w.manager.store.lock().await;
        let current = store
            .manager_succession(succession.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(current.state, ManagerRootState::Committed);
        let custody = store
            .live_custody_for_session(succession.candidate_session_id)
            .unwrap();
        assert_eq!(custody.source_commit, w.handoff.source_commit);
        assert_eq!(
            h1_7g_git(
                std::path::Path::new(&custody.sandbox_root),
                &["rev-parse", "HEAD"]
            ),
            w.handoff.source_commit
        );
    }
    assert_eq!(h1_7g_git(dir, &["rev-parse", "HEAD"]), shared_head);
    let _ = process;
    drop_controller_candidate_test_process(succession.candidate_session_id);
}

/// #640: shared HEAD descending from the handoff commit is insufficient when
/// its handoff blob changed and no private ref anchors the accepted commit.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_sandboxless_seat_refuses_a_changed_shared_head_blob_without_a_private_ref_640() {
    let w = RootWorld::new().await;
    let dir = w.dir.path();
    std::fs::write(dir.join("handoff.md"), "Changed in shared HEAD.\n").unwrap();
    h1_7g_git(dir, &["commit", "-qam", "change shared handoff"]);
    assert_eq!(
        h1_7g_git(dir, &["rev-parse", "HEAD^"]),
        w.handoff.source_commit
    );
    let request = w
        .request(
            w.owner,
            "changed-shared-blob-640",
            SessionProvider::Codex,
            "gpt-6-astra",
            None,
        )
        .await;
    let refused = w
        .manager
        .agent_control()
        .agent_manager_control(w.owner, request)
        .await
        .unwrap_err();
    assert_eq!(
        crate::session::manager_actions::safe_action_error(&refused),
        "manager_succession_handoff_head_changed"
    );
}

/// #172 (hub #3328): a sandboxless project manager seat (a reseated seat in
/// the shared project root, where no agent commits) writes its handoff commit
/// with plumbing only and points its private handoff ref at it. The root's
/// HEAD, index, branches and status are unchanged, `succeed_manager` anchors
/// the handoff at the ref and the successor is established; without the ref
/// the same unrelated handoff commit is refused at HEAD.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sandboxless_manager_seat_succeeds_from_its_private_handoff_ref_172() {
    fn plumbing(dir: &std::path::Path, args: &[&str], stdin: Option<&str>) -> String {
        use std::io::Write;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run git");
        let mut input = child.stdin.take().unwrap();
        input.write_all(stdin.unwrap_or("").as_bytes()).unwrap();
        drop(input);
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
    let w = RootWorld::new().await;
    tokio::time::timeout(Duration::from_secs(30), async {
        let root = w.dir.path().to_path_buf();
        let observe = || {
            (
                h1_7g_git(&root, &["rev-parse", "HEAD"]),
                std::fs::read(root.join(".git/index")).unwrap(),
                h1_7g_git(&root, &["for-each-ref", "refs/heads"]),
                h1_7g_git(&root, &["--no-optional-locks", "status", "--porcelain"]),
            )
        };
        let before = observe();
        let scratch = tempfile::tempdir().unwrap();
        let file = scratch.path().join("seat-handoff.md");
        std::fs::write(&file, "Handoff anchored at the seat's private ref.\n").unwrap();
        let blob = plumbing(&root, &["hash-object", "-w", file.to_str().unwrap()], None);
        let tree = plumbing(
            &root,
            &["mktree"],
            Some(&format!("100644 blob {blob}\thandoff.md\n")),
        );
        let commit = plumbing(
            &root,
            &[
                "-c",
                "user.name=seat",
                "-c",
                "user.email=seat@rsi.local",
                "commit-tree",
                "-m",
                "handoff",
                &tree,
            ],
            None,
        );
        let handoff = ManagerCommittedHandoffV2 {
            source_commit: commit.clone(),
            relative_path: "handoff.md".into(),
            blob_oid: blob,
        };
        let mut request = w
            .request(
                w.owner,
                "ref-anchored",
                SessionProvider::Codex,
                "gpt-6-astra",
                None,
            )
            .await;
        if let ManagerActionV2::SucceedManager { handoff: h, .. } = &mut request.operation {
            *h = handoff;
        }
        // No ref yet: the commit is not an ancestor of HEAD, so HEAD refuses it.
        let refused = w
            .manager
            .agent_control()
            .agent_manager_control(w.owner, request.clone())
            .await
            .unwrap_err();
        assert_eq!(
            crate::session::manager_actions::safe_action_error(&refused),
            "manager_succession_handoff_head_changed"
        );
        let anchor = crate::sandbox::git_worktree::manager_handoff_ref(w.owner);
        assert_eq!(anchor, format!("refs/rsi/manager-handoff/{}", w.owner));
        plumbing(&root, &["update-ref", &anchor, &commit], None);
        assert_eq!(
            observe(),
            before,
            "plumbing leaves HEAD, index, branches and status"
        );

        let (receipt, succession) = w.enqueue(w.owner, request).await;
        assert_eq!(receipt.state, ManagerActionStateV2::Queued);
        w.terminal(w.owner).await;
        let process = install_controller_candidate_test_process(succession.candidate_session_id);
        assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 1);
        {
            let store = w.manager.store.lock().await;
            let current = store
                .manager_succession(succession.operation_id)
                .unwrap()
                .unwrap();
            assert_eq!(current.state, ManagerRootState::Committed);
            assert_eq!(
                store
                    .get_harness_manager(w.project)
                    .unwrap()
                    .unwrap()
                    .current_session_id,
                Some(succession.candidate_session_id)
            );
            let candidate = store
                .get_session(succession.candidate_session_id)
                .unwrap()
                .unwrap();
            assert_eq!(candidate.continued_from, Some(w.owner));
        }
        // The daemon left the shared root's HEAD, index and tree alone; the
        // only new branch is the successor's own sandbox branch (#169).
        let after = observe();
        assert_eq!(
            (&after.0, &after.1, &after.3),
            (&before.0, &before.1, &before.3)
        );
        let own = format!("refs/heads/rsi/{}", succession.candidate_session_id);
        let branches: Vec<&str> = after
            .2
            .lines()
            .filter(|line| !line.ends_with(own.as_str()))
            .collect();
        assert_eq!(branches, before.2.lines().collect::<Vec<_>>());
        w.stop_scripted(succession.candidate_session_id, &process)
            .await;
    })
    .await
    .expect("ref-anchored succession settles within the fixture deadline");
}

/// #185: once a sandboxless seat's successor is published, the daemon prunes
/// the handoff refs of that lineage's predecessors older than the kept two
/// (`MANAGER_HANDOFF_REFS_KEPT`), matched by the session ids and commits the
/// store recorded. The just-read ref and the one before it stay, a ref moved
/// since it was recorded stays, another lineage's ref and `refs/heads` stay.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_published_succession_prunes_aged_out_handoff_refs_of_its_lineage_185() {
    let w = RootWorld::new().await;
    tokio::time::timeout(Duration::from_secs(30), async {
        let root = w.dir.path().to_path_buf();
        let recorded = w.handoff.source_commit.clone();
        let moved = h1_7g_git(&root, &["commit-tree", "HEAD^{tree}", "-m", "moved"]);
        // Published lineage behind the live seat: older1 -> owner,
        // older2 -> older1, older3 -> older2 (newest first from the owner).
        let (older1, older2, older3, foreign) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        {
            let store = w.manager.store.lock().await;
            let stamp = crate::store::harness_manager_v2::now();
            store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
            for (predecessor, candidate) in [(older1, w.owner), (older2, older1), (older3, older2)]
            {
                let frozen = serde_json::json!({
                    "predecessor": {"working_dir": root},
                    "predecessor_custody": null,
                    "handoff": {"source_commit": recorded},
                });
                store
                    .conn
                    .execute(
                        "INSERT INTO manager_root_successions
                         (operation_id,project_id,manager_session_id,predecessor_session_id,
                          candidate_session_id,launch_attempt_id,model_invocation_id,scope_version,
                          policy_version,authority_epoch,frozen_json,state,row_version,
                          published_epoch,created_at,updated_at)
                         VALUES(?1,?2,?3,?3,?4,?5,?6,1,1,1,?7,'committed',1,1,?8,?8)",
                        rusqlite::params![
                            Uuid::new_v4().to_string(),
                            w.project.to_string(),
                            predecessor.to_string(),
                            candidate.to_string(),
                            Uuid::new_v4().to_string(),
                            Uuid::new_v4().to_string(),
                            frozen.to_string(),
                            stamp
                        ],
                    )
                    .unwrap();
            }
            store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        }
        let handoff_ref = crate::sandbox::git_worktree::manager_handoff_ref;
        for id in [w.owner, older1, older2, foreign] {
            h1_7g_git(&root, &["update-ref", &handoff_ref(id), &recorded]);
        }
        // Recorded at `recorded`, since moved: never the daemon's to delete.
        h1_7g_git(&root, &["update-ref", &handoff_ref(older3), &moved]);
        let heads = h1_7g_git(&root, &["for-each-ref", "refs/heads"]);

        let request = w
            .request(
                w.owner,
                "prune-refs",
                SessionProvider::Codex,
                "gpt-6-astra",
                None,
            )
            .await;
        let (receipt, succession) = w.enqueue(w.owner, request).await;
        assert_eq!(receipt.state, ManagerActionStateV2::Queued);
        w.terminal(w.owner).await;
        let process = install_controller_candidate_test_process(succession.candidate_session_id);
        assert_eq!(w.manager.reconcile_manager_actions_once().await.unwrap(), 1);
        let live = succession.candidate_session_id;
        let lineage = w
            .manager
            .store
            .lock()
            .await
            .manager_handoff_lineage(live, 16)
            .unwrap();
        assert_eq!(
            lineage.iter().map(|hop| hop.session_id).collect::<Vec<_>>(),
            vec![w.owner, older1, older2, older3]
        );
        let refs = || {
            h1_7g_git(
                &root,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/rsi/",
                ],
            )
        };
        // The publication prunes off its own path; wait for it.
        while refs().contains(&handoff_ref(older2)) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut expected = vec![
            format!("{} {recorded}", handoff_ref(w.owner)),
            format!("{} {recorded}", handoff_ref(older1)),
            format!("{} {moved}", handoff_ref(older3)),
            format!("{} {recorded}", handoff_ref(foreign)),
        ];
        expected.sort();
        let mut listed: Vec<String> = refs().lines().map(str::to_owned).collect();
        listed.sort();
        assert_eq!(listed, expected);
        let own = format!("refs/heads/rsi/{live}");
        let after: Vec<String> = h1_7g_git(&root, &["for-each-ref", "refs/heads"])
            .lines()
            .filter(|line| !line.ends_with(own.as_str()))
            .map(str::to_owned)
            .collect();
        assert_eq!(after, heads.lines().map(str::to_owned).collect::<Vec<_>>());
        // A second pass over the same lineage finds nothing left to prune.
        assert!(
            crate::session::manager_succession::prune_manager_handoff_refs(&w.manager.store, live)
                .await
                .unwrap()
                .is_empty()
        );
        w.stop_scripted(live, &process).await;
    })
    .await
    .expect("handoff-ref prune settles within the fixture deadline");
}

/// #505: catch-up reaches beyond the former 256-hop limit, retaining only the
/// audit window, live/pending refs, moved refs and refs outside this lineage.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn handoff_ref_catch_up_covers_300_predecessors_in_bounded_pages_505() {
    use crate::store::manager_successions::{
        MANAGER_HANDOFF_LINEAGE_BATCH, ManagerHandoffLineageCursor,
    };
    let w = RootWorld::new().await;
    let root = w.dir.path();
    let recorded = &w.handoff.source_commit;
    let moved = h1_7g_git(root, &["commit-tree", "HEAD^{tree}", "-m", "moved"]);
    let older: Vec<Uuid> = (0..300).map(|_| Uuid::new_v4()).collect();
    let pending = Uuid::new_v4();
    let foreign = Uuid::new_v4();
    {
        let store = w.manager.store.lock().await;
        let stamp = crate::store::harness_manager_v2::now();
        store
            .conn
            .execute_batch("PRAGMA foreign_keys=OFF; BEGIN")
            .unwrap();
        // Published rows are the immutable source records, as in the #185
        // fixture above; no provider launch or synthetic operation is needed.
        for (index, predecessor) in older.iter().enumerate() {
            let candidate = if index == 0 {
                w.owner
            } else {
                older[index - 1]
            };
            let frozen = serde_json::json!({
                "predecessor": {"working_dir": root},
                "predecessor_custody": null,
                "handoff": {"source_commit": recorded},
            });
            store
                .conn
                .execute(
                    "INSERT INTO manager_root_successions
                 (operation_id,project_id,manager_session_id,predecessor_session_id,
                  candidate_session_id,launch_attempt_id,model_invocation_id,scope_version,
                  policy_version,authority_epoch,frozen_json,state,row_version,
                  published_epoch,created_at,updated_at)
                 VALUES(?1,?2,?3,?3,?4,?5,?6,1,1,1,?7,'committed',1,1,?8,?8)",
                    rusqlite::params![
                        Uuid::new_v4().to_string(),
                        w.project.to_string(),
                        predecessor.to_string(),
                        candidate.to_string(),
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        frozen.to_string(),
                        stamp
                    ],
                )
                .unwrap();
        }
        // Even a pending succession involving an old seat protects that ref;
        // its unpublished candidate is never followed as a lineage edge.
        store
            .conn
            .execute(
                "INSERT INTO manager_root_successions
             (operation_id,project_id,manager_session_id,predecessor_session_id,
              candidate_session_id,launch_attempt_id,model_invocation_id,scope_version,
              policy_version,authority_epoch,frozen_json,state,row_version,created_at,updated_at)
             VALUES(?1,?2,?3,?3,?4,?5,?6,1,1,1,'{}','reserved',1,?7,?7)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    w.project.to_string(),
                    older[270].to_string(),
                    pending.to_string(),
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    stamp
                ],
            )
            .unwrap();
        store
            .conn
            .execute_batch("COMMIT; PRAGMA foreign_keys=ON")
            .unwrap();

        let mut cursor = ManagerHandoffLineageCursor::new(w.owner);
        let first = store
            .manager_handoff_lineage_page(&mut cursor, usize::MAX)
            .unwrap();
        let second = store
            .manager_handoff_lineage_page(&mut cursor, usize::MAX)
            .unwrap();
        assert_eq!(first.len(), MANAGER_HANDOFF_LINEAGE_BATCH);
        assert_eq!(second.len(), 44);
        assert_eq!(
            first
                .iter()
                .chain(&second)
                .map(|hop| hop.session_id)
                .collect::<Vec<_>>(),
            older
        );
        assert!(second[270 - MANAGER_HANDOFF_LINEAGE_BATCH].protected);
        assert!(
            store
                .manager_handoff_lineage_page(&mut cursor, usize::MAX)
                .unwrap()
                .is_empty()
        );
    }
    let handoff_ref = crate::sandbox::git_worktree::manager_handoff_ref;
    // Seed every recorded ref in one plumbing call, avoiding 300 subprocesses.
    let updates: String = older
        .iter()
        .copied()
        .chain([w.owner, pending, foreign])
        .map(|id| {
            format!(
                "update {} {}\n",
                handoff_ref(id),
                if id == older[280] {
                    moved.as_str()
                } else {
                    recorded.as_str()
                }
            )
        })
        .collect();
    let mut child = Command::new("git")
        .args(["update-ref", "--stdin"])
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), updates.as_bytes()).unwrap();
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    let heads = h1_7g_git(root, &["for-each-ref", "refs/heads"]);

    let mut pruned =
        crate::session::manager_succession::prune_manager_handoff_refs(&w.manager.store, w.owner)
            .await
            .unwrap();
    let mut eligible: Vec<Uuid> = older
        .iter()
        .copied()
        .skip(2)
        .filter(|id| *id != older[270] && *id != older[280])
        .collect();
    pruned.sort();
    eligible.sort();
    assert_eq!(pruned, eligible);
    let mut expected: Vec<String> = [w.owner, older[0], older[1], older[270], pending, foreign]
        .into_iter()
        .map(|id| format!("{} {recorded}", handoff_ref(id)))
        .collect();
    expected.push(format!("{} {moved}", handoff_ref(older[280])));
    expected.sort();
    let mut listed: Vec<String> = h1_7g_git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/rsi/manager-handoff/",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    listed.sort();
    assert_eq!(listed, expected);
    assert_eq!(h1_7g_git(root, &["for-each-ref", "refs/heads"]), heads);
    assert!(
        crate::session::manager_succession::prune_manager_handoff_refs(&w.manager.store, w.owner)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Hub #3315: a home seat's `succeed_manager` is refused with its own code
/// before any git runs, and the operator's home repository is untouched.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_home_seat_succeed_manager_is_refused_before_any_git_in_home_3315() {
    let w = RootWorld::new().await;
    let home = w.dir.path().to_path_buf();
    let before = (
        h1_7g_git(&home, &["rev-parse", "HEAD"]),
        std::fs::read(home.join(".git/index")).unwrap(),
        h1_7g_git(&home, &["for-each-ref"]),
    );
    let request = w
        .request(
            w.owner,
            "home-seat",
            SessionProvider::Codex,
            "gpt-6-astra",
            None,
        )
        .await;
    {
        let store = w.manager.store.lock().await;
        store.ensure_home_project(&home).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET project_id=?2 WHERE id=?1",
                rusqlite::params![w.owner.to_string(), Project::HOME_ID.to_string()],
            )
            .unwrap();
    }
    let refused = w
        .manager
        .agent_control()
        .agent_manager_control(w.owner, request)
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains(Project::HOME_SUCCESSION_REFUSED),
        "{refused}"
    );
    let source = w
        .manager
        .store
        .lock()
        .await
        .get_session(w.owner)
        .unwrap()
        .unwrap();
    let handoff = w
        .manager
        .custody_execution_runtime()
        .prepare_manager_handoff(&source, &w.handoff)
        .await
        .unwrap_err();
    assert!(
        handoff
            .to_string()
            .contains(Project::HOME_SUCCESSION_REFUSED),
        "{handoff}"
    );
    let after = (
        h1_7g_git(&home, &["rev-parse", "HEAD"]),
        std::fs::read(home.join(".git/index")).unwrap(),
        h1_7g_git(&home, &["for-each-ref"]),
    );
    assert_eq!(after, before);
}
