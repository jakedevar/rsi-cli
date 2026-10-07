//! #1266: a scheduled tier-mail delivery settles from its continuation's
//! result. Under the #945 rule it is at most once: a continuation that fails
//! after its effect claim records `failed` and never replays the mail.
#![allow(clippy::unwrap_used, clippy::large_futures)]

use super::*;
use crate::store::portfolio_nodes::PortfolioGrantor;
use rsi_common::manager_tier_routing::{AgentSendDownRequestV1, ManagerNodeRefV1};
use rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1;

/// A portfolio node over the pilot's project, seated by a fresh session.
async fn portfolio_seat(p: &Pilot) -> Uuid {
    let seat = Uuid::new_v4();
    let store = p.manager.store.lock().await;
    let mut row = bare_session(seat);
    row.working_dir = p.repo.clone();
    store.insert_session(&row).unwrap();
    // This mail fixture intentionally lowers the pilot's creation caps to zero.
    store
        .configure_portfolio_node_confirmed(
            &ConfigurePortfolioNodeRequestV1 {
                node_id: None,
                parent_node_id: None,
                adopt_node_ids: vec![],
                expected_parent_grant_version: None,
                tier_label: "global".into(),
                seat_session_id: seat,
                project_ids: vec![p.project],
                allowed_launches: p.policy.allowed_launches.clone(),
                policy: ManagerPolicyV2::default(),
                child_policy: None,
                max_direct_reports: 5,
                expected_node_grant_version: 0,
                expected_authority_epoch: 0,
                idempotency_key: format!("tier-mail-{seat}"),
            },
            PortfolioGrantor::Operator,
            "operator:test",
            true,
        )
        .unwrap();
    seat
}

async fn send_down(p: &Pilot, seat: Uuid, key: &str) -> Uuid {
    p.manager
        .store
        .lock()
        .await
        .tier_send_down(
            seat,
            &AgentSendDownRequestV1 {
                target: ManagerNodeRefV1::Project {
                    project_id: p.project,
                },
                message: format!("Down-mail {key}."),
                idempotency_key: key.into(),
            },
        )
        .unwrap()
        .message_id
}

async fn job_enabled(p: &Pilot, id: Uuid) -> bool {
    p.manager
        .store
        .lock()
        .await
        .get_scheduled_job(&id)
        .unwrap()
        .is_some_and(|job| job.enabled)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tier_delivery_that_fails_admission_settles_failed_and_a_launched_one_delivered() {
    use crate::issue_tracker::poller::SessionLauncher;
    let p = pilot().await;
    let seat = portfolio_seat(&p).await;

    // Claimed, then model admission refuses: failed, shown, not replayed.
    let failed = send_down(&p, seat, "tier-failed").await;
    assert!(job_enabled(&p, failed).await);
    super::super::lifecycle::fail_next_continue_admission_for_test(p.owner);
    let refused = SessionLauncher::resume_scheduled_job(
        &p.manager,
        p.owner,
        "tier delivery (admission refused)".into(),
        vec![failed],
    )
    .await
    .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("injected model admission refusal"),
        "{refused}"
    );
    let shown = {
        let store = p.manager.store.lock().await;
        assert!(!store.global_message_deliverable(failed).unwrap());
        store.undelivered_tier_mail_for_session(p.owner).unwrap()
    };
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].message_id, failed);
    assert_eq!(shown[0].role, "recipient");
    assert_eq!(shown[0].state, "failed");
    assert!(
        shown[0]
            .settle_reason
            .contains("injected model admission refusal"),
        "{}",
        shown[0].settle_reason
    );
    assert!(!job_enabled(&p, failed).await);
    // No replay: delivering the same row again is refused before any effect.
    let replay = SessionLauncher::resume_scheduled_job(
        &p.manager,
        p.owner,
        "tier delivery (replay)".into(),
        vec![failed],
    )
    .await;
    assert!(replay.is_err());
    let after = p
        .manager
        .store
        .lock()
        .await
        .undelivered_tier_mail_for_session(p.owner)
        .unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].state, "failed");

    // The normal path: the continuation launches and the mail is delivered.
    let delivered = send_down(&p, seat, "tier-delivered").await;
    let process = super::super::launch::install_controller_candidate_test_process(p.owner);
    let resumed = SessionLauncher::resume_scheduled_job(
        &p.manager,
        p.owner,
        "tier delivery (launched)".into(),
        vec![delivered],
    )
    .await
    .unwrap();
    assert_eq!(resumed, p.owner);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    {
        let store = p.manager.store.lock().await;
        assert!(!store.global_message_deliverable(delivered).unwrap());
        let undelivered = store.undelivered_tier_mail_for_session(p.owner).unwrap();
        assert_eq!(undelivered.len(), 1, "only the failed mail is undelivered");
        assert_eq!(undelivered[0].message_id, failed);
    }
    assert!(!job_enabled(&p, delivered).await);
    super::super::launch::drop_controller_candidate_test_process(p.owner);
}

/// #1294: the continuation launched, but its settlement write failed. The
/// recorded outcome is kept; once the failure clears, the recovery pass
/// settles the row `delivered` without delivering it a second time.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_settlement_write_is_retried_by_the_recovery_pass_without_redelivery() {
    use crate::issue_tracker::poller::SessionLauncher;
    let p = pilot().await;
    let seat = portfolio_seat(&p).await;
    let mail = send_down(&p, seat, "tier-resettle").await;
    super::super::tier_settlement::fail_next_tier_settlement_write_for_test(mail);
    let process = super::super::launch::install_controller_candidate_test_process(p.owner);
    let resumed = SessionLauncher::resume_scheduled_job(
        &p.manager,
        p.owner,
        "tier delivery (settlement write fails)".into(),
        vec![mail],
    )
    .await
    .unwrap();
    assert_eq!(resumed, p.owner);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    let state = |p: &Pilot| {
        let store = p.manager.store.clone();
        async move { store.lock().await.tier_message_state(mail).unwrap() }
    };
    // The write failed: the row is still claimed (and never redeliverable).
    assert_eq!(state(&p).await.as_deref(), Some("claimed"));
    assert!(
        !p.manager
            .store
            .lock()
            .await
            .global_message_deliverable(mail)
            .unwrap()
    );

    // The failure has cleared; the next pass writes the recorded outcome.
    assert_eq!(p.manager.resettle_tier_mail().await.unwrap(), 1);
    assert_eq!(state(&p).await.as_deref(), Some("delivered"));
    assert!(!job_enabled(&p, mail).await);
    assert_eq!(p.manager.resettle_tier_mail().await.unwrap(), 0);
    assert_eq!(state(&p).await.as_deref(), Some("delivered"));
    assert_eq!(
        process.productive_start_count.load(Ordering::SeqCst),
        1,
        "settlement never delivers again"
    );
    super::super::launch::drop_controller_candidate_test_process(p.owner);
}

/// #1307: a recovery pass delayed waiting for the Store while a continuation
/// registers and claims its mail must not sweep that live claim, even when
/// the pass runs long after the claim (an injected clock three minutes
/// ahead). The pass reads its exclusions under the Store lock, so the claim
/// is excluded; once the continuation is gone with no outcome, the next pass
/// settles it `uncertain`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_registered_while_the_recovery_pass_waits_is_not_swept() {
    use crate::store::manager_actions::fence::ContinuationAuthorityV1;
    let p = pilot().await;
    let seat = portfolio_seat(&p).await;
    let mail = send_down(&p, seat, "tier-live-claim").await;
    let late = || chrono::Utc::now() + chrono::Duration::minutes(3);
    let manager = &p.manager;
    // The test holds the Store, so the pass below is delayed at its lock.
    let held = manager.store.lock().await;
    let (swept, in_flight) =
        tokio::join!(manager.resettle_tier_mail_with_clock(late), async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                super::super::tier_settlement::RESETTLE_WAITING_FOR_STORE.notified(),
            )
            .await
            .expect("the recovery pass started");
            // A continuation registers in flight, then claims (its order).
            let in_flight = manager.tier_in_flight(&[mail]);
            let fence = held
                .capture_continuation_fence(p.owner, ContinuationAuthorityV1::Automated)
                .unwrap()
                .expect("a fence for the PM seat");
            held.claim_continuation_effect(&fence, &[mail]).unwrap();
            assert_eq!(
                held.tier_message_state(mail).unwrap().as_deref(),
                Some("claimed")
            );
            drop(held);
            in_flight
        });
    assert_eq!(swept.unwrap(), 0, "the live claim is not swept");
    assert_eq!(
        manager
            .store
            .lock()
            .await
            .tier_message_state(mail)
            .unwrap()
            .as_deref(),
        Some("claimed")
    );
    // Still in flight: a later late pass leaves it alone too.
    assert_eq!(
        manager.resettle_tier_mail_with_clock(late).await.unwrap(),
        0
    );
    // The continuation is gone with no recorded outcome: now it is swept.
    drop(in_flight);
    assert_eq!(
        manager.resettle_tier_mail_with_clock(late).await.unwrap(),
        1
    );
    let shown = manager
        .store
        .lock()
        .await
        .undelivered_tier_mail_for_session(p.owner)
        .unwrap();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].message_id, mail);
    assert_eq!(shown[0].state, "uncertain");
    assert_eq!(
        shown[0].settle_reason,
        crate::store::manager_tier_routing::TIER_SETTLEMENT_LOST_REASON
    );
    assert!(!job_enabled(&p, mail).await);
}
