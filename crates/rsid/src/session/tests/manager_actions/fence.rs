//! K2 continuation fence on the manager-recovery path (design test 11).
#![allow(clippy::unwrap_used, clippy::large_futures)]

use super::*;

/// Design test 11. Under RPC-1 C1 a published successor of the lead also
/// moves the lead, so the manager lead fence refuses first. The continuation
/// fence is what still protects a lead whose legacy (pre-RPC-1) live
/// successor never received the lead: the exact target is not the published
/// tip, so `ManagerRecovery` is refused `continuation_tip_changed` instead of
/// restarting the superseded incarnation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_resume_of_a_superseded_exact_target_is_refused_tip_changed() {
    let p = pilot().await;
    let admitted = p
        .admit(
            "resume-superseded",
            ManagerActionV2::ResumeLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "resume the lead".into(),
            },
        )
        .await;
    let successor = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        let mut row = bare_session(successor);
        row.project_id = Some(p.project);
        row.working_dir = p.repo.clone();
        row.session_kind = SessionKind::Feature;
        row.parent_id = Some(p.epic);
        row.provider = SessionProvider::Claude;
        row.model = Some("manager-scripted-provider".into());
        row.claude_session_id = Some(format!("provider-{successor}"));
        row.continued_from = Some(p.lead);
        store.insert_session(&row).unwrap();
    }
    let process = super::super::launch::install_controller_candidate_test_process(p.lead);
    let refused = p.execute().await.unwrap_err().to_string();
    assert!(
        refused.contains(crate::store::manager_actions::fence::CONTINUATION_TIP_CHANGED),
        "{refused}"
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        p.receipt(admitted.operation_id).await.target_session_id,
        Some(p.lead)
    );
    assert_eq!(
        p.manager
            .store
            .lock()
            .await
            .published_lineage_tip(p.lead)
            .unwrap(),
        Some(successor)
    );
    super::super::launch::drop_controller_candidate_test_process(p.lead);
}

/// Wait until `session` has a persisted event whose content contains `text`.
async fn persisted_event_count(p: &Pilot, session: Uuid, text: &str) -> usize {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let count = p
            .manager
            .store
            .lock()
            .await
            .load_events(session)
            .unwrap()
            .iter()
            .filter(|event| event.content.contains(text))
            .count();
        if count > 0 || tokio::time::Instant::now() >= deadline {
            return count;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Design test 10. `ManagerRecovery` waives only the retirement witness: the
/// manager resumes a retired lead (receipt Succeeded, one new User event),
/// that event makes the witness stale, and a later scheduled wake to the same
/// lead is delivered by the ordinary fenced path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manager_resume_of_a_retired_lead_stales_the_witness_and_permits_a_later_wake() {
    use crate::issue_tracker::poller::SessionLauncher;
    let p = pilot().await;
    let retire = p
        .admit(
            "retire-before-recovery",
            ManagerActionV2::RetireLeadContinuations {
                epic_id: p.epic,
                expected: p.fence().await,
            },
        )
        .await;
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(retire.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert!(
        p.manager
            .store
            .lock()
            .await
            .manager_lead_program_outcome_superseded(p.lead)
            .unwrap(),
        "the retirement witness is live"
    );

    let process = super::super::launch::install_controller_candidate_test_process(p.lead);
    let resume = p
        .admit(
            "resume-retired",
            ManagerActionV2::ResumeLead {
                epic_id: p.epic,
                expected: p.fence().await,
                message: "resume the retired lead".into(),
            },
        )
        .await;
    p.execute().await.unwrap();
    assert_eq!(
        p.receipt(resume.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert_eq!(
        persisted_event_count(&p, p.lead, "resume the retired lead").await,
        1
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    assert!(
        !p.manager
            .store
            .lock()
            .await
            .manager_lead_program_outcome_superseded(p.lead)
            .unwrap(),
        "the recovery's User event made the witness stale"
    );

    // Settle the recovered turn, then a later wake reaches the same lead.
    super::super::lifecycle::interrupt_active_in_maps(&p.manager.active, p.lead)
        .await
        .unwrap();
    super::super::launch::drop_controller_candidate_test_stream(p.lead);
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while p.manager.active.read().await.contains_key(&p.lead)
            || p.manager.persistence.pending.load(Ordering::SeqCst) != 0
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let wake = manager_program_job(&p, "resume", true);
    p.manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&wake)
        .unwrap();
    let later = super::super::launch::install_controller_candidate_test_process(p.lead);
    let delivered = SessionLauncher::resume_scheduled_job(
        &p.manager,
        p.lead,
        "later scheduled wake".into(),
        vec![wake.id],
    )
    .await
    .unwrap();
    assert_eq!(delivered, p.lead);
    assert_eq!(
        persisted_event_count(&p, p.lead, "later scheduled wake").await,
        1
    );
    assert_eq!(later.productive_start_count.load(Ordering::SeqCst), 1);
    super::super::launch::drop_controller_candidate_test_process(p.lead);
}
