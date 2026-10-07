//! K2 (#380/#390): manager recovery of uncertain actions and stale lead
//! continuations. Every assertion states the positive end state.
use super::*;
use crate::store::manager_actions::LEAD_RETIREMENT_KIND;

const HUMAN_GATE_OUTCOME: &str = "orchestration_outcome_v1: {\"schema_version\":1,\"mode\":\"program\",\"next_slice_ready\":false,\"continuation_state\":\"human_gate\",\"blocker_class\":\"production\",\"evidence\":\"operator approval is required\"}";

fn fenced(
    key: &str,
    scope_version: i64,
    policy_version: i64,
    operation: ManagerActionV2,
) -> AgentManagerControlRequestV2 {
    AgentManagerControlRequestV2 {
        project_id: None,
        fence: ManagerFenceV2 {
            scope_version,
            policy_version,
        },
        idempotency_key: key.into(),
        operation,
    }
}

impl Pilot {
    /// Admit, claim and record an uncertain result exactly as the lost-owner
    /// and bounded-interrupt paths do. Returns the uncertain row version.
    async fn uncertain(&self, key: &str, operation: ManagerActionV2, outcome: &str) -> (Uuid, i64) {
        let receipt = self.admit(key, operation).await;
        let claim = self.claim().await;
        assert_eq!(claim.id(), receipt.operation_id);
        let store = self.manager.store.lock().await;
        store.manager_action_runtime_gate(&claim, true).unwrap();
        let settled = store
            .finish_manager_action(&claim, ManagerActionStateV2::Uncertain, outcome)
            .unwrap();
        (receipt.operation_id, settled.row_version)
    }

    async fn stored_payloads(&self, id: Uuid) -> (String, String) {
        let store = self.manager.store.lock().await;
        let payload: String = store
            .conn
            .query_row(
                "SELECT payload_json FROM harness_manager_v2_operations WHERE id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        let context: String = store
            .conn
            .query_row(
                "SELECT payload_json FROM harness_manager_v2_records WHERE kind='lifecycle_context' AND record_key=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        (payload, context)
    }

    async fn settled_evidence(&self, id: Uuid) -> serde_json::Value {
        let raw: String = self
            .manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT payload_json FROM harness_manager_v2_events WHERE kind='action_settled' AND record_key=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    async fn stop_session(&self, id: Uuid) {
        crate::session::lifecycle::interrupt_active_in_maps(&self.manager.active, id)
            .await
            .unwrap();
        crate::session::launch::drop_controller_candidate_test_stream(id);
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if !self.manager.active.read().await.contains_key(&id)
                    && self.manager.persistence.pending.load(Ordering::SeqCst) == 0
                    && self
                        .manager
                        .store
                        .lock()
                        .await
                        .get_session(id)
                        .unwrap()
                        .unwrap()
                        .status
                        == SessionStatus::Interrupted
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn append_lead_output(&self, role: Role, content: &str) {
        let store = self.manager.store.lock().await;
        let sequence = store
            .load_events(self.lead)
            .unwrap()
            .last()
            .map_or(0, |e| e.sequence + 1);
        manager_program_event(&store, self.lead, sequence, Some(role), content.into());
    }

    /// The reconciler is process-wide single-flight; a concurrent test may
    /// own it. Retry until this operation leaves the queue.
    async fn reconcile_until_claimed(&self, id: Uuid) {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                self.manager.reconcile_manager_actions_once().await.unwrap();
                if self.receipt(id).await.state != ManagerActionStateV2::Queued {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn retire(&self, key: &str) -> Result<ManagerActionReceiptV2> {
        self.manager
            .agent_control()
            .agent_manager_control(
                self.owner,
                self.request(
                    key,
                    ManagerActionV2::RetireLeadContinuations {
                        epic_id: self.epic,
                        expected: self.fence().await,
                    },
                ),
            )
            .await
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_recovery_380_uncertain_pause_settles_and_unfences_exact_resume() {
    let p = pilot().await;
    let created = p
        .admit(
            "initial",
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "start".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    let lead = created.target_session_id.unwrap();
    let _initial = super::super::launch::install_controller_candidate_test_process(lead);
    p.execute().await.unwrap();
    wait_launch_event(&p, lead).await;
    super::super::launch::send_controller_candidate_test_event(
        lead,
        crate::claude::StreamEvent {
            event_type: "system".into(),
            data: serde_json::json!({"subtype":"init","session_id":"recovery-resumable-provider"}),
        },
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let resumable = p
                .manager
                .store
                .lock()
                .await
                .get_session(lead)
                .unwrap()
                .unwrap()
                .claude_session_id
                .as_deref()
                == Some("recovery-resumable-provider");
            if resumable && p.manager.persistence.pending.load(Ordering::SeqCst) == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    // The interrupt was delivered but settlement timed out (#380): the lead
    // is now durably Interrupted, yet the pause stayed uncertain.
    let before_stop = p.fence().await.event_sequence;
    p.stop_session(lead).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            p.manager.persistence.barrier().await.unwrap();
            if p.fence().await.event_sequence > before_stop {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("interruption event is durable before pause admission");
    let (pause, uncertain_version) = p
        .uncertain(
            "pause",
            ManagerActionV2::PauseLead {
                epic_id: p.epic,
                expected: p.fence().await,
                reason: "manager pause".into(),
            },
            "manager_v2_predecessor_unsettled",
        )
        .await;
    let before = p.stored_payloads(pause).await;
    let resume = p
        .admit(
            "exact-resume",
            ManagerActionV2::ResumeLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "continue the paused work".into(),
            },
        )
        .await;
    let queued_resume = p.receipt(resume.operation_id).await;
    assert_eq!(queued_resume.state, ManagerActionStateV2::Queued);
    let process = super::super::launch::install_controller_candidate_test_process(lead);
    #[cfg(target_os = "linux")]
    let orphan_fixture = crate::session::reaper::StartupReaperFixture::new();
    #[cfg(target_os = "linux")]
    let _orphan_guard = orphan_fixture.scoped_runtime_reap_root(lead).unwrap();
    p.reconcile_until_claimed(resume.operation_id).await;

    let settled = p.receipt(pause).await;
    assert_eq!(settled.state, ManagerActionStateV2::Succeeded);
    assert_eq!(settled.outcome.as_deref(), Some("lead_paused_reconciled"));
    assert_eq!(settled.result, Some(ManagerActionResultV2::LeadPaused));
    assert_eq!(settled.row_version, uncertain_version + 1);
    // Settlement appended evidence; the original request/context is intact.
    assert_eq!(p.stored_payloads(pause).await, before);
    let evidence = p.settled_evidence(pause).await;
    assert_eq!(
        evidence["witness"]["predecessor"]["session_id"],
        serde_json::json!(lead)
    );
    assert_eq!(evidence["witness"]["reconciled_by"], "daemon");
    assert_eq!(evidence["prior_receipt"]["state"], "uncertain");

    let resumed = p.receipt(resume.operation_id).await;
    assert_eq!(
        resumed.state,
        ManagerActionStateV2::Succeeded,
        "{resumed:?}"
    );
    assert_eq!(resumed.result, Some(ManagerActionResultV2::LeadResumed));
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, lead)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(lead);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_recovery_settle_proven_absent_and_unobservable() {
    let p = pilot().await;
    // Provably absent: the reserved candidate was never established.
    let (create, version) = p
        .uncertain(
            "create",
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Feature,
                query: "work".into(),
                launch: p.policy.allowed_launches[0].clone(),
                sandbox_source: None,
            },
            "execution_owner_lost_unconfirmed",
        )
        .await;
    p.admit(
        "settle-create",
        ManagerActionV2::SettleUncertainAction {
            operation_id: create,
            expected_row_version: version,
        },
    )
    .await;
    p.execute().await.unwrap();
    let absent = p.receipt(create).await;
    assert_eq!(absent.state, ManagerActionStateV2::Failed);
    assert_eq!(
        absent.outcome.as_deref(),
        Some("manager_v2_recovered_effect_absent")
    );

    // Proven: the idle, Completed lead's pause effect is visible.
    let (pause, version) = p
        .uncertain(
            "pause",
            ManagerActionV2::PauseLead {
                epic_id: p.epic,
                expected: p.fence().await,
                reason: "hold".into(),
            },
            "execution_owner_lost_unconfirmed",
        )
        .await;
    let settle = p
        .admit(
            "settle-pause",
            ManagerActionV2::SettleUncertainAction {
                operation_id: pause,
                expected_row_version: version,
            },
        )
        .await;
    assert_eq!(
        settle.action_kind,
        ManagerActionKindV2::SettleUncertainAction
    );
    assert_eq!(
        settle.target_type,
        ManagerActionTargetTypeV2::ManagerOperation
    );
    // Not self-fenced: claimable while the operation it settles is uncertain.
    p.execute().await.unwrap();
    let own = p.receipt(settle.operation_id).await;
    assert_eq!(own.state, ManagerActionStateV2::Succeeded);
    assert_eq!(
        own.outcome.as_deref(),
        Some("uncertain_action_settled_succeeded")
    );
    assert_eq!(
        own.result,
        Some(ManagerActionResultV2::UncertainActionSettled)
    );
    let original = p.receipt(pause).await;
    assert_eq!(original.state, ManagerActionStateV2::Succeeded);
    assert_eq!(original.result, Some(ManagerActionResultV2::LeadPaused));
    assert_eq!(
        p.settled_evidence(pause).await["settled_by_operation_id"],
        serde_json::json!(settle.operation_id)
    );

    // Unobservable: the lead is not durably terminal.
    p.manager
        .store
        .lock()
        .await
        .update_session_status(p.lead, SessionStatus::Running)
        .unwrap();
    let (second, version) = p
        .uncertain(
            "pause-2",
            ManagerActionV2::PauseLead {
                epic_id: p.epic,
                expected: p.fence().await,
                reason: "hold again".into(),
            },
            "manager_v2_predecessor_unsettled",
        )
        .await;
    let refused = p
        .admit(
            "settle-unobservable",
            ManagerActionV2::SettleUncertainAction {
                operation_id: second,
                expected_row_version: version,
            },
        )
        .await;
    // Neither the daemon reconciler nor the typed request may settle it.
    p.reconcile_until_claimed(refused.operation_id).await;
    let still = p.receipt(second).await;
    assert_eq!(still.state, ManagerActionStateV2::Uncertain);
    assert_eq!(still.row_version, version);
    let refused = p.receipt(refused.operation_id).await;
    assert_eq!(refused.state, ManagerActionStateV2::Blocked);
    assert_eq!(
        refused.outcome.as_deref(),
        Some("manager_v2_effect_unobservable")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn manager_recovery_settle_refuses_foreign_revoked_and_missing_capability() {
    let p = pilot().await;
    let (create, version) = p
        .uncertain(
            "create",
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Feature,
                query: "work".into(),
                launch: p.policy.allowed_launches[0].clone(),
                sandbox_source: None,
            },
            "execution_owner_lost_unconfirmed",
        )
        .await;
    let settle = |key: &str, scope: i64, policy: i64| {
        fenced(
            key,
            scope,
            policy,
            ManagerActionV2::SettleUncertainAction {
                operation_id: create,
                expected_row_version: version,
            },
        )
    };
    let control = p.manager.agent_control();
    // A lead/worker caller holds no manager capability.
    let lead = control
        .agent_manager_control(p.lead, settle("lead", 1, 1))
        .await
        .unwrap_err()
        .to_string();
    assert!(lead.contains("manager_v2_capability_denied"), "{lead}");

    // A foreign project's uncertain operation is out of scope.
    let foreign = {
        let other = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let group = Uuid::new_v4();
        let epic = Uuid::new_v4();
        let store = p.manager.store.lock().await;
        store
            .insert_project(&Project {
                id: other,
                name: "Other project".into(),
                path: Some(p.repo.clone()),
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
        for (id, kind, parent) in [
            (owner, SessionKind::Standard, None),
            (group, SessionKind::Group, None),
            (epic, SessionKind::Epic, Some(group)),
        ] {
            let mut row = bare_session(id);
            row.project_id = Some(other);
            row.working_dir = p.repo.clone();
            row.session_kind = kind;
            row.parent_id = parent;
            store.insert_session(&row).unwrap();
        }
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: other,
                session_id: owner,
                epic_ids: Some(vec![epic]),
                expected_row_version: 0,
            })
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: other,
                expected_scope_version: 1,
                expected_policy_version: 0,
                idempotency_key: "grant".into(),
                policy: ManagerPolicyV2 {
                    group_ids: vec![group],
                    ..p.policy.clone()
                },
            })
            .unwrap();
        store
            .enqueue_manager_action(
                ManagerActionOriginV2::Agent { caller: owner },
                fenced(
                    "foreign-update",
                    1,
                    1,
                    ManagerActionV2::UpdateContainer {
                        container_id: epic,
                        expected_updated_at: store.get_session(epic).unwrap().unwrap().updated_at,
                        name: "Renamed".into(),
                        description: None,
                    },
                ),
            )
            .unwrap()
            .operation_id
    };
    let out_of_scope = control
        .agent_manager_control(
            p.owner,
            fenced(
                "foreign",
                1,
                1,
                ManagerActionV2::SettleUncertainAction {
                    operation_id: foreign,
                    expected_row_version: 1,
                },
            ),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        out_of_scope.contains("manager_v2_action_out_of_scope"),
        "{out_of_scope}"
    );

    // Missing the original action's capability, then missing LeadControl.
    let mut version_policy = 1;
    for missing in [
        ManagerCapabilityV2::SessionCreate,
        ManagerCapabilityV2::LeadControl,
    ] {
        let mut policy = p.policy.clone();
        policy.capabilities.retain(|c| *c != missing);
        p.manager
            .store
            .lock()
            .await
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: p.project,
                expected_scope_version: 1,
                expected_policy_version: version_policy,
                idempotency_key: format!("without-{missing:?}"),
                policy,
            })
            .unwrap();
        version_policy += 1;
        let denied = control
            .agent_manager_control(
                p.owner,
                settle(&format!("denied-{missing:?}"), 1, version_policy),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(denied.contains("manager_v2_capability_denied"), "{denied}");
    }
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: version_policy,
            idempotency_key: "restored".into(),
            policy: p.policy.clone(),
        })
        .unwrap();
    version_policy += 1;
    // Revoked scope: a queued settlement cannot act under a changed scope.
    let queued = control
        .agent_manager_control(p.owner, settle("queued", 1, version_policy))
        .await
        .unwrap();
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: vec![p.group],
            project_id: p.project,
            session_id: p.owner,
            epic_ids: None,
            expected_row_version: 1,
        })
        .unwrap();
    p.reconcile_until_claimed(queued.operation_id).await;
    let revoked = p.receipt(queued.operation_id).await;
    assert_eq!(revoked.state, ManagerActionStateV2::Revoked, "{revoked:?}");
    let original = p.receipt(create).await;
    assert_eq!(original.state, ManagerActionStateV2::Uncertain);
    assert_eq!(original.row_version, version);
}

/// Completed predecessor + agent-declared human_gate + disabled guard +
/// enabled resume wake: assign is blocked until the manager retires the
/// stale continuations; then a live Epic worker becomes the single lead.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_recovery_390_retire_then_assign_live_worker_single_lead() {
    let p = pilot().await;
    let worker = p
        .admit(
            "worker",
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Feature,
                query: "audit work".into(),
                launch: p.policy.allowed_launches[0].clone(),
                sandbox_source: None,
            },
        )
        .await;
    let worker_id = worker.target_session_id.unwrap();
    let _process = super::super::launch::install_controller_candidate_test_process(worker_id);
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(worker.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let sentinel = manager_program_job(&p, "program_guard", false);
    let wake = manager_program_job(&p, "resume", true);
    {
        let store = p.manager.store.lock().await;
        store
            .update_session_status(p.lead, SessionStatus::Completed)
            .unwrap();
        store.insert_scheduled_job(&sentinel).unwrap();
        store.insert_scheduled_job(&wake).unwrap();
    }
    p.append_lead_output(Role::User, "Continue the authorized program.")
        .await;
    p.append_lead_output(Role::Assistant, HUMAN_GATE_OUTCOME)
        .await;
    let stale = p.fence().await;
    let blocked = p
        .admit(
            "assign-blocked",
            ManagerActionV2::AssignLead {
                epic_id: p.epic,
                expected: stale.clone(),
                session_id: Some(worker_id),
            },
        )
        .await;
    p.reconcile_until_claimed(blocked.operation_id).await;
    let blocked = p.receipt(blocked.operation_id).await;
    assert_eq!(blocked.state, ManagerActionStateV2::Blocked);
    assert_eq!(
        blocked.outcome.as_deref(),
        Some("manager_v2_human_or_recovery_owner")
    );

    let retire = p.retire("retire").await.unwrap();
    assert_eq!(
        retire.action_kind,
        ManagerActionKindV2::RetireLeadContinuations
    );
    p.execute().await.unwrap();
    let retired = p.receipt(retire.operation_id).await;
    assert_eq!(retired.state, ManagerActionStateV2::Succeeded);
    assert_eq!(
        retired.result,
        Some(ManagerActionResultV2::LeadContinuationsRetired)
    );
    {
        let store = p.manager.store.lock().await;
        // Disabled, never deleted.
        let job = store.get_scheduled_job(&wake.id).unwrap().unwrap();
        assert!(!job.enabled);
        assert_eq!(job.wake_session_id, Some(p.lead));
        assert!(store.scheduled_job_exists(&sentinel.id).unwrap());
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        let record = store
            .manager_v2_record(&config, LEAD_RETIREMENT_KIND, &p.lead.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(record.payload["superseded"], "human_or_recovery_owner");
        assert_eq!(
            record.payload["operation_id"],
            serde_json::json!(retire.operation_id)
        );
        assert_eq!(
            record.payload["disabled_job_ids"],
            serde_json::json!([wake.id.to_string()])
        );
        let audited: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_v2_events WHERE kind='lead_continuations_retired' AND record_key=?1",
                [retire.operation_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(audited, 1);
        store.manager_action_human_gate(p.lead).unwrap();
    }

    p.admit(
        "assign",
        ManagerActionV2::AssignLead {
            epic_id: p.epic,
            expected: p.fence().await,
            session_id: Some(worker_id),
        },
    )
    .await;
    p.execute().await.unwrap();
    let epic = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.epic)
        .unwrap()
        .unwrap();
    assert_eq!(epic.lead_session_id, Some(worker_id));
    // Single-lead CAS: the pre-assignment fence cannot install a second lead.
    let stale_error = p
        .manager
        .agent_control()
        .agent_manager_control(
            p.owner,
            p.request(
                "stale",
                ManagerActionV2::AssignLead {
                    epic_id: p.epic,
                    expected: stale,
                    session_id: Some(p.lead),
                },
            ),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        stale_error.contains("manager_v2_lead_changed"),
        "{stale_error}"
    );
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(p.epic)
            .unwrap()
            .unwrap()
            .lead_session_id,
        Some(worker_id)
    );
    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, worker_id)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(worker_id);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_recovery_retirement_admission_pauses_intent_until_explicit_resume() {
    let p = pilot().await;
    let policy_version = p
        .manager
        .store
        .lock()
        .await
        .get_harness_manager_policy(p.project)
        .unwrap()
        .unwrap()
        .row_version;
    p.manager
        .agent_control()
        .agent_manager_update(
            p.owner,
            AgentManagerUpdateRequestV2 {
                project_id: None,
                fence: ManagerFenceV2 {
                    scope_version: 1,
                    policy_version,
                },
                idempotency_key: "retirement-gap-work".into(),
                change: ManagerUpdateV2::Work {
                    key: "retirement-gap-work".into(),
                    expected_row_version: 0,
                    epic_id: p.epic,
                    title: "unfinished retirement-gap work".into(),
                    kind: ManagerWorkKindV2::Product,
                    priority: 1,
                    weight: 1,
                    required_gates: vec![
                        ManagerWorkStageV2::Implementation,
                        ManagerWorkStageV2::Review,
                        ManagerWorkStageV2::Verification,
                    ],
                    risk_tier: Default::default(),
                },
            },
        )
        .await
        .unwrap();

    manager_program_status(&p, SessionStatus::Failed).await;
    let sentinel = manager_program_job(&p, "program_guard", false);
    let wake = manager_program_job(&p, "resume", true);
    {
        let store = p.manager.store.lock().await;
        store.insert_scheduled_job(&sentinel).unwrap();
        store.insert_scheduled_job(&wake).unwrap();
    }
    p.append_lead_output(Role::User, "Continue the authorized program.")
        .await;
    p.append_lead_output(Role::Assistant, HUMAN_GATE_OUTCOME)
        .await;

    // Retirement admission establishes the durable pause before its queued
    // effect runs, closing the window in which Execute intent could recover.
    let retire = p.retire("retire-with-gap-pause").await.unwrap();
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        assert!(store.manager_v2_lead_pause(&config, p.epic).unwrap().1);
    }
    let intent = p
        .manager
        .store
        .lock()
        .await
        .reconcile_manager_intent(p.project, true)
        .unwrap();
    assert_eq!(intent.queued, 0);
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        let intent = store
            .manager_v2_record(&config, "intent", &p.epic.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(intent.payload["state"], "manager_paused");
        let recovery_actions: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_v2_operations WHERE project_id=?1 AND kind='lifecycle_action' AND json_extract(payload_json,'$.origin.origin')='operating_intent'",
                [p.project.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recovery_actions, 0);
    }

    p.execute().await.unwrap();
    let retired = p.receipt(retire.operation_id).await;
    assert_eq!(retired.state, ManagerActionStateV2::Succeeded);
    assert_eq!(
        retired.result,
        Some(ManagerActionResultV2::LeadContinuationsRetired)
    );
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        assert!(store.manager_v2_lead_pause(&config, p.epic).unwrap().1);
        store.manager_action_human_gate(p.lead).unwrap();
    }

    let resume = p
        .admit(
            "explicit-resume-after-retirement",
            ManagerActionV2::ResumeLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "manager explicitly resumes after retirement".into(),
            },
        )
        .await;
    let process = super::super::launch::install_controller_candidate_test_process(p.lead);
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(resume.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        assert!(!store.manager_v2_lead_pause(&config, p.epic).unwrap().1);
    }
    p.stop_session(p.lead).await;
    drop(process);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn manager_recovery_retire_resolves_unknown_program_evidence() {
    let p = pilot().await;
    manager_program_status(&p, SessionStatus::Interrupted).await;
    let sentinel = manager_program_job(&p, "program_guard", true);
    {
        let store = p.manager.store.lock().await;
        store.insert_scheduled_job(&sentinel).unwrap();
        // Unparseable sentinel evidence.
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET schedule_json='{' WHERE id=?1",
                [sentinel.id.to_string()],
            )
            .unwrap();
    }
    p.append_lead_output(Role::User, "Continue.").await;
    p.append_lead_output(Role::Assistant, "orchestration_outcome_v1: {")
        .await;
    let before = p
        .manager
        .store
        .lock()
        .await
        .manager_action_human_gate(p.lead)
        .unwrap_err()
        .to_string();
    assert!(
        before.contains("manager_v2_program_evidence_unknown"),
        "{before}"
    );
    let retire = p.retire("retire-unknown").await.unwrap();
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(retire.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let store = p.manager.store.lock().await;
    store.manager_action_human_gate(p.lead).unwrap();
    let enabled: bool = store
        .conn
        .query_row(
            "SELECT enabled FROM scheduled_jobs WHERE id=?1",
            [sentinel.id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!enabled);
    let config = store.get_harness_manager(p.project).unwrap().unwrap();
    let record = store
        .manager_v2_record(&config, LEAD_RETIREMENT_KIND, &p.lead.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(record.payload["superseded"], "program_evidence_unknown");
    drop(store);
    // Supersession is exact: later lead output restores normal evaluation
    // of the (still malformed) sentinel evidence.
    p.append_lead_output(Role::Assistant, "more output").await;
    let after = p
        .manager
        .store
        .lock()
        .await
        .manager_action_human_gate(p.lead)
        .unwrap_err()
        .to_string();
    assert!(
        after.contains("manager_v2_program_evidence_unknown"),
        "{after}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn manager_recovery_retire_refuses_genuine_operator_gates() {
    for case in ["question", "approval", "operator_pause"] {
        let p = pilot().await;
        p.append_lead_output(Role::Assistant, HUMAN_GATE_OUTCOME)
            .await;
        {
            let store = p.manager.store.lock().await;
            match case {
                "question" => {
                    store
                        .conn
                        .execute(
                            "UPDATE sessions SET pending_question_json='{\"question\":\"Deploy?\"}' WHERE id=?1",
                            [p.lead.to_string()],
                        )
                        .unwrap();
                }
                "approval" => store
                    .insert_approval(&rsi_common::types::Approval {
                        id: Uuid::new_v4(),
                        session_id: p.lead,
                        tool_name: "operator decision".into(),
                        tool_input: serde_json::json!({"operation":"deploy"}),
                        status: rsi_common::types::ApprovalStatus::Pending,
                        created_at: chrono::Utc::now(),
                        resolved_at: None,
                    })
                    .unwrap(),
                _ => store.record_manager_operator_pause(p.lead, true).unwrap(),
            }
        }
        let refused = p.retire(case).await.unwrap_err().to_string();
        assert!(
            refused.contains("manager_v2_human_or_recovery_owner"),
            "{case}: {refused}"
        );
        let store = p.manager.store.lock().await;
        let queued: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_v2_operations WHERE json_extract(payload_json,'$.request.operation.action')='retire_lead_continuations'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(queued, 0, "{case}: refusal changes nothing");
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        assert!(
            store
                .manager_v2_record(&config, LEAD_RETIREMENT_KIND, &p.lead.to_string())
                .unwrap()
                .is_none()
        );
        let gate = store
            .manager_action_human_gate(p.lead)
            .unwrap_err()
            .to_string();
        assert!(
            gate.contains("manager_v2_human_or_recovery_owner"),
            "{case}"
        );
    }
    // A gate appearing after admission is rechecked at the effect boundary.
    let p = pilot().await;
    let retire = p.retire("late-question").await.unwrap();
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET pending_question_json='{\"question\":\"Deploy?\"}' WHERE id=?1",
            [p.lead.to_string()],
        )
        .unwrap();
    p.reconcile_until_claimed(retire.operation_id).await;
    let receipt = p.receipt(retire.operation_id).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Blocked);
    assert_eq!(
        receipt.outcome.as_deref(),
        Some("manager_v2_human_or_recovery_owner")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn manager_recovery_evidence_survives_reopen_and_replays() {
    let p = pilot().await;
    p.append_lead_output(Role::Assistant, HUMAN_GATE_OUTCOME)
        .await;
    let retire_request = p.request(
        "retire",
        ManagerActionV2::RetireLeadContinuations {
            epic_id: p.epic,
            expected: p.fence().await,
        },
    );
    let retire = p
        .manager
        .agent_control()
        .agent_manager_control(p.owner, retire_request.clone())
        .await
        .unwrap();
    p.execute().await.unwrap();
    let (create, version) = p
        .uncertain(
            "create",
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Feature,
                query: "work".into(),
                launch: p.policy.allowed_launches[0].clone(),
                sandbox_source: None,
            },
            "execution_owner_lost_unconfirmed",
        )
        .await;
    let settle_request = p.request(
        "settle",
        ManagerActionV2::SettleUncertainAction {
            operation_id: create,
            expected_row_version: version,
        },
    );
    let settle = p
        .manager
        .agent_control()
        .agent_manager_control(p.owner, settle_request.clone())
        .await
        .unwrap();
    p.execute().await.unwrap();

    let reopened = crate::store::Store::open(&p._dir.path().join("rsi.db")).unwrap();
    reopened.manager_action_human_gate(p.lead).unwrap();
    let settled = reopened.manager_action_operation(create).unwrap().unwrap();
    assert_eq!(settled.receipt.state, ManagerActionStateV2::Failed);
    let evidence: i64 = reopened
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_events WHERE kind='action_settled' AND record_key=?1",
            [create.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(evidence, 1);
    for (request, original) in [(retire_request, retire), (settle_request, settle)] {
        let replay = reopened
            .enqueue_manager_action(ManagerActionOriginV2::Agent { caller: p.owner }, request)
            .unwrap();
        assert_eq!(replay.operation_id, original.operation_id);
        assert_eq!(replay.state, ManagerActionStateV2::Succeeded);
        assert!(replay.deduplicated);
    }
}

/// Review (a): a Resume wake captured by the scheduler before retirement
/// commits is revalidated under the lead's spawn guard and cannot restart the
/// retired lead.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_recovery_retired_resume_snapshot_cannot_restart_lead() {
    use crate::issue_tracker::poller::SessionLauncher;
    let p = pilot().await;
    let wake = manager_program_job(&p, "resume", true);
    p.manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&wake)
        .unwrap();
    // The scheduler's due-list snapshot, taken before the retirement.
    let captured = p
        .manager
        .store
        .lock()
        .await
        .get_scheduled_job(&wake.id)
        .unwrap()
        .unwrap();
    assert!(captured.enabled);
    let process = super::super::launch::install_controller_candidate_test_process(p.lead);
    let retire = p.retire("retire-before-dispatch").await.unwrap();
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(retire.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let refused = SessionLauncher::resume_scheduled_job(
        &p.manager,
        p.lead,
        "scheduled wake".into(),
        vec![captured.id],
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        refused.contains("scheduled_wake_retired_or_disabled"),
        "{refused}"
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    let store = p.manager.store.lock().await;
    let lead = store.get_session(p.lead).unwrap().unwrap();
    assert_eq!(lead.status, SessionStatus::Completed);
    let retained = store.get_scheduled_job(&wake.id).unwrap().unwrap();
    assert!(!retained.enabled);
    drop(store);
    super::super::launch::drop_controller_candidate_test_process(p.lead);
}

/// Review (b): an agent-armed child watch owned by the old lead is retired
/// with it; the child terminating afterwards does not wake the old lead, even
/// from a watch snapshot captured before the retirement.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_recovery_retired_child_watch_does_not_wake_old_lead() {
    use crate::issue_tracker::poller::WatchFireOutcome;
    let p = pilot().await;
    let watch = manager_program_job(&p, "on_terminal", true);
    let rsi_common::types::WakeMode::OnTerminal(child) = watch.wake_mode else {
        panic!("fixture child watch");
    };
    {
        let store = p.manager.store.lock().await;
        let mut row = bare_session(child);
        row.project_id = Some(p.project);
        row.parent_id = Some(p.epic);
        row.session_kind = SessionKind::Feature;
        row.status = SessionStatus::Running;
        store.insert_session(&row).unwrap();
        store.insert_scheduled_job(&watch).unwrap();
    }
    let captured = p
        .manager
        .store
        .lock()
        .await
        .get_scheduled_job(&watch.id)
        .unwrap()
        .unwrap();
    let process = super::super::launch::install_controller_candidate_test_process(p.lead);
    let retire = p.retire("retire-watch").await.unwrap();
    p.execute().await.unwrap();
    {
        let store = p.manager.store.lock().await;
        let retained = store.get_scheduled_job(&watch.id).unwrap().unwrap();
        assert!(!retained.enabled);
        assert_eq!(retained.wake_session_id, Some(p.lead));
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        let record = store
            .manager_v2_record(&config, LEAD_RETIREMENT_KIND, &p.lead.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(
            record.payload["disabled_job_ids"],
            serde_json::json!([watch.id.to_string()])
        );
        assert_eq!(
            record.payload["operation_id"],
            serde_json::json!(retire.operation_id)
        );
        store
            .update_session_status(child, SessionStatus::Completed)
            .unwrap();
    }
    let outcome = p.manager.fire_terminal_watch(&captured).await.unwrap();
    assert!(matches!(outcome, WatchFireOutcome::NotReady), "{outcome:?}");
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(p.lead)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Completed
    );
    super::super::launch::drop_controller_candidate_test_process(p.lead);
}

/// Review (c): settling a settlement resolves the nested operation and
/// checks it against the current manager scope before admission.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn manager_recovery_nested_settlement_is_scope_checked() {
    let p = pilot().await;
    let (create, version) = p
        .uncertain(
            "create",
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Feature,
                query: "work".into(),
                launch: p.policy.allowed_launches[0].clone(),
                sandbox_source: None,
            },
            "execution_owner_lost_unconfirmed",
        )
        .await;
    let (settle, settle_version) = p
        .uncertain(
            "settle",
            ManagerActionV2::SettleUncertainAction {
                operation_id: create,
                expected_row_version: version,
            },
            "execution_owner_lost_unconfirmed",
        )
        .await;
    // Rescope the manager to a different Epic of the same project.
    let other_epic = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        let mut row = bare_session(other_epic);
        row.project_id = Some(p.project);
        row.working_dir = p.repo.clone();
        row.session_kind = SessionKind::Epic;
        row.parent_id = Some(p.group);
        store.insert_session(&row).unwrap();
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: p.project,
                session_id: p.owner,
                epic_ids: Some(vec![other_epic]),
                expected_row_version: 1,
            })
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: p.project,
                expected_scope_version: 2,
                expected_policy_version: 1,
                idempotency_key: "rescoped".into(),
                policy: ManagerPolicyV2 {
                    group_ids: Vec::new(),
                    ..p.policy.clone()
                },
            })
            .unwrap();
    }
    let policy_version = p
        .manager
        .store
        .lock()
        .await
        .get_harness_manager_policy(p.project)
        .unwrap()
        .unwrap()
        .row_version;
    let nested = p
        .manager
        .agent_control()
        .agent_manager_control(
            p.owner,
            fenced(
                "nested",
                2,
                policy_version,
                ManagerActionV2::SettleUncertainAction {
                    operation_id: settle,
                    expected_row_version: settle_version,
                },
            ),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(nested.contains("out_of_scope"), "{nested}");
    let store = p.manager.store.lock().await;
    let retained = store.manager_action_operation(settle).unwrap().unwrap();
    assert_eq!(retained.receipt.state, ManagerActionStateV2::Uncertain);
    assert_eq!(retained.receipt.row_version, settle_version);
}

/// Review round 2: a wake armed by a rotation predecessor resolves through
/// the lineage chase to the retired lead and must not restart it after the
/// manager assigned a replacement. The scheduler settles the job as usual.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_recovery_predecessor_wake_cannot_restart_retired_lineage_tip() {
    use crate::issue_tracker::poller::SessionLauncher;
    let p = pilot().await;
    // L1 (the pilot lead) arms a resume wake, then rotates to L2.
    let wake = manager_program_job(&p, "resume", true);
    let successor = Uuid::new_v4();
    let replacement = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        store.insert_scheduled_job(&wake).unwrap();
        for (id, continued_from) in [(successor, Some(p.lead)), (replacement, None)] {
            let mut row = bare_session(id);
            row.project_id = Some(p.project);
            row.working_dir = p.repo.clone();
            row.session_kind = SessionKind::Feature;
            row.parent_id = Some(p.epic);
            row.provider = SessionProvider::Claude;
            row.model = Some("manager-scripted-provider".into());
            row.claude_session_id = Some(format!("provider-{id}"));
            row.continued_from = continued_from;
            store.insert_session(&row).unwrap();
        }
        store.set_lead_session(p.epic, Some(successor)).unwrap();
    }
    assert_eq!(
        p.manager.resolve_wake_tip(p.lead).await.unwrap(),
        Some(successor)
    );
    // The manager retires L2 and assigns a replacement lead.
    let retire = p.retire("retire-tip").await.unwrap();
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(retire.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let assign = p
        .admit(
            "assign-replacement",
            ManagerActionV2::AssignLead {
                epic_id: p.epic,
                expected: p.fence().await,
                session_id: Some(replacement),
            },
        )
        .await;
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(assign.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let captured = {
        let store = p.manager.store.lock().await;
        let job = store.get_scheduled_job(&wake.id).unwrap().unwrap();
        // Named for L1, so retirement of L2 did not disable it.
        assert!(job.enabled);
        assert_eq!(job.wake_session_id, Some(p.lead));
        job
    };
    let process = super::super::launch::install_controller_candidate_test_process(successor);
    // Typed refusal at the guarded dispatch boundary.
    let refused = SessionLauncher::resume_scheduled_job(
        &p.manager,
        p.lead,
        "scheduled wake".into(),
        vec![captured.id],
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        refused.contains("scheduled_wake_target_retired"),
        "{refused}"
    );
    // The real scheduler path refuses and settles the one-shot job.
    let Pilot {
        manager,
        _dir: dir,
        epic,
        ..
    } = p;
    let manager = std::sync::Arc::new(manager);
    let launcher: std::sync::Arc<dyn SessionLauncher> = manager.clone();
    crate::scheduler::fire_job_for_test(&manager.store, manager.event_bus(), &launcher, &captured)
        .await;
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    {
        let store = manager.store.lock().await;
        let settled = store.get_scheduled_job(&wake.id).unwrap().unwrap();
        assert!(!settled.enabled);
        assert!(settled.last_fired_at.is_some());
        assert_eq!(
            store.get_session(epic).unwrap().unwrap().lead_session_id,
            Some(replacement)
        );
        assert_eq!(
            store.get_session(successor).unwrap().unwrap().status,
            SessionStatus::Completed
        );
    }
    super::super::launch::drop_controller_candidate_test_process(successor);
    drop(dir);
}

/// #652: a Completed lead whose sandbox tuple is a historical cleanup failure
/// (the on-disk root is only a path-only startup candidate) refuses its
/// scheduled resume with the typed `sandbox_custody:cleanup_failed` code. The
/// refusal must reach the manager as a durable notice naming the code, the
/// sandbox directory must survive, and no provider process may start.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn path_only_sandbox_lead_resume_refusal_reaches_the_manager_as_a_typed_notice() {
    use crate::issue_tracker::poller::SessionLauncher;
    let p = pilot().await;
    let wake = manager_program_job(&p, "resume", true);
    let (base, path_only_root) = {
        let base = p.manager.sandbox_allocator.base_dir().to_path_buf();
        let root = base.join(p.lead.to_string());
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("unmerged-work"), "operator data\n").unwrap();
        (base, root)
    };
    {
        let store = p.manager.store.lock().await;
        store.insert_scheduled_job(&wake).unwrap();
        store
            .update_session_status(p.lead, SessionStatus::Completed)
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET sandbox_kind='GitWorktree', sandbox_root=NULL, sandbox_branch=NULL, sandbox_cleanup_state='Failed' WHERE id=?1",
                [p.lead.to_string()],
            )
            .unwrap();
        let row = store.get_session(p.lead).unwrap().unwrap();
        drop(store);
        p.manager
            .completed
            .write()
            .await
            .get_mut(&p.lead)
            .unwrap()
            .session = row;
    }
    // The restart's custody pass retains the path-only candidate.
    p.manager.sandbox_orphan_sweep().await.unwrap();
    assert!(path_only_root.join("unmerged-work").exists(), "{base:?}");

    let Pilot {
        manager,
        _dir: dir,
        lead,
        ..
    } = p;
    let process = super::super::launch::install_controller_candidate_test_process(lead);
    let manager = std::sync::Arc::new(manager);
    let launcher: std::sync::Arc<dyn SessionLauncher> = manager.clone();
    crate::scheduler::fire_job_for_test(&manager.store, manager.event_bus(), &launcher, &wake)
        .await;

    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    assert!(path_only_root.join("unmerged-work").exists());
    let store = manager.store.lock().await;
    let (kind, state, settled, retired): (String, String, Option<String>, Option<String>) = store
        .conn
        .query_row(
            "SELECT kind,state_json,settled_at,retired_at FROM harness_manager_notices
             WHERE subject_id=?1 AND subject_version='custody_refused:cleanup_failed'",
            [lead.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("typed custody refusal notice exists for the lead");
    assert_eq!(kind, "session_state");
    assert_eq!((settled, retired), (None, None));
    let state: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert_eq!(
        state["recovery_code"].as_str(),
        Some("sandbox_custody:cleanup_failed")
    );
    assert_eq!(state["next_action"].as_str(), Some("inspect_lead"));
    // A second firing of the same refusal never duplicates the notice.
    let error = crate::error::sandbox_custody_error(rsi_common::types::SandboxCustodyErrorV1 {
        version: 1,
        code: rsi_common::types::SandboxCustodyErrorCodeV1::CleanupFailed,
        session_id: Some(lead),
        transition: rsi_common::types::SandboxCustodyTransitionV1::ResumeWake,
        retryable: false,
        recovery: rsi_common::types::SandboxCustodyRecoveryV1::InspectStatus,
    });
    assert!(
        store
            .record_manager_custody_refusal_notice(lead, &error)
            .unwrap()
    );
    let count: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_notices WHERE subject_id=?1 AND subject_version LIKE 'custody_refused:%'",
            [lead.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    // A transient reclaim gate is deferred by the scheduler, never noticed.
    let transient = crate::error::sandbox_custody_error(rsi_common::types::SandboxCustodyErrorV1 {
        version: 1,
        code: rsi_common::types::SandboxCustodyErrorCodeV1::ReclaimPrepared,
        session_id: Some(lead),
        transition: rsi_common::types::SandboxCustodyTransitionV1::ResumeWake,
        retryable: true,
        recovery: rsi_common::types::SandboxCustodyRecoveryV1::RetryAfterReconcile,
    });
    assert!(
        !store
            .record_manager_custody_refusal_notice(lead, &transient)
            .unwrap()
    );
    drop(store);
    super::super::launch::drop_controller_candidate_test_process(lead);
    drop(dir);
}

impl Pilot {
    /// Record the keyed invocation of an uncertain lifecycle action exactly as
    /// launch settlement leaves it: failed, admitted, with the given class and
    /// optional provider usage (tokens and wall time) evidence.
    async fn failed_action_invocation(&self, id: Uuid, error_class: &str, usage: Option<i64>) {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        self.manager.store.lock().await.conn.execute(
            "INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,dedup_key,policy_snapshot_json,usage_confidence,error_class,input_tokens,output_tokens,wall_time_ms,created_at,completed_at)
             VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable','admitted','failed','manager-recovery-test',?2,?3,'{}',?4,?5,?6,?6,?6,?7,?7)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                self.lead.to_string(),
                format!("manager.action:{id}"),
                if usage.is_some() { "partial" } else { "unavailable" },
                error_class,
                usage,
                now,
            ],
        ).unwrap();
    }

    /// Uncertain resume of the idle lead, then a queued fresh-boundary retry
    /// for the same Epic that is past its delay but fenced by the resume.
    async fn uncertain_resume_with_fenced_retry(
        &self,
        error_class: Option<&str>,
        usage: Option<i64>,
    ) -> (Uuid, i64, Uuid) {
        let (resume, version) = self
            .uncertain(
                "resume",
                ManagerActionV2::ResumeLead {
                    epic_id: self.epic,
                    expected: self.fence().await,
                    message: "continue after reboot".into(),
                },
                "manager_v2_lifecycle_unconfirmed",
            )
            .await;
        if let Some(error_class) = error_class {
            self.failed_action_invocation(resume, error_class, usage)
                .await;
        }
        super::manager_program_status(self, SessionStatus::Failed).await;
        let retry = self
            .admit(
                "retry",
                ManagerActionV2::RetryLead {
                    epic_id: self.epic,
                    expected: self.fence().await,
                    message: "retry from a fresh boundary".into(),
                    launch: None,
                },
            )
            .await;
        assert_eq!(retry.state, ManagerActionStateV2::Queued);
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let fenced = self
            .manager
            .store
            .lock()
            .await
            .claim_manager_action(self.manager.program_run_boot_id)
            .unwrap();
        assert!(fenced.is_none(), "uncertain resume must fence the retry");
        (resume, version, retry.operation_id)
    }

    async fn claim_retry_and_release(&self, retry: Uuid) {
        let claim = self.claim().await;
        assert_eq!(claim.id(), retry);
        self.manager
            .store
            .lock()
            .await
            .finish_manager_action(&claim, ManagerActionStateV2::Blocked, "test_settled")
            .unwrap();
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn manager_recovery_prespawn_refused_resume_settles_absent_via_settle_action() {
    let p = pilot().await;
    let (resume, version, retry) = p
        .uncertain_resume_with_fenced_retry(
            Some(crate::codex::CODEX_TOOL_HISTORY_ERROR_CLASS),
            None,
        )
        .await;
    let settle = p
        .admit(
            "settle-resume",
            ManagerActionV2::SettleUncertainAction {
                operation_id: resume,
                expected_row_version: version,
            },
        )
        .await;
    p.execute().await.unwrap();
    let own = p.receipt(settle.operation_id).await;
    assert_eq!(own.state, ManagerActionStateV2::Succeeded, "{own:?}");
    let settled = p.receipt(resume).await;
    assert_eq!(settled.state, ManagerActionStateV2::Failed);
    assert_eq!(
        settled.outcome.as_deref(),
        Some("manager_v2_recovered_effect_absent")
    );
    let evidence = p.settled_evidence(resume).await;
    assert_eq!(
        evidence["witness"]["error_class"],
        crate::codex::CODEX_TOOL_HISTORY_ERROR_CLASS
    );
    assert_eq!(evidence["witness"]["provider_started"], false);
    assert_eq!(
        evidence["settled_by_operation_id"],
        serde_json::json!(settle.operation_id)
    );
    p.claim_retry_and_release(retry).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn manager_recovery_keyed_tool_history_refusal_auto_settles_and_unfences_retry() {
    let p = pilot().await;
    let (resume, version, retry) = p
        .uncertain_resume_with_fenced_retry(
            Some(crate::codex::CODEX_TOOL_HISTORY_ERROR_CLASS),
            None,
        )
        .await;
    p.manager.reconcile_uncertain_lead_actions().await.unwrap();
    let settled = p.receipt(resume).await;
    assert_eq!(settled.state, ManagerActionStateV2::Failed);
    assert_eq!(settled.row_version, version + 1);
    assert_eq!(
        settled.outcome.as_deref(),
        Some("manager_v2_recovered_effect_absent")
    );
    let evidence = p.settled_evidence(resume).await;
    assert_eq!(evidence["witness"]["reconciled_by"], "daemon");
    assert_eq!(
        evidence["witness"]["error_class"],
        crate::codex::CODEX_TOOL_HISTORY_ERROR_CLASS
    );
    p.claim_retry_and_release(retry).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn manager_recovery_resume_without_keyed_invocation_waits_for_explicit_settlement() {
    let p = pilot().await;
    let (resume, version, retry) = p.uncertain_resume_with_fenced_retry(None, None).await;
    // A missing invocation is not enough for automatic absence after restart.
    p.manager.reconcile_uncertain_lead_actions().await.unwrap();
    let still = p.receipt(resume).await;
    assert_eq!(still.state, ManagerActionStateV2::Uncertain);
    assert_eq!(still.row_version, version);
    let settle = p
        .admit(
            "settle-resume",
            ManagerActionV2::SettleUncertainAction {
                operation_id: resume,
                expected_row_version: version,
            },
        )
        .await;
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(settle.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let settled = p.receipt(resume).await;
    assert_eq!(settled.state, ManagerActionStateV2::Failed);
    assert_eq!(settled.row_version, version + 1);
    assert_eq!(
        settled.outcome.as_deref(),
        Some("manager_v2_recovered_effect_absent")
    );
    let evidence = p.settled_evidence(resume).await;
    assert!(evidence["witness"]["invocation"].is_null());
    p.claim_retry_and_release(retry).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn manager_recovery_resume_that_reached_provider_stays_unobservable() {
    let p = pilot().await;
    // The provider ran and the session exit settled the invocation.
    let (resume, version, _retry) = p
        .uncertain_resume_with_fenced_retry(Some("failed"), Some(7))
        .await;
    p.manager.reconcile_uncertain_lead_actions().await.unwrap();
    let still = p.receipt(resume).await;
    assert_eq!(still.state, ManagerActionStateV2::Uncertain);
    assert_eq!(still.row_version, version);
    // A pre-spawn class with provider usage evidence is not proof of absence.
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE model_invocations SET error_class=?2 WHERE dedup_key=?1",
            rusqlite::params![
                format!("manager.action:{resume}"),
                crate::codex::CODEX_TOOL_HISTORY_ERROR_CLASS
            ],
        )
        .unwrap();
    p.manager.reconcile_uncertain_lead_actions().await.unwrap();
    let still = p.receipt(resume).await;
    assert_eq!(still.state, ManagerActionStateV2::Uncertain);
    assert_eq!(still.row_version, version);
    // Wall time alone is also evidence that provider work may have started.
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE model_invocations SET input_tokens=NULL,output_tokens=NULL,usage_confidence='unavailable' WHERE dedup_key=?1",
            [format!("manager.action:{resume}")],
        )
        .unwrap();
    p.manager.reconcile_uncertain_lead_actions().await.unwrap();
    let still = p.receipt(resume).await;
    assert_eq!(still.state, ManagerActionStateV2::Uncertain);
    assert_eq!(still.row_version, version);
    let refused = p
        .admit(
            "settle-reached-provider",
            ManagerActionV2::SettleUncertainAction {
                operation_id: resume,
                expected_row_version: version,
            },
        )
        .await;
    p.reconcile_until_claimed(refused.operation_id).await;
    let refused = p.receipt(refused.operation_id).await;
    assert_eq!(refused.state, ManagerActionStateV2::Blocked);
    assert_eq!(
        refused.outcome.as_deref(),
        Some("manager_v2_effect_unobservable")
    );
    assert_eq!(
        p.receipt(resume).await.state,
        ManagerActionStateV2::Uncertain
    );
}
