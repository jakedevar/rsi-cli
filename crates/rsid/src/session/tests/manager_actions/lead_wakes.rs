//! #953: a manager lead handover is not blocked by the outgoing lead's own
//! ordinary resume wakes, and it retires the superseded lineage's resume
//! wakes (disabled, never deleted) atomically with the lead CAS.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening,
    clippy::large_futures
)]

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use super::*;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use crate::session::harness::tools::schedule_wake::{
    ScheduleWakeRequest, build_agent_scheduled_job,
};

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn wake(p: &Pilot, owner: Uuid, mode: &str, every_seconds: Option<i64>) -> Uuid {
    let job = build_agent_scheduled_job(ScheduleWakeRequest {
        message: "Continue the Epic".into(),
        in_seconds: Some(600),
        at: None,
        name: None,
        every_seconds,
        mode: Some(mode.into()),
        working_dir: p.repo.clone(),
        provider: Some(SessionProvider::Claude),
        model: Some("manager-scripted-provider".into()),
        project_id: Some(p.project),
        origin_session_id: Some(owner),
        watch_session_id: (mode == "on_terminal").then(Uuid::new_v4),
    })
    .unwrap();
    let id = job.id;
    let store = p.manager.store.try_lock().unwrap();
    store.insert_scheduled_job(&job).unwrap();
    id
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn enabled(p: &Pilot, job: Uuid) -> bool {
    p.manager
        .store
        .lock()
        .await
        .get_scheduled_job(&job)
        .unwrap()
        .expect("retired wakes are disabled, never deleted")
        .enabled
}

/// The lead's rotation predecessor: an earlier hop of the same lineage.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn earlier_hop(p: &Pilot) -> Uuid {
    let older = Uuid::new_v4();
    let store = p.manager.store.lock().await;
    let mut row = bare_session(older);
    row.project_id = Some(p.project);
    row.working_dir = p.repo.clone();
    row.session_kind = SessionKind::Feature;
    row.parent_id = Some(p.epic);
    row.provider = SessionProvider::Codex;
    row.model = Some("gpt-6-sol".into());
    store.insert_session(&row).unwrap();
    store
        .conn
        .execute(
            "UPDATE sessions SET continued_from=?2 WHERE id=?1",
            [p.lead.to_string(), older.to_string()],
        )
        .unwrap();
    assert_eq!(store.published_lineage_tip(older).unwrap(), Some(p.lead));
    older
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn retired_wakes(p: &Pilot, operation: Uuid) -> Vec<Uuid> {
    let payload: String = p
        .manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT payload_json FROM harness_manager_v2_events WHERE kind=?1 AND record_key=?2",
            ["lead_wakes_retired".to_string(), operation.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_value(
        serde_json::from_str::<serde_json::Value>(&payload).unwrap()["retired_wake_job_ids"]
            .clone(),
    )
    .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn replace(p: &Pilot, expected: ManagerLeadFenceV2) -> ManagerActionV2 {
    ManagerActionV2::ReplaceLead {
        epic_id: p.epic,
        expected,
        query: "take the Epic on the new provider".into(),
        launch: p.policy.allowed_launches[0].clone(),
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn prepared_replace(p: &Pilot) -> AgentManagerPrepareControlRequestV2 {
    AgentManagerPrepareControlRequestV2 {
        project_id: None,
        operation: PreparedManagerActionV2::ReplaceLead {
            epic_id: p.epic,
            query: "take the Epic on the new provider".into(),
            launch: p.policy.allowed_launches[0].clone(),
        },
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_replace_of_idle_lead_with_resume_wake_retires_lineage_wakes_atomically() {
    let p = pilot().await;
    let older = earlier_hop(&p).await;
    let own = wake(&p, p.lead, "resume", None);
    let recurring = wake(&p, older, "resume", Some(600));
    let child_watch = wake(&p, p.lead, "on_terminal", None);
    let unrelated = wake(&p, p.owner, "resume", None);

    let receipt = p
        .admit("replace-idle-lead", replace(&p, p.fence().await))
        .await;
    let candidate = receipt.target_session_id.unwrap();
    let _process = super::super::launch::install_controller_candidate_test_process(candidate);
    p.execute().await.unwrap();

    let operation = receipt.operation_id;
    let receipt = p.receipt(operation).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    assert_eq!(receipt.outcome.as_deref(), Some("lead_committed"));
    let mut expected = vec![own, recurring];
    expected.sort();
    assert_eq!(retired_wakes(&p, operation).await, expected);
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(p.epic)
            .unwrap()
            .unwrap()
            .lead_session_id,
        Some(candidate)
    );
    assert!(!enabled(&p, own).await);
    assert!(!enabled(&p, recurring).await);
    // Child watches deliver to the published tip, which is now the new lead.
    assert!(enabled(&p, child_watch).await);
    assert!(enabled(&p, unrelated).await);

    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, candidate)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(candidate);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_replace_of_lead_with_pending_question_stays_refused_and_keeps_its_wake() {
    let p = pilot().await;
    let own = wake(&p, p.lead, "resume", None);
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET pending_question_json='{}' WHERE id=?1",
            [p.lead.to_string()],
        )
        .unwrap();
    p.admit("replace-questioned-lead", replace(&p, p.fence().await))
        .await;
    let refused = p.execute().await.unwrap_err().to_string();
    assert!(
        refused.contains("manager_v2_human_or_recovery_owner"),
        "{refused}"
    );
    assert!(enabled(&p, own).await);
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
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn prepared_lead_handover_is_ready_despite_own_resume_wake_but_resume_still_waits() {
    use crate::store::manager_actions::OperatorPause;

    let p = pilot().await;
    wake(&p, p.lead, "resume", None);
    let store = p.manager.store.lock().await;
    store.manager_lead_handover_human_gate(p.lead).unwrap();
    assert_eq!(
        store
            .prepare_manager_action(p.owner, prepared_replace(&p))
            .unwrap()
            .readiness,
        ManagerPreparedActionReadinessV2::Ready
    );
    // The lead's own wake still owns an ordinary manager resume.
    assert_eq!(
        store
            .prepare_manager_action(p.owner, prepared_resume(&p))
            .unwrap()
            .readiness,
        ManagerPreparedActionReadinessV2::Blocked
    );
    // A hard operator pause is a genuine human owner: the handover waits.
    store
        .set_operator_pause(p.lead, OperatorPause::Hard)
        .unwrap();
    assert!(store.manager_lead_handover_human_gate(p.lead).is_err());
    assert_eq!(
        store
            .prepare_manager_action(p.owner, prepared_replace(&p))
            .unwrap()
            .readiness,
        ManagerPreparedActionReadinessV2::Blocked
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_replace_retires_earlier_lineage_resume_wakes_in_the_lead_cas() {
    let p = pilot().await;
    let older = earlier_hop(&p).await;
    let recurring = wake(&p, older, "resume", Some(600));
    let child_watch = wake(&p, p.lead, "on_terminal", None);
    let unrelated = wake(&p, p.owner, "resume", None);

    let receipt = p
        .admit("replace-lineage-lead", replace(&p, p.fence().await))
        .await;
    let candidate = receipt.target_session_id.unwrap();
    let _process = super::super::launch::install_controller_candidate_test_process(candidate);
    p.execute().await.unwrap();

    let operation = receipt.operation_id;
    let receipt = p.receipt(operation).await;
    assert_eq!(receipt.state, ManagerActionStateV2::Succeeded);
    assert_eq!(receipt.outcome.as_deref(), Some("lead_committed"));
    assert_eq!(retired_wakes(&p, operation).await, vec![recurring]);
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .get_session(p.epic)
            .unwrap()
            .unwrap()
            .lead_session_id,
        Some(candidate)
    );
    assert!(!enabled(&p, recurring).await);
    // Child watches deliver to the published tip, which is now the new lead.
    assert!(enabled(&p, child_watch).await);
    assert!(enabled(&p, unrelated).await);

    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, candidate)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(candidate);
}

/// #1042: `pause_lead` is not refused for the lead's own resume wakes; it
/// suspends exactly those (recorded, not deleted) and `resume_lead` restores
/// exactly those.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_pause_lead_suspends_resume_wakes_and_resume_lead_restores_exactly_those() {
    let p = pilot().await;
    let own = wake(&p, p.lead, "resume", None);
    let recurring = wake(&p, p.lead, "resume", Some(600));
    let already_off = wake(&p, p.lead, "resume", None);
    let child_watch = wake(&p, p.lead, "on_terminal", None);
    let unrelated = wake(&p, p.owner, "resume", None);
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
            [already_off.to_string()],
        )
        .unwrap();
    // The ordinary wake ownership holds the generic manager gate ...
    assert!(
        p.manager
            .store
            .lock()
            .await
            .manager_action_human_gate(p.lead)
            .is_err()
    );

    // ... but not pause_lead, which suspends the wakes with its own success.
    p.admit(
        "pause-with-wakes",
        ManagerActionV2::PauseLead {
            epic_id: p.epic,
            expected: p.fence().await,
            reason: "hold the Epic".into(),
        },
    )
    .await;
    let claim = p.claim().await;
    {
        let store = p.manager.store.lock().await;
        store.manager_action_runtime_gate(&claim, false).unwrap();
        store
            .finish_manager_action(&claim, ManagerActionStateV2::Succeeded, "lead_paused")
            .unwrap();
    }
    assert!(!enabled(&p, own).await);
    assert!(!enabled(&p, recurring).await);
    assert!(!enabled(&p, already_off).await);
    assert!(enabled(&p, child_watch).await);
    assert!(enabled(&p, unrelated).await);
    let record: String = p
        .manager
        .store
        .lock()
        .await
        .get_daemon_setting(&format!("manager_suspended_wakes:{}", p.lead))
        .unwrap()
        .expect("suspension is recorded");
    let mut recorded: Vec<Uuid> = serde_json::from_value(
        serde_json::from_str::<serde_json::Value>(&record).unwrap()["job_ids"].clone(),
    )
    .unwrap();
    recorded.sort();
    let mut expected = vec![own, recurring];
    expected.sort();
    assert_eq!(recorded, expected);

    p.admit(
        "resume-after-pause",
        ManagerActionV2::ResumeLead {
            epic_id: p.epic,
            expected: p.fence().await,
            message: "continue".into(),
        },
    )
    .await;
    let claim = p.claim().await;
    {
        let store = p.manager.store.lock().await;
        store.manager_action_runtime_gate(&claim, false).unwrap();
        store
            .finish_manager_action(&claim, ManagerActionStateV2::Succeeded, "lead_resumed")
            .unwrap();
    }
    assert!(enabled(&p, own).await);
    assert!(enabled(&p, recurring).await);
    // A job that was already disabled before the pause is not resurrected.
    assert!(!enabled(&p, already_off).await);
    assert!(enabled(&p, child_watch).await);
    assert!(enabled(&p, unrelated).await);
}

/// #1042: a genuine operator hold still refuses `pause_lead`, and the lead's
/// wakes stay untouched.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_pause_lead_with_pending_question_stays_refused_and_keeps_its_wake() {
    let p = pilot().await;
    let own = wake(&p, p.lead, "resume", None);
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET pending_question_json='{}' WHERE id=?1",
            [p.lead.to_string()],
        )
        .unwrap();
    p.admit(
        "pause-questioned-lead",
        ManagerActionV2::PauseLead {
            epic_id: p.epic,
            expected: p.fence().await,
            reason: "hold the Epic".into(),
        },
    )
    .await;
    let claim = p.claim().await;
    let refused = p
        .manager
        .store
        .lock()
        .await
        .manager_action_runtime_gate(&claim, false)
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("manager_v2_human_or_recovery_owner"),
        "{refused}"
    );
    assert!(enabled(&p, own).await);
}
