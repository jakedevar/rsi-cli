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
    p.stop_session(lead).await;
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
