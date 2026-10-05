//! Issue #1015 slice C/D pins: heal scheduling, budget, notices.

use super::*;
use crate::store::manager_coordinator::tests::fixture;
use rsi_common::harness_manager_v2::ManagerPolicyV2;

fn failed(store: &Store, terminal_reason: Option<&str>, stop_reason: Option<&str>) -> Session {
    let mut session = crate::test_support::test_session(
        Uuid::new_v4(),
        std::path::PathBuf::from("/var/tmp/transient-heal"),
    );
    session.status = SessionStatus::Failed;
    session.max_retries = None;
    session.retry_attempt = None;
    session.terminal_reason = terminal_reason.map(str::to_string);
    session.stop_reason = stop_reason.map(str::to_string);
    store.insert_session(&session).expect("session");
    session
}

fn scheduled(outcome: TransientHealOutcome) -> HealSchedule {
    match outcome {
        TransientHealOutcome::Scheduled(schedule) => schedule,
        other => panic!("expected Scheduled, got {other:?}"),
    }
}

/// Model the scheduler firing the one-shot resume job and the session failing
/// again: the row is settled (disabled) and the session is Failed.
fn fire(store: &Store, job: Uuid) {
    store
        .conn
        .execute(
            "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
            [job.to_string()],
        )
        .unwrap();
}

fn next_fire(store: &Store, job: Uuid) -> DateTime<Utc> {
    store.get_scheduled_job(&job).unwrap().unwrap().next_fire_at
}

fn notice_versions(store: &Store, session: Uuid) -> Vec<String> {
    let mut stmt = store
        .conn
        .prepare(
            "SELECT subject_version FROM harness_manager_notices
             WHERE subject_id=?1 AND subject_version LIKE 'transient_heal%'
             ORDER BY subject_version",
        )
        .unwrap();
    stmt.query_map([session.to_string()], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn first_transient_failure_creates_resume_job_with_attempt_1_backoff() {
    let store = Store::open_in_memory().unwrap();
    let session = failed(&store, Some("aborted_streaming"), None);
    let now = Utc::now();
    let schedule = scheduled(store.schedule_transient_heal(session.id, now).unwrap());
    assert_eq!(schedule.attempt, 1);
    assert_eq!(schedule.reason, "aborted_streaming");
    let job = store.get_scheduled_job(&schedule.job_id).unwrap().unwrap();
    assert!(job.enabled);
    assert_eq!(job.wake_mode, WakeMode::Resume);
    assert_eq!(job.wake_session_id, Some(session.id));
    assert_eq!(job.next_fire_at, now + chrono::Duration::seconds(30));
    let state = store.transient_heal_state(job.id).unwrap().unwrap();
    assert_eq!(state.attempts, 1);
    // A repeated pass while the job is armed changes nothing.
    assert_eq!(
        store.schedule_transient_heal(session.id, now).unwrap(),
        TransientHealOutcome::Ineligible(HealIneligible::ResumeJobExists)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn second_transient_failure_on_resumed_successor_advances_attempt_and_backoff() {
    let store = Store::open_in_memory().unwrap();
    let session = failed(&store, Some("aborted_streaming"), None);
    let t0 = Utc::now();
    let first = scheduled(store.schedule_transient_heal(session.id, t0).unwrap());
    fire(&store, first.job_id);
    let t1 = t0 + chrono::Duration::minutes(1);
    let second = scheduled(store.schedule_transient_heal(session.id, t1).unwrap());
    assert_eq!(second.job_id, first.job_id);
    assert_eq!(second.attempt, 2);
    assert_eq!(
        next_fire(&store, second.job_id),
        t1 + chrono::Duration::seconds(60)
    );
    assert!(
        store
            .get_scheduled_job(&second.job_id)
            .unwrap()
            .unwrap()
            .enabled
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn ninth_attempt_exhausts_and_stops_creating_jobs() {
    let store = Store::open_in_memory().unwrap();
    let session = failed(&store, Some("aborted_streaming"), None);
    let t0 = Utc::now();
    let mut job = None;
    for attempt in 1..=TRANSIENT_HEAL_MAX_ATTEMPTS {
        let now = t0 + chrono::Duration::seconds(i64::from(attempt));
        let schedule = scheduled(store.schedule_transient_heal(session.id, now).unwrap());
        assert_eq!(schedule.attempt, attempt);
        fire(&store, schedule.job_id);
        job = Some(schedule.job_id);
    }
    let job = job.unwrap();
    let now = t0 + chrono::Duration::seconds(30);
    let TransientHealOutcome::Exhausted(exhausted) =
        store.schedule_transient_heal(session.id, now).unwrap()
    else {
        panic!("the ninth attempt must exhaust");
    };
    assert_eq!(exhausted.attempt, TRANSIENT_HEAL_MAX_ATTEMPTS + 1);
    assert_eq!(exhausted.not_before, None);
    assert!(!store.get_scheduled_job(&job).unwrap().unwrap().enabled);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn sixty_minute_window_exhausts_regardless_of_attempt_count() {
    let store = Store::open_in_memory().unwrap();
    let session = failed(&store, Some("aborted_streaming"), None);
    let t0 = Utc::now();
    let first = scheduled(store.schedule_transient_heal(session.id, t0).unwrap());
    fire(&store, first.job_id);
    let late = t0 + chrono::Duration::minutes(61);
    assert!(matches!(
        store.schedule_transient_heal(session.id, late).unwrap(),
        TransientHealOutcome::Exhausted(_)
    ));
    assert!(
        !store
            .get_scheduled_job(&first.job_id)
            .unwrap()
            .unwrap()
            .enabled
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn transient_heal_state_distinct_from_continuation_retry_state() {
    let store = Store::open_in_memory().unwrap();
    let session = failed(&store, Some("aborted_streaming"), None);
    let t0 = Utc::now();
    let first = scheduled(store.schedule_transient_heal(session.id, t0).unwrap());
    store
        .record_continuation_retry(first.job_id, "circuit_open", None, t0, false)
        .unwrap();
    // Both namespaces coexist on the same row.
    assert_eq!(
        store
            .continuation_retry(first.job_id)
            .unwrap()
            .unwrap()
            .attempts,
        1
    );
    assert_eq!(
        store
            .transient_heal_state(first.job_id)
            .unwrap()
            .unwrap()
            .attempts,
        1
    );
    // Clearing the continuation retry leaves the heal counter alone.
    store.clear_continuation_retry(first.job_id).unwrap();
    assert_eq!(
        store
            .transient_heal_state(first.job_id)
            .unwrap()
            .unwrap()
            .attempts,
        1
    );
    // A re-armed heal starts a fresh continuation-retry cycle.
    store
        .record_continuation_retry(first.job_id, "circuit_open", None, t0, false)
        .unwrap();
    fire(&store, first.job_id);
    scheduled(store.schedule_transient_heal(session.id, t0).unwrap());
    assert!(store.continuation_retry(first.job_id).unwrap().is_none());
    assert_eq!(
        store
            .transient_heal_state(first.job_id)
            .unwrap()
            .unwrap()
            .attempts,
        2
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn manager_evidence_cases_are_classified_for_the_heal() {
    let store = Store::open_in_memory().unwrap();
    let now = Utc::now();
    // Context full: rotation's case, never resumed.
    let full = failed(
        &store,
        Some("blocking_limit"),
        Some("terminal_failure:non-zero-exit"),
    );
    assert_eq!(
        store.schedule_transient_heal(full.id, now).unwrap(),
        TransientHealOutcome::NotTransient("blocking_limit")
    );
    // Operator message interrupted the tool call: a human action.
    let interrupted = failed(&store, None, Some("aborted_tools"));
    assert_eq!(
        store.schedule_transient_heal(interrupted.id, now).unwrap(),
        TransientHealOutcome::NotTransient("aborted_tools")
    );
    // OOM-killed build (exit 137) under the shared cap: resume after backoff.
    let killed = failed(&store, None, Some("terminal_failure:non-zero-exit"));
    let schedule = scheduled(store.schedule_transient_heal(killed.id, now).unwrap());
    assert_eq!(schedule.reason, "terminal_failure:non-zero-exit");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn paused_manager_policy_blocks_the_heal() {
    let store = Store::open_in_memory().unwrap();
    let (_, lead) = fixture(
        &store,
        ManagerPolicyV2 {
            paused: true,
            ..ManagerPolicyV2::default()
        },
    );
    store
        .conn
        .execute(
            "UPDATE sessions SET status='Failed', terminal_reason='aborted_streaming',
                 max_retries=NULL, retry_attempt=NULL WHERE id=?1",
            [lead.id.to_string()],
        )
        .unwrap();
    assert_eq!(
        store.schedule_transient_heal(lead.id, Utc::now()).unwrap(),
        TransientHealOutcome::Ineligible(HealIneligible::Paused)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn lead_heal_schedules_manager_notice_once_per_attempt() {
    let store = Store::open_in_memory().unwrap();
    let (_, lead) = fixture(&store, ManagerPolicyV2::default());
    store
        .conn
        .execute(
            "UPDATE sessions SET status='Failed', terminal_reason='aborted_streaming',
                 max_retries=NULL, retry_attempt=NULL WHERE id=?1",
            [lead.id.to_string()],
        )
        .unwrap();
    let t0 = Utc::now();
    let first = scheduled(store.schedule_transient_heal(lead.id, t0).unwrap());
    assert!(first.manager_notified);
    assert_eq!(
        notice_versions(&store, lead.id),
        ["transient_heal_scheduled:1"]
    );
    // A repeated reconcile pass for the same attempt adds no notice.
    for _ in 0..2 {
        assert!(
            store
                .record_manager_heal_notice(
                    lead.id,
                    "transient_heal_scheduled:1",
                    &serde_json::json!({"attempt": 1}),
                )
                .unwrap()
        );
    }
    assert_eq!(
        notice_versions(&store, lead.id),
        ["transient_heal_scheduled:1"]
    );
    // The next attempt is a new notice.
    fire(&store, first.job_id);
    scheduled(
        store
            .schedule_transient_heal(lead.id, t0 + chrono::Duration::minutes(1))
            .unwrap(),
    );
    assert_eq!(
        notice_versions(&store, lead.id),
        ["transient_heal_scheduled:1", "transient_heal_scheduled:2"]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[tokio::test]
async fn child_heal_publishes_session_heal_scheduled_event() {
    use crate::bus::{DaemonEvent, EventBus};
    let store = Store::open_in_memory().unwrap();
    let mut parent = crate::test_support::test_session(
        Uuid::new_v4(),
        std::path::PathBuf::from("/var/tmp/transient-heal"),
    );
    parent.status = SessionStatus::Running;
    store.insert_session(&parent).unwrap();
    let mut child = failed(&store, Some("aborted_streaming"), None);
    store
        .conn
        .execute(
            "UPDATE sessions SET parent_id=?2 WHERE id=?1",
            params![child.id.to_string(), parent.id.to_string()],
        )
        .unwrap();
    child.parent_id = Some(parent.id);
    let store = std::sync::Arc::new(tokio::sync::Mutex::new(store));
    let bus = std::sync::Arc::new(EventBus::new(16));
    let mut events = bus.subscribe();
    crate::store::session_transient_heal::heal_failed_session(&store, &bus, child.id).await;
    let event = events.recv().await.expect("heal event");
    match &*event {
        DaemonEvent::SessionHealScheduled {
            session_id,
            owner_session_id,
            attempt,
            max_attempts,
            not_before,
            reason,
        } => {
            assert_eq!(*session_id, child.id);
            assert_eq!(*owner_session_id, Some(parent.id));
            assert_eq!((*attempt, *max_attempts), (1, TRANSIENT_HEAL_MAX_ATTEMPTS));
            assert!(not_before.is_some());
            assert_eq!(reason, "aborted_streaming");
        }
        other => panic!("expected SessionHealScheduled, got {other:?}"),
    }
}

fn insert_event(store: &Store, session: Uuid, sequence: i64, event_type: &str, at: DateTime<Utc>) {
    store
        .conn
        .execute(
            "INSERT INTO conversation_events
                (session_id, sequence, event_type, role, content, created_at)
             VALUES (?1, ?2, ?3, NULL, 'x', ?4)",
            params![session.to_string(), sequence, event_type, rfc3339(at)],
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn healthy_run_after_a_heal_starts_a_fresh_budget_epoch() {
    let store = Store::open_in_memory().unwrap();
    let session = failed(&store, Some("aborted_streaming"), None);
    let t0 = Utc::now() - chrono::Duration::hours(3);
    let first = scheduled(store.schedule_transient_heal(session.id, t0).unwrap());
    fire(&store, first.job_id);
    // The heal resumed and ran healthy for a long while: a completed turn
    // well after the attempt plus the longest backoff.
    insert_event(
        &store,
        session.id,
        1,
        "Message",
        t0 + chrono::Duration::minutes(30),
    );
    let later = t0 + chrono::Duration::hours(2);
    let second = scheduled(store.schedule_transient_heal(session.id, later).unwrap());
    assert_eq!(second.job_id, first.job_id);
    assert_eq!(second.attempt, 1, "a fresh epoch restarts the counter");
    let state = store.transient_heal_state(second.job_id).unwrap().unwrap();
    assert_eq!(state.attempts, 1);
    assert!(!state.exhausted);
    assert_eq!(
        next_fire(&store, second.job_id),
        later + chrono::Duration::seconds(30)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn repeated_failure_inside_the_window_still_exhausts_despite_activity() {
    let store = Store::open_in_memory().unwrap();
    let session = failed(&store, Some("aborted_streaming"), None);
    let t0 = Utc::now() - chrono::Duration::hours(3);
    let first = scheduled(store.schedule_transient_heal(session.id, t0).unwrap());
    fire(&store, first.job_id);
    // Output that arrived before the last attempt's backoff cap is the failed
    // retry itself, not a healthy run; System events never count.
    insert_event(
        &store,
        session.id,
        1,
        "Message",
        t0 + chrono::Duration::seconds(45),
    );
    insert_event(
        &store,
        session.id,
        2,
        "System",
        t0 + chrono::Duration::minutes(40),
    );
    let late = t0 + chrono::Duration::minutes(61);
    assert!(matches!(
        store.schedule_transient_heal(session.id, late).unwrap(),
        TransientHealOutcome::Exhausted(_)
    ));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn exhausted_row_expires_after_a_quiet_window_but_not_before() {
    let store = Store::open_in_memory().unwrap();
    let session = failed(&store, Some("aborted_streaming"), None);
    let t0 = Utc::now() - chrono::Duration::hours(6);
    let first = scheduled(store.schedule_transient_heal(session.id, t0).unwrap());
    fire(&store, first.job_id);
    assert!(matches!(
        store
            .schedule_transient_heal(session.id, t0 + chrono::Duration::minutes(61))
            .unwrap(),
        TransientHealOutcome::Exhausted(_)
    ));
    // Still inside the quiet window of the last attempt: stays exhausted.
    assert_eq!(
        store
            .schedule_transient_heal(session.id, t0 + chrono::Duration::minutes(59))
            .unwrap(),
        TransientHealOutcome::Ineligible(HealIneligible::BudgetExhausted)
    );
    let quiet = t0 + chrono::Duration::hours(3);
    let again = scheduled(store.schedule_transient_heal(session.id, quiet).unwrap());
    assert_eq!(again.attempt, 1);
    assert!(
        store
            .get_scheduled_job(&again.job_id)
            .unwrap()
            .unwrap()
            .enabled
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn pause_refusal_keeps_backoff_without_spending_budget_or_window() {
    let store = Store::open_in_memory().unwrap();
    let session = failed(&store, Some("aborted_streaming"), None);
    let t0 = Utc::now() - chrono::Duration::hours(5);
    let first = scheduled(store.schedule_transient_heal(session.id, t0).unwrap());
    // Far more refusals than the budget, spread over more than the window.
    let mut now = t0;
    for _ in 0..(TRANSIENT_HEAL_MAX_ATTEMPTS + 3) {
        now += chrono::Duration::minutes(10);
        let outcome = store
            .defer_transient_heal(first.job_id, "manager_v2_paused", now, false)
            .unwrap();
        let TransientHealOutcome::Deferred(deferred) = outcome else {
            panic!("a no-spend refusal re-arms: {outcome:?}");
        };
        assert_eq!(deferred.attempt, 1);
        assert_eq!(
            next_fire(&store, first.job_id),
            now + chrono::Duration::seconds(30)
        );
    }
    let state = store.transient_heal_state(first.job_id).unwrap().unwrap();
    assert_eq!(state.attempts, 1);
    assert!(!state.exhausted);
    assert!(!state.window_expired(now + chrono::Duration::minutes(1)));
    // The heal finally runs and fails again: the paused time was not budget.
    fire(&store, first.job_id);
    let next = scheduled(
        store
            .schedule_transient_heal(session.id, now + chrono::Duration::minutes(2))
            .unwrap(),
    );
    assert_eq!(next.attempt, 2);
}
