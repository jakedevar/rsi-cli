//! R01/R02 integration tests. Script only the provider; retain real journals,
//! admission, runtime guards, event persistence, custody and restart hydration.
use super::*;
use crate::session::launch;
use serde_json::{Value, json};
use std::time::Duration;

#[tokio::test]
async fn manager_intent_interrupted_partial_program_keeps_automatic_recovery_held() {
    let p = pilot().await;
    manager_program_status(&p, SessionStatus::Interrupted).await;
    manager_program_fixture(&p, "partial", true, false).await;
    work(&p, "interrupted-program").await;
    assert_eq!(reconcile(&p).await.queued, 0);
    assert_eq!(count_actions(&p).await, 0);
    assert!(
        intent(&p).await["reason"]
            .as_str()
            .unwrap()
            .contains("manager_v2_program_evidence_unknown")
    );
}

// A different pilot can hold the process-wide nonblocking reconciler. Its
// no-op result is not evidence that this pilot's due action already settled.
async fn settle_action(p: &Pilot, id: Uuid) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            p.manager.reconcile_manager_actions_once().await.unwrap();
            if !matches!(
                p.receipt(id).await.state,
                ManagerActionStateV2::Queued | ManagerActionStateV2::Running
            ) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the global reconciler eventually settles this exact action");
}

async fn work(p: &Pilot, key: &str) {
    update(
        p,
        key,
        ManagerUpdateV2::Work {
            key: key.into(),
            expected_row_version: 0,
            epic_id: p.epic,
            title: format!("{key} deliverable"),
            kind: ManagerWorkKindV2::Product,
            priority: 1,
            weight: 1,
            required_gates: vec![
                ManagerWorkStageV2::Implementation,
                ManagerWorkStageV2::Review,
                ManagerWorkStageV2::Verification,
            ],
        },
    )
    .await;
}
async fn update(p: &Pilot, key: &str, change: ManagerUpdateV2) {
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
                fence: ManagerFenceV2 {
                    scope_version: 1,
                    policy_version,
                },
                idempotency_key: key.into(),
                change,
            },
        )
        .await
        .unwrap();
}
async fn policy(p: &Pilot, edit: impl FnOnce(&mut ManagerPolicyV2)) {
    let store = p.manager.store.lock().await;
    let grant = store
        .get_harness_manager_policy(p.project)
        .unwrap()
        .unwrap();
    let mut policy = grant.policy;
    edit(&mut policy);
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: grant.row_version,
            idempotency_key: format!("policy-{}", grant.row_version),
            policy,
        })
        .unwrap();
}
async fn reconcile(p: &Pilot) -> crate::store::manager_intent::ManagerIntentReconciliationV2 {
    p.manager
        .store
        .lock()
        .await
        .reconcile_manager_intent(p.project, true)
        .unwrap()
}
async fn intent(p: &Pilot) -> Value {
    let store = p.manager.store.lock().await;
    let config = store.get_harness_manager(p.project).unwrap().unwrap();
    store
        .manager_v2_record(&config, "intent", &p.epic.to_string())
        .unwrap()
        .unwrap()
        .payload
}
async fn count_actions(p: &Pilot) -> i64 {
    p.manager.store.lock().await.conn.query_row(
        "SELECT count(*) FROM harness_manager_v2_operations WHERE project_id=?1 AND kind='lifecycle_action' AND json_extract(payload_json,'$.origin.origin')='operating_intent'",
        [p.project.to_string()], |r| r.get(0)).unwrap()
}
async fn operation(p: &Pilot) -> Uuid {
    let text: String = p.manager.store.lock().await.conn.query_row(
        "SELECT id FROM harness_manager_v2_operations WHERE project_id=?1 AND kind='lifecycle_action' AND json_extract(payload_json,'$.origin.origin')='operating_intent' ORDER BY created_at DESC,id DESC LIMIT 1",
        [p.project.to_string()], |r| r.get(0)).unwrap();
    Uuid::parse_str(&text).unwrap()
}
async fn due(p: &Pilot, id: Uuid) -> String {
    p.manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT not_before FROM harness_manager_v2_operations WHERE id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .unwrap()
}
pub(super) async fn wait_due(p: &Pilot, id: Uuid) {
    let due = chrono::DateTime::parse_from_rfc3339(&due(p, id).await).unwrap();
    if let Ok(wait) = (due.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std() {
        assert!(
            wait < Duration::from_secs(5),
            "fixture uses a bounded real durable delay"
        );
        tokio::time::sleep(wait + Duration::from_millis(10)).await;
    }
}
async fn pause(p: &Pilot, key: &str) -> ManagerActionReceiptV2 {
    p.admit(
        key,
        ManagerActionV2::PauseLead {
            epic_id: p.epic,
            expected: p.fence().await,
            reason: format!("{key}: wait for manager"),
        },
    )
    .await
}
async fn paused(p: &Pilot) -> bool {
    let store = p.manager.store.lock().await;
    let config = store.get_harness_manager(p.project).unwrap().unwrap();
    store.manager_v2_lead_pause(&config, p.epic).unwrap().1
}
async fn wait_terminal(p: &Pilot, id: Uuid, status: SessionStatus) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if !p.manager.active.read().await.contains_key(&id)
                && p.manager
                    .completed
                    .read()
                    .await
                    .get(&id)
                    .is_some_and(|s| s.session.status == status)
                && p.manager.persistence.pending.load(Ordering::SeqCst) == 0
                && p.manager
                    .store
                    .lock()
                    .await
                    .get_session(id)
                    .unwrap()
                    .unwrap()
                    .status
                    == status
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn finish_turn(p: &Pilot, id: Uuid, process: &launch::ControllerCandidateTestProcess) {
    launch::send_controller_candidate_test_event(
        id,
        crate::claude::StreamEvent {
            event_type: "system".into(),
            data: json!({"subtype":"init","session_id":format!("intent-provider-{id}")}),
        },
    )
    .await;
    launch::send_controller_candidate_test_event(id, crate::claude::StreamEvent {
        event_type: "result".into(), data: json!({"subtype":"success", "is_error":false,"result":"ordinary completed turn", "duration_ms":1,"num_turns":1,"total_cost_usd":0.0}),
    }).await;
    process.alive.store(false, Ordering::SeqCst);
    launch::drop_controller_candidate_test_stream(id);
    wait_terminal(p, id, SessionStatus::Completed).await;
}
async fn stop(p: &Pilot, id: Uuid) {
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, id)
        .await
        .unwrap();
    launch::drop_controller_candidate_test_stream(id);
    wait_terminal(p, id, SessionStatus::Interrupted).await;
}
async fn restart(p: &mut Pilot) {
    assert!(p.manager.active.read().await.is_empty());
    assert_eq!(p.manager.persistence.pending.load(Ordering::SeqCst), 0);
    let store = Store::open(&p._dir.path().join("rsi.db")).unwrap();
    let replacement = SessionManager::new(
        std::sync::Arc::new(EventBus::new(16)),
        store,
        false,
        p._dir.path().join("daemon.sock"),
        None,
        Vec::new(),
        RuntimeConfig::from_config(&Config::from_env()),
        p._dir.path().join("sandboxes"),
    )
    .unwrap();
    p.manager = replacement;
    p.manager.restore_sessions().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_intent_empty_launch_list_resumes_current_lead_after_restart_and_obeys_budget() {
    let mut p = pilot().await;
    let receipt = p
        .admit(
            "initial-lead",
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "initial turn".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    p.lead = receipt.target_session_id.unwrap();
    let first = launch::install_controller_candidate_test_process(p.lead);
    p.execute().await.unwrap();
    wait_launch_event(&p, p.lead).await;
    finish_turn(&p, p.lead, &first).await;
    let before = p
        .manager
        .store
        .lock()
        .await
        .live_custody_for_session(p.lead)
        .unwrap();
    std::fs::write(
        std::path::Path::new(&before.sandbox_root).join("unfinished"),
        "retain this work",
    )
    .unwrap();
    policy(&p, |policy| {
        policy.max_recovery_attempts = 1;
        policy.allowed_launches.clear();
    })
    .await;
    work(&p, "product").await;
    assert_eq!(reconcile(&p).await.queued, 1);
    let id = operation(&p).await;
    let timestamp = due(&p, id).await;
    assert_eq!(intent(&p).await["state"], "recovery_action");
    assert_eq!(p.manager.reconcile_manager_actions_once().await.unwrap(), 0);
    for _ in 0..3 {
        assert_eq!(reconcile(&p).await.queued, 0);
    }
    assert_eq!(count_actions(&p).await, 1);
    restart(&mut p).await;
    assert_eq!(due(&p, id).await, timestamp);
    assert_eq!(reconcile(&p).await.queued, 0);
    let resumed = launch::install_controller_candidate_test_process(p.lead);
    wait_due(&p, id).await;
    settle_action(&p, id).await;
    assert_eq!(p.receipt(id).await.state, ManagerActionStateV2::Succeeded);
    assert_eq!(resumed.productive_start_count.load(Ordering::SeqCst), 1);
    assert_eq!(p.manager.reconcile_manager_actions_once().await.unwrap(), 0);
    let after = p
        .manager
        .store
        .lock()
        .await
        .live_custody_for_session(p.lead)
        .unwrap();
    assert_eq!(
        (after.custody_id, after.generation),
        (before.custody_id, before.generation)
    );
    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(&after.sandbox_root).join("unfinished"))
            .unwrap(),
        "retain this work"
    );
    let key: String = p.manager.store.lock().await.conn.query_row(
        "SELECT m.dedup_key FROM sessions s JOIN model_invocations m ON m.id=s.model_invocation_id WHERE s.id=?1",
        [p.lead.to_string()], |r| r.get(0)).unwrap();
    assert_eq!(key, format!("manager.action:{id}"));
    wait_launch_event(&p, p.lead).await;
    finish_turn(&p, p.lead, &resumed).await;
    reconcile(&p).await;
    assert!(
        intent(&p).await["reason"]
            .as_str()
            .unwrap()
            .contains("intent_recovery_budget_exhausted")
    );
    restart(&mut p).await;
    reconcile(&p).await;
    assert_eq!(intent(&p).await["state"], "blocked");
    assert!(
        intent(&p).await["reason"]
            .as_str()
            .unwrap()
            .contains("intent_recovery_budget_exhausted")
    );
    assert_eq!(count_actions(&p).await, 1);
}

#[tokio::test]
async fn manager_intent_empty_launch_list_retries_current_choice_and_requires_known_model() {
    for has_model in [true, false] {
        let p = pilot().await;
        policy(&p, |policy| policy.allowed_launches.clear()).await;
        work(&p, "product").await;
        {
            let store = p.manager.store.lock().await;
            store
                .conn
                .execute(
                    "UPDATE sessions SET status='Failed',model=CASE WHEN ?2 THEN model ELSE NULL END WHERE id=?1",
                    rusqlite::params![p.lead.to_string(), has_model],
                )
                .unwrap();
        }
        let result = reconcile(&p).await;
        if has_model {
            assert_eq!(result.queued, 1);
            let id = operation(&p).await;
            let row = p
                .manager
                .store
                .lock()
                .await
                .manager_action_operation(id)
                .unwrap()
                .unwrap();
            let choice = p.policy.allowed_launches[0].clone();
            assert_eq!(row.context.launch, Some(choice.clone()));
            assert!(matches!(
                row.context.request.operation,
                ManagerActionV2::RetryLead { launch: Some(launch), .. } if launch == choice
            ));
        } else {
            assert_eq!(result.queued, 0);
            assert_eq!(count_actions(&p).await, 0);
            assert!(
                intent(&p).await["reason"]
                    .as_str()
                    .unwrap()
                    .contains("manager_v2_no_admitted_launch_choice")
            );
        }
    }
}

#[tokio::test]
async fn manager_intent_status_and_monitor_preserve_idle_observation_without_execution() {
    for mode in [
        ManagerOperatingModeV2::Status,
        ManagerOperatingModeV2::Monitor,
    ] {
        let p = pilot().await;
        work(&p, "product").await;
        policy(&p, |policy| policy.mode = mode).await;
        reconcile(&p).await;
        assert_eq!(count_actions(&p).await, 0);
        assert_eq!(
            intent(&p).await["state"],
            if mode == ManagerOperatingModeV2::Status {
                "status_only"
            } else {
                "monitoring"
            }
        );
        assert_eq!(reconcile(&p).await.queued, 0);
        assert_eq!(p.manager.reconcile_manager_actions_once().await.unwrap(), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_intent_pause_survives_restart_assignment_and_explicit_manager_resume() {
    let mut p = pilot().await;
    work(&p, "product").await;
    let paused_receipt = pause(&p, "manager-pause").await;
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(paused_receipt.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    for _ in 0..3 {
        assert_eq!(reconcile(&p).await.queued, 0);
        assert_eq!(intent(&p).await["state"], "manager_paused");
    }
    restart(&mut p).await;
    let replacement = Uuid::new_v4();
    let mut lead = bare_session(replacement);
    lead.project_id = Some(p.project);
    lead.parent_id = Some(p.epic);
    lead.session_kind = SessionKind::Feature;
    lead.working_dir = p.repo.clone();
    lead.model = Some("manager-scripted-provider".into());
    lead.provider = SessionProvider::Claude;
    lead.claude_session_id = Some("replacement-provider".into());
    p.manager.store.lock().await.insert_session(&lead).unwrap();
    p.manager
        .completed
        .write()
        .await
        .insert(replacement, CompletedSession::for_test(lead));
    p.admit(
        "assign-while-paused",
        ManagerActionV2::AssignLead {
            epic_id: p.epic,
            expected: p.fence().await,
            session_id: Some(replacement),
        },
    )
    .await;
    p.execute().await.unwrap();
    p.lead = replacement;
    assert_eq!(reconcile(&p).await.queued, 0);
    assert!(paused(&p).await);
    let resume = p
        .admit(
            "explicit-resume",
            ManagerActionV2::ResumeLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "manager explicitly resumes".into(),
            },
        )
        .await;
    let process = launch::install_controller_candidate_test_process(p.lead);
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(resume.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert!(!paused(&p).await);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    stop(&p, p.lead).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_intent_human_pause_survives_manager_resume_and_operator_explicitly_releases_both()
{
    let mut p = pilot().await;
    work(&p, "product").await;
    let initial = launch::install_controller_candidate_test_process(p.lead);
    p.manager
        .continue_session_operator(p.lead, "initial operator turn".into())
        .await
        .unwrap();
    wait_launch_event(&p, p.lead).await;
    p.manager.interrupt_session_operator(p.lead).await.unwrap();
    launch::drop_controller_candidate_test_stream(p.lead);
    wait_terminal(&p, p.lead, SessionStatus::Interrupted).await;
    assert_eq!(initial.productive_start_count.load(Ordering::SeqCst), 1);
    let manager_pause = pause(&p, "manager-pause").await;
    settle_action(&p, manager_pause.operation_id).await;
    assert_eq!(
        p.receipt(manager_pause.operation_id).await.state,
        ManagerActionStateV2::Blocked
    );
    assert_eq!(
        p.receipt(manager_pause.operation_id)
            .await
            .outcome
            .as_deref(),
        Some("manager_v2_human_or_recovery_owner")
    );
    restart(&mut p).await;
    let receipt = p
        .admit(
            "cannot-clear-human-pause",
            ManagerActionV2::ResumeLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "resume".into(),
            },
        )
        .await;
    let process = launch::install_controller_candidate_test_process(p.lead);
    settle_action(&p, receipt.operation_id).await;
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Blocked
    );
    assert_eq!(
        p.receipt(receipt.operation_id).await.outcome.as_deref(),
        Some("manager_v2_human_or_recovery_owner")
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    assert!(paused(&p).await);
    reconcile(&p).await;
    assert_eq!(count_actions(&p).await, 0);
    assert_eq!(intent(&p).await["state"], "operator_paused");
    assert_eq!(intent(&p).await["reason"], "persisted_operator_pause");
    p.manager
        .continue_session_operator(p.lead, "operator resumes".into())
        .await
        .unwrap();
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    assert!(!paused(&p).await);
    p.manager
        .store
        .lock()
        .await
        .manager_action_human_gate(p.lead)
        .unwrap();
    stop(&p, p.lead).await;
}

async fn decision(p: &Pilot) {
    update(
        p,
        "operator-choice",
        ManagerUpdateV2::Decision {
            key: "operator-choice".into(),
            expected_row_version: 0,
            epic_id: p.epic,
            question: "Choose the feature behavior".into(),
            request_id: None,
            work_key: Some("product".into()),
        },
    )
    .await;
}
async fn block_dependencies(p: &Pilot) {
    work(p, "prerequisite").await;
    update(
        p,
        "prerequisite-blocked",
        ManagerUpdateV2::Stage {
            key: "prerequisite".into(),
            expected_row_version: 1,
            stage: ManagerWorkStageV2::Implementation,
            state: ManagerStageStateV2::Blocked,
            note: "upstream implementation required".into(),
            evidence: None,
        },
    )
    .await;
    update(
        p,
        "depends",
        ManagerUpdateV2::Dependency {
            key: "product".into(),
            expected_row_version: 0,
            prerequisite: "prerequisite".into(),
            require_integrated: true,
            enabled: true,
        },
    )
    .await;
}

#[tokio::test]
async fn manager_intent_dependencies_and_decisions_explain_unfinished_idle() {
    for needs_decision in [false, true] {
        let p = pilot().await;
        work(&p, "product").await;
        if needs_decision {
            decision(&p).await;
        } else {
            block_dependencies(&p).await;
        }
        assert_eq!(reconcile(&p).await.queued, 0);
        assert_eq!(count_actions(&p).await, 0);
        let row = intent(&p).await;
        if needs_decision {
            assert_eq!(row["state"], "blocked");
            assert_eq!(row["reason"], "pending_operator_decision");
        } else {
            assert_eq!(row["state"], "dependency_wait");
            assert!(
                row["dependencies"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|w| w["work_key"] == "product")
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_intent_live_decision_pause_and_dependency_races_refuse_at_custody_provider_boundary()
 {
    for gate in ["decision", "pause", "dependency"] {
        let p = pilot().await;
        work(&p, "product").await;
        // A different explicitly granted model requires the normal replacement
        // path, with a real allocated/authenticated sandbox before admission.
        policy(&p, |policy| {
            policy.allowed_launches[0].model = "fallback-scripted-provider".into()
        })
        .await;
        assert_eq!(reconcile(&p).await.queued, 1);
        let id = operation(&p).await;
        let candidate = p.receipt(id).await.target_session_id.unwrap();
        assert_ne!(candidate, p.lead);
        let process = launch::install_controller_candidate_test_process(candidate);
        wait_due(&p, id).await;
        let (reached, resume) =
            launch::install_direct_launch_custody_test_pause(&format!("manager.action:{id}"));
        let change = async {
            assert_eq!(reached.await.unwrap(), candidate);
            match gate {
                "decision" => decision(&p).await,
                "dependency" => block_dependencies(&p).await,
                "pause" => {
                    let mut request = p.request(
                        "racing-manager-pause",
                        ManagerActionV2::PauseLead {
                            epic_id: p.epic,
                            expected: p.fence().await,
                            reason: "pause before effect".into(),
                        },
                    );
                    request.fence.policy_version = 2;
                    p.manager
                        .agent_control()
                        .agent_manager_control(p.owner, request)
                        .await
                        .unwrap();
                }
                _ => unreachable!(),
            }
            // No intent reconciliation here: the effect must query live state.
            resume.send(()).unwrap();
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            let ((), ()) = tokio::join!(settle_action(&p, id), change);
        })
        .await
        .expect("bounded provider race settles");
        let receipt = p.receipt(id).await;
        assert_eq!(
            receipt.state,
            ManagerActionStateV2::Blocked,
            "{gate}: {receipt:?}"
        );
        assert_eq!(
            receipt.outcome.as_deref(),
            Some(match gate {
                "decision" => "manager_v2_pending_operator_decision",
                "dependency" => "manager_v2_no_ready_work",
                _ => "manager_v2_manager_paused",
            })
        );
        assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
        let store = p.manager.store.lock().await;
        assert_eq!(
            store.get_session(p.epic).unwrap().unwrap().lead_session_id,
            Some(p.lead)
        );
        let allocated = store.get_session(candidate).unwrap().unwrap();
        assert_eq!(allocated.status, SessionStatus::Failed);
        assert!(allocated.sandbox_root.unwrap().is_dir());
        let invocation = store
            .session_model_invocation_id(candidate)
            .unwrap()
            .unwrap();
        let status: String = store
            .conn
            .query_row(
                "SELECT status FROM model_invocations WHERE id=?1",
                [invocation.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "failed");
    }
}

#[tokio::test]
async fn manager_intent_new_pause_cannot_be_cleared_by_older_explicit_resume() {
    let p = pilot().await;
    pause(&p, "first-pause").await;
    p.execute().await.unwrap();
    let resume = p
        .admit(
            "old-resume",
            ManagerActionV2::ResumeLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "resume earlier pause".into(),
            },
        )
        .await;
    let claim = p.claim().await;
    p.manager
        .check_manager_action_runtime(&claim, false)
        .await
        .unwrap();
    let newer = pause(&p, "newer-pause").await;
    let error = p.manager.execute_manager_action(&claim).await.unwrap_err();
    assert!(error.to_string().contains("manager_v2_manager_paused"));
    p.manager
        .store
        .lock()
        .await
        .finish_manager_action(
            &claim,
            ManagerActionStateV2::Blocked,
            "manager_v2_manager_paused",
        )
        .unwrap();
    assert!(paused(&p).await);
    assert_eq!(
        p.receipt(resume.operation_id).await.state,
        ManagerActionStateV2::Blocked
    );
    p.execute().await.unwrap();
    let store = p.manager.store.lock().await;
    let config = store.get_harness_manager(p.project).unwrap().unwrap();
    let record = store
        .manager_v2_record(&config, "manager_lead_pause", &p.epic.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(
        record.payload["operation_id"],
        newer.operation_id.to_string()
    );
    assert_eq!(record.payload["paused"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_intent_uncertain_effect_blocks_live_authority_but_survives_explicit_lead_repair_as_evidence()
 {
    let mut p = pilot().await;
    work(&p, "product").await;
    assert_eq!(reconcile(&p).await.queued, 1);
    let lost_id = operation(&p).await;
    wait_due(&p, lost_id).await;
    let claim = p.claim().await;
    // Model crash immediately after the durable effect-intent checkpoint.
    // Recovery cannot assume a missing provider row proves no external effect.
    p.manager
        .check_manager_action_runtime(&claim, true)
        .await
        .unwrap();
    restart(&mut p).await;
    p.manager
        .store
        .lock()
        .await
        .recover_manager_actions_startup(p.manager.program_run_boot_id)
        .unwrap();
    assert_eq!(reconcile(&p).await.queued, 0);
    assert_eq!(
        intent(&p).await["reason"],
        "unconfirmed_effect_for_current_lead"
    );
    let replacement = Uuid::new_v4();
    let mut row = bare_session(replacement);
    row.project_id = Some(p.project);
    row.parent_id = Some(p.epic);
    row.session_kind = SessionKind::Feature;
    row.working_dir = p.repo.clone();
    row.provider = SessionProvider::Claude;
    row.model = Some("manager-scripted-provider".into());
    row.claude_session_id = Some("repaired-lead".into());
    {
        let store = p.manager.store.lock().await;
        store.insert_session(&row).unwrap();
        // Explicit operator repair uses the existing production lead CAS path.
        store.set_lead_session(p.epic, Some(replacement)).unwrap();
    }
    p.manager
        .completed
        .write()
        .await
        .insert(replacement, CompletedSession::for_test(row));
    p.lead = replacement;
    assert_eq!(reconcile(&p).await.queued, 1);
    let next = operation(&p).await;
    assert_ne!(lost_id, next);
    assert_eq!(
        intent(&p).await["historical_uncertainties"][0]["operation_id"],
        lost_id.to_string()
    );
    let process = launch::install_controller_candidate_test_process(p.lead);
    wait_due(&p, next).await;
    settle_action(&p, next).await;
    assert_eq!(p.receipt(next).await.state, ManagerActionStateV2::Succeeded);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    let old = p
        .manager
        .store
        .lock()
        .await
        .manager_action_operation(lost_id)
        .unwrap()
        .unwrap();
    assert_eq!(old.receipt.state, ManagerActionStateV2::Uncertain);
    assert!(old.effect_started);
    stop(&p, p.lead).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_intent_resolved_dependency_reconsiders_blocked_action_once() {
    let p = pilot().await;
    work(&p, "product").await;
    // Declare the upstream work as blocked before the first admission, so it
    // cannot independently make the Epic runnable when the edge is added.
    work(&p, "prerequisite").await;
    update(
        &p,
        "upstream-blocked",
        ManagerUpdateV2::Stage {
            key: "prerequisite".into(),
            expected_row_version: 1,
            stage: ManagerWorkStageV2::Implementation,
            state: ManagerStageStateV2::Blocked,
            note: "upstream work is still blocked".into(),
            evidence: None,
        },
    )
    .await;
    assert_eq!(reconcile(&p).await.queued, 1);
    let first = operation(&p).await;
    update(
        &p,
        "dependency-added",
        ManagerUpdateV2::Dependency {
            key: "product".into(),
            expected_row_version: 0,
            prerequisite: "prerequisite".into(),
            require_integrated: true,
            enabled: true,
        },
    )
    .await;
    wait_due(&p, first).await;
    settle_action(&p, first).await;
    assert_eq!(p.receipt(first).await.state, ManagerActionStateV2::Blocked);
    assert_eq!(
        p.receipt(first).await.outcome.as_deref(),
        Some("manager_v2_no_ready_work")
    );
    assert_eq!(reconcile(&p).await.queued, 0);
    assert_eq!(intent(&p).await["state"], "dependency_wait");
    update(
        &p,
        "dependency-released",
        ManagerUpdateV2::Dependency {
            key: "product".into(),
            expected_row_version: 1,
            prerequisite: "prerequisite".into(),
            require_integrated: true,
            enabled: false,
        },
    )
    .await;
    assert_eq!(reconcile(&p).await.queued, 1);
    let resumed = operation(&p).await;
    assert_ne!(first, resumed);
    for _ in 0..3 {
        assert_eq!(reconcile(&p).await.queued, 0);
    }
    let process = launch::install_controller_candidate_test_process(p.lead);
    wait_due(&p, resumed).await;
    settle_action(&p, resumed).await;
    assert_eq!(
        p.receipt(resumed).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    assert_eq!(count_actions(&p).await, 2);
    assert_eq!(p.receipt(first).await.state, ManagerActionStateV2::Blocked);
    stop(&p, p.lead).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_intent_repeated_admission_refusal_keeps_one_semantic_notice_after_restart() {
    let mut p = pilot().await;
    policy(&p, |policy| {
        policy.max_created_sessions = 0;
        policy.max_recovery_attempts = 1;
    })
    .await;
    work(&p, "ready").await;
    p.manager
        .store
        .lock()
        .await
        .update_session_status(p.lead, SessionStatus::Failed)
        .unwrap();
    let first = reconcile(&p).await;
    assert_eq!(first.queued, 0);
    assert_eq!(intent(&p).await["state"], "blocked");
    assert!(
        intent(&p).await["reason"]
            .as_str()
            .unwrap()
            .contains("creation_limit")
    );
    async fn snapshot(p: &Pilot) -> (i64, i64, String) {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        store.manager_v2_reconcile_notices(&config).unwrap();
        let version = store
            .manager_v2_record(&config, "intent", &p.epic.to_string())
            .unwrap()
            .unwrap()
            .row_version;
        let events: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_v2_events WHERE project_id=?1 AND kind='intent'",
                [p.project.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        let signature:String=store.conn.query_row("SELECT attention_signature FROM harness_manager_watches WHERE project_id=?1 AND direction='to_manager'",[p.project.to_string()],|r|r.get(0)).unwrap();
        (version, events, signature)
    }
    let before = snapshot(&p).await;
    for _ in 0..3 {
        assert_eq!(reconcile(&p).await.changed, 0);
        assert_eq!(snapshot(&p).await, before);
    }
    let before_payload = intent(&p).await;
    restart(&mut p).await;
    // Restore imports legacy invocation telemetry. Its new unknown-cost
    // observation may update Resources; the unchanged refusal must retain its
    // own intent event/version and notice generation.
    assert_eq!(reconcile(&p).await.queued, 0);
    assert_eq!(intent(&p).await, before_payload);
    assert_eq!(snapshot(&p).await, before);
    policy(&p, |policy| policy.max_created_sessions = 1).await;
    assert_eq!(reconcile(&p).await.queued, 1);
    let after = snapshot(&p).await;
    assert_eq!(intent(&p).await["state"], "recovery_action");
    assert_eq!(reconcile(&p).await.queued, 0);
    assert_eq!(snapshot(&p).await, after);
}

#[tokio::test]
async fn manager_intent_scoped_action_pages_include_unpublished_candidates_and_vacant_assignments()
{
    let p = pilot().await;
    let replacement = p
        .admit(
            "visible-replacement",
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "candidate not published yet".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    let unassign = p
        .admit(
            "visible-unassignment",
            ManagerActionV2::AssignLead {
                epic_id: p.epic,
                expected: p.fence().await,
                session_id: None,
            },
        )
        .await;
    assert!(
        p.manager
            .store
            .lock()
            .await
            .get_session(replacement.target_session_id.unwrap())
            .unwrap()
            .is_none()
    );
    let mut cursor = None;
    let mut found = std::collections::BTreeSet::new();
    loop {
        let page = p
            .manager
            .agent_control()
            .agent_manager_inspect(
                p.lead,
                AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Actions,
                    epic_id: Some(p.epic),
                    cursor,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        for row in page.rows {
            found.insert(row["id"].as_str().unwrap().to_string());
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        found,
        std::collections::BTreeSet::from([
            replacement.operation_id.to_string(),
            unassign.operation_id.to_string()
        ])
    );
}
