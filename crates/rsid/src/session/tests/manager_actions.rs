use super::*;
use crate::session::harness::tools::rsi_control::{ManagerControlToolKind, execute_manager_tool};
use crate::store::manager_actions::{ManagerActionClaimV2, ManagerActionOriginV2};
use rsi_common::harness_manager::{
    AgentManagerInboxRequestV1, AgentManagerReplyRequestV1, AgentManagerSendRequestV1,
    AgentManagerWorkViewRequestV1, ConfigureHarnessManagerRequestV1,
};
use rsi_common::harness_manager_v2::*;
use rsi_common::types::Project;

mod fence;
mod recovery;

const OBSERVED_INTERRUPTED_PROGRAM_TEXT: &str = "The fresh re-review is terminal. I’m re-registering the program guard first, then I’ll validate its one-file commit and stored strict handoff. Zero findings will open V13; any finding returns to the exact implementer.";

fn manager_program_job(p: &Pilot, mode: &str, enabled: bool) -> rsi_common::types::ScheduledJob {
    use crate::session::harness::tools::schedule_wake::{
        ScheduleWakeRequest, build_agent_scheduled_job,
    };
    let mut job = build_agent_scheduled_job(ScheduleWakeRequest {
        message: "Continue the registered program".into(),
        in_seconds: (mode != "program_guard").then_some(60),
        at: None,
        name: None,
        every_seconds: None,
        mode: Some(mode.into()),
        working_dir: p.repo.clone(),
        provider: Some(SessionProvider::Claude),
        model: Some("manager-scripted-provider".into()),
        project_id: Some(p.project),
        origin_session_id: Some(p.lead),
        watch_session_id: (mode == "on_terminal").then(Uuid::new_v4),
    })
    .unwrap();
    job.enabled = enabled;
    job
}

async fn manager_program_output(p: &Pilot, content: String) {
    let store = p.manager.store.lock().await;
    let sequence = store
        .load_events(p.lead)
        .unwrap()
        .last()
        .map_or(0, |e| e.sequence + 1);
    manager_program_event(&store, p.lead, sequence, Some(Role::Assistant), content);
}

async fn manager_program_status(p: &Pilot, status: SessionStatus) {
    p.manager
        .store
        .lock()
        .await
        .update_session_status(p.lead, status)
        .unwrap();
    p.manager
        .completed
        .write()
        .await
        .get_mut(&p.lead)
        .unwrap()
        .session
        .status = status;
}

fn manager_program_event(
    store: &crate::store::Store,
    target: Uuid,
    sequence: i32,
    role: Option<Role>,
    content: String,
) {
    store
        .insert_event(&ConversationEvent {
            id: 0,
            session_id: target,
            sequence,
            event_type: EventType::System,
            role,
            content,
            tool_name: None,
            tool_input: None,
            created_at: chrono::Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        })
        .unwrap();
}

async fn manager_program_fixture(
    p: &Pilot,
    mode: &str,
    present: bool,
    enabled: bool,
) -> (Uuid, Uuid) {
    let sentinel = manager_program_job(p, "program_guard", true);
    let job = manager_program_job(
        p,
        if mode == "partial" {
            "on_terminal"
        } else {
            mode
        },
        enabled,
    );
    {
        let store = p.manager.store.lock().await;
        store.insert_scheduled_job(&sentinel).unwrap();
        if present {
            if let rsi_common::types::WakeMode::OnTerminal(watched) = job.wake_mode {
                let mut child = bare_session(watched);
                child.project_id = Some(p.project);
                child.parent_id = Some(p.epic);
                child.session_kind = SessionKind::Feature;
                child.status = if enabled {
                    SessionStatus::Running
                } else {
                    SessionStatus::Completed
                };
                store.insert_session(&child).unwrap();
            }
            store.insert_scheduled_job(&job).unwrap();
        }
        if mode == "partial" {
            for _ in 0..5 {
                store
                    .insert_scheduled_job(&manager_program_job(p, "on_terminal", false))
                    .unwrap();
            }
            assert!(
                store
                    .load_events(p.lead)
                    .unwrap()
                    .last()
                    .is_none_or(|e| e.sequence < 310)
            );
            manager_program_event(
                &store,
                p.lead,
                310,
                Some(Role::User),
                "Continue the same authorized program.".into(),
            );
            manager_program_event(
                &store,
                p.lead,
                312,
                Some(Role::Assistant),
                OBSERVED_INTERRUPTED_PROGRAM_TEXT.into(),
            );
            manager_program_event(&store, p.lead, 314, None, "interrupted".into());
            return (sentinel.id, job.id);
        }
        let next = store
            .load_events(p.lead)
            .unwrap()
            .last()
            .map_or(0, |e| e.sequence + 1);
        manager_program_event(
            &store,
            p.lead,
            next,
            Some(Role::User),
            "Continue the same authorized program.".into(),
        );
    }
    manager_program_output(p, format!("orchestration_outcome_v1: {}\n", serde_json::json!({
        "schema_version": 1, "mode": "program", "next_slice_ready": true,
        "continuation_state": if mode == "on_terminal" { "child_watch" } else { "resume_wake" },
        "continuation_job_id": job.id, "evidence": "bounded fixture program remains incomplete"
    }))).await;
    (sentinel.id, job.id)
}

async fn manager_program_stop(p: &Pilot) {
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, p.lead)
        .await
        .unwrap();
    crate::session::launch::drop_controller_candidate_test_stream(p.lead);
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            if !p.manager.active.read().await.contains_key(&p.lead)
                && p.manager
                    .completed
                    .read()
                    .await
                    .get(&p.lead)
                    .is_some_and(|s| s.session.status == SessionStatus::Interrupted)
                && p.manager.persistence.pending.load(Ordering::SeqCst) == 0
                && p.manager
                    .store
                    .lock()
                    .await
                    .get_session(p.lead)
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

async fn manager_program_resume_cases(cases: &[(&str, bool)]) {
    for &(mode, present) in cases {
        let mut p = pilot().await;
        let initial = p
            .admit(
                "initial-program",
                ManagerActionV2::ReplaceLead {
                    epic_id: p.epic,
                    expected: p.fence().await,
                    query: "start".into(),
                    launch: p.policy.allowed_launches[0].clone(),
                },
            )
            .await;
        p.lead = initial.target_session_id.unwrap();
        let first = crate::session::launch::install_controller_candidate_test_process(p.lead);
        p.execute().await.unwrap();
        wait_launch_event(&p, p.lead).await;
        crate::session::launch::send_controller_candidate_test_event(p.lead, crate::claude::StreamEvent {
            event_type: "system".into(),
            data: serde_json::json!({"subtype":"init", "session_id":"program-resumable-provider"}),
        }).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if p.manager
                    .store
                    .lock()
                    .await
                    .get_session(p.lead)
                    .unwrap()
                    .unwrap()
                    .claude_session_id
                    .as_deref()
                    == Some("program-resumable-provider")
                    && p.manager.persistence.pending.load(Ordering::SeqCst) == 0
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        manager_program_stop(&p).await;
        assert_eq!(first.productive_start_count.load(Ordering::SeqCst), 1);
        let before = p
            .manager
            .store
            .lock()
            .await
            .live_custody_for_session(p.lead)
            .unwrap();
        let root = std::path::Path::new(&before.sandbox_root);
        std::fs::write(root.join("unfinished"), "retain program edit").unwrap();
        let (sentinel, _) = manager_program_fixture(&p, mode, present, false).await;
        if mode == "partial" {
            let store = p.manager.store.lock().await;
            let events = store.load_events(p.lead).unwrap();
            let assistant: Vec<_> = events
                .iter()
                .filter(|e| e.sequence > 310 && e.role == Some(Role::Assistant))
                .collect();
            assert_eq!(assistant.len(), 1);
            assert_eq!(
                (assistant[0].sequence, assistant[0].content.as_str()),
                (312, OBSERVED_INTERRUPTED_PROGRAM_TEXT)
            );
            assert_eq!(assistant[0].content.len(), 221);
            assert_eq!(
                store
                    .manager_action_lead_fence(p.epic)
                    .unwrap()
                    .event_sequence,
                314
            );
            assert!(
                matches!(rsi_common::agent_contract::parse_orchestration_outcome_v1(&assistant[0].content), Err(rsi_common::agent_contract::ContractError::MissingField { field }) if field == "orchestration_outcome_v1")
            );
        }
        let output = p
            .manager
            .store
            .lock()
            .await
            .load_events(p.lead)
            .unwrap()
            .last()
            .unwrap()
            .content
            .clone();
        assert_eq!(
            p.manager
                .agent_control()
                .enforce_master_no_idle(
                    p.lead,
                    p.fence().await.event_sequence.try_into().unwrap(),
                    &output
                )
                .await
                .unwrap(),
            crate::session::agent_verbs::MasterNoIdleOutcome::NotApplicable
        );
        let request = p.request(
            "resume-program",
            ManagerActionV2::ResumeLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "recover the stranded program".into(),
            },
        );
        let receipt = p
            .manager
            .agent_control()
            .agent_manager_control(p.owner, request.clone())
            .await
            .unwrap();
        let process = crate::session::launch::install_controller_candidate_test_process(p.lead);
        #[cfg(target_os = "linux")]
        let orphan_fixture = crate::session::reaper::StartupReaperFixture::new();
        #[cfg(target_os = "linux")]
        let _orphan_guard = orphan_fixture.scoped_runtime_reap_root(p.lead).unwrap();
        p.execute().await.unwrap();
        assert_eq!(
            p.receipt(receipt.operation_id).await.state,
            ManagerActionStateV2::Succeeded
        );
        assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
        let replay = p
            .manager
            .agent_control()
            .agent_manager_control(p.owner, request.clone())
            .await
            .unwrap();
        assert_eq!(replay.operation_id, receipt.operation_id);
        assert_eq!(replay.state, ManagerActionStateV2::Succeeded);
        wait_launch_event(&p, p.lead).await;
        let previous_sequence = p.fence().await.event_sequence;
        manager_program_output(&p, "new durable provider observation".into()).await;
        assert!(p.fence().await.event_sequence > previous_sequence);
        let mut stale = request;
        stale.idempotency_key = "stale-program-resume".into();
        if let Ok(stale) = p
            .manager
            .agent_control()
            .agent_manager_control(p.owner, stale)
            .await
        {
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    p.manager.reconcile_manager_actions_once().await.unwrap();
                    if p.receipt(stale.operation_id).await.state != ManagerActionStateV2::Queued {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                p.receipt(stale.operation_id).await.state,
                ManagerActionStateV2::Blocked
            );
        }
        assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
        {
            let store = p.manager.store.lock().await;
            let after = store.live_custody_for_session(p.lead).unwrap();
            assert_eq!(
                (after.custody_id, after.generation, &after.sandbox_root),
                (before.custody_id, before.generation, &before.sandbox_root)
            );
            assert_eq!(
                store.get_session(p.epic).unwrap().unwrap().lead_session_id,
                Some(p.lead)
            );
            assert_eq!(
                std::fs::read_to_string(root.join("unfinished")).unwrap(),
                "retain program edit"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("source")).unwrap(),
                "committed source\n"
            );
            assert!(store.get_scheduled_job(&sentinel).unwrap().unwrap().enabled);
            assert_eq!(
                store
                    .list_scheduled_jobs()
                    .unwrap()
                    .iter()
                    .filter(|job| job.wake_session_id == Some(p.lead))
                    .count(),
                if mode == "partial" {
                    7
                } else if present {
                    2
                } else {
                    1
                }
            );
            let count: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM model_invocations WHERE dedup_key=?1",
                    [format!("manager.action:{}", receipt.operation_id)],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
        }
        manager_program_stop(&p).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_program_stranded_resume_preserves_source_and_one_writer() {
    manager_program_resume_cases(&[("on_terminal", true), ("resume", false)]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_program_interrupted_partial_turn_resumes_once() {
    manager_program_resume_cases(&[("partial", true)]).await;
}

async fn manager_program_report_fragments_remain_unknown(fragments: &[&str]) {
    for &fragment in fragments {
        let p = pilot().await;
        manager_program_status(&p, SessionStatus::Interrupted).await;
        let (sentinel, _) = manager_program_fixture(&p, "partial", true, false).await;
        p.manager.store.lock().await.conn.execute(
            "UPDATE conversation_events SET content=?2 WHERE session_id=?1 AND role='Assistant'",
            rusqlite::params![p.lead.to_string(), fragment],
        ).unwrap();
        let request = p.request(
            "truncated-legacy-report",
            ManagerActionV2::ResumeLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "recover only if unowned and evidence is conclusive".into(),
            },
        );
        let receipt = p
            .manager
            .agent_control()
            .agent_manager_control(p.owner, request.clone())
            .await
            .unwrap();
        let process = crate::session::launch::install_controller_candidate_test_process(p.lead);
        #[cfg(target_os = "linux")]
        let orphan_fixture = crate::session::reaper::StartupReaperFixture::new();
        #[cfg(target_os = "linux")]
        let _orphan_guard = orphan_fixture.scoped_runtime_reap_root(p.lead).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                p.manager.reconcile_manager_actions_once().await.unwrap();
                if p.receipt(receipt.operation_id).await.state != ManagerActionStateV2::Queued {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let settled = p.receipt(receipt.operation_id).await;
        let starts = process.productive_start_count.load(Ordering::SeqCst);
        // Clean up a regressed launch before reporting the failure.
        if p.manager.active.read().await.contains_key(&p.lead) {
            manager_program_stop(&p).await;
        } else {
            crate::session::launch::drop_controller_candidate_test_stream(p.lead);
        }
        assert_eq!(
            starts, 0,
            "{fragment:?} must remain unknown before provider effect: {settled:?}"
        );
        assert_eq!(settled.state, ManagerActionStateV2::Blocked, "{fragment:?}");
        assert_eq!(
            settled.outcome.as_deref(),
            Some("manager_v2_program_evidence_unknown"),
            "{fragment:?}: {settled:?}"
        );
        let replay = p
            .manager
            .agent_control()
            .agent_manager_control(p.owner, request)
            .await
            .unwrap();
        assert_eq!(replay.operation_id, receipt.operation_id);
        assert_eq!(replay.state, ManagerActionStateV2::Blocked);
        let store = p.manager.store.lock().await;
        let invocations: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM model_invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(invocations, 0, "{fragment:?}");
        assert_eq!(
            store.get_session(p.lead).unwrap().unwrap().status,
            SessionStatus::Interrupted
        );
        assert!(store.get_scheduled_job(&sentinel).unwrap().unwrap().enabled);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_program_truncated_legacy_report_is_unknown_before_provider() {
    manager_program_report_fragments_remain_unknown(&[
        // Exact P2-01 reproducer, followed by adjacent heading/field truncations.
        "ORCHESTRATION COMPLETE\nMode",
        "ORCHESTRATION COMPLETE",
        "ORCHESTRATION",
        "ORCHESTRATION COMPLE",
        "\t## OrChEsTrAtIoN\tCoMpLe\r\n",
        "The prior worker returned.\nORCHESTRATION COMPLETE",
        "Mode",
        "The prior worker returned.\nMode",
        "The prior worker returned.\r\n \tmOdE\tprogram",
    ])
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_program_markdown_emphasis_report_is_unknown_before_provider() {
    manager_program_report_fragments_remain_unknown(&[
        // Exact independent re-review reproducer.
        "**ORCHESTRATION COMPLETE**\n**Mode**",
        "__ORCHESTRATION COMPLETE__\n__Mode__",
        "*ORCHESTRATION COMPLETE*\n*Mode*",
        "_ORCHESTRATION COMPLETE_\n_Mode_",
        "***ORCHESTRATION COMPLETE***\n***Mode***",
        "~~ORCHESTRATION COMPLETE~~\n~~Mode~~",
        "**ORCHESTRATION** **COMPLETE**\n**Mode**",
        "**ORCHESTRATION COMPL",
        // A recognized heading must not mask missing formatted-field coverage.
        "**Mode**",
        "**Mode** program",
        "The prior worker returned.\n__M**od**e__",
    ])
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_program_markdown_blocks_report_is_unknown_before_provider() {
    manager_program_report_fragments_remain_unknown(&[
        // Exact independent re-review reproducer.
        "> ORCHESTRATION COMPLETE\n> Mode",
        "> > ### **ORCHESTRATION COMPLETE** ###\n> > - __Mode__",
        "- **ORCHESTRATION COMPLETE**\n- **Mode**",
        "+ ORCHESTRATION COMPLETE\n+ Mode",
        "* ORCHESTRATION COMPLETE\n* Mode",
        "1. ORCHESTRATION COMPLETE\n2. Mode",
        "1) **ORCHESTRATION COMPLETE**\n2) __Mode__",
        "| **ORCHESTRATION COMPLETE** |\n| **Mode** |",
        "> **ORCHESTRATION** _COMPL",
        "> Mode",
        "The prior worker returned.\n- __Mode__ program",
    ])
    .await;
}

#[tokio::test]
async fn manager_actions_program_store_requires_conclusive_missing_or_disabled_owner() {
    for mode in ["on_terminal", "resume"] {
        for present in [false, true] {
            let p = pilot().await;
            manager_program_status(&p, SessionStatus::Interrupted).await;
            manager_program_fixture(&p, mode, present, false).await;
            let store = p.manager.store.lock().await;
            assert!(store.manager_action_human_gate(p.lead).is_err());
            store
                .manager_action_human_gate_with_interrupted_resume(p.lead, true)
                .unwrap();
            // Inbox notices retain their existing, narrower permission. A
            // manager lifecycle action is the explicit recovery authority.
            let notice =
                crate::session::lifecycle::check_manager_notice_program_gate(&store, p.lead);
            assert_eq!(notice.is_ok(), mode == "on_terminal");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_program_protected_and_unknown_owners_keep_safe_receipts() {
    for case in [
        "live_watch",
        "other_watch",
        "resume",
        "operator_pause",
        "raw_question",
        "approval",
        "pending_approval",
        "retry_owner",
        "retry_marker",
        "human_gate",
        "missing_output",
        "duplicate_output",
        "large_output",
        "malformed_sentinel",
        "closed_sentinel",
        "malformed_job",
        "wrong_job_owner",
        "wrong_job_mode",
        "declared_sentinel",
        "partial_report",
        "carrier_fragment",
        "raw_json",
        "legacy_report",
        "question_text",
        "no_user_boundary",
    ] {
        let p = pilot().await;
        manager_program_status(&p, SessionStatus::Interrupted).await;
        let report = matches!(
            case,
            "closed_sentinel"
                | "malformed_job"
                | "wrong_job_owner"
                | "wrong_job_mode"
                | "declared_sentinel"
                | "duplicate_output"
        );
        let (sentinel, job) = manager_program_fixture(
            &p,
            if report { "on_terminal" } else { "partial" },
            true,
            false,
        )
        .await;
        let request = p.request(
            case,
            ManagerActionV2::ResumeLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "recover if unowned".into(),
            },
        );
        let receipt = p
            .manager
            .agent_control()
            .agent_manager_control(p.owner, request.clone())
            .await
            .unwrap();
        // Add the competing owner after admission: execution must consult the
        // current evidence, not the admission-time declaration.
        let expected = {
            let store = p.manager.store.lock().await;
            match case {
                "live_watch" => {
                    store.toggle_scheduled_job(&job).unwrap();
                    let rsi_common::types::WakeMode::OnTerminal(watched) =
                        store.get_scheduled_job(&job).unwrap().unwrap().wake_mode
                    else {
                        panic!("fixture child watch");
                    };
                    store
                        .update_session_status(watched, SessionStatus::Running)
                        .unwrap();
                }
                "other_watch" | "resume" => {
                    store
                        .insert_scheduled_job(&manager_program_job(
                            &p,
                            if case == "resume" {
                                "resume"
                            } else {
                                "on_terminal"
                            },
                            true,
                        ))
                        .unwrap();
                }
                "operator_pause" => store.record_manager_operator_pause(p.lead, true).unwrap(),
                "raw_question" => {
                    store
                        .conn
                        .execute(
                            "UPDATE sessions SET pending_question_json='{' WHERE id=?1",
                            [p.lead.to_string()],
                        )
                        .unwrap();
                }
                "approval" => store
                    .update_session_status(p.lead, SessionStatus::WaitingApproval)
                    .unwrap(),
                "pending_approval" => store
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
                "retry_owner" => {
                    p.manager
                        .completed
                        .write()
                        .await
                        .get_mut(&p.lead)
                        .unwrap()
                        .superseded_by_retry = Some(Uuid::new_v4());
                }
                "retry_marker" => {
                    store.conn.execute("INSERT INTO daemon_settings(key,value,updated_at) VALUES(?1,'pending',?2)", rusqlite::params![crate::store::daemon_settings::c5_autofile_pending_key(p.lead), chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)]).unwrap();
                }
                "human_gate" | "missing_output" | "large_output" | "declared_sentinel"
                | "partial_report" | "carrier_fragment" | "raw_json" | "legacy_report"
                | "question_text" => {
                    let content = match case {
                        "human_gate" => "orchestration_outcome_v1: {\"schema_version\":1,\"mode\":\"program\",\"next_slice_ready\":false,\"continuation_state\":\"human_gate\",\"blocker_class\":\"production\",\"evidence\":\"operator approval is required\"}".into(),
                        "missing_output" => String::new(),
                        "large_output" => "x".repeat(256 * 1024 + 1),
                        "partial_report" => "orchestration_outcome_v1: {".into(),
                        "carrier_fragment" => "orchestration_outcome_v".into(),
                        "raw_json" => "{\"mode\":\"program\"".into(),
                        "legacy_report" => "Mode: program\nNext slice ready: yes".into(),
                        "question_text" => "May I deploy the pending change?".into(),
                        _ => format!("orchestration_outcome_v1: {}", serde_json::json!({"schema_version":1,"mode":"program","next_slice_ready":true,"continuation_state":"resume_wake","continuation_job_id":sentinel,"evidence":"sentinel is identity only"})),
                    };
                    store.conn.execute("UPDATE conversation_events SET content=?2 WHERE session_id=?1 AND role='Assistant'", rusqlite::params![p.lead.to_string(), content]).unwrap();
                }
                "duplicate_output" => {
                    store.conn.execute("UPDATE conversation_events SET content=content||content WHERE session_id=?1 AND role='Assistant'", [p.lead.to_string()]).unwrap();
                }
                "malformed_sentinel" | "malformed_job" => {
                    store
                        .conn
                        .execute(
                            "UPDATE scheduled_jobs SET schedule_json='{' WHERE id=?1",
                            [if case == "malformed_sentinel" {
                                sentinel
                            } else {
                                job
                            }
                            .to_string()],
                        )
                        .unwrap();
                }
                "closed_sentinel" => {
                    store.toggle_scheduled_job(&sentinel).unwrap();
                }
                "wrong_job_owner" => {
                    store
                        .conn
                        .execute(
                            "UPDATE scheduled_jobs SET wake_session_id=?2 WHERE id=?1",
                            rusqlite::params![job.to_string(), Uuid::new_v4().to_string()],
                        )
                        .unwrap();
                }
                "wrong_job_mode" => {
                    store
                        .conn
                        .execute(
                            "UPDATE scheduled_jobs SET wake_mode='fresh' WHERE id=?1",
                            [job.to_string()],
                        )
                        .unwrap();
                }
                "no_user_boundary" => {
                    store.conn.execute("UPDATE conversation_events SET role=NULL WHERE session_id=?1 AND role='User'", [p.lead.to_string()]).unwrap();
                }
                _ => unreachable!(),
            }
            if matches!(
                case,
                "live_watch"
                    | "other_watch"
                    | "resume"
                    | "operator_pause"
                    | "raw_question"
                    | "approval"
                    | "pending_approval"
                    | "retry_owner"
                    | "retry_marker"
                    | "human_gate"
            ) {
                "manager_v2_human_or_recovery_owner"
            } else {
                "manager_v2_program_evidence_unknown"
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                p.manager.reconcile_manager_actions_once().await.unwrap();
                if p.receipt(receipt.operation_id).await.state != ManagerActionStateV2::Queued {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let settled = p.receipt(receipt.operation_id).await;
        assert_eq!(
            settled.state,
            ManagerActionStateV2::Blocked,
            "{case}: {settled:?}"
        );
        assert!(
            serde_json::to_string(&settled).unwrap().contains(expected),
            "{case}: {settled:?}"
        );
        let replay = p
            .manager
            .agent_control()
            .agent_manager_control(p.owner, request)
            .await
            .unwrap();
        assert_eq!(replay.operation_id, receipt.operation_id);
        assert_eq!(replay.state, ManagerActionStateV2::Blocked);
        let store = p.manager.store.lock().await;
        let invocations: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM model_invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(invocations, 0, "{case} must refuse before model admission");
        assert_eq!(
            store.get_session(p.epic).unwrap().unwrap().lead_session_id,
            Some(p.lead)
        );
    }
}

#[test]
fn manager_actions_program_safe_error_retains_redacted_owner_class() {
    assert_eq!(
        crate::session::manager_actions::safe_action_error(&DaemonError::InvalidParam(
            "manager_notice_deferred".into()
        )),
        "manager_v2_human_or_recovery_owner"
    );
    assert_eq!(
        crate::session::manager_actions::safe_action_error(&DaemonError::InvalidParam(
            "manager_v2_program_evidence_unknown: private raw evidence".into()
        )),
        "manager_v2_program_evidence_unknown"
    );
}

#[tokio::test]
async fn manager_actions_program_partial_turn_requires_interrupted_status_and_bounded_evidence() {
    let p = pilot().await;
    manager_program_fixture(&p, "partial", true, false).await;
    for status in [
        SessionStatus::Completed,
        SessionStatus::Failed,
        SessionStatus::Starting,
        SessionStatus::Running,
        SessionStatus::WaitingApproval,
        SessionStatus::Archived,
        SessionStatus::Deleted,
    ] {
        manager_program_status(&p, status).await;
        assert!(
            p.manager
                .store
                .lock()
                .await
                .manager_action_human_gate_with_interrupted_resume(p.lead, true)
                .is_err(),
            "{status:?}"
        );
    }
    manager_program_status(&p, SessionStatus::Interrupted).await;
    {
        let store = p.manager.store.lock().await;
        assert!(
            store.manager_action_human_gate(p.lead).is_err(),
            "default/automatic gate retains ownership"
        );
        store
            .manager_action_human_gate_with_interrupted_resume(p.lead, true)
            .unwrap();
    }
    for _ in 0..256 {
        manager_program_output(&p, "another interrupted fragment".into()).await;
    }
    let error = p
        .manager
        .store
        .lock()
        .await
        .manager_action_human_gate_with_interrupted_resume(p.lead, true)
        .unwrap_err();
    assert_eq!(
        crate::session::manager_actions::safe_action_error(&error),
        "manager_v2_program_evidence_unknown"
    );
}

#[tokio::test]
async fn manager_actions_program_partial_turn_does_not_authorize_pause_or_replacement() {
    for replace in [false, true] {
        let p = pilot().await;
        manager_program_status(&p, SessionStatus::Interrupted).await;
        manager_program_fixture(&p, "partial", true, false).await;
        let operation = if replace {
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "replace".into(),
                launch: p.policy.allowed_launches[0].clone(),
            }
        } else {
            ManagerActionV2::PauseLead {
                epic_id: p.epic,
                expected: p.fence().await,
                reason: "pause".into(),
            }
        };
        p.admit("other-action", operation).await;
        let error = p.execute().await.unwrap_err();
        assert_eq!(
            crate::session::manager_actions::safe_action_error(&error),
            "manager_v2_program_evidence_unknown"
        );
        let store = p.manager.store.lock().await;
        let invocations: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM model_invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(invocations, 0);
        assert_eq!(
            store.get_session(p.epic).unwrap().unwrap().lead_session_id,
            Some(p.lead)
        );
    }
}

#[tokio::test]
async fn manager_actions_program_partial_turn_preserves_capacity_owner() {
    let p = pilot().await;
    let (sentinel, _) = manager_program_fixture(&p, "partial", true, false).await;
    let store = p.manager.store.lock().await;
    let invocation = Uuid::new_v4();
    let now = chrono::Utc::now();
    store.conn.execute("UPDATE sessions SET provider='Codex',stop_reason='provider_error:codex_usage_limit' WHERE id=?1", [p.lead.to_string()]).unwrap();
    store.conn.execute("INSERT INTO model_invocations(id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,policy_snapshot_json,usage_confidence,created_at,completed_at)
        VALUES(?1,'session_continue','session_lifecycle','foreground','paid_capable','admitted','failed','manager-program-test',?2,'{}','unavailable',?3,?3)",
        rusqlite::params![invocation.to_string(), p.lead.to_string(), now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)]).unwrap();
    store
        .set_session_model_invocation(p.lead, Some(invocation))
        .unwrap();
    store
        .update_failed_and_stage_c5_autofile(
            p.lead,
            crate::store::daemon_settings::AutofileCause::ProcessDied,
        )
        .unwrap();
    store
        .settle_capacity_failure(p.lead, p.lead, sentinel, invocation, 314, now)
        .unwrap();
    store
        .update_session_status(p.lead, SessionStatus::Interrupted)
        .unwrap();
    store.conn.execute("UPDATE scheduled_jobs SET enabled=0 WHERE id IN (SELECT wake_job_id FROM master_no_idle_capacity_incidents WHERE controller_session_id=?1)", [p.lead.to_string()]).unwrap();
    let error = store
        .manager_action_human_gate_with_interrupted_resume(p.lead, true)
        .unwrap_err();
    assert_eq!(
        crate::session::manager_actions::safe_action_error(&error),
        "manager_v2_human_or_recovery_owner"
    );
    let state: String = store
        .conn
        .query_row(
            "SELECT state FROM master_no_idle_capacity_incidents WHERE controller_session_id=?1",
            [p.lead.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "open");
    assert!(store.get_scheduled_job(&sentinel).unwrap().unwrap().enabled);
}

struct Pilot {
    manager: SessionManager,
    _dir: TempDir,
    repo: std::path::PathBuf,
    owner: Uuid,
    project: Uuid,
    group: Uuid,
    epic: Uuid,
    lead: Uuid,
    policy: ManagerPolicyV2,
}

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

async fn pilot() -> Pilot {
    let (manager, dir) = manager();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Lifecycle fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    std::fs::write(repo.join("source"), "committed source\n").unwrap();
    git(&repo, &["add", "source"]);
    git(&repo, &["commit", "-qm", "fixture"]);
    let project = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let group = Uuid::new_v4();
    let epic = Uuid::new_v4();
    let lead = Uuid::new_v4();
    let choice = ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "manager-scripted-provider".into(),
        effort: None,
    };
    let policy = ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        capabilities: vec![
            ManagerCapabilityV2::LeadControl,
            ManagerCapabilityV2::WorkPlan,
            ManagerCapabilityV2::Topology,
            ManagerCapabilityV2::SessionCreate,
            ManagerCapabilityV2::LeadAssign,
        ],
        group_ids: vec![group],
        allow_create_groups: true,
        max_created_containers: 8,
        max_created_sessions: 12,
        max_active_sessions: 4,
        allowed_launches: vec![choice],
        max_recovery_attempts: 3,
        retry_delay_seconds: 1,
        ..Default::default()
    };
    {
        let store = manager.store.lock().await;
        store
            .insert_project(&Project {
                id: project,
                name: "Lifecycle project".into(),
                path: Some(repo.clone()),
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
            (lead, SessionKind::Feature, Some(epic)),
        ] {
            let mut row = bare_session(id);
            row.project_id = Some(project);
            row.working_dir = repo.clone();
            row.session_kind = kind;
            row.parent_id = parent;
            row.provider = SessionProvider::Claude;
            row.model = Some("manager-scripted-provider".into());
            row.claude_session_id = Some(format!("provider-{id}"));
            store.insert_session(&row).unwrap();
            manager
                .completed
                .write()
                .await
                .insert(id, CompletedSession::for_test(row));
        }
        store.set_lead_session(epic, Some(lead)).unwrap();
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: owner,
                epic_ids: Some(vec![epic]),
                expected_row_version: 0,
            })
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: 1,
                expected_policy_version: 0,
                idempotency_key: "grant".into(),
                policy: policy.clone(),
            })
            .unwrap();
    }
    Pilot {
        manager,
        _dir: dir,
        repo,
        owner,
        project,
        group,
        epic,
        lead,
        policy,
    }
}

impl Pilot {
    async fn fence(&self) -> ManagerLeadFenceV2 {
        self.manager
            .store
            .lock()
            .await
            .manager_action_lead_fence(self.epic)
            .unwrap()
    }
    fn request(&self, key: &str, operation: ManagerActionV2) -> AgentManagerControlRequestV2 {
        AgentManagerControlRequestV2 {
            fence: ManagerFenceV2 {
                scope_version: 1,
                policy_version: 1,
            },
            idempotency_key: key.into(),
            operation,
        }
    }
    async fn admit(&self, key: &str, operation: ManagerActionV2) -> ManagerActionReceiptV2 {
        self.manager
            .agent_control()
            .agent_manager_control(self.owner, self.request(key, operation))
            .await
            .unwrap()
    }
    async fn claim(&self) -> ManagerActionClaimV2 {
        self.manager
            .store
            .lock()
            .await
            .claim_manager_action(self.manager.program_run_boot_id)
            .unwrap()
            .unwrap()
    }
    async fn receipt(&self, id: Uuid) -> ManagerActionReceiptV2 {
        self.manager
            .store
            .lock()
            .await
            .manager_action_operation(id)
            .unwrap()
            .unwrap()
            .receipt
    }
    async fn execute(&self) -> Result<()> {
        let claim = self.claim().await;
        self.manager.execute_manager_action(&claim).await
    }
}

fn prepared_resume(p: &Pilot) -> AgentManagerPrepareControlRequestV2 {
    AgentManagerPrepareControlRequestV2 {
        operation: PreparedManagerActionV2::ResumeLead {
            epic_id: p.epic,
            message: "continue prepared work".into(),
        },
    }
}

#[tokio::test]
async fn prepared_manager_action_is_preflight_only_then_commits_once() {
    let p = pilot().await;
    let prepared = p
        .manager
        .agent_control()
        .agent_manager_prepare_control(p.owner, prepared_resume(&p))
        .await
        .unwrap();
    assert_eq!(prepared.readiness, ManagerPreparedActionReadinessV2::Ready);
    assert!(prepared.blockers.is_empty());
    {
        let store = p.manager.store.lock().await;
        let actions: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM harness_manager_v2_operations WHERE kind='lifecycle_action'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let sessions: i64 = store
            .conn
            .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(actions, 0);
        assert_eq!(sessions, 4);
    }

    let request = AgentManagerCommitPreparedControlRequestV2 {
        prepared_id: prepared.prepared_id,
        target_digest: prepared.target_digest.clone(),
        idempotency_key: "prepared-commit-once".into(),
    };
    let first = p
        .manager
        .agent_control()
        .agent_manager_commit_prepared_control(p.owner, request.clone())
        .await
        .unwrap();
    let ManagerPreparedActionCommitResultV2::Queued { receipt: first } = first else {
        panic!("ready preparation did not queue")
    };
    assert!(!first.deduplicated);
    let replay = p
        .manager
        .agent_control()
        .agent_manager_commit_prepared_control(p.owner, request)
        .await
        .unwrap();
    let ManagerPreparedActionCommitResultV2::Queued { receipt: replay } = replay else {
        panic!("commit replay did not return the queued receipt")
    };
    assert_eq!(replay.operation_id, first.operation_id);
    assert!(replay.deduplicated);
    let store = p.manager.store.lock().await;
    let actions: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_operations WHERE kind='lifecycle_action'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let prepared_state: (String, String) = store
        .conn
        .query_row(
            "SELECT state,operation_id FROM manager_prepared_actions WHERE id=?1",
            [prepared.prepared_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(actions, 1);
    assert_eq!(
        prepared_state,
        ("consumed".into(), first.operation_id.to_string())
    );
    drop(store);
    assert_eq!(
        p.manager
            .agent_control()
            .agent_manager_get_action(
                p.owner,
                AgentManagerGetActionRequestV2 {
                    operation_id: first.operation_id,
                },
            )
            .await
            .unwrap()
            .operation_id,
        first.operation_id
    );
    assert!(
        p.manager
            .agent_control()
            .agent_manager_get_action(
                p.lead,
                AgentManagerGetActionRequestV2 {
                    operation_id: first.operation_id,
                },
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn prepared_manager_native_tools_route_end_to_end() {
    let p = pilot().await;
    let control = p.manager.agent_control();
    let prepared: ManagerPreparedActionReceiptV2 = serde_json::from_value(
        execute_manager_tool(
            &control,
            p.owner,
            ManagerControlToolKind::PrepareControl,
            serde_json::json!({
                "operation":{"action":"resume_lead","epic_id":p.epic,"message":"continue natively"}
            }),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let committed: ManagerPreparedActionCommitResultV2 = serde_json::from_value(
        execute_manager_tool(
            &control,
            p.owner,
            ManagerControlToolKind::CommitPreparedControl,
            serde_json::json!({
                "prepared_id":prepared.prepared_id,
                "target_digest":prepared.target_digest,
                "idempotency_key":"native-prepared-commit"
            }),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let ManagerPreparedActionCommitResultV2::Queued { receipt } = committed else {
        panic!("native commit did not queue the prepared action")
    };
    let observed: ManagerActionReceiptV2 = serde_json::from_value(
        execute_manager_tool(
            &control,
            p.owner,
            ManagerControlToolKind::GetAction,
            serde_json::json!({"operation_id":receipt.operation_id}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(observed.operation_id, receipt.operation_id);
    assert_eq!(observed.state, ManagerActionStateV2::Queued);
}

#[tokio::test]
async fn prepared_manager_action_refuses_changed_lead_without_journal_effect() {
    let p = pilot().await;
    let prepared = p
        .manager
        .store
        .lock()
        .await
        .prepare_manager_action(p.owner, prepared_resume(&p))
        .unwrap();
    {
        let store = p.manager.store.lock().await;
        manager_program_event(
            &store,
            p.lead,
            1,
            Some(Role::Assistant),
            "lead advanced after preparation".into(),
        );
    }
    let error = p
        .manager
        .store
        .lock()
        .await
        .commit_prepared_manager_action(
            p.owner,
            AgentManagerCommitPreparedControlRequestV2 {
                prepared_id: prepared.prepared_id,
                target_digest: prepared.target_digest,
                idempotency_key: "prepared-stale-lead".into(),
            },
        )
        .unwrap_err();
    assert_eq!(
        crate::session::manager_actions::safe_action_error(&error),
        "manager_v2_lead_changed"
    );
    let store = p.manager.store.lock().await;
    let actions: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_operations WHERE kind='lifecycle_action'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(actions, 0);
}

#[tokio::test]
async fn prepared_manager_action_refuses_changed_scope_and_policy_without_journal_effect() {
    let p = pilot().await;
    let scope_prepared = p
        .manager
        .store
        .lock()
        .await
        .prepare_manager_action(p.owner, prepared_resume(&p))
        .unwrap();
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: vec![p.group],
            project_id: p.project,
            session_id: p.owner,
            epic_ids: Some(Vec::new()),
            expected_row_version: 1,
        })
        .unwrap();
    let error = p
        .manager
        .store
        .lock()
        .await
        .commit_prepared_manager_action(
            p.owner,
            AgentManagerCommitPreparedControlRequestV2 {
                prepared_id: scope_prepared.prepared_id,
                target_digest: scope_prepared.target_digest,
                idempotency_key: "prepared-stale-scope".into(),
            },
        )
        .unwrap_err();
    assert_eq!(
        crate::session::manager_actions::safe_action_error(&error),
        "manager_v2_policy_changed"
    );

    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 2,
            expected_policy_version: 1,
            idempotency_key: "regrant-after-scope-change".into(),
            policy: p.policy.clone(),
        })
        .unwrap();

    let policy_prepared = p
        .manager
        .store
        .lock()
        .await
        .prepare_manager_action(p.owner, prepared_resume(&p))
        .unwrap();
    let mut changed_policy = p.policy.clone();
    changed_policy.request_timeout_seconds += 1;
    let changed = p
        .manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 2,
            expected_policy_version: 2,
            idempotency_key: "change-policy-after-prepare".into(),
            policy: changed_policy,
        })
        .unwrap();
    assert_eq!(changed.row_version, 3);
    let prepared_policy_version: i64 = p
        .manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT policy_version FROM manager_prepared_actions WHERE id=?1",
            [policy_prepared.prepared_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(prepared_policy_version, 2);
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_harness_manager_policy(p.project)
            .unwrap()
            .unwrap()
            .row_version,
        3
    );
    let error = p
        .manager
        .store
        .lock()
        .await
        .commit_prepared_manager_action(
            p.owner,
            AgentManagerCommitPreparedControlRequestV2 {
                prepared_id: policy_prepared.prepared_id,
                target_digest: policy_prepared.target_digest,
                idempotency_key: "prepared-stale-policy".into(),
            },
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_prepared_authority_changed")
    );
    let actions: i64 = p
        .manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_operations WHERE kind='lifecycle_action'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(actions, 0);
}

#[tokio::test]
async fn prepared_manager_action_reports_runtime_blocker_without_queueing() {
    let p = pilot().await;
    let prepared = {
        let store = p.manager.store.lock().await;
        store.record_manager_operator_pause(p.lead, true).unwrap();
        store
            .prepare_manager_action(p.owner, prepared_resume(&p))
            .unwrap()
    };
    assert_eq!(
        prepared.readiness,
        ManagerPreparedActionReadinessV2::Blocked
    );
    assert_eq!(
        prepared.blockers[0].code,
        ManagerPreparedActionBlockerCodeV2::HumanOrRecoveryOwner
    );
    let result = p
        .manager
        .store
        .lock()
        .await
        .commit_prepared_manager_action(
            p.owner,
            AgentManagerCommitPreparedControlRequestV2 {
                prepared_id: prepared.prepared_id,
                target_digest: prepared.target_digest,
                idempotency_key: "prepared-blocked".into(),
            },
        )
        .unwrap();
    assert!(matches!(
        result,
        ManagerPreparedActionCommitResultV2::Blocked { .. }
    ));
    let store = p.manager.store.lock().await;
    let actions: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_v2_operations WHERE kind='lifecycle_action'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(actions, 0);
}

#[tokio::test]
async fn prepared_manager_action_attributes_the_live_rotated_manager() {
    let p = pilot().await;
    let successor = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        let mut session = bare_session(successor);
        session.project_id = Some(p.project);
        session.working_dir = p.repo.clone();
        session.session_kind = SessionKind::Standard;
        session.continued_from = Some(p.owner);
        session.rotation_depth = 1;
        session.provider = SessionProvider::Claude;
        session.model = Some("manager-scripted-provider".into());
        store.insert_session(&session).unwrap();
        store
            .update_session_status(p.owner, SessionStatus::Archived)
            .unwrap();
        store
            .record_harness_manager_rotation(p.owner, successor)
            .unwrap();
    }
    let prepared = p
        .manager
        .store
        .lock()
        .await
        .prepare_manager_action(successor, prepared_resume(&p))
        .unwrap();
    let committed = p
        .manager
        .store
        .lock()
        .await
        .commit_prepared_manager_action(
            successor,
            AgentManagerCommitPreparedControlRequestV2 {
                prepared_id: prepared.prepared_id,
                target_digest: prepared.target_digest,
                idempotency_key: "rotated-manager-prepared".into(),
            },
        )
        .unwrap();
    let ManagerPreparedActionCommitResultV2::Queued { receipt } = committed else {
        panic!("rotated manager preparation did not queue")
    };
    let store = p.manager.store.lock().await;
    let identity: (String, String, String) = store
        .conn
        .query_row(
            "SELECT p.manager_session_id,p.caller_session_id,o.actor_session_id
             FROM manager_prepared_actions p
             JOIN harness_manager_v2_operations o ON o.id=p.operation_id
             WHERE p.id=?1",
            [prepared.prepared_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        identity,
        (
            p.owner.to_string(),
            successor.to_string(),
            successor.to_string()
        )
    );
    assert_eq!(
        store
            .manager_action_receipt_for_caller(
                successor,
                AgentManagerGetActionRequestV2 {
                    operation_id: receipt.operation_id,
                },
            )
            .unwrap()
            .operation_id,
        receipt.operation_id
    );
}

#[tokio::test]
async fn manager_actions_empty_launch_list_admits_all_providers_and_retains_creation_limit() {
    let mut p = pilot().await;
    p.policy.allowed_launches.clear();
    p.policy.max_active_sessions = 12;
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "unrestricted-launches".into(),
            policy: p.policy.clone(),
        })
        .unwrap();

    for (i, provider) in [
        SessionProvider::Claude,
        SessionProvider::Codex,
        SessionProvider::Pioneer,
        SessionProvider::Local,
        SessionProvider::Antigravity,
        SessionProvider::CodexAppServer,
        SessionProvider::Harness,
    ]
    .into_iter()
    .enumerate()
    {
        let mut request = p.request(
            &format!("unrestricted-provider-{i}"),
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Task,
                query: "admission only; do not start a provider".into(),
                launch: ManagerLaunchChoiceV2 {
                    provider,
                    model: format!("configured-model-{i}"),
                    effort: (i % 2 == 0).then(|| "high".into()),
                },
            },
        );
        request.fence.policy_version = 2;
        let receipt = p
            .manager
            .agent_control()
            .agent_manager_control(p.owner, request)
            .await
            .unwrap();
        assert_eq!(receipt.state, ManagerActionStateV2::Queued, "{provider:?}");
        assert_eq!(
            receipt.action_kind,
            ManagerActionKindV2::CreateSession,
            "{provider:?}"
        );
        assert_eq!(
            receipt.target_type,
            ManagerActionTargetTypeV2::ProviderSession,
            "{provider:?}"
        );
        assert_eq!(receipt.result, None, "{provider:?}");
        assert!(receipt.target_session_id.is_some());
    }

    p.policy.max_created_sessions = 0;
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 2,
            idempotency_key: "stop-creation".into(),
            policy: p.policy.clone(),
        })
        .unwrap();
    let mut request = p.request(
        "unrestricted-still-limited",
        ManagerActionV2::CreateSession {
            parent_id: p.epic,
            kind: SessionKind::Task,
            query: "creation quota still applies".into(),
            launch: ManagerLaunchChoiceV2 {
                provider: SessionProvider::Codex,
                model: "another-configured-model".into(),
                effort: Some("medium".into()),
            },
        },
    );
    request.fence.policy_version = 3;
    let error = p
        .manager
        .agent_control()
        .agent_manager_control(p.owner, request)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("manager_v2_creation_limit"));
}

#[tokio::test]
async fn manager_actions_empty_launch_list_retains_model_and_effort_validation() {
    let mut p = pilot().await;
    p.policy.allowed_launches.clear();
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "unrestricted-launches".into(),
            policy: p.policy.clone(),
        })
        .unwrap();
    for (i, (model, effort)) in [
        (String::new(), None),
        ("m".repeat(257), None),
        ("configured-model".into(), Some(String::new())),
        ("configured-model".into(), Some("e".repeat(33))),
        ("configured-model".into(), Some("high\0".into())),
    ]
    .into_iter()
    .enumerate()
    {
        let mut request = p.request(
            &format!("invalid-unrestricted-choice-{i}"),
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Task,
                query: "launch choice must still be valid".into(),
                launch: ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Codex,
                    model,
                    effort,
                },
            },
        );
        request.fence.policy_version = 2;
        let error = p
            .manager
            .agent_control()
            .agent_manager_control(p.owner, request)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("manager_v2_invalid_text"));
    }
}

#[tokio::test]
async fn manager_actions_populated_launch_list_requires_exact_provider_model_and_effort() {
    let p = pilot().await;
    let allowed = p.policy.allowed_launches[0].clone();
    let mut different_provider = allowed.clone();
    different_provider.provider = SessionProvider::Codex;
    let mut different_model = allowed.clone();
    different_model.model = "another-configured-model".into();
    let mut different_effort = allowed.clone();
    different_effort.effort = Some("high".into());
    for (i, launch) in [different_provider, different_model, different_effort]
        .into_iter()
        .enumerate()
    {
        let request = p.request(
            &format!("restricted-choice-{i}"),
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Task,
                query: "requires exact configured choice".into(),
                launch,
            },
        );
        let error = p
            .manager
            .agent_control()
            .agent_manager_control(p.owner, request)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("manager_v2_launch_not_granted"));
    }
    let receipt = p
        .admit(
            "exact-allowed-choice",
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Task,
                query: "exact configured choice".into(),
                launch: allowed,
            },
        )
        .await;
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
}

#[tokio::test]
async fn manager_actions_container_atomic_enrollment_replay_and_nonempty_refusal() {
    let p = pilot().await;
    let action = ManagerActionV2::CreateContainer {
        parent_id: Some(p.group),
        kind: SessionKind::Epic,
        name: "Created feature".into(),
        tags: vec!["Delivery".into(), "delivery".into()],
    };
    let receipt = p.admit("create", action.clone()).await;
    let id = receipt.target_session_id.unwrap();
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    assert_eq!(receipt.action_kind, ManagerActionKindV2::CreateContainer);
    assert_eq!(
        receipt.target_type,
        ManagerActionTargetTypeV2::EpicContainer
    );
    assert_eq!(receipt.result, None);
    let queued_actions = p
        .manager
        .agent_control()
        .agent_manager_inspect(
            p.owner,
            AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Actions,
                epic_id: None,
                cursor: None,
                limit: 32,
            },
        )
        .await
        .unwrap();
    let queued_row = queued_actions
        .rows
        .iter()
        .find(|row| row["id"] == receipt.operation_id.to_string())
        .unwrap();
    assert_eq!(queued_row["action_kind"], "create_container");
    assert_eq!(queued_row["target_type"], "epic_container");
    assert_eq!(queued_row["receipt_state"], "queued");
    assert_eq!(queued_row["result"], serde_json::Value::Null);
    assert!(
        p.manager
            .store
            .lock()
            .await
            .get_session(id)
            .unwrap()
            .is_none()
    );
    p.execute().await.unwrap();
    let store = p.manager.store.lock().await;
    let row = store.get_session(id).unwrap().unwrap();
    assert_eq!(row.title.as_deref(), Some("Created feature"));
    assert_eq!(row.tags, vec!["delivery"]);
    assert_eq!(row.lead_session_id, None);
    let scope = store.get_harness_manager(p.project).unwrap().unwrap();
    assert_eq!(scope.row_version, 1);
    assert!(scope.epic_ids.contains(&id));
    drop(store);
    let replay = p.admit("create", action).await;
    assert_eq!(replay.operation_id, receipt.operation_id);
    assert!(replay.deduplicated);
    assert_eq!(replay.state, ManagerActionStateV2::Succeeded);
    assert_eq!(replay.outcome.as_deref(), Some("container_committed"));
    assert_eq!(
        replay.result,
        Some(ManagerActionResultV2::ContainerCommitted {
            lead_state: Some(ManagerLeadAssignmentStateV2::Unassigned),
        })
    );
    let actions = p
        .manager
        .agent_control()
        .agent_manager_inspect(
            p.owner,
            AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Actions,
                epic_id: Some(id),
                cursor: None,
                limit: 32,
            },
        )
        .await
        .unwrap();
    let action_row = actions
        .rows
        .iter()
        .find(|row| row["id"] == receipt.operation_id.to_string())
        .unwrap();
    assert_eq!(action_row["action_kind"], "create_container");
    assert_eq!(action_row["target_type"], "epic_container");
    assert_eq!(action_row["receipt_state"], "succeeded");
    assert_eq!(action_row["result"]["target_state"], "container_committed");
    assert_eq!(action_row["result"]["lead_state"], "unassigned");
    assert_eq!(action_row["outcome"]["outcome"], "container_committed");
    let existing = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.epic)
        .unwrap()
        .unwrap();
    p.manager
        .store
        .lock()
        .await
        .update_session_status(p.lead, SessionStatus::Running)
        .unwrap();
    let denied = p
        .manager
        .agent_control()
        .agent_manager_control(
            p.owner,
            p.request(
                "archive-nonempty",
                ManagerActionV2::ArchiveContainer {
                    container_id: p.epic,
                    expected_updated_at: existing.updated_at,
                },
            ),
        )
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("container_not_terminal"));
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(p.epic)
            .unwrap()
            .unwrap()
            .status,
        existing.status
    );
    p.manager
        .store
        .lock()
        .await
        .update_session_status(p.lead, SessionStatus::Completed)
        .unwrap();
    let archived = p
        .admit(
            "archive",
            ManagerActionV2::ArchiveContainer {
                container_id: id,
                expected_updated_at: row.updated_at,
            },
        )
        .await;
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(archived.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let row = p
        .manager
        .store
        .lock()
        .await
        .get_session(id)
        .unwrap()
        .unwrap();
    assert_eq!(row.status, SessionStatus::Archived);
    p.admit(
        "restore",
        ManagerActionV2::RestoreContainer {
            container_id: id,
            expected_updated_at: row.updated_at,
        },
    )
    .await;
    p.execute().await.unwrap();
    let row = p
        .manager
        .store
        .lock()
        .await
        .get_session(id)
        .unwrap()
        .unwrap();
    assert_eq!(row.status, SessionStatus::Completed);
    assert_eq!(row.lead_session_id, None);
}

#[tokio::test]
#[allow(
    clippy::clone_on_copy,
    clippy::expect_used,
    clippy::large_futures,
    clippy::significant_drop_tightening,
    clippy::too_many_lines,
    clippy::unwrap_used
)]
async fn manager_container_archive_cascades_atomically_and_restore_uses_recorded_set() {
    let p = pilot().await;
    let worker = Uuid::new_v4();
    let nested_worker = Uuid::new_v4();
    let pre_archived = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        for (id, status) in [
            (worker, SessionStatus::Completed),
            (pre_archived, SessionStatus::Archived),
        ] {
            let mut row = bare_session(id);
            row.project_id = Some(p.project);
            row.working_dir = p.repo.clone();
            row.session_kind = SessionKind::Task;
            row.parent_id = Some(p.epic);
            row.status = status;
            store.insert_session(&row).unwrap();
        }
        let mut nested = bare_session(nested_worker);
        nested.project_id = Some(p.project);
        nested.working_dir = p.repo.clone();
        nested.session_kind = SessionKind::Task;
        nested.parent_id = Some(worker);
        store.insert_session(&nested).unwrap();
        let delete_request = p.request(
            "delete-nonempty-cascade",
            ManagerActionV2::DeleteContainer {
                container_id: p.group,
                expected_updated_at: store.get_session(p.group).unwrap().unwrap().updated_at,
            },
        );
        drop(store);
        let denied_delete = p
            .manager
            .agent_control()
            .agent_manager_control(p.owner, delete_request)
            .await;
        assert!(
            denied_delete
                .unwrap_err()
                .to_string()
                .contains("container_not_empty")
        );
    }
    let worker_row = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker)
        .unwrap()
        .unwrap();
    p.manager
        .completed
        .write()
        .await
        .insert(worker, CompletedSession::for_test(worker_row));
    let nested_worker_row = p
        .manager
        .store
        .lock()
        .await
        .get_session(nested_worker)
        .unwrap()
        .unwrap();
    p.manager
        .completed
        .write()
        .await
        .insert(nested_worker, CompletedSession::for_test(nested_worker_row));
    let pre_archived_updated_at = p
        .manager
        .store
        .lock()
        .await
        .get_session(pre_archived)
        .unwrap()
        .unwrap()
        .updated_at;
    let worker_running = p
        .manager
        .store
        .lock()
        .await
        .get_session(worker)
        .unwrap()
        .unwrap();
    p.manager
        .store
        .lock()
        .await
        .update_session_status(worker, SessionStatus::Running)
        .unwrap();
    let group_version = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.group)
        .unwrap()
        .unwrap()
        .updated_at;
    let denied = p
        .manager
        .agent_control()
        .agent_manager_control(
            p.owner,
            p.request(
                "archive-running-descendant",
                ManagerActionV2::ArchiveContainer {
                    container_id: p.group,
                    expected_updated_at: group_version.clone(),
                },
            ),
        )
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("container_not_terminal"));
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(worker)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Running
    );
    p.manager
        .store
        .lock()
        .await
        .update_session_status(worker, worker_running.status)
        .unwrap();
    p.manager
        .store
        .lock()
        .await
        .update_session_status(worker, SessionStatus::WaitingApproval)
        .unwrap();
    let waiting = p
        .manager
        .agent_control()
        .agent_manager_control(
            p.owner,
            p.request(
                "archive-waiting-approval",
                ManagerActionV2::ArchiveContainer {
                    container_id: p.group,
                    expected_updated_at: group_version.clone(),
                },
            ),
        )
        .await
        .unwrap_err();
    assert!(waiting.to_string().contains("container_not_terminal"));
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(worker)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::WaitingApproval
    );
    p.manager
        .store
        .lock()
        .await
        .update_session_status(worker, SessionStatus::Completed)
        .unwrap();
    {
        let store = p.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE sessions SET pending_question_json='{}' WHERE id=?1",
                [worker.to_string()],
            )
            .unwrap();
    }
    let question = p
        .manager
        .agent_control()
        .agent_manager_control(
            p.owner,
            p.request(
                "archive-pending-question",
                ManagerActionV2::ArchiveContainer {
                    container_id: p.group,
                    expected_updated_at: group_version,
                },
            ),
        )
        .await
        .unwrap_err();
    assert!(
        question
            .to_string()
            .contains("manager_v2_human_or_recovery_owner")
    );
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT pending_question_json FROM sessions WHERE id=?1",
                [worker.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap(),
        Some("{}".into())
    );
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET pending_question_json=NULL WHERE id=?1",
            [worker.to_string()],
        )
        .unwrap();

    let group = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.group)
        .unwrap()
        .unwrap();
    let mut cascade_events = p.manager.event_bus.subscribe();
    let archive = p
        .admit(
            "archive-group-cascade",
            ManagerActionV2::ArchiveContainer {
                container_id: p.group,
                expected_updated_at: group.updated_at,
            },
        )
        .await;
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(archive.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let mut archived_events = Vec::new();
    while let Ok(event) = cascade_events.try_recv() {
        if let crate::bus::DaemonEvent::SessionArchived { session_id, .. } = event.as_ref() {
            archived_events.push(*session_id);
        }
    }
    archived_events.sort();
    let mut expected_events = vec![p.group, p.epic, p.lead, worker, nested_worker];
    expected_events.sort();
    assert_eq!(archived_events, expected_events);
    let ids = p
        .manager
        .store
        .lock()
        .await
        .manager_action_cascade_archive_ids(p.group)
        .unwrap()
        .unwrap()
        .ids;
    assert_eq!(ids.len(), 5);
    assert!(
        ids.contains(&p.group)
            && ids.contains(&p.epic)
            && ids.contains(&p.lead)
            && ids.contains(&worker)
            && ids.contains(&nested_worker)
    );
    assert!(!ids.contains(&pre_archived));
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(pre_archived)
            .unwrap()
            .unwrap()
            .updated_at,
        pre_archived_updated_at
    );
    for id in [p.group, p.epic, p.lead, worker, nested_worker] {
        assert_eq!(
            p.manager
                .store
                .lock()
                .await
                .get_session(id)
                .unwrap()
                .unwrap()
                .status,
            SessionStatus::Archived
        );
        assert!(!p.manager.completed.read().await.contains_key(&id));
    }
    let replay = p
        .admit(
            "archive-group-cascade",
            ManagerActionV2::ArchiveContainer {
                container_id: p.group,
                expected_updated_at: group.updated_at,
            },
        )
        .await;
    assert_eq!(replay.operation_id, archive.operation_id);
    assert!(replay.deduplicated);
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .manager_action_cascade_archive_ids(p.group)
            .unwrap()
            .unwrap()
            .ids,
        ids
    );

    let archived_group = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.group)
        .unwrap()
        .unwrap();
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET pending_archive=1 WHERE id=?1",
            [p.lead.to_string()],
        )
        .unwrap();
    p.admit(
        "restore-group-cascade",
        ManagerActionV2::RestoreContainer {
            container_id: p.group,
            expected_updated_at: archived_group.updated_at,
        },
    )
    .await;
    p.execute().await.unwrap();
    let mut restored_events = Vec::new();
    while let Ok(event) = cascade_events.try_recv() {
        if let crate::bus::DaemonEvent::SessionUnarchived { session_id } = event.as_ref() {
            restored_events.push(*session_id);
        }
    }
    restored_events.sort();
    assert_eq!(restored_events, expected_events);
    p.manager.event_bus.unsubscribe();
    assert!(
        !p.manager
            .store
            .lock()
            .await
            .get_session(p.lead)
            .unwrap()
            .unwrap()
            .pending_archive
    );
    for id in [p.group, p.epic, p.lead, worker, nested_worker] {
        assert_eq!(
            p.manager
                .store
                .lock()
                .await
                .get_session(id)
                .unwrap()
                .unwrap()
                .status,
            SessionStatus::Completed
        );
        assert!(
            p.manager
                .completed
                .read()
                .await
                .get(&id)
                .unwrap()
                .events_hydrated
        );
    }
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(pre_archived)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Archived
    );
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(pre_archived)
            .unwrap()
            .unwrap()
            .updated_at,
        pre_archived_updated_at
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
        None
    );
    let restored_group = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.group)
        .unwrap()
        .unwrap();
    p.manager
        .store
        .lock()
        .await
        .update_session_status(p.group, SessionStatus::Archived)
        .unwrap();
    let externally_archived = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.group)
        .unwrap()
        .unwrap();
    assert!(externally_archived.updated_at > restored_group.updated_at);
    let stale_restore = p
        .manager
        .agent_control()
        .agent_manager_control(
            p.owner,
            p.request(
                "restore-after-external-archive",
                ManagerActionV2::RestoreContainer {
                    container_id: p.group,
                    expected_updated_at: externally_archived.updated_at,
                },
            ),
        )
        .await
        .unwrap_err();
    assert!(stale_restore.to_string().contains("container_not_empty"));
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(worker)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Completed
    );
}

#[tokio::test]
#[allow(clippy::expect_used, clippy::unwrap_used)]
#[allow(clippy::large_futures, clippy::significant_drop_tightening)]
async fn manager_container_archive_refuses_an_active_map_descendant() {
    let p = pilot().await;
    let child = Uuid::new_v4();
    let mut row = bare_session(child);
    row.project_id = Some(p.project);
    row.working_dir = p.repo.clone();
    row.session_kind = SessionKind::Task;
    row.parent_id = Some(p.epic);
    p.manager.store.lock().await.insert_session(&row).unwrap();
    p.manager
        .completed
        .write()
        .await
        .insert(child, CompletedSession::for_test(row));
    p.manager
        .active
        .write()
        .await
        .insert(child, super::fake_alive_tracked(child).await);
    let group = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.group)
        .unwrap()
        .unwrap();
    p.admit(
        "active-map-cascade-refusal",
        ManagerActionV2::ArchiveContainer {
            container_id: p.group,
            expected_updated_at: group.updated_at,
        },
    )
    .await;
    assert!(
        p.execute()
            .await
            .unwrap_err()
            .to_string()
            .contains("container_not_terminal")
    );
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(child)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Completed
    );
}

#[tokio::test]
#[allow(clippy::expect_used, clippy::unwrap_used)]
#[allow(clippy::large_futures, clippy::significant_drop_tightening)]
async fn manager_container_archive_refuses_a_retry_owned_descendant() {
    let p = pilot().await;
    let child = Uuid::new_v4();
    let mut row = bare_session(child);
    row.project_id = Some(p.project);
    row.working_dir = p.repo.clone();
    row.session_kind = SessionKind::Task;
    row.parent_id = Some(p.epic);
    p.manager.store.lock().await.insert_session(&row).unwrap();
    let (retry_cancel, mut retry_observer) = tokio::sync::oneshot::channel();
    let mut completed = CompletedSession::for_test(row);
    completed.retry_cancel = Some(retry_cancel);
    p.manager.completed.write().await.insert(child, completed);
    let group = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.group)
        .unwrap()
        .unwrap();
    p.admit(
        "retry-owner-cascade-refusal",
        ManagerActionV2::ArchiveContainer {
            container_id: p.group,
            expected_updated_at: group.updated_at,
        },
    )
    .await;
    assert!(
        p.execute()
            .await
            .unwrap_err()
            .to_string()
            .contains("manager_v2_human_or_recovery_owner")
    );
    assert!(matches!(
        retry_observer.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(child)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Completed
    );
}

#[tokio::test]
#[allow(clippy::expect_used, clippy::unwrap_used)]
#[allow(clippy::large_futures)]
async fn manager_container_restore_refuses_a_stale_recorded_member() {
    let p = pilot().await;
    let group = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.group)
        .unwrap()
        .unwrap();
    p.admit(
        "archive-for-member-stale",
        ManagerActionV2::ArchiveContainer {
            container_id: p.group,
            expected_updated_at: group.updated_at,
        },
    )
    .await;
    p.execute().await.unwrap();
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET updated_at=?2 WHERE id=?1",
            rusqlite::params![
                p.lead.to_string(),
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
    let archived_group = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.group)
        .unwrap()
        .unwrap();
    p.admit(
        "restore-stale-member",
        ManagerActionV2::RestoreContainer {
            container_id: p.group,
            expected_updated_at: archived_group.updated_at,
        },
    )
    .await;
    assert!(
        p.execute()
            .await
            .unwrap_err()
            .to_string()
            .contains("manager_v2_cascade_record_stale")
    );
    for id in [p.group, p.epic, p.lead] {
        assert_eq!(
            p.manager
                .store
                .lock()
                .await
                .get_session(id)
                .unwrap()
                .unwrap()
                .status,
            SessionStatus::Archived
        );
    }
}

#[tokio::test]
#[allow(clippy::expect_used, clippy::unwrap_used)]
#[allow(clippy::large_futures)]
async fn manager_container_restore_refuses_a_purged_gitworktree_leaf() {
    let p = pilot().await;
    let group = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.group)
        .unwrap()
        .unwrap();
    p.admit(
        "archive-for-purged-leaf",
        ManagerActionV2::ArchiveContainer {
            container_id: p.group,
            expected_updated_at: group.updated_at,
        },
    )
    .await;
    p.execute().await.unwrap();
    p.manager.store.lock().await.conn.execute(
        "UPDATE sessions SET sandbox_kind='GitWorktree',sandbox_cleanup_state='Purged' WHERE id=?1",
        [p.lead.to_string()],
    ).unwrap();
    let archived_group = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.group)
        .unwrap()
        .unwrap();
    p.admit(
        "restore-purged-leaf",
        ManagerActionV2::RestoreContainer {
            container_id: p.group,
            expected_updated_at: archived_group.updated_at,
        },
    )
    .await;
    assert!(
        p.execute()
            .await
            .unwrap_err()
            .to_string()
            .contains("manager_v2_historical_restore_refused")
    );
    for id in [p.group, p.epic, p.lead] {
        assert_eq!(
            p.manager
                .store
                .lock()
                .await
                .get_session(id)
                .unwrap()
                .unwrap()
                .status,
            SessionStatus::Archived
        );
    }
}

#[tokio::test]
async fn manager_actions_exact_payload_authority_and_restart_claims() {
    let p = pilot().await;
    let action = ManagerActionV2::PauseLead {
        epic_id: p.epic,
        expected: p.fence().await,
        reason: "manager pause".into(),
    };
    let receipt = p.admit("pause", action.clone()).await;
    let changed = ManagerActionV2::PauseLead {
        epic_id: p.epic,
        expected: p.fence().await,
        reason: "different content".into(),
    };
    let conflict = p
        .manager
        .agent_control()
        .agent_manager_control(p.owner, p.request("pause", changed))
        .await
        .unwrap_err();
    assert!(conflict.to_string().contains("idempotency_conflict"));
    assert!(
        p.manager
            .agent_control()
            .agent_manager_control(p.lead, p.request("spoof", action.clone()))
            .await
            .is_err()
    );
    let claim = p.claim().await;
    assert!(
        p.manager
            .store
            .lock()
            .await
            .claim_manager_action(Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    p.manager
        .store
        .lock()
        .await
        .recover_manager_actions_startup(Uuid::new_v4())
        .unwrap();
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Uncertain
    );
    assert!(
        p.manager
            .store
            .lock()
            .await
            .manager_action_runtime_gate(&claim, true)
            .is_err()
    );
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "revoke".into(),
            policy: ManagerPolicyV2::default(),
        })
        .unwrap();
    assert!(
        p.manager
            .agent_control()
            .agent_manager_control(p.owner, p.request("pause", action))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn manager_actions_aba_raw_question_operator_pause_and_retry_switch() {
    let p = pilot().await;
    let original = p.fence().await;
    let receipt = p
        .admit(
            "pause",
            ManagerActionV2::PauseLead {
                epic_id: p.epic,
                expected: original.clone(),
                reason: "pause".into(),
            },
        )
        .await;
    let claim = p.claim().await;
    {
        let store = p.manager.store.lock().await;
        store.set_lead_session(p.epic, None).unwrap();
        store.set_lead_session(p.epic, Some(p.lead)).unwrap();
    }
    assert!(
        p.manager
            .execute_manager_action(&claim)
            .await
            .unwrap_err()
            .to_string()
            .contains("lead_changed")
    );
    p.manager
        .store
        .lock()
        .await
        .finish_manager_action(&claim, ManagerActionStateV2::Blocked, "stale")
        .unwrap();
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Blocked
    );
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET pending_question_json='malformed' WHERE id=?1",
            [p.lead.to_string()],
        )
        .unwrap();
    p.admit(
        "question",
        ManagerActionV2::ResumeLead {
            epic_id: p.epic,
            expected: p.fence().await,
            message: "resume".into(),
        },
    )
    .await;
    let question = p.claim().await;
    assert!(
        p.manager
            .execute_manager_action(&question)
            .await
            .unwrap_err()
            .to_string()
            .contains("human_or_recovery_owner")
    );
    p.manager
        .store
        .lock()
        .await
        .finish_manager_action(&question, ManagerActionStateV2::Blocked, "question")
        .unwrap();
    {
        let store = p.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE sessions SET pending_question_json=NULL,status='Failed' WHERE id=?1",
                [p.lead.to_string()],
            )
            .unwrap();
        store.record_manager_operator_pause(p.lead, true).unwrap();
    }
    p.admit(
        "operator",
        ManagerActionV2::ResumeLead {
            epic_id: p.epic,
            expected: p.fence().await,
            message: "resume".into(),
        },
    )
    .await;
    let claim = p.claim().await;
    assert!(p.manager.execute_manager_action(&claim).await.is_err());
    p.manager
        .store
        .lock()
        .await
        .finish_manager_action(&claim, ManagerActionStateV2::Blocked, "operator_pause")
        .unwrap();
    p.manager
        .store
        .lock()
        .await
        .record_manager_operator_pause(p.lead, false)
        .unwrap();
    p.manager
        .runtime_config
        .retry_enabled
        .store(false, Ordering::Relaxed);
    p.admit(
        "retry",
        ManagerActionV2::RetryLead {
            epic_id: p.epic,
            expected: p.fence().await,
            message: "retry".into(),
            launch: None,
        },
    )
    .await;
    assert!(
        p.manager
            .store
            .lock()
            .await
            .claim_manager_action(p.manager.program_run_boot_id)
            .unwrap()
            .is_none()
    );
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(
        p.execute()
            .await
            .unwrap_err()
            .to_string()
            .contains("retry_disabled")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_fresh_provider_custody_and_explicit_assignment() {
    let p = pilot().await;
    // The first child of this newly created Epic must remain unappointed.
    let parent = p
        .admit(
            "empty-epic",
            ManagerActionV2::CreateContainer {
                parent_id: Some(p.group),
                kind: SessionKind::Epic,
                name: "Separate assignment".into(),
                tags: vec!["manager".into()],
            },
        )
        .await
        .target_session_id
        .unwrap();
    p.execute().await.unwrap();
    let receipt = p
        .admit(
            "spawn",
            ManagerActionV2::CreateSession {
                parent_id: parent,
                kind: SessionKind::Feature,
                query: "implement scoped work".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    assert_eq!(receipt.action_kind, ManagerActionKindV2::CreateSession);
    assert_eq!(
        receipt.target_type,
        ManagerActionTargetTypeV2::ProviderSession
    );
    assert_eq!(receipt.result, None);
    let child = receipt.target_session_id.unwrap();
    let process = super::super::launch::install_controller_candidate_test_process(child);
    p.execute().await.unwrap();
    let established = p.receipt(receipt.operation_id).await;
    assert_eq!(established.state, ManagerActionStateV2::Succeeded);
    assert_eq!(established.outcome.as_deref(), Some("session_established"));
    assert_eq!(
        established.result,
        Some(ManagerActionResultV2::ProviderEstablished { lead_state: None })
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    {
        let store = p.manager.store.lock().await;
        let row = store.get_session(child).unwrap().unwrap();
        assert_eq!(row.parent_id, Some(parent));
        assert_eq!(
            store.get_session(parent).unwrap().unwrap().lead_session_id,
            None
        );
        let custody = store.live_custody_for_session(child).unwrap();
        assert_eq!(custody.owner_session_id, child);
        assert!(store.session_model_invocation_id(child).unwrap().is_some());
        assert_eq!(
            git(row.sandbox_root.as_ref().unwrap(), &["rev-parse", "HEAD"]),
            git(&p.repo, &["rev-parse", "HEAD"])
        );
    }
    let expected = p
        .manager
        .store
        .lock()
        .await
        .manager_action_lead_fence(parent)
        .unwrap();
    p.admit(
        "assign",
        ManagerActionV2::AssignLead {
            epic_id: parent,
            expected,
            session_id: Some(child),
        },
    )
    .await;
    p.execute().await.unwrap();
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(parent)
            .unwrap()
            .unwrap()
            .lead_session_id,
        Some(child)
    );
    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, child)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(child);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_replacement_preserves_committed_predecessor_and_blocks_dirty_source() {
    let p = pilot().await;
    let source_before = git(&p.repo, &["rev-parse", "HEAD"]);
    let receipt = p
        .admit(
            "replace",
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "continue preserved work".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    let child = receipt.target_session_id.unwrap();
    let process = super::super::launch::install_controller_candidate_test_process(child);
    p.execute().await.unwrap();
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    let row = p
        .manager
        .store
        .lock()
        .await
        .get_session(child)
        .unwrap()
        .unwrap();
    let root = row.sandbox_root.unwrap();
    assert_ne!(root, p.repo);
    assert_eq!(row.continued_from, Some(p.lead));
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), source_before);
    assert_eq!(git(&p.repo, &["rev-parse", "HEAD"]), source_before);
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(p.epic)
            .unwrap()
            .unwrap()
            .lead_session_id,
        Some(child)
    );
    std::fs::write(root.join("dirty-work"), "preserve me").unwrap();
    let error = p
        .manager
        .agent_control()
        .agent_manager_control(
            p.owner,
            p.request(
                "dirty-replace",
                ManagerActionV2::ReplaceLead {
                    epic_id: p.epic,
                    expected: p.fence().await,
                    query: "replacement".into(),
                    launch: p.policy.allowed_launches[0].clone(),
                },
            ),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("source_worktree_dirty"));
    assert_eq!(
        std::fs::read_to_string(root.join("dirty-work")).unwrap(),
        "preserve me"
    );
    wait_launch_event(&p, child).await;
    p.admit(
        "pause-for-commit",
        ManagerActionV2::PauseLead {
            epic_id: p.epic,
            expected: p.fence().await,
            reason: "commit predecessor work".into(),
        },
    )
    .await;
    p.execute().await.unwrap();
    git(&root, &["add", "dirty-work"]);
    git(
        &root,
        &["commit", "-qm", "preserved predecessor implementation"],
    );
    let committed = git(&root, &["rev-parse", "HEAD"]);
    assert_ne!(committed, source_before);
    let next = p
        .admit(
            "preserved-replace",
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "continue committed predecessor work".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await
        .target_session_id
        .unwrap();
    let successor = super::super::launch::install_controller_candidate_test_process(next);
    p.execute().await.unwrap();
    assert_eq!(successor.productive_start_count.load(Ordering::SeqCst), 1);
    let successor_root = p
        .manager
        .store
        .lock()
        .await
        .get_session(next)
        .unwrap()
        .unwrap()
        .sandbox_root
        .unwrap();
    assert_ne!(successor_root, root);
    assert_eq!(git(&successor_root, &["rev-parse", "HEAD"]), committed);
    assert_eq!(git(&p.repo, &["rev-parse", "HEAD"]), source_before);
    assert_eq!(
        std::fs::read_to_string(root.join("dirty-work")).unwrap(),
        "preserve me"
    );
    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, next)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(next);
    super::super::launch::drop_controller_candidate_test_stream(child);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(
    clippy::unwrap_used,
    clippy::significant_drop_tightening,
    clippy::large_futures
)]
async fn manager_replace_lead_reissues_request_and_new_lead_reply_reaches_manager() {
    let p = pilot().await;
    let original = p
        .manager
        .store
        .lock()
        .await
        .manager_send(
            p.owner,
            &AgentManagerSendRequestV1 {
                epic_id: p.epic,
                message: "Report replacement evidence".into(),
                idempotency_key: "replace-mail".into(),
            },
        )
        .unwrap();
    let receipt = p
        .admit(
            "replace-with-mail",
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "continue work and report".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    let child = receipt.target_session_id.unwrap();
    let _process = super::super::launch::install_controller_candidate_test_process(child);
    p.execute().await.unwrap();
    let store = p.manager.store.lock().await;
    let inbox = store
        .manager_inbox(child, &AgentManagerInboxRequestV1::default())
        .unwrap();
    assert_eq!(inbox.messages.len(), 1);
    let new_id = inbox.messages[0].message_id;
    assert_eq!(inbox.messages[0].message, "Report replacement evidence");
    assert_eq!(
        inbox.messages[0].readdressed_from,
        Some(original.message_id)
    );
    let stale = store
        .manager_reply(
            child,
            &AgentManagerReplyRequestV1 {
                request_id: original.message_id,
                message: "old ID".into(),
                idempotency_key: "stale-replacement-reply".into(),
            },
        )
        .unwrap_err();
    assert!(stale.to_string().contains("manager_request_readdressed"));
    assert!(stale.to_string().contains(&new_id.to_string()));
    store
        .manager_reply(
            child,
            &AgentManagerReplyRequestV1 {
                request_id: new_id,
                message: "Replacement evidence complete".into(),
                idempotency_key: "replacement-reply".into(),
            },
        )
        .unwrap();
    let manager_inbox = store
        .manager_inbox(p.owner, &AgentManagerInboxRequestV1::default())
        .unwrap();
    assert!(
        manager_inbox
            .messages
            .iter()
            .any(|message| message.request_id == Some(new_id)
                && message.readdressed_from == Some(original.message_id))
    );
    let rows = store
        .manager_v2_request_rows(
            &store.get_harness_manager(p.project).unwrap().unwrap(),
            Some(p.epic),
            "",
            32,
            false,
        )
        .unwrap();
    assert_eq!(
        rows.iter()
            .find(|row| row["request_id"] == new_id.to_string())
            .unwrap()["readdressed_from"],
        original.message_id.to_string()
    );
    drop(store);
    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, child)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(child);
}

#[tokio::test]
#[allow(clippy::unwrap_used, clippy::significant_drop_tightening)]
async fn operator_set_epic_lead_reissues_request_without_duplicate_on_replay() {
    let p = pilot().await;
    let original = p
        .manager
        .store
        .lock()
        .await
        .manager_send(
            p.owner,
            &AgentManagerSendRequestV1 {
                epic_id: p.epic,
                message: "Operator replacement report".into(),
                idempotency_key: "operator-lead-mail".into(),
            },
        )
        .unwrap();
    let next = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        let mut candidate = store.get_session(p.lead).unwrap().unwrap();
        candidate.id = next;
        candidate.continued_from = None;
        candidate.status = SessionStatus::Completed;
        store.insert_session(&candidate).unwrap();
    }
    p.manager.set_epic_lead(p.epic, Some(next)).await.unwrap();
    p.manager.set_epic_lead(p.epic, Some(next)).await.unwrap();
    let store = p.manager.store.lock().await;
    let inbox = store
        .manager_inbox(next, &AgentManagerInboxRequestV1::default())
        .unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(
        inbox.messages[0].readdressed_from,
        Some(original.message_id)
    );
    let progress = store.manager_progress(p.owner).unwrap();
    assert!(
        progress
            .recent_requests
            .iter()
            .any(|item| item.request_id == original.message_id
                && item.readdressed_to == Some(inbox.messages[0].message_id))
    );
}

#[tokio::test]
async fn manager_actions_internal_intent_has_daemon_attribution_and_policy_gate() {
    let p = pilot().await;
    let request = p.request(
        "intent",
        ManagerActionV2::PauseLead {
            epic_id: p.epic,
            expected: p.fence().await,
            reason: "intent pause".into(),
        },
    );
    let receipt = p
        .manager
        .store
        .lock()
        .await
        .enqueue_manager_action(
            ManagerActionOriginV2::OperatingIntent {
                project_id: p.project,
                intent_id: Uuid::new_v4(),
            },
            request,
        )
        .unwrap();
    let actor: Option<String> = p
        .manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT actor_session_id FROM harness_manager_v2_operations WHERE id=?1",
            [receipt.operation_id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actor, None);
    intent::wait_due(&p, receipt.operation_id).await;
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
}

async fn wait_launch_event(p: &Pilot, id: Uuid) {
    tokio::time::timeout(std::time::Duration::from_secs(10),async {
        loop {
            let ready=p.manager.store.lock().await.conn.query_row("SELECT EXISTS(SELECT 1 FROM conversation_events WHERE session_id=?1 AND role='User')",[id.to_string()],|r|r.get::<_,bool>(0)).unwrap();
            if ready && p.manager.persistence.pending.load(Ordering::SeqCst)==0 {break}
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_live_pause_then_same_session_resume_preserves_dirty_custody() {
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
    let id = created.target_session_id.unwrap();
    let initial = super::super::launch::install_controller_candidate_test_process(id);
    p.execute().await.unwrap();
    wait_launch_event(&p, id).await;
    let before_init = p.fence().await.event_sequence;
    super::super::launch::send_controller_candidate_test_event(
        id,
        crate::claude::StreamEvent {
            event_type: "system".into(),
            data: serde_json::json!({"subtype":"init","session_id":"manager-resumable-provider"}),
        },
    )
    .await;
    // Capture is projected before the init event is durably appended. Observe
    // that append before taking the pause fence; an active-map-only wait races
    // the very event-sequence guard this test is exercising.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if p.fence().await.event_sequence > before_init
                && p.manager.persistence.pending.load(Ordering::SeqCst) == 0
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let root = p
        .manager
        .store
        .lock()
        .await
        .get_session(id)
        .unwrap()
        .unwrap()
        .sandbox_root
        .unwrap();
    let custody = p
        .manager
        .store
        .lock()
        .await
        .live_custody_for_session(id)
        .unwrap();
    let pause = p
        .admit(
            "pause-live",
            ManagerActionV2::PauseLead {
                epic_id: p.epic,
                expected: p.fence().await,
                reason: "manager pause".into(),
            },
        )
        .await;
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(pause.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert!(initial.interrupt_count.load(Ordering::SeqCst) >= 1);
    assert!(!p.manager.active.read().await.contains_key(&id));
    std::fs::write(root.join("unfinished"), "retain dirty edit").unwrap();
    let resume = p
        .admit(
            "resume-dirty",
            ManagerActionV2::ResumeLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "continue the existing work".into(),
            },
        )
        .await;
    let process = super::super::launch::install_controller_candidate_test_process(id);
    // The scripted provider has no OS descendants. Exercise the ordinary
    // checked reaper against this test's inventory, not unrelated host PIDs.
    #[cfg(target_os = "linux")]
    let orphan_fixture = crate::session::reaper::StartupReaperFixture::new();
    #[cfg(target_os = "linux")]
    let _orphan_guard = orphan_fixture.scoped_runtime_reap_root(id).unwrap();
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(resume.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    let invocation_key:String=p.manager.store.lock().await.conn.query_row("SELECT m.dedup_key FROM model_invocations m JOIN sessions s ON s.model_invocation_id=m.id WHERE s.id=?1",[id.to_string()],|r|r.get(0)).unwrap();
    assert_eq!(
        invocation_key,
        format!("manager.action:{}", resume.operation_id)
    );
    let after = p
        .manager
        .store
        .lock()
        .await
        .live_custody_for_session(id)
        .unwrap();
    assert_eq!(after.custody_id, custody.custody_id);
    assert_eq!(after.generation, custody.generation);
    assert_eq!(after.sandbox_root, root);
    assert_eq!(
        std::fs::read_to_string(root.join("unfinished")).unwrap(),
        "retain dirty edit"
    );
    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, id)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_guard_rechecks_revocation_before_provider_and_preserves_allocation() {
    let p = pilot().await;
    let receipt = p
        .admit(
            "late-revoke",
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "start".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    let id = receipt.target_session_id.unwrap();
    let process = super::super::launch::install_controller_candidate_test_process(id);
    let (reached, resume) = super::super::launch::install_direct_launch_custody_test_pause(
        &format!("manager.action:{}", receipt.operation_id),
    );
    let claim = p.claim().await;
    let revoke = async {
        assert_eq!(reached.await.unwrap(), id);
        p.manager
            .store
            .lock()
            .await
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: p.project,
                expected_scope_version: 1,
                expected_policy_version: 1,
                idempotency_key: "revoke-live".into(),
                policy: ManagerPolicyV2::default(),
            })
            .unwrap();
        resume.send(()).unwrap();
    };
    let execution = async {
        let result = p.manager.execute_manager_action(&claim).await;
        if let Err(error) = &result {
            assert!(
                error.to_string().contains("policy_changed"),
                "unexpected pre-fence launch failure: {error}"
            );
        }
        result
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(execution, revoke)
    })
    .await
    .expect("revocation fixture must finish or report its launch refusal");
    assert!(result.unwrap_err().to_string().contains("policy_changed"));
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    let store = p.manager.store.lock().await;
    assert_eq!(
        store.get_session(p.epic).unwrap().unwrap().lead_session_id,
        Some(p.lead)
    );
    let candidate = store.get_session(id).unwrap().unwrap();
    assert_eq!(candidate.status, SessionStatus::Failed);
    assert!(candidate.sandbox_root.unwrap().is_dir());
    let invocation = store.session_model_invocation_id(id).unwrap().unwrap();
    let status: String = store
        .conn
        .query_row(
            "SELECT status FROM model_invocations WHERE id=?1",
            [invocation.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "failed");
    super::super::launch::drop_controller_candidate_test_process(id);
}

#[tokio::test]
async fn manager_actions_container_transaction_rolls_back_tags_enrollment_and_success_receipt() {
    let p = pilot().await;
    let receipt = p
        .admit(
            "atomic",
            ManagerActionV2::CreateContainer {
                parent_id: Some(p.group),
                kind: SessionKind::Epic,
                name: "Atomic creation".into(),
                tags: vec!["test".into()],
            },
        )
        .await;
    let id = receipt.target_session_id.unwrap();
    p.manager.store.lock().await.conn.execute_batch("CREATE TEMP TRIGGER reject_manager_tag BEFORE INSERT ON session_tags BEGIN SELECT RAISE(ABORT,'injected tag failure'); END;").unwrap();
    assert!(p.execute().await.is_err());
    let store = p.manager.store.lock().await;
    assert!(store.get_session(id).unwrap().is_none());
    assert_eq!(
        store
            .get_harness_manager(p.project)
            .unwrap()
            .unwrap()
            .epic_ids,
        vec![p.epic]
    );
    assert_eq!(
        store
            .manager_action_operation(receipt.operation_id)
            .unwrap()
            .unwrap()
            .receipt
            .state,
        ManagerActionStateV2::Running
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_codex_app_server_establishment_precedes_assignment() {
    use std::os::unix::fs::PermissionsExt;
    let mut p = pilot().await;
    let choice = ManagerLaunchChoiceV2 {
        provider: SessionProvider::CodexAppServer,
        model: "gpt-5.4".into(),
        effort: None,
    };
    p.policy.allowed_launches.push(choice.clone());
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "appserver".into(),
            policy: p.policy.clone(),
        })
        .unwrap();
    let mut request = p.request(
        "appserver-launch",
        ManagerActionV2::ReplaceLead {
            epic_id: p.epic,
            expected: p.fence().await,
            query: "start an app-server lead".into(),
            launch: choice,
        },
    );
    request.fence.policy_version = 2;
    let receipt = p
        .manager
        .agent_control()
        .agent_manager_control(p.owner, request)
        .await
        .unwrap();
    let id = receipt.target_session_id.unwrap();
    let binary = p._dir.path().join("fake-app-server");
    std::fs::write(&binary,r#"#!/bin/sh
trap 'exit 0' INT TERM
while IFS= read -r request; do
  case "$request" in
    *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}' ;;
    *'"method":"thread/start"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"manager-thread"}}}' ;;
  esac
done
"#).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    super::super::launch::install_controller_candidate_test_app_server_binary(id, binary);
    tokio::time::timeout(std::time::Duration::from_secs(20), p.execute())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Succeeded
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
        Some(id)
    );
    assert!(matches!(
        p.manager.active.read().await.get(&id).unwrap().process,
        Some(super::super::types::ProviderProcess::CodexAppServer(_))
    ));
    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, id)
        .await
        .unwrap();
}

#[tokio::test]
async fn manager_actions_resource_decision_holds_and_queued_policy_changes_are_fenced() {
    use crate::store::manager_actions::*;
    let p = pilot().await;
    let receipt = p
        .admit(
            "hold",
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Feature,
                query: "work".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    let claim = p.claim().await;
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        let key = manager_action_hold_key(ManagerActionHoldReasonV2::Decision, Some(p.epic));
        store
            .manager_v2_put_record(
                &config,
                MANAGER_ACTION_HOLD_KIND,
                &key,
                Some(p.epic),
                0,
                &serde_json::to_value(ManagerActionRuntimeHoldV2 { blocked: true }).unwrap(),
            )
            .unwrap();
    }
    assert!(
        p.manager
            .execute_manager_action(&claim)
            .await
            .unwrap_err()
            .to_string()
            .contains("decision_hold")
    );
    assert!(
        p.manager
            .store
            .lock()
            .await
            .get_session(receipt.target_session_id.unwrap())
            .unwrap()
            .is_none()
    );
    p.manager
        .store
        .lock()
        .await
        .finish_manager_action(&claim, ManagerActionStateV2::Blocked, "decision_hold")
        .unwrap();
    let mut policy = p.policy.clone();
    policy.paused_epic_ids = vec![p.epic];
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "pause-policy".into(),
            policy,
        })
        .unwrap();
    assert!(
        p.manager
            .agent_control()
            .agent_manager_control(p.owner, p.request("hold", claim.action().clone()))
            .await
            .unwrap_err()
            .to_string()
            .contains("policy_changed")
    );
}

#[tokio::test]
async fn manager_actions_update_delete_restore_and_group_scope_are_distinct() {
    let p = pilot().await;
    let mut policy = p.policy.clone();
    policy.group_ids.clear();
    policy.allow_create_groups = false;
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "only-epic".into(),
            policy,
        })
        .unwrap();
    let group = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.group)
        .unwrap()
        .unwrap();
    let mut denied = p.request(
        "forbidden-group",
        ManagerActionV2::UpdateContainer {
            container_id: p.group,
            expected_updated_at: group.updated_at,
            name: "scope escape".into(),
            description: None,
        },
    );
    denied.fence.policy_version = 2;
    assert!(
        p.manager
            .agent_control()
            .agent_manager_control(p.owner, denied)
            .await
            .is_err()
    );
    let epic = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.epic)
        .unwrap()
        .unwrap();
    let mut update = p.request(
        "allowed-epic",
        ManagerActionV2::UpdateContainer {
            container_id: p.epic,
            expected_updated_at: epic.updated_at,
            name: "Scoped feature title".into(),
            description: Some("Scoped description".into()),
        },
    );
    update.fence.policy_version = 2;
    p.manager
        .agent_control()
        .agent_manager_control(p.owner, update)
        .await
        .unwrap();
    p.execute().await.unwrap();
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(p.epic)
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some("Scoped feature title")
    );
    // A separate root Group is authorized by its creation provenance.
    let q = pilot().await;
    let id = q
        .admit(
            "group",
            ManagerActionV2::CreateContainer {
                parent_id: None,
                kind: SessionKind::Group,
                name: "Retained group".into(),
                tags: vec!["group".into()],
            },
        )
        .await
        .target_session_id
        .unwrap();
    q.execute().await.unwrap();
    let row = q
        .manager
        .store
        .lock()
        .await
        .get_session(id)
        .unwrap()
        .unwrap();
    q.admit(
        "delete",
        ManagerActionV2::DeleteContainer {
            container_id: id,
            expected_updated_at: row.updated_at,
        },
    )
    .await;
    q.execute().await.unwrap();
    let row = q
        .manager
        .store
        .lock()
        .await
        .get_session(id)
        .unwrap()
        .unwrap();
    assert_eq!(row.status, SessionStatus::Deleted);
    q.admit(
        "restore-deleted",
        ManagerActionV2::RestoreContainer {
            container_id: id,
            expected_updated_at: row.updated_at,
        },
    )
    .await;
    q.execute().await.unwrap();
    assert_eq!(
        q.manager
            .store
            .lock()
            .await
            .get_session(id)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Completed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_retry_uses_own_journal_budget_without_forging_c5() {
    let p = pilot().await;
    p.manager
        .store
        .lock()
        .await
        .update_session_status(p.lead, SessionStatus::Failed)
        .unwrap();
    p.manager
        .runtime_config
        .retry_enabled
        .store(true, Ordering::Relaxed);
    let receipt = p
        .admit(
            "recover-failed",
            ManagerActionV2::RetryLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "recover the failed work".into(),
                launch: None,
            },
        )
        .await;
    let id = receipt.target_session_id.unwrap();
    let process = super::super::launch::install_controller_candidate_test_process(id);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    p.execute().await.unwrap();
    let store = p.manager.store.lock().await;
    let child = store.get_session(id).unwrap().unwrap();
    assert_eq!(child.continued_from, Some(p.lead));
    assert_eq!(child.retry_attempt, Some(0));
    assert_eq!(
        store.get_session(p.epic).unwrap().unwrap().lead_session_id,
        Some(id)
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    let marker: bool = store
        .conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM daemon_settings WHERE key=?1)",
            [crate::store::daemon_settings::c5_autofile_pending_key(
                p.lead,
            )],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!marker);
    drop(store);
    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, id)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_live_reassignment_settles_old_incarnation_and_reaper_failure_blocks_turnover()
 {
    let p = pilot().await;
    let first = p
        .admit(
            "first-lead",
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "old work".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await
        .target_session_id
        .unwrap();
    let old_process = super::super::launch::install_controller_candidate_test_process(first);
    p.execute().await.unwrap();
    wait_launch_event(&p, first).await;
    let next = p
        .admit(
            "candidate",
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Feature,
                query: "new work".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await
        .target_session_id
        .unwrap();
    let new_process = super::super::launch::install_controller_candidate_test_process(next);
    p.execute().await.unwrap();
    wait_launch_event(&p, next).await;
    p.admit(
        "assign-live",
        ManagerActionV2::AssignLead {
            epic_id: p.epic,
            expected: p.fence().await,
            session_id: Some(next),
        },
    )
    .await;
    p.execute().await.unwrap();
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(p.epic)
            .unwrap()
            .unwrap()
            .lead_session_id,
        Some(next)
    );
    assert!(!p.manager.active.read().await.contains_key(&first));
    assert!(old_process.interrupt_count.load(Ordering::SeqCst) >= 1);
    assert!(new_process.alive.load(Ordering::SeqCst));
    p.admit(
        "settlement-failure",
        ManagerActionV2::AssignLead {
            epic_id: p.epic,
            expected: p.fence().await,
            session_id: Some(first),
        },
    )
    .await;
    super::super::reaper::fail_runtime_orphan_reap_for_test(next);
    assert!(p.execute().await.is_err());
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(p.epic)
            .unwrap()
            .unwrap()
            .lead_session_id,
        Some(next)
    );
    super::super::launch::drop_controller_candidate_test_stream(first);
    super::super::launch::drop_controller_candidate_test_stream(next);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_cancelled_reconciler_retains_uncertain_claim_without_second_launch() {
    let p = pilot().await;
    let receipt = p
        .admit(
            "cancelled",
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Feature,
                query: "work".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    let id = receipt.target_session_id.unwrap();
    let process = super::super::launch::install_controller_candidate_test_process(id);
    let (reached, _resume) = super::super::launch::install_direct_launch_custody_test_pause(
        &format!("manager.action:{}", receipt.operation_id),
    );
    {
        let reconcile = p.manager.reconcile_manager_actions_once();
        tokio::pin!(reconcile);
        tokio::select! {
            ready=reached=>{assert_eq!(ready.unwrap(),id);}
            unexpected=&mut reconcile=>panic!("reconciliation ended before cancellation: {unexpected:?}"),
        }
    }
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Running
    );
    p.manager.reconcile_manager_actions_once().await.unwrap();
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Uncertain
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    p.manager.reconcile_manager_actions_startup().await.unwrap();
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Uncertain
    );
    super::super::launch::drop_controller_candidate_test_process(id);
}

#[tokio::test]
async fn manager_actions_operator_pause_publishes_intent_before_waiting_on_spawn_guard() {
    let p = pilot().await;
    let guard = super::super::spawn_single_flight::acquire_spawn_guard(p.lead).await;
    let pause = p.manager.interrupt_session_operator(p.lead);
    tokio::pin!(pause);
    tokio::time::timeout(std::time::Duration::from_secs(5),async {
        tokio::select! {
            result=&mut pause=>panic!("operator pause bypassed held spawn guard: {result:?}"),
            ()=async {loop {
                let marked:bool=p.manager.store.lock().await.conn.query_row("SELECT EXISTS(SELECT 1 FROM daemon_settings WHERE key=?1 AND value='true')",[format!("manager_operator_pause:{}",p.lead)],|r|r.get(0)).unwrap();
                if marked {break}tokio::task::yield_now().await;
            }}=>{},
        }
    }).await.unwrap();
    drop(guard);
    // The idle fixture has no process to interrupt; durable operator intent
    // still fences a newly queued manager continuation.
    assert!(pause.await.is_err());
    p.admit(
        "after-operator",
        ManagerActionV2::ResumeLead {
            epic_id: p.epic,
            expected: p.fence().await,
            message: "manager continuation".into(),
        },
    )
    .await;
    assert!(
        p.execute()
            .await
            .unwrap_err()
            .to_string()
            .contains("human_or_recovery_owner")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_deferred_revocation_settles_starting_candidate_without_launch() {
    let mut p = pilot().await;
    let choice = ManagerLaunchChoiceV2 {
        provider: SessionProvider::CodexAppServer,
        model: "gpt-5.4".into(),
        effort: None,
    };
    p.policy.allowed_launches.push(choice.clone());
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "deferred-grant".into(),
            policy: p.policy.clone(),
        })
        .unwrap();
    let mut request = p.request(
        "deferred-revoke",
        ManagerActionV2::CreateSession {
            parent_id: p.epic,
            kind: SessionKind::Feature,
            query: "work".into(),
            launch: choice,
        },
    );
    request.fence.policy_version = 2;
    let receipt = p
        .manager
        .agent_control()
        .agent_manager_control(p.owner, request)
        .await
        .unwrap();
    let id = receipt.target_session_id.unwrap();
    let (reached, resume) = super::super::launch::install_controller_candidate_test_pause(
        id,
        super::super::launch::ControllerCandidateTestPhase::SuccessorBeforeDeferredProviderStart,
    );
    // Register a real executable that would fail immediately if the fence let
    // thread/start run. No provider credential or network request is involved.
    super::super::launch::install_controller_candidate_test_app_server_binary(
        id,
        std::path::PathBuf::from("/bin/false"),
    );
    let revoke = async {
        reached.await.unwrap();
        p.manager
            .store
            .lock()
            .await
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: p.project,
                expected_scope_version: 1,
                expected_policy_version: 2,
                idempotency_key: "deferred-revoke-grant".into(),
                policy: ManagerPolicyV2::default(),
            })
            .unwrap();
        resume.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(p.manager.reconcile_manager_actions_once(), revoke);
    result.unwrap();
    assert!(!p.manager.active.read().await.contains_key(&id));
    let store = p.manager.store.lock().await;
    let row = store.get_session(id).unwrap().unwrap();
    assert_eq!(row.status, SessionStatus::Failed);
    assert!(row.sandbox_root.unwrap().is_dir());
    assert_eq!(
        store.get_session(p.epic).unwrap().unwrap().lead_session_id,
        Some(p.lead)
    );
    let op = store
        .manager_action_operation(receipt.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(op.receipt.state, ManagerActionStateV2::Revoked);
    assert!(!op.effect_started);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_actions_stale_candidate_cannot_assign_or_stop_operator_continuation() {
    let p = pilot().await;
    let receipt = p
        .admit(
            "incarnation",
            ManagerActionV2::ReplaceLead {
                epic_id: p.epic,
                expected: p.fence().await,
                query: "manager candidate".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    let id = receipt.target_session_id.unwrap();
    let original = super::super::launch::install_controller_candidate_test_process(id);
    let (reached, resume) = super::super::launch::install_controller_candidate_test_pause(
        id,
        super::super::launch::ControllerCandidateTestPhase::BeforeAssignment,
    );
    let operator = async {
        reached.await.unwrap();
        wait_launch_event(&p, id).await;
        let before: i64 = p
            .manager
            .store
            .lock()
            .await
            .conn
            .query_row(
                "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        super::super::launch::send_controller_candidate_test_event(id,crate::claude::StreamEvent{event_type:"system".into(),data:serde_json::json!({"subtype":"init","session_id":"operator-resume-candidate"})}).await;
        tokio::time::timeout(std::time::Duration::from_secs(5),async {loop {
            let current:i64=p.manager.store.lock().await.conn.query_row("SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",[id.to_string()],|r|r.get(0)).unwrap();if current>before {break}tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }}).await.unwrap();
        let next = super::super::launch::install_controller_candidate_test_process(id);
        p.manager
            .continue_session_operator(id, "operator continuation".into())
            .await
            .unwrap();
        resume.send(()).unwrap();
        next
    };
    let (result, next) = tokio::join!(p.manager.reconcile_manager_actions_once(), operator);
    result.unwrap();
    assert_eq!(original.productive_start_count.load(Ordering::SeqCst), 1);
    assert!(original.interrupt_count.load(Ordering::SeqCst) >= 1);
    assert_eq!(next.productive_start_count.load(Ordering::SeqCst), 1);
    assert_eq!(next.interrupt_count.load(Ordering::SeqCst), 0);
    assert!(next.alive.load(Ordering::SeqCst));
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(p.epic)
            .unwrap()
            .unwrap()
            .lead_session_id,
        Some(p.lead)
    );
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Uncertain
    );
    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, id)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(id);
}

#[path = "manager_intent.rs"]
mod intent;

#[path = "manager_seat.rs"]
mod seat;

#[path = "manager_session_actions.rs"]
mod session_actions;

#[path = "manager_operator_delegation.rs"]
mod operator_delegation;

#[tokio::test]
async fn manager_actions_group_scope_enrolls_created_epics_above_32_with_explicit_policy() {
    let p = pilot().await;
    {
        let store = p.manager.store.lock().await;
        let template = store.get_session(p.epic).unwrap().unwrap();
        for n in 0..32 {
            let mut epic = template.clone();
            epic.id = Uuid::new_v4();
            epic.title = Some(format!("Group member {n}"));
            epic.lead_session_id = None;
            store.insert_session(&epic).unwrap();
        }
        let scope = store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                project_id: p.project,
                session_id: p.owner,
                epic_ids: None,
                group_ids: vec![p.group],
                expected_row_version: 1,
            })
            .unwrap();
        assert_eq!(scope.epic_ids.len(), 33);
        let mut policy = p.policy.clone();
        policy.group_ids.clear();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: p.project,
                expected_scope_version: 2,
                expected_policy_version: 1,
                idempotency_key: "group-policy".into(),
                policy,
            })
            .unwrap();
    }
    let mut request = p.request(
        "group-create",
        ManagerActionV2::CreateContainer {
            parent_id: Some(p.group),
            kind: SessionKind::Epic,
            name: "Created under selected Group".into(),
            tags: vec!["delivery".into()],
        },
    );
    request.fence = ManagerFenceV2 {
        scope_version: 2,
        policy_version: 2,
    };
    let receipt = p
        .manager
        .agent_control()
        .agent_manager_control(p.owner, request)
        .await
        .unwrap();
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let store = p.manager.store.lock().await;
    let scope = store.get_harness_manager(p.project).unwrap().unwrap();
    assert_eq!(scope.epic_ids.len(), 34);
    assert!(scope.epic_ids.contains(&receipt.target_session_id.unwrap()));
    assert_eq!(scope.group_ids, vec![p.group]);
    assert!(scope.explicit_epic_ids().is_empty());
}

// ── Slice 2a: integrate executor end-to-end tests (D20 finding 3) ──────

use crate::store::manager_ledger::{Acceptance, WorkRecord};

/// Create a work record with acceptance for `source_commit`.
fn integrate_work_record(epic: Uuid, key: &str, source_commit: &str) -> WorkRecord {
    WorkRecord {
        key: key.into(),
        epic_id: epic,
        title: format!("Work {key}"),
        kind: ManagerWorkKindV2::Program,
        priority: 1,
        weight: 1,
        required_gates: vec![],
        spec_revision: 0,
        source_session_id: None,
        source_commit: Some(source_commit.into()),
        stages: vec![],
        acceptance: Some(Acceptance {
            source_commit: source_commit.into(),
            spec_revision: 0,
            evidence_digest: "sha256:abc".into(),
            method: "independent_review".into(),
            accepted_at: "2026-09-20T00:00:00Z".into(),
        }),
        integration: None,
        pending_acceptance: None,
    }
}

/// Create a source commit on top of `parent` in a detached worktree, returning
/// the new commit SHA. The worktree is removed afterwards.
fn commit_atop(repo: &std::path::Path, temp_root: &std::path::Path, parent: &str) -> String {
    let builder = temp_root.join("builder-source");
    let path = builder.to_str().expect("UTF-8 temp path");
    git(repo, &["worktree", "add", "-q", "--detach", path, parent]);
    std::fs::write(builder.join("feature.txt"), "feature\n").unwrap();
    git(&builder, &["add", "feature.txt"]);
    git(&builder, &["commit", "-q", "-m", "feature"]);
    let oid = git(&builder, &["rev-parse", "HEAD"]);
    git(repo, &["worktree", "remove", "--force", path]);
    oid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integrate_executor_advances_target_ref_and_succeeds() {
    let mut p = pilot().await;
    // Add GitEffect to the policy and reconfigure.
    p.policy.capabilities.push(ManagerCapabilityV2::GitEffect);
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "add-giteffect".into(),
            policy: p.policy.clone(),
        })
        .unwrap();
    // Create a rolling branch pointing to the initial commit.
    git(&p.repo, &["branch", "rolling"]);
    let rolling_tip = git(&p.repo, &["rev-parse", "refs/heads/rolling"]);
    // Create a source commit (fast-forward: descendant of rolling tip).
    let source = commit_atop(&p.repo, p._dir.path(), &rolling_tip);
    // Put a work record with acceptance for the source commit.
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        store
            .manager_v2_put_record(
                &config,
                "work",
                "w1",
                Some(p.epic),
                0,
                &serde_json::to_value(&integrate_work_record(p.epic, "w1", &source)).unwrap(),
            )
            .unwrap();
    }
    // Queue the integrate action via AgentManagerControl.
    let mut request = p.request(
        "integrate-ff",
        ManagerActionV2::Integrate {
            work_key: "w1".into(),
            target_ref: "refs/heads/rolling".into(),
            expected_tip: rolling_tip.clone(),
            source_commit: source.clone(),
        },
    );
    request.fence.policy_version = 2;
    let receipt = p
        .manager
        .agent_control()
        .agent_manager_control(p.owner, request)
        .await
        .unwrap();
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    // Run reconciliation until terminal.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            p.manager.reconcile_manager_actions_once().await.unwrap();
            if p.receipt(receipt.operation_id).await.state != ManagerActionStateV2::Queued {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Verify the receipt is Succeeded.
    let final_receipt = p.receipt(receipt.operation_id).await;
    assert_eq!(final_receipt.state, ManagerActionStateV2::Succeeded);
    assert_eq!(final_receipt.action_kind, ManagerActionKindV2::Integrate);
    // Verify the target ref was advanced to the source commit.
    assert_eq!(git(&p.repo, &["rev-parse", "refs/heads/rolling"]), source);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integrate_executor_blocks_when_acceptance_revoked_before_effect() {
    let mut p = pilot().await;
    // Add GitEffect to the policy and reconfigure.
    p.policy.capabilities.push(ManagerCapabilityV2::GitEffect);
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "add-giteffect".into(),
            policy: p.policy.clone(),
        })
        .unwrap();
    // Create a rolling branch pointing to the initial commit.
    git(&p.repo, &["branch", "rolling"]);
    let rolling_tip = git(&p.repo, &["rev-parse", "refs/heads/rolling"]);
    // Create a source commit (fast-forward: descendant of rolling tip).
    let source = commit_atop(&p.repo, p._dir.path(), &rolling_tip);
    // Put a work record with acceptance for the source commit.
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        store
            .manager_v2_put_record(
                &config,
                "work",
                "w1",
                Some(p.epic),
                0,
                &serde_json::to_value(&integrate_work_record(p.epic, "w1", &source)).unwrap(),
            )
            .unwrap();
    }
    // Queue the integrate action via AgentManagerControl.
    let mut request = p.request(
        "integrate-revoked",
        ManagerActionV2::Integrate {
            work_key: "w1".into(),
            target_ref: "refs/heads/rolling".into(),
            expected_tip: rolling_tip.clone(),
            source_commit: source.clone(),
        },
    );
    request.fence.policy_version = 2;
    let receipt = p
        .manager
        .agent_control()
        .agent_manager_control(p.owner, request)
        .await
        .unwrap();
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    // Remove acceptance before execution: replace the work record with one
    // that has no acceptance.
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        let prior = store
            .manager_v2_record(&config, "work", "w1")
            .unwrap()
            .unwrap();
        let mut work = integrate_work_record(p.epic, "w1", &source);
        work.acceptance = None;
        store
            .manager_v2_put_record(
                &config,
                "work",
                "w1",
                Some(p.epic),
                prior.row_version,
                &serde_json::to_value(&work).unwrap(),
            )
            .unwrap();
    }
    // Run reconciliation until terminal.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            p.manager.reconcile_manager_actions_once().await.unwrap();
            if p.receipt(receipt.operation_id).await.state != ManagerActionStateV2::Queued {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Verify the receipt is Blocked with the acceptance-required outcome.
    let final_receipt = p.receipt(receipt.operation_id).await;
    assert_eq!(
        final_receipt.state,
        ManagerActionStateV2::Blocked,
        "revoked acceptance must block, not uncertain or succeed: {final_receipt:?}"
    );
    assert_eq!(
        final_receipt.outcome.as_deref(),
        Some("manager_v2_source_acceptance_required"),
        "outcome must name the acceptance requirement: {final_receipt:?}"
    );
    // Verify the target ref was NOT changed (no git effect occurred).
    assert_eq!(
        git(&p.repo, &["rev-parse", "refs/heads/rolling"]),
        rolling_tip,
        "target ref must be unchanged when acceptance was revoked"
    );
}

// ── RME-S2A-003: executor error-path and boot-recovery E2E tests ─────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integrate_executor_error_path_never_leaves_running() {
    let mut p = pilot().await;
    p.policy.capabilities.push(ManagerCapabilityV2::GitEffect);
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "add-giteffect-err".into(),
            policy: p.policy.clone(),
        })
        .unwrap();
    git(&p.repo, &["branch", "rolling"]);
    let rolling_tip = git(&p.repo, &["rev-parse", "refs/heads/rolling"]);
    let source = commit_atop(&p.repo, p._dir.path(), &rolling_tip);
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        store
            .manager_v2_put_record(
                &config,
                "work",
                "w1",
                Some(p.epic),
                0,
                &serde_json::to_value(&integrate_work_record(p.epic, "w1", &source)).unwrap(),
            )
            .unwrap();
    }
    // Install a test hook that makes publish fail after effect_started.
    // We do this by using a stale expected_tip so the engine refuses.
    let mut request = p.request(
        "integrate-err",
        ManagerActionV2::Integrate {
            work_key: "w1".into(),
            target_ref: "refs/heads/rolling".into(),
            expected_tip: rolling_tip.clone(),
            source_commit: source.clone(),
        },
    );
    request.fence.policy_version = 2;
    let receipt = p
        .manager
        .agent_control()
        .agent_manager_control(p.owner, request)
        .await
        .unwrap();
    // Move the ref after queue but before execution so publish sees a stale tip.
    git(&p.repo, &["update-ref", "refs/heads/rolling", &source]);
    // Run reconciliation — the executor will fail at publish (stale target).
    let mut settled = false;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if let Err(_) = p.manager.reconcile_manager_actions_once().await {
                // Error handler should settle the claim.
            }
            let r = p.receipt(receipt.operation_id).await;
            if r.state != ManagerActionStateV2::Queued && r.state != ManagerActionStateV2::Running {
                settled = true;
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(settled, "integrate action never settled");
    let final_receipt = p.receipt(receipt.operation_id).await;
    // The action must NOT be Running — the error handler must have finished it.
    assert_ne!(
        final_receipt.state,
        ManagerActionStateV2::Running,
        "effect_started integrate must not remain Running after error: {final_receipt:?}"
    );
    // The ref was moved by our update-ref, so the engine's CAS should have
    // refused publication (StaleTarget). The error handler should finish
    // the action as Blocked or Uncertain.
    assert!(
        matches!(
            final_receipt.state,
            ManagerActionStateV2::Blocked | ManagerActionStateV2::Uncertain
        ),
        "expected Blocked or Uncertain, got {:?}",
        final_receipt
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integrate_boot_recovery_settles_uncertain_and_releases_singleton() {
    let mut p = pilot().await;
    p.policy.capabilities.push(ManagerCapabilityV2::GitEffect);
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "add-giteffect-boot".into(),
            policy: p.policy.clone(),
        })
        .unwrap();
    git(&p.repo, &["branch", "rolling"]);
    let rolling_tip = git(&p.repo, &["rev-parse", "refs/heads/rolling"]);
    let source = commit_atop(&p.repo, p._dir.path(), &rolling_tip);
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        store
            .manager_v2_put_record(
                &config,
                "work",
                "w1",
                Some(p.epic),
                0,
                &serde_json::to_value(&integrate_work_record(p.epic, "w1", &source)).unwrap(),
            )
            .unwrap();
    }
    let mut request = p.request(
        "integrate-boot",
        ManagerActionV2::Integrate {
            work_key: "w1".into(),
            target_ref: "refs/heads/rolling".into(),
            expected_tip: rolling_tip.clone(),
            source_commit: source.clone(),
        },
    );
    request.fence.policy_version = 2;
    let receipt = p
        .manager
        .agent_control()
        .agent_manager_control(p.owner, request)
        .await
        .unwrap();
    // Claim and mark effect_started + persist candidate OID,
    // then simulate a crash by finishing as Uncertain directly.
    {
        let store = p.manager.store.lock().await;
        let boot_id = p.manager.program_run_boot_id;
        let claim = store.claim_manager_action(boot_id).unwrap().unwrap();
        store
            .manager_v2_integrate_runtime_gate(&claim, true)
            .unwrap();
        store.persist_integrate_candidate(&claim, &source).unwrap();
        store
            .finish_manager_action(&claim, ManagerActionStateV2::Uncertain, "crash")
            .unwrap();
    }
    // Verify it is Uncertain and the singleton is blocked.
    {
        let store = p.manager.store.lock().await;
        let op = store
            .manager_action_operation(receipt.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(op.receipt.state, ManagerActionStateV2::Uncertain);
    }
    // Simulate that the publish succeeded before the crash: move the
    // ref to the candidate OID (source).
    git(&p.repo, &["update-ref", "refs/heads/rolling", &source]);
    // Run reconcile_uncertain_integrate_claims via a full reconcile pass.
    // The ref now matches the candidate OID (source), so it should
    // settle to Succeeded and release the singleton.
    p.manager.reconcile_manager_actions_once().await.unwrap();
    let final_receipt = p.receipt(receipt.operation_id).await;
    assert_eq!(
        final_receipt.state,
        ManagerActionStateV2::Succeeded,
        "uncertain integrate should settle to Succeeded when ref matches candidate: {final_receipt:?}"
    );
    assert_eq!(
        final_receipt.outcome.as_deref(),
        Some("integrated_crash_reconciled")
    );
    // Verify the singleton is released: a new integrate for the same target can enqueue.
    let mut request2 = p.request(
        "integrate-after-boot",
        ManagerActionV2::Integrate {
            work_key: "w1".into(),
            target_ref: "refs/heads/rolling".into(),
            expected_tip: source.clone(),
            source_commit: source.clone(),
        },
    );
    request2.fence.policy_version = 2;
    // This should succeed because the singleton was released.
    let result = p
        .manager
        .agent_control()
        .agent_manager_control(p.owner, request2)
        .await;
    // It may fail with source_acceptance_required (acceptance was consumed),
    // but must NOT fail with integrate_in_progress (singleton still held).
    if let Err(e) = &result {
        assert!(
            !e.to_string().contains("integrate_in_progress"),
            "singleton not released after boot recovery: {e}"
        );
    }
}

/// #674: regrant the pilot with an exact lifetime session quota; returns the
/// new policy version the request fence must carry.
async fn pilot_session_quota(p: &mut Pilot, cap: u16) -> i64 {
    p.policy.max_created_sessions = cap;
    p.manager
        .store
        .lock()
        .await
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: p.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: format!("session-quota-{cap}"),
            policy: p.policy.clone(),
        })
        .unwrap()
        .row_version
}

fn pilot_worker(p: &Pilot, query: &str) -> ManagerActionV2 {
    ManagerActionV2::CreateSession {
        parent_id: p.epic,
        kind: SessionKind::Task,
        query: query.into(),
        launch: p.policy.allowed_launches[0].clone(),
    }
}

async fn pilot_control(
    p: &Pilot,
    policy_version: i64,
    key: &str,
    operation: ManagerActionV2,
) -> crate::error::Result<ManagerActionReceiptV2> {
    let mut request = p.request(key, operation);
    request.fence.policy_version = policy_version;
    p.manager
        .agent_control()
        .agent_manager_control(p.owner, request)
        .await
}

async fn pilot_seed(
    p: &Pilot,
    operation: ManagerActionV2,
    state: ManagerActionStateV2,
    target: Option<Uuid>,
) -> Uuid {
    let store = p.manager.store.lock().await;
    let config = store.get_harness_manager(p.project).unwrap().unwrap();
    store
        .seed_manager_action_for_test(&config, operation, state, target)
        .unwrap()
}

async fn pilot_created_usage(p: &Pilot) -> i64 {
    let store = p.manager.store.lock().await;
    let config = store.get_harness_manager(p.project).unwrap().unwrap();
    store.manager_v2_created_usage(&config, false).unwrap()
}

#[tokio::test]
async fn creation_budget_charges_queued_and_uncertain_but_not_uncreated_blocked_or_revoked() {
    let mut p = pilot().await;
    let version = pilot_session_quota(&mut p, 4).await;
    let expected = p.fence().await;
    // Blocked/revoked before anything was created: free.
    pilot_seed(
        &p,
        pilot_worker(&p, "blocked before launch"),
        ManagerActionStateV2::Blocked,
        Some(Uuid::new_v4()),
    )
    .await;
    pilot_seed(
        &p,
        pilot_worker(&p, "revoked before launch"),
        ManagerActionStateV2::Revoked,
        Some(Uuid::new_v4()),
    )
    .await;
    pilot_seed(
        &p,
        ManagerActionV2::RetryLead {
            epic_id: p.epic,
            expected: expected.clone(),
            message: "blocked retry without a target".into(),
            launch: None,
        },
        ManagerActionStateV2::Blocked,
        None,
    )
    .await;
    assert_eq!(pilot_created_usage(&p).await, 0);
    // A blocked operation whose target session materialized stays charged.
    pilot_seed(
        &p,
        pilot_worker(&p, "blocked after its session row existed"),
        ManagerActionStateV2::Blocked,
        Some(p.lead),
    )
    .await;
    // An uncertain operation may have created its target: charged.
    pilot_seed(
        &p,
        pilot_worker(&p, "uncertain launch"),
        ManagerActionStateV2::Uncertain,
        Some(Uuid::new_v4()),
    )
    .await;
    assert_eq!(pilot_created_usage(&p).await, 2);
    // A root succession keeps its charge even when blocked before its
    // candidate existed: its root occurrence is counted exactly once.
    pilot_seed(
        &p,
        ManagerActionV2::SucceedManager {
            expected: ManagerSuccessionFenceV2 {
                authority_epoch: 1,
                custody_generation: None,
            },
            launch: p.policy.allowed_launches[0].clone(),
            handoff: ManagerCommittedHandoffV2 {
                source_commit: "a".repeat(40),
                relative_path: "thoughts/handoff.md".into(),
                blob_oid: "b".repeat(40),
            },
        },
        ManagerActionStateV2::Blocked,
        Some(Uuid::new_v4()),
    )
    .await;
    assert_eq!(pilot_created_usage(&p).await, 3);
    let queued = pilot_control(&p, version, "last-free-slot", pilot_worker(&p, "admitted"))
        .await
        .unwrap();
    assert_eq!(queued.state, ManagerActionStateV2::Queued);
    assert_eq!(pilot_created_usage(&p).await, 4);
    let error = pilot_control(&p, version, "over-quota", pilot_worker(&p, "refused"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("manager_v2_creation_limit"));
    // The refusal journaled nothing, so it charged nothing.
    assert_eq!(pilot_created_usage(&p).await, 4);
}

#[tokio::test]
async fn creation_limit_still_bounds_workers_retry_and_replace_while_review_launches_are_excluded()
{
    let mut p = pilot().await;
    let version = pilot_session_quota(&mut p, 2).await;
    let expected = p.fence().await;
    pilot_seed(
        &p,
        pilot_worker(&p, "earlier worker"),
        ManagerActionStateV2::Succeeded,
        Some(p.lead),
    )
    .await;
    // Three DB-native review launches: linked to review assignments, so the
    // lifetime quota does not see them.
    {
        let store = p.manager.store.lock().await;
        let config = store.get_harness_manager(p.project).unwrap().unwrap();
        let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        for ordinal in 0..3 {
            let op = store
                .seed_manager_action_for_test(
                    &config,
                    pilot_worker(&p, "DB-native review"),
                    ManagerActionStateV2::Succeeded,
                    Some(p.lead),
                )
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO manager_review_assignments(
                        assignment_id,project_id,epic_id,manager_session_id,scope_version,
                        work_key,spec_revision,author_session_id,source_sha,state,row_version,
                        request_json,request_fingerprint,action_operation_id,failure_code,
                        created_at,updated_at,terminal_at)
                     VALUES(?1,?2,?3,?4,1,'product',1,?5,?6,'failed',1,'{}',?7,?8,
                            'manager_review_test_history',?9,?9,?9)",
                    rusqlite::params![
                        Uuid::new_v4().to_string(),
                        p.project.to_string(),
                        p.epic.to_string(),
                        p.owner.to_string(),
                        p.lead.to_string(),
                        format!("{ordinal:040x}"),
                        format!("sha256:{}", "b".repeat(64)),
                        op.to_string(),
                        stamp
                    ],
                )
                .unwrap();
        }
    }
    assert_eq!(pilot_created_usage(&p).await, 1);
    let admitted = pilot_control(&p, version, "second-worker", pilot_worker(&p, "admitted"))
        .await
        .unwrap();
    assert_eq!(admitted.state, ManagerActionStateV2::Queued);
    assert_eq!(pilot_created_usage(&p).await, 2);

    let worker = pilot_control(&p, version, "third-worker", pilot_worker(&p, "at cap"))
        .await
        .unwrap_err();
    assert!(worker.to_string().contains("manager_v2_creation_limit"));
    let replace = pilot_control(
        &p,
        version,
        "replace-at-cap",
        ManagerActionV2::ReplaceLead {
            epic_id: p.epic,
            expected: expected.clone(),
            query: "replacement at cap".into(),
            launch: p.policy.allowed_launches[0].clone(),
        },
    )
    .await
    .unwrap_err();
    assert!(replace.to_string().contains("manager_v2_creation_limit"));
    p.manager
        .store
        .lock()
        .await
        .update_session_status(p.lead, SessionStatus::Failed)
        .unwrap();
    let retry = pilot_control(
        &p,
        version,
        "retry-at-cap",
        ManagerActionV2::RetryLead {
            epic_id: p.epic,
            expected: p.fence().await,
            message: "retry at cap".into(),
            launch: None,
        },
    )
    .await
    .unwrap_err();
    assert!(retry.to_string().contains("manager_v2_creation_limit"));
}

/// Issue #548: a session the manager created through V2 `create_session`
/// reads its Epic's live work and granted file ownership without a relay turn;
/// a lead-owned session does not.
#[tokio::test]
#[allow(clippy::significant_drop_tightening, clippy::large_futures)]
async fn manager_created_worker_reads_its_work_and_ownership_view() {
    let p = pilot().await;
    let receipt = p
        .admit(
            "work-view-spawn",
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Task,
                query: "implement the granted slice".into(),
                launch: p.policy.allowed_launches[0].clone(),
            },
        )
        .await;
    let worker = receipt.target_session_id.unwrap();
    let _process = super::super::launch::install_controller_candidate_test_process(worker);
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    let control = p.manager.agent_control();
    for (key, change) in [
        (
            "work-view-work",
            ManagerUpdateV2::Work {
                key: "slice".into(),
                expected_row_version: 0,
                epic_id: p.epic,
                title: "Granted slice".into(),
                kind: ManagerWorkKindV2::Product,
                priority: 1,
                weight: 1,
                required_gates: vec![
                    ManagerWorkStageV2::Implementation,
                    ManagerWorkStageV2::Review,
                    ManagerWorkStageV2::Verification,
                ],
            },
        ),
        (
            "work-view-claim",
            ManagerUpdateV2::Ownership {
                key: "slice".into(),
                expected_row_version: 0,
                domain: "crates/rsid/src/rpc.rs".into(),
                mode: ManagerOwnershipModeV2::Exclusive,
                files: vec!["crates/rsid/src/rpc.rs".into()],
                active: true,
            },
        ),
    ] {
        control
            .agent_manager_update(
                p.owner,
                AgentManagerUpdateRequestV2 {
                    fence: ManagerFenceV2 {
                        scope_version: 1,
                        policy_version: 1,
                    },
                    idempotency_key: key.into(),
                    change,
                },
            )
            .await
            .unwrap();
    }
    let page = control
        .agent_manager_work_view(worker, AgentManagerWorkViewRequestV1::default())
        .await
        .unwrap();
    assert_eq!(page.epic_id, p.epic);
    assert_eq!(page.manager_session_id, p.owner);
    assert_eq!(page.works.len(), 1);
    assert_eq!(page.works[0].work_key, "slice");
    assert_eq!(page.works[0].title, "Granted slice");
    assert_eq!(page.ownership.len(), 1);
    assert_eq!(page.ownership[0].work_key, "slice");
    assert_eq!(
        page.ownership[0].files,
        vec!["crates/rsid/src/rpc.rs".to_owned()]
    );
    assert_eq!(page.ownership[0].mode, ManagerOwnershipModeV2::Exclusive);
    let lead = control
        .agent_manager_work_view(p.lead, AgentManagerWorkViewRequestV1::default())
        .await
        .unwrap_err();
    assert!(lead.to_string().contains("manager_work_view_not_managed"));
}

// ---- Issue #670 R1: retry_lead for Interrupted / unresumable leads ----

/// Settle the pilot lead into `status` with or without a captured provider
/// session id, mirroring the row in the runtime completed map.
async fn settle_pilot_lead(p: &Pilot, status: SessionStatus, provider_session: bool) {
    let store = p.manager.store.lock().await;
    store.update_session_status(p.lead, status).unwrap();
    let id = provider_session.then(|| format!("provider-{}", p.lead));
    store
        .conn
        .execute(
            "UPDATE sessions SET claude_session_id=?2 WHERE id=?1",
            rusqlite::params![p.lead.to_string(), id],
        )
        .unwrap();
    let row = store.get_session(p.lead).unwrap().unwrap();
    drop(store);
    if let Some(entry) = p.manager.completed.write().await.get_mut(&p.lead) {
        entry.session = row;
    }
}

fn retry(p: &Pilot, expected: ManagerLeadFenceV2) -> ManagerActionV2 {
    ManagerActionV2::RetryLead {
        epic_id: p.epic,
        expected,
        message: "recover the stranded lead".into(),
        launch: None,
    }
}

async fn admit_err(p: &Pilot, key: &str, operation: ManagerActionV2) -> DaemonError {
    p.manager
        .agent_control()
        .agent_manager_control(p.owner, p.request(key, operation))
        .await
        .unwrap_err()
}

fn typed_next_action(error: &DaemonError) -> (String, String) {
    let DaemonError::StructuredRpc { message, data, .. } = error else {
        panic!("expected a typed manager refusal, got {error}");
    };
    assert_eq!(data["code"].as_str(), Some(message.as_str()));
    (
        message.clone(),
        data["next_action"].as_str().unwrap_or_default().to_owned(),
    )
}

async fn retry_operation_count(p: &Pilot) -> i64 {
    p.manager.store.lock().await.conn.query_row(
        "SELECT count(*) FROM harness_manager_v2_operations WHERE json_extract(payload_json,'$.request.operation.action')='retry_lead' AND json_extract(payload_json,'$.request.operation.epic_id')=?1",
        [p.epic.to_string()],
        |r| r.get(0),
    ).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_retry_lead_recovers_interrupted_lead_without_provider_session() {
    let p = pilot().await;
    settle_pilot_lead(&p, SessionStatus::Interrupted, false).await;
    p.manager
        .runtime_config
        .retry_enabled
        .store(true, Ordering::Relaxed);
    let source_commit = git(&p.repo, &["rev-parse", "HEAD"]);
    {
        let store = p.manager.store.lock().await;
        assert!(store.load_events(p.lead).unwrap().is_empty());
        assert_eq!(
            store
                .get_session(p.lead)
                .unwrap()
                .unwrap()
                .claude_session_id,
            None
        );
    }
    let receipt = p
        .admit("recover-interrupted", retry(&p, p.fence().await))
        .await;
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    assert_eq!(receipt.action_kind, ManagerActionKindV2::RetryLead);
    let id = receipt.target_session_id.unwrap();
    let process = super::super::launch::install_controller_candidate_test_process(id);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    p.execute().await.unwrap();
    let settled = p.receipt(receipt.operation_id).await;
    assert_eq!(settled.state, ManagerActionStateV2::Succeeded);
    assert_eq!(
        settled.result,
        Some(ManagerActionResultV2::ProviderEstablished {
            lead_state: Some(ManagerLeadAssignmentStateV2::Assigned),
        })
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    {
        let store = p.manager.store.lock().await;
        let child = store.get_session(id).unwrap().unwrap();
        assert_eq!(child.continued_from, Some(p.lead));
        assert_eq!(child.parent_id, Some(p.epic));
        assert_eq!(
            store.get_session(p.epic).unwrap().unwrap().lead_session_id,
            Some(id)
        );
        // Fresh successor custody forked from the predecessor's frozen source.
        let root = child.sandbox_root.unwrap();
        assert!(root.is_dir());
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), source_commit);
        assert_eq!(
            store.live_custody_for_session(id).unwrap().owner_session_id,
            id
        );
    }
    // One operation row, counted against max_recovery_attempts.
    assert_eq!(retry_operation_count(&p).await, 1);
    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, id)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_retry_lead_admits_completed_lead_only_without_provider_session() {
    let p = pilot().await;
    settle_pilot_lead(&p, SessionStatus::Completed, true).await;
    let error = admit_err(&p, "resumable", retry(&p, p.fence().await)).await;
    assert_eq!(
        typed_next_action(&error),
        (
            "manager_v2_retry_lead_resumable".to_owned(),
            "resume_lead".to_owned()
        )
    );
    settle_pilot_lead(&p, SessionStatus::Completed, false).await;
    let receipt = p.admit("unresumable", retry(&p, p.fence().await)).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    assert!(receipt.target_session_id.is_some_and(|id| id != p.lead));
    assert_eq!(retry_operation_count(&p).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_retry_lead_refuses_running_and_starting_leads() {
    let p = pilot().await;
    for (key, status) in [
        ("running", SessionStatus::Running),
        ("starting", SessionStatus::Starting),
    ] {
        settle_pilot_lead(&p, status, false).await;
        let error = admit_err(&p, key, retry(&p, p.fence().await)).await;
        assert_eq!(
            error.to_string(),
            "Invalid parameter: manager_v2_retry_requires_terminal",
            "{key}"
        );
    }
    // Execution re-proves admission: a lead restarted after queueing is refused.
    settle_pilot_lead(&p, SessionStatus::Interrupted, false).await;
    p.manager
        .runtime_config
        .retry_enabled
        .store(true, Ordering::Relaxed);
    let receipt = p.admit("queued", retry(&p, p.fence().await)).await;
    settle_pilot_lead(&p, SessionStatus::Running, false).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    p.manager.reconcile_manager_actions_once().await.unwrap();
    let blocked = p.receipt(receipt.operation_id).await;
    assert_eq!(blocked.state, ManagerActionStateV2::Blocked);
    assert_eq!(
        blocked.outcome.as_deref(),
        Some("manager_v2_retry_requires_terminal")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_resume_lead_without_provider_session_names_retry_lead() {
    let p = pilot().await;
    settle_pilot_lead(&p, SessionStatus::Interrupted, false).await;
    let resume = |expected| ManagerActionV2::ResumeLead {
        epic_id: p.epic,
        expected,
        message: "resume".into(),
    };
    let error = admit_err(&p, "admission", resume(p.fence().await)).await;
    assert_eq!(
        typed_next_action(&error),
        (
            "manager_v2_resume_unavailable".to_owned(),
            "retry_lead".to_owned()
        )
    );
    // Execution: the provider session id was lost after admission.
    settle_pilot_lead(&p, SessionStatus::Interrupted, true).await;
    let receipt = p.admit("execution", resume(p.fence().await)).await;
    settle_pilot_lead(&p, SessionStatus::Interrupted, false).await;
    p.manager.reconcile_manager_actions_once().await.unwrap();
    let blocked = p.receipt(receipt.operation_id).await;
    assert_eq!(blocked.state, ManagerActionStateV2::Blocked);
    assert_eq!(
        blocked.outcome.as_deref(),
        Some("manager_v2_resume_unavailable")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_retry_lead_budget_and_delay_apply_to_interrupted_leads() {
    let p = pilot().await;
    settle_pilot_lead(&p, SessionStatus::Interrupted, false).await;
    p.manager
        .runtime_config
        .retry_enabled
        .store(true, Ordering::Relaxed);
    for attempt in 0..p.policy.max_recovery_attempts {
        let receipt = p
            .admit(&format!("attempt-{attempt}"), retry(&p, p.fence().await))
            .await;
        assert_eq!(receipt.state, ManagerActionStateV2::Queued);
        // retry_delay_seconds holds the claim.
        assert!(
            p.manager
                .store
                .lock()
                .await
                .claim_manager_action(p.manager.program_run_boot_id)
                .unwrap()
                .is_none()
        );
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let claim = p.claim().await;
        assert_eq!(claim.id(), receipt.operation_id);
        p.manager
            .store
            .lock()
            .await
            .finish_manager_action(&claim, ManagerActionStateV2::Blocked, "test_settled")
            .unwrap();
    }
    let error = admit_err(&p, "over-budget", retry(&p, p.fence().await)).await;
    assert_eq!(
        error.to_string(),
        "Invalid parameter: manager_v2_retry_budget_exhausted"
    );
    assert_eq!(
        retry_operation_count(&p).await,
        i64::from(p.policy.max_recovery_attempts)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_appserver_lead_with_provider_session_is_retried_not_resumed() {
    let p = pilot().await;
    settle_pilot_lead(&p, SessionStatus::Completed, true).await;
    {
        let store = p.manager.store.lock().await;
        store
            .conn
            .execute(
                "UPDATE sessions SET provider='CodexAppServer' WHERE id=?1",
                [p.lead.to_string()],
            )
            .unwrap();
        let row = store.get_session(p.lead).unwrap().unwrap();
        assert_eq!(row.provider, SessionProvider::CodexAppServer);
        assert_eq!(
            row.claude_session_id.as_deref(),
            Some(format!("provider-{}", p.lead).as_str())
        );
    }
    let error = admit_err(
        &p,
        "appserver-resume",
        ManagerActionV2::ResumeLead {
            epic_id: p.epic,
            expected: p.fence().await,
            message: "resume".into(),
        },
    )
    .await;
    assert_eq!(
        typed_next_action(&error),
        (
            "manager_v2_resume_unavailable".to_owned(),
            "retry_lead".to_owned()
        )
    );
    let receipt = p
        .admit(
            "appserver-retry",
            ManagerActionV2::RetryLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "recover the AppServer lead".into(),
                launch: Some(p.policy.allowed_launches[0].clone()),
            },
        )
        .await;
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
    assert_eq!(receipt.action_kind, ManagerActionKindV2::RetryLead);
    assert!(receipt.target_session_id.is_some_and(|id| id != p.lead));
    assert_eq!(retry_operation_count(&p).await, 1);
}

/// `resume_lead` admission, `retry_lead` admission and the manager continuation
/// gate agree for every provider, settled/unsettled status and id presence.
#[test]
fn manager_resume_and_retry_predicates_agree_with_continuation_gate() {
    use crate::session::lifecycle::check_manager_resume_target as gate;
    use crate::store::manager_actions::{manager_resume_available, manager_retry_admissible};
    let code = |result: &Result<()>| match result {
        Ok(()) => "ok".to_owned(),
        Err(DaemonError::StructuredRpc { message, data, .. }) => {
            format!("{message}->{}", data["next_action"].as_str().unwrap_or(""))
        }
        Err(error) => error.to_string(),
    };
    let providers = [
        SessionProvider::Claude,
        SessionProvider::Codex,
        SessionProvider::Pioneer,
        SessionProvider::OpenRouter,
        SessionProvider::Local,
        SessionProvider::Antigravity,
        SessionProvider::CodexAppServer,
        SessionProvider::Harness,
    ];
    let mut rows = 0;
    for provider in providers {
        for status in [
            SessionStatus::Completed,
            SessionStatus::Interrupted,
            SessionStatus::Failed,
            SessionStatus::Starting,
            SessionStatus::Running,
            SessionStatus::WaitingApproval,
        ] {
            for captured in [true, false] {
                let mut lead = bare_session(Uuid::new_v4());
                lead.session_kind = SessionKind::Feature;
                lead.provider = provider;
                lead.status = status;
                lead.claude_session_id = captured.then(|| "provider-session".to_owned());
                let case = format!("{provider:?}/{status:?}/captured={captured}");
                let (gate, resume, retry) = (
                    gate(&lead),
                    manager_resume_available(&lead),
                    manager_retry_admissible(&lead),
                );
                let settled = matches!(
                    status,
                    SessionStatus::Completed | SessionStatus::Interrupted | SessionStatus::Failed
                );
                if settled {
                    // Admission and the gate return the same verdict.
                    assert_eq!(code(&resume), code(&gate), "{case}");
                    if gate.is_ok() {
                        assert!(
                            provider != SessionProvider::CodexAppServer
                                && (captured || provider == SessionProvider::Local),
                            "{case}"
                        );
                        let expected_retry = if status == SessionStatus::Completed {
                            "manager_v2_retry_lead_resumable->resume_lead"
                        } else {
                            "ok"
                        };
                        assert_eq!(code(&retry), expected_retry, "{case}");
                    } else {
                        // Every settled lead the gate cannot resume is retryable.
                        assert_eq!(
                            code(&gate),
                            "manager_v2_resume_unavailable->retry_lead",
                            "{case}"
                        );
                        assert_eq!(code(&retry), "ok", "{case}");
                    }
                } else {
                    // An unsettled lead's status can still change before the
                    // claim runs, so admission defers to the execution gate.
                    assert_eq!(code(&resume), "ok", "{case}");
                    assert_eq!(
                        code(&gate),
                        "Invalid parameter: manager_v2_lead_not_resumable",
                        "{case}"
                    );
                    assert_eq!(
                        code(&retry),
                        "Invalid parameter: manager_v2_retry_requires_terminal",
                        "{case}"
                    );
                }
                rows += 1;
            }
        }
    }
    assert_eq!(rows, 8 * 6 * 2);
}
