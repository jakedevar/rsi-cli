//! Issue #1861: a manager seat that ends its turn asking in chat is recorded
//! and given one safety wake; a seat that reported up or holds a wake is not.
use super::*;
use crate::session::manager_question_stall::{
    STALL_CAPPED_CODE, STALL_CODE, asks_for_decision, guard_completed_manager, turn_ends_asking,
};
use crate::store::manager_question_stall::{
    MANAGER_QUESTION_NUDGE_CAP, MANAGER_QUESTION_WAKE_NAME_PREFIX, ManagerQuestionArm,
};
use rsi_common::types::{ConversationEvent, EventType, Project, Role};

fn event(kind: EventType, role: Option<Role>, content: &str) -> ConversationEvent {
    let mut event = SessionManager::create_user_event(Uuid::new_v4(), 1, content);
    event.event_type = kind;
    event.role = role;
    event
}

fn assistant(text: &str) -> ConversationEvent {
    event(EventType::Message, Some(Role::Assistant), text)
}

fn user(text: &str) -> ConversationEvent {
    event(EventType::Message, Some(Role::User), text)
}

fn tool(name: &str) -> ConversationEvent {
    let mut call = event(EventType::ToolUse, None, "");
    call.tool_name = Some(name.to_string());
    call
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn a_closing_question_or_request_phrase_asks_for_a_decision() {
    assert!(asks_for_decision(
        "Four workers were interrupted.\n\nTell me how you want to proceed"
    ));
    assert!(asks_for_decision(
        "Done with the sweep. Should I re-launch them?"
    ));
    assert!(asks_for_decision("**Your call.**\n\nFriction: none"));
    assert!(!asks_for_decision("Landed #12.\n\nFriction: none"));
    assert!(!asks_for_decision(
        "Waiting for the job; the wake is armed."
    ));
    assert!(!asks_for_decision("Friction: why did X fail?"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn only_a_turn_without_a_report_up_ends_asking() {
    let asking = vec![user("go"), assistant("Which should I pick?")];
    assert!(turn_ends_asking(&asking));

    let reported = vec![
        user("go"),
        tool("mcp__rsi-agent__rsi_control_report_up"),
        assistant("Which should I pick?"),
    ];
    assert!(!turn_ends_asking(&reported));

    // A report-up from an earlier turn does not excuse this one.
    let earlier = vec![
        tool("AgentReportUp"),
        user("go"),
        assistant("Which should I pick?"),
    ];
    assert!(turn_ends_asking(&earlier));

    let settled = vec![user("go"), assistant("Done. Wake armed.")];
    assert!(!turn_ends_asking(&settled));
}

struct Seat {
    manager: SessionManager,
    _dir: TempDir,
    session: Session,
}

async fn seat(appointed: bool) -> Seat {
    let (manager, dir) = manager();
    let project = Uuid::new_v4();
    let now = chrono::Utc::now();
    let mut session = bare_session(Uuid::new_v4());
    session.project_id = Some(project);
    session.status = SessionStatus::Completed;
    {
        let store = manager.store.lock().await;
        store
            .insert_project(&Project {
                id: project,
                name: format!("stall {project}"),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        store.insert_session(&session).unwrap();
        if appointed {
            store
                .configure_harness_manager(
                    &rsi_common::harness_manager::ConfigureHarnessManagerRequestV1 {
                        project_id: project,
                        session_id: session.id,
                        epic_ids: None,
                        group_ids: vec![],
                        expected_row_version: 0,
                    },
                )
                .unwrap();
        }
    }
    Seat {
        manager,
        _dir: dir,
        session,
    }
}

async fn armed_wakes(seat: &Seat) -> usize {
    seat.manager
        .store
        .lock()
        .await
        .list_owned_scheduled_jobs(seat.session.id, false, 50)
        .unwrap()
        .into_iter()
        .filter(|(job, _)| job.name.starts_with(MANAGER_QUESTION_WAKE_NAME_PREFIX))
        .count()
}

async fn friction_codes(seat: &Seat) -> Vec<String> {
    seat.manager
        .store
        .lock()
        .await
        .friction_rollup(
            &rsi_common::friction::ListFrictionRollupRequestV1::default(),
            chrono::Utc::now(),
        )
        .unwrap()
        .rows
        .into_iter()
        .map(|row| row.signature)
        .collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn an_appointed_seat_asking_in_chat_gets_one_safety_wake_and_friction() {
    let seat = seat(true).await;
    let events = vec![user("go"), assistant("Tell me how you want to proceed")];
    guard_completed_manager(&seat.manager.store, &seat.session, &events).await;
    assert_eq!(armed_wakes(&seat).await, 1);
    assert!(
        friction_codes(&seat)
            .await
            .contains(&format!("terminal:{STALL_CODE}"))
    );
    // A second ending while the wake is armed holds a resume wake: no second one.
    guard_completed_manager(&seat.manager.store, &seat.session, &events).await;
    assert_eq!(armed_wakes(&seat).await, 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn a_seat_that_reported_up_or_is_not_a_seat_is_left_alone() {
    let events = vec![user("go"), assistant("Which should I pick?")];
    let reported = vec![
        user("go"),
        tool("AgentReportUp"),
        assistant("Which should I pick?"),
    ];

    let appointed = seat(true).await;
    guard_completed_manager(&appointed.manager.store, &appointed.session, &reported).await;
    assert_eq!(armed_wakes(&appointed).await, 0);

    let worker = seat(false).await;
    guard_completed_manager(&worker.manager.store, &worker.session, &events).await;
    assert_eq!(armed_wakes(&worker).await, 0);
    assert!(friction_codes(&worker).await.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn the_nudge_budget_is_capped_then_only_friction_is_recorded() {
    let seat = seat(true).await;
    let store = seat.manager.store.lock().await;
    for _ in 0..MANAGER_QUESTION_NUDGE_CAP {
        assert_eq!(
            store.arm_manager_question_wake(&seat.session).unwrap(),
            ManagerQuestionArm::Armed
        );
        // The wake fired and was delivered: the marker is in the transcript.
        store
            .conn
            .execute("UPDATE scheduled_jobs SET enabled=0", [])
            .unwrap();
        let mut delivered = SessionManager::create_user_event(
            seat.session.id,
            1,
            crate::store::manager_question_stall::MANAGER_QUESTION_MARKER,
        );
        delivered.role = Some(Role::User);
        store.insert_event(&delivered).unwrap();
    }
    assert_eq!(
        store.arm_manager_question_wake(&seat.session).unwrap(),
        ManagerQuestionArm::CapReached
    );
    drop(store);
    let events = vec![user("go"), assistant("Which should I pick?")];
    guard_completed_manager(&seat.manager.store, &seat.session, &events).await;
    assert!(
        friction_codes(&seat)
            .await
            .contains(&format!("terminal:{STALL_CAPPED_CODE}"))
    );
}
