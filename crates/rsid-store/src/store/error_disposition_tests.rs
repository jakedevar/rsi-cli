use super::*;
use crate::test_support::test_session;
use std::path::PathBuf;

fn failed(store: &Store, status: SessionStatus, reason: Option<&str>, turns: u32) -> Session {
    let mut s = test_session(Uuid::new_v4(), PathBuf::from("/var/tmp/error-disposition"));
    s.status = status;
    s.stop_reason = reason.map(str::to_string);
    s.claude_session_id = None;
    s.turn_count = Some(turns);
    s.num_turns = Some(turns);
    s.retry_attempt = None;
    s.max_retries = None;
    s.updated_at = Utc::now() - chrono::Duration::hours(48);
    store.insert_session(&s).unwrap();
    s
}

fn apply(store: &Store, s: &Session, work: &WorkEvidence, draining: bool) -> Disposition {
    let now = Utc::now();
    let Some(facts) = store.error_disposition_facts(s.id, now).unwrap() else {
        return Disposition::Hold;
    };
    store
        .apply_error_disposition(&facts, work, 3, 15, draining, now)
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn never_started_is_soft_deleted_with_diagnosis_once() {
    let store = Store::open_in_memory().unwrap();
    let s = failed(
        &store,
        SessionStatus::Failed,
        Some("provider_spawn_failed"),
        0,
    );
    assert_eq!(
        apply(&store, &s, &WorkEvidence::default(), false),
        Disposition::Delete
    );
    assert_eq!(
        store.get_session(s.id).unwrap().unwrap().status,
        SessionStatus::Deleted
    );
    assert_eq!(
        apply(&store, &s, &WorkEvidence::default(), false),
        Disposition::Hold
    );
    let events = store.load_events(s.id).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].metadata.as_ref().unwrap()["error_disposition"]["rule"],
        "Delete"
    );
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT count(*) FROM friction_events WHERE session_id=?1",
                [s.id.to_string()],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn restart_interruption_resumes_once_and_drain_holds_without_spending() {
    let store = Store::open_in_memory().unwrap();
    let s = failed(
        &store,
        SessionStatus::Interrupted,
        Some("daemon_restart_resume:dispatch_failed"),
        5,
    );
    assert_eq!(
        apply(&store, &s, &WorkEvidence::default(), true),
        Disposition::Hold
    );
    assert_eq!(store.list_scheduled_jobs().unwrap().len(), 0);
    assert_eq!(
        apply(&store, &s, &WorkEvidence::default(), false),
        Disposition::Resume
    );
    let jobs = store.list_scheduled_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].wake_mode, WakeMode::Resume);
    assert_eq!(jobs[0].wake_session_id, Some(s.id));
    assert_eq!(
        apply(&store, &s, &WorkEvidence::default(), false),
        Disposition::Hold
    );
    let facts = store
        .error_disposition_facts(s.id, Utc::now())
        .unwrap()
        .unwrap();
    assert_eq!(facts.attempts.len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn final_failure_archives_and_keeps_preservation_ref_in_diagnosis() {
    let store = Store::open_in_memory().unwrap();
    let s = failed(&store, SessionStatus::Failed, Some("model_unsupported"), 23);
    let reference = format!("refs/rsi/preserved/{}", s.id);
    let work = WorkEvidence {
        commits: true,
        preserved_ref: Some(reference.clone()),
        ..Default::default()
    };
    assert_eq!(apply(&store, &s, &work, false), Disposition::Archive);
    assert_eq!(
        store.get_session(s.id).unwrap().unwrap().status,
        SessionStatus::Archived
    );
    assert_eq!(
        store.load_events(s.id).unwrap()[0]
            .metadata
            .as_ref()
            .unwrap()["error_disposition"]["preserved_ref"],
        reference
    );
    assert_eq!(apply(&store, &s, &work, false), Disposition::Hold);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn superseded_non_tip_archives_after_grace() {
    let store = Store::open_in_memory().unwrap();
    let s = failed(&store, SessionStatus::Interrupted, None, 0);
    let mut successor = test_session(Uuid::new_v4(), s.working_dir.clone());
    successor.continued_from = Some(s.id);
    store.insert_session(&successor).unwrap();
    assert_eq!(
        apply(&store, &s, &WorkEvidence::default(), false),
        Disposition::Superseded
    );
    assert_eq!(
        store.get_session(s.id).unwrap().unwrap().status,
        SessionStatus::Archived
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn human_gates_eval_retry_and_containers_remain_with_their_owners() {
    let store = Store::open_in_memory().unwrap();
    for mode in ["eval", "retry", "pause", "question", "container"] {
        let mut s = failed(&store, SessionStatus::Failed, Some("model_unsupported"), 0);
        match mode {
            "eval" => store
                .conn
                .execute(
                    "UPDATE sessions SET is_eval=1 WHERE id=?1",
                    [s.id.to_string()],
                )
                .unwrap(),
            "retry" => store
                .conn
                .execute(
                    "UPDATE sessions SET max_retries=3,retry_attempt=0 WHERE id=?1",
                    [s.id.to_string()],
                )
                .unwrap(),
            "pause" => {
                store
                    .set_daemon_setting(&format!("manager_operator_pause:{}", s.id), "true")
                    .unwrap();
                0
            }
            "question" => {
                store
                    .update_session_pending_question_json(s.id, Some(r#"{"questions":[]}"#))
                    .unwrap();
                0
            }
            "container" => {
                s.session_kind = rsi_common::types::SessionKind::Epic;
                store
                    .conn
                    .execute(
                        "UPDATE sessions SET session_kind='Epic' WHERE id=?1",
                        [s.id.to_string()],
                    )
                    .unwrap()
            }
            _ => unreachable!(),
        };
        assert_eq!(
            apply(&store, &s, &WorkEvidence::default(), false),
            Disposition::Hold,
            "{mode}"
        );
        assert_eq!(
            store.get_session(s.id).unwrap().unwrap().status,
            SessionStatus::Failed
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn unclassified_produces_one_manager_notice_and_protected_seats_survive() {
    use crate::store::manager_coordinator::tests::fixture;
    use rsi_common::harness_manager_v2::ManagerPolicyV2;
    let store = Store::open_in_memory().unwrap();
    let (config, lead) = fixture(&store, ManagerPolicyV2::default());
    let mut s = failed(&store, SessionStatus::Interrupted, None, 3);
    s.project_id = Some(config.project_id);
    store
        .conn
        .execute(
            "UPDATE sessions SET project_id=?2 WHERE id=?1",
            params![s.id.to_string(), config.project_id.to_string()],
        )
        .unwrap();
    assert_eq!(
        apply(&store, &s, &WorkEvidence::default(), false),
        Disposition::Notice
    );
    assert_eq!(
        apply(&store, &s, &WorkEvidence::default(), false),
        Disposition::Hold
    );
    let notices: i64 = store
        .conn
        .query_row(
            "SELECT count(*) FROM harness_manager_notices WHERE subject_id=?1",
            [format!("error_disposition:{}", s.id)],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(notices, 1);
    assert_eq!(
        store.get_session(s.id).unwrap().unwrap().status,
        SessionStatus::Interrupted
    );
    // The shared manager fixture starts with an open retry budget. Exhaust it
    // so this assertion specifically exercises protected seats, not retry ownership.
    for id in [lead.id, config.current_session_id.unwrap()] {
        store
            .conn
            .execute(
                "UPDATE sessions SET status='Failed',stop_reason='model_unsupported',
                    max_retries=0,retry_attempt=0 WHERE id=?1",
                [id.to_string()],
            )
            .unwrap();
        let facts = store
            .error_disposition_facts(id, Utc::now())
            .unwrap()
            .unwrap();
        assert!(facts.protected);
        assert_eq!(
            facts.classify(&WorkEvidence::default(), 3, Utc::now()),
            Disposition::Notice
        );
        assert_eq!(
            store.get_session(id).unwrap().unwrap().status,
            SessionStatus::Failed
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn budget_spent_settled_and_landed_work_archive_instead_of_resuming() {
    let store = Store::open_in_memory().unwrap();
    let s = failed(
        &store,
        SessionStatus::Interrupted,
        Some("daemon_restart_resume:dispatch_failed"),
        4,
    );
    let now = Utc::now();
    let mut facts = store.error_disposition_facts(s.id, now).unwrap().unwrap();
    facts.attempts = vec![now; 3];
    assert_eq!(
        facts.classify(&WorkEvidence::default(), 3, now),
        Disposition::Archive
    );
    facts.attempts.clear();
    facts.settled = true;
    assert_eq!(
        facts.classify(&WorkEvidence::default(), 3, now),
        Disposition::Archive
    );
    facts.settled = false;
    assert_eq!(
        facts.classify(
            &WorkEvidence {
                commits: true,
                landed: true,
                ..Default::default()
            },
            3,
            now
        ),
        Disposition::Archive
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn page_is_bounded_and_uses_cursor_and_grace() {
    let store = Store::open_in_memory().unwrap();
    for _ in 0..PAGE_SIZE + 3 {
        failed(&store, SessionStatus::Failed, None, 0);
    }
    let now = Utc::now();
    let page = store
        .error_disposition_page(None, now - chrono::Duration::minutes(15))
        .unwrap();
    assert_eq!(page.len(), PAGE_SIZE);
    let tail = store
        .error_disposition_page(page.last().copied(), now)
        .unwrap();
    assert_eq!(tail.len(), 3);
    assert_eq!(
        store
            .error_disposition_page(None, now - chrono::Duration::days(3))
            .unwrap()
            .len(),
        0
    );
}
