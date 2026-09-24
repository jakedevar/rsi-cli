//! Store-level pins for issue #669 manager seat classification and bounds.

use super::*;
use crate::store::manager_coordinator::tests::{fixture, raise_question};
use crate::store::manager_resources::tests::{admitted, complete};
use rsi_common::harness_manager::{
    AgentManagerInboxRequestV1, AgentManagerReplyRequestV1, AgentManagerSendRequestV1,
};
use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
use rsi_common::types::{ConversationEvent, EventType, Role};

fn execute(max: u16, delay: u32) -> ManagerPolicyV2 {
    ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        max_recovery_attempts: max,
        retry_delay_seconds: delay,
        ..ManagerPolicyV2::default()
    }
}

fn set_policy(
    store: &Store,
    config: &HarnessManagerConfigV1,
    edit: impl FnOnce(&mut ManagerPolicyV2),
) {
    let mut grant = store
        .get_harness_manager_policy(config.project_id)
        .unwrap()
        .unwrap();
    edit(&mut grant.policy);
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: config.project_id,
            expected_scope_version: config.row_version,
            expected_policy_version: grant.row_version,
            idempotency_key: Uuid::new_v4().to_string(),
            policy: grant.policy,
        })
        .unwrap();
}

fn fail(store: &Store, tip: Uuid) {
    store
        .conn
        .execute(
            "UPDATE sessions SET status='Failed' WHERE id=?1",
            [tip.to_string()],
        )
        .unwrap();
}

fn pass(store: &Store, project: Uuid, retry: bool, now: DateTime<Utc>) -> ManagerSeatPassV1 {
    store
        .reconcile_manager_seat(project, |_| false, retry, Uuid::new_v4(), now)
        .unwrap()
}

fn seat(store: &Store, config: &HarnessManagerConfigV1) -> ManagerSeatStateV1 {
    store.manager_seat_state(config).unwrap().unwrap()
}

fn attempts(store: &Store, project: Uuid) -> Vec<(String, String)> {
    let mut stmt = store
        .conn
        .prepare(
            "SELECT idempotency_key,state FROM harness_manager_v2_operations
             WHERE project_id=?1 AND kind='seat_recovery' ORDER BY created_at",
        )
        .unwrap();
    stmt.query_map([project.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

fn output(store: &Store, session: Uuid, at: DateTime<Utc>) {
    let sequence: i32 = store
        .conn
        .query_row(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM conversation_events WHERE session_id=?1",
            [session.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    store
        .insert_event(&ConversationEvent {
            id: 0,
            session_id: session,
            sequence,
            event_type: EventType::Message,
            role: Some(Role::Assistant),
            created_at: at,
            content: "manager back on duty".into(),
            tool_name: None,
            tool_input: None,
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        })
        .unwrap();
}

fn manager(config: &HarnessManagerConfigV1) -> Uuid {
    config.current_session_id.unwrap()
}

#[test]
fn seat_recovery_fail_closed_by_default() {
    let variants: [(ManagerPolicyV2, bool, &str); 5] = [
        (ManagerPolicyV2::default(), true, SEAT_DISABLED_REASON),
        (
            ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Status,
                ..execute(2, 5)
            },
            true,
            "manager_seat_recovery_requires_execute",
        ),
        (
            ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Monitor,
                ..execute(2, 5)
            },
            true,
            "manager_seat_recovery_requires_execute",
        ),
        (
            ManagerPolicyV2 {
                paused: true,
                ..execute(2, 5)
            },
            true,
            "manager_v2_policy_paused",
        ),
        (execute(2, 5), false, "manager_v2_retry_disabled"),
    ];
    for (policy, retry, reason) in variants {
        let store = Store::open_in_memory().unwrap();
        let (config, _) = fixture(&store, policy);
        fail(&store, manager(&config));
        let now = Utc::now();
        for offset in [0, 3600, 86_400 * 3] {
            let result = pass(
                &store,
                config.project_id,
                retry,
                now + Duration::seconds(offset),
            );
            assert_eq!(result.claim, None, "{reason}");
        }
        let state = seat(&store, &config);
        assert_eq!(state.state, ManagerSeatConditionV1::Down, "{reason}");
        assert_eq!(state.reason, reason);
        assert_eq!(state.tip_session_id, manager(&config));
        assert!(
            state
                .next_action
                .is_some_and(|text| text.to_lowercase().contains("resume"))
        );
        assert_eq!(attempts(&store, config.project_id), Vec::new());
    }
}

#[test]
fn seat_recovery_backoff_and_budget_exhaust_typed() {
    assert_eq!(seat_backoff(10, 1), Duration::seconds(10));
    assert_eq!(seat_backoff(10, 3), Duration::seconds(40));
    assert_eq!(seat_backoff(86_400, 9), Duration::seconds(86_400));
    let store = Store::open_in_memory().unwrap();
    let (config, _) = fixture(&store, execute(2, 10));
    let tip = manager(&config);
    fail(&store, tip);
    let t0 = Utc::now();
    let first = pass(&store, config.project_id, true, t0);
    assert_eq!(first.claim, None);
    let (level, message) = first.notice.unwrap();
    assert_eq!(level, "error");
    assert!(message.starts_with(SEAT_MESSAGE_PREFIX));
    let scheduled = seat(&store, &config);
    assert_eq!(scheduled.state, ManagerSeatConditionV1::Recovering);
    assert_eq!(scheduled.not_before, Some(t0 + Duration::seconds(10)));
    assert_eq!(
        pass(&store, config.project_id, true, t0 + Duration::seconds(9)).claim,
        None
    );
    let claim = pass(&store, config.project_id, true, t0 + Duration::seconds(10))
        .claim
        .unwrap();
    assert_eq!(
        (claim.tip_session_id, claim.attempt, claim.max_attempts),
        (tip, 1, 2)
    );
    assert!(store.finish_manager_seat_claim(&claim, Ok(())).unwrap());
    // The resumed turn failed again: attempt 2 waits 2 * delay from the new failure.
    let t1 = t0 + Duration::seconds(11);
    assert_eq!(pass(&store, config.project_id, true, t1).claim, None);
    assert_eq!(
        seat(&store, &config).not_before,
        Some(t1 + Duration::seconds(20))
    );
    assert_eq!(
        pass(&store, config.project_id, true, t1 + Duration::seconds(19)).claim,
        None
    );
    let second = pass(&store, config.project_id, true, t1 + Duration::seconds(20))
        .claim
        .unwrap();
    assert_eq!(second.attempt, 2);
    assert!(
        store
            .finish_manager_seat_claim(&second, Err("provider exited"))
            .unwrap()
    );
    let exhausted = pass(&store, config.project_id, true, t1 + Duration::seconds(30));
    let (level, message) = exhausted.notice.unwrap();
    assert_eq!(level, "error");
    assert!(message.contains("exhausted"));
    let state = seat(&store, &config);
    assert_eq!(state.state, ManagerSeatConditionV1::Exhausted);
    assert_eq!(state.reason, SEAT_EXHAUSTED_REASON);
    assert_eq!((state.attempts, state.max_attempts), (2, 2));
    assert!(state.next_action.unwrap().contains("appoint a new manager"));
    for days in 1..4 {
        let later = pass(&store, config.project_id, true, t1 + Duration::days(days));
        assert_eq!(later.claim, None);
        assert_eq!(later.notice, None);
    }
    assert_eq!(
        attempts(&store, config.project_id),
        vec![
            (format!("seat:{tip}:1"), "succeeded".to_string()),
            (format!("seat:{tip}:2"), "failed".to_string()),
        ]
    );
}

#[test]
fn seat_aborted_streaming_resume_is_transient_and_retried() {
    let store = Store::open_in_memory().unwrap();
    let (config, _) = fixture(&store, execute(3, 5));
    let tip = manager(&config);
    // The observed #669 shape: a resume after a turn that left a background
    // task outstanding aborts before streaming, with no provider output.
    store
        .conn
        .execute(
            "UPDATE sessions SET status='Failed',terminal_reason='aborted_streaming' WHERE id=?1",
            [tip.to_string()],
        )
        .unwrap();
    let t0 = Utc::now();
    pass(&store, config.project_id, true, t0);
    let state = seat(&store, &config);
    assert_eq!(state.state, ManagerSeatConditionV1::Recovering);
    assert_eq!(
        state.last_terminal_reason.as_deref(),
        Some("aborted_streaming")
    );
    let claim = pass(&store, config.project_id, true, t0 + Duration::seconds(5))
        .claim
        .unwrap();
    assert_eq!((claim.tip_session_id, claim.attempt), (tip, 1));
}

#[test]
fn seat_recovery_respects_human_gate_and_spend_hold() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, execute(3, 1));
    let tip = manager(&config);
    raise_question(&store, tip, 1);
    fail(&store, tip);
    let now = Utc::now() + Duration::hours(1);
    assert_eq!(pass(&store, config.project_id, true, now).claim, None);
    let held = seat(&store, &config);
    assert_eq!(held.state, ManagerSeatConditionV1::Down);
    assert_eq!(held.reason, "manager_v2_human_or_recovery_owner");
    store
        .conn
        .execute(
            "UPDATE sessions SET pending_question_json=NULL WHERE id=?1",
            [tip.to_string()],
        )
        .unwrap();
    let spent = admitted(&store, &lead);
    complete(&store, spent, 2.0);
    set_policy(&store, &config, |policy| policy.max_spend_usd = Some(1.0));
    assert_eq!(pass(&store, config.project_id, true, now).claim, None);
    let spend = seat(&store, &config);
    assert_eq!(spend.state, ManagerSeatConditionV1::Down);
    assert_eq!(spend.reason, "manager_v2_spend_hold");
    assert_eq!(attempts(&store, config.project_id), Vec::new());
}

#[test]
fn seat_recovery_restart_marks_running_claim_uncertain() {
    let store = Store::open_in_memory().unwrap();
    let (config, _) = fixture(&store, execute(3, 1));
    let tip = manager(&config);
    fail(&store, tip);
    let t0 = Utc::now();
    pass(&store, config.project_id, true, t0);
    let old_boot = store
        .reconcile_manager_seat(
            config.project_id,
            |_| false,
            true,
            Uuid::new_v4(),
            t0 + Duration::seconds(1),
        )
        .unwrap()
        .claim
        .unwrap();
    // A second pass while the claim runs never admits another attempt.
    assert_eq!(
        pass(&store, config.project_id, true, t0 + Duration::hours(1)).claim,
        None
    );
    let new_boot = Uuid::new_v4();
    assert_eq!(
        store.recover_manager_seat_claims(Some(new_boot)).unwrap(),
        1
    );
    assert!(!store.finish_manager_seat_claim(&old_boot, Ok(())).unwrap());
    let after = store
        .reconcile_manager_seat(
            config.project_id,
            |_| false,
            true,
            new_boot,
            t0 + Duration::hours(2),
        )
        .unwrap();
    assert_eq!(after.claim, None);
    let state = seat(&store, &config);
    assert_eq!(state.state, ManagerSeatConditionV1::Down);
    assert_eq!(state.reason, SEAT_UNCONFIRMED_REASON);
    assert_eq!(
        attempts(&store, config.project_id),
        vec![(format!("seat:{tip}:1"), "uncertain".to_string())]
    );
}

#[test]
fn seat_state_clears_only_on_provider_output() {
    let store = Store::open_in_memory().unwrap();
    let (config, _) = fixture(&store, ManagerPolicyV2::default());
    let tip = manager(&config);
    fail(&store, tip);
    let t0 = Utc::now();
    pass(&store, config.project_id, true, t0);
    // The operator resumed; the turn ended without provider output.
    store
        .update_session_status(tip, SessionStatus::Completed)
        .unwrap();
    let quiet = pass(&store, config.project_id, true, t0 + Duration::seconds(5));
    assert_eq!(quiet.notice, None);
    assert_eq!(seat(&store, &config).state, ManagerSeatConditionV1::Down);
    output(&store, tip, t0 + Duration::seconds(6));
    let live = pass(&store, config.project_id, true, t0 + Duration::seconds(7));
    let (level, message) = live.notice.unwrap();
    assert_eq!(level, "info");
    assert!(message.starts_with(SEAT_MESSAGE_PREFIX) && message.contains("recovered"));
    let state = seat(&store, &config);
    assert_eq!(state.state, ManagerSeatConditionV1::Live);
    assert_eq!(state.reason, "provider_output_observed");
}

#[test]
fn lead_inbox_and_reply_receipt_show_manager_seat_down() {
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let tip = manager(&config);
    let request = store
        .manager_send(
            tip,
            &AgentManagerSendRequestV1 {
                epic_id: config.epic_ids[0],
                message: "Report status".into(),
                idempotency_key: "seat-request".into(),
            },
        )
        .unwrap();
    fail(&store, tip);
    pass(&store, config.project_id, true, Utc::now());
    let inbox = store
        .manager_inbox(lead.id, &AgentManagerInboxRequestV1::default())
        .unwrap();
    let seat = inbox.manager_seat.unwrap();
    assert_eq!(seat.state, ManagerSeatConditionV1::Down);
    assert_eq!(seat.tip_session_id, tip);
    assert_eq!(inbox.messages[0].message, "Report status");
    let reply = store
        .manager_reply(
            lead.id,
            &AgentManagerReplyRequestV1 {
                request_id: request.message_id,
                message: "Done; evidence in the handoff".into(),
                idempotency_key: "seat-reply".into(),
            },
        )
        .unwrap();
    assert!(!reply.deduplicated);
    let seat = reply.manager_seat.unwrap();
    assert_eq!(seat.state, ManagerSeatConditionV1::Down);
    assert_eq!(seat.reason, SEAT_DISABLED_REASON);
}

#[test]
fn new_tip_resets_seat_budget() {
    let store = Store::open_in_memory().unwrap();
    let (config, _) = fixture(&store, execute(1, 1));
    let old = manager(&config);
    fail(&store, old);
    let t0 = Utc::now();
    pass(&store, config.project_id, true, t0);
    let claim = pass(&store, config.project_id, true, t0 + Duration::seconds(1))
        .claim
        .unwrap();
    store.finish_manager_seat_claim(&claim, Ok(())).unwrap();
    pass(&store, config.project_id, true, t0 + Duration::seconds(2));
    assert_eq!(
        seat(&store, &config).state,
        ManagerSeatConditionV1::Exhausted
    );
    // The operator rotated the seat to a new tip, which failed too.
    let mut next = store.get_session(old).unwrap().unwrap();
    next.id = Uuid::new_v4();
    next.continued_from = Some(old);
    next.rotation_depth += 1;
    next.status = SessionStatus::Completed;
    store.insert_session(&next).unwrap();
    store
        .update_session_status(old, SessionStatus::Archived)
        .unwrap();
    store.record_harness_manager_rotation(old, next.id).unwrap();
    fail(&store, next.id);
    let t1 = t0 + Duration::seconds(10);
    pass(&store, config.project_id, true, t1);
    let state = seat(&store, &config);
    assert_eq!(
        (state.tip_session_id, state.state),
        (next.id, ManagerSeatConditionV1::Recovering)
    );
    assert_eq!(state.attempts, 0);
    let claim = pass(&store, config.project_id, true, t1 + Duration::seconds(1))
        .claim
        .unwrap();
    assert_eq!((claim.tip_session_id, claim.attempt), (next.id, 1));
}

#[test]
fn seat_classifier_is_live_only_on_output_after_evidence() {
    let now = Utc::now();
    let base = ManagerSeatObservationV1 {
        tip_failed: false,
        in_flight: false,
        unconfirmed: false,
        bounds: Ok(()),
        attempts: 1,
        max_attempts: 3,
        retry_delay_seconds: 1,
        down_since: now,
        evidence_after: Some(now),
        last_output: Some(now - Duration::seconds(1)),
        previous: Some(ManagerSeatConditionV1::Recovering),
        now,
    };
    assert_eq!(classify_manager_seat(&base), ManagerSeatVerdictV1::Retain);
    let fresh = ManagerSeatObservationV1 {
        last_output: Some(now + Duration::seconds(1)),
        ..base.clone()
    };
    assert_eq!(classify_manager_seat(&fresh), ManagerSeatVerdictV1::Live);
    let running = ManagerSeatObservationV1 {
        tip_failed: true,
        in_flight: true,
        ..base
    };
    assert_eq!(
        classify_manager_seat(&running),
        ManagerSeatVerdictV1::Retain
    );
}

/// Round 2 (blocked_claim_stalls): a claim refused because the tip was busy
/// is charged, so the next Failed turn claims a fresh `seat:<tip>:<n+1>` key
/// and the budget still exhausts deterministically.
#[test]
fn seat_busy_refusal_is_charged_and_later_failed_turn_claims_fresh_key() {
    let store = Store::open_in_memory().unwrap();
    let (config, _) = fixture(&store, execute(2, 1));
    let tip = manager(&config);
    fail(&store, tip);
    let t0 = Utc::now();
    pass(&store, config.project_id, true, t0);
    let first = pass(&store, config.project_id, true, t0 + Duration::seconds(1))
        .claim
        .unwrap();
    // A scheduler wake won the race: the executor saw a busy tip.
    assert!(
        store
            .finish_manager_seat_claim(
                &first,
                Err("scheduled resume skipped: session is still active")
            )
            .unwrap()
    );
    // That turn failed as well; the next pass schedules and claims attempt 2.
    let t1 = t0 + Duration::seconds(5);
    pass(&store, config.project_id, true, t1);
    let second = pass(&store, config.project_id, true, t1 + Duration::seconds(2))
        .claim
        .unwrap();
    assert_eq!((second.tip_session_id, second.attempt), (tip, 2));
    assert!(
        store
            .finish_manager_seat_claim(&second, Err(SEAT_BUSY_OUTCOME))
            .unwrap()
    );
    pass(&store, config.project_id, true, t1 + Duration::seconds(10));
    let state = seat(&store, &config);
    assert_eq!(state.state, ManagerSeatConditionV1::Exhausted);
    assert_eq!((state.attempts, state.max_attempts), (2, 2));
    assert_eq!(
        attempts(&store, config.project_id),
        vec![
            (format!("seat:{tip}:1"), "blocked".to_string()),
            (format!("seat:{tip}:2"), "blocked".to_string()),
        ]
    );
}

/// Round 2 (appserver_new_row): a tip K13's shared predicate marks
/// non-resumable is never claimed; the seat records a typed unavailable
/// outcome with an operator/manager retry-or-replace next action.
#[test]
fn seat_codex_appserver_tip_records_recovery_unavailable() {
    let store = Store::open_in_memory().unwrap();
    let (config, _) = fixture(&store, execute(3, 1));
    let tip = manager(&config);
    store
        .conn
        .execute(
            "UPDATE sessions SET provider='CodexAppServer',status='Failed' WHERE id=?1",
            [tip.to_string()],
        )
        .unwrap();
    let t0 = Utc::now();
    for offset in [0, 10, 3600] {
        let result = pass(
            &store,
            config.project_id,
            true,
            t0 + Duration::seconds(offset),
        );
        assert_eq!(result.claim, None);
    }
    let state = seat(&store, &config);
    assert_eq!(state.state, ManagerSeatConditionV1::Down);
    assert_eq!(state.reason, SEAT_UNAVAILABLE_REASON);
    assert!(state.next_action.unwrap().contains("retry or replace"));
    assert_eq!(attempts(&store, config.project_id), Vec::new());
}
