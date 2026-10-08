use super::*;
use crate::{
    bus::EventBus,
    config::{Config, RuntimeConfig},
    session::{launch, types::CompletedSession},
    store::Store,
};
use rsi_common::{
    remote_pending_decisions::*,
    remote_read::{Text, WireUuid},
    types::*,
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use uuid::Uuid;

async fn fixture() -> (SessionManager, tempfile::TempDir, AnswerPendingDecisionV1) {
    let dir = crate::test_support::disk_backed_tempdir("remote-answer-delivery");
    let store = Store::open(&dir.path().join("answers.db")).unwrap();
    let stamp = chrono::Utc::now();
    let project = Project {
        id: Uuid::new_v4(),
        name: "Remote delivery".into(),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: stamp,
        updated_at: stamp,
    };
    store.insert_project(&project).unwrap();
    let mut session =
        crate::session::agent_verbs::tests::test_session(Uuid::new_v4(), dir.path().to_owned());
    session.provider = SessionProvider::Claude;
    session.status = SessionStatus::WaitingApproval;
    session.project_id = Some(project.id);
    session.claude_session_id = Some(Uuid::new_v4().to_string());
    store.insert_session(&session).unwrap();
    let target = publish(&store, session.id, 1);
    let request = request(project.id, session.id, &target);
    let manager = SessionManager::new(
        Arc::new(EventBus::new(64)),
        store,
        false,
        dir.path().join("unused.sock"),
        None,
        vec![],
        RuntimeConfig::from_config(&Config::from_env()),
        dir.path().join("sandboxes"),
    )
    .unwrap();
    let store = manager.store.lock().await;
    let mut cached = CompletedSession::for_test(store.get_session(session.id).unwrap().unwrap());
    cached.events = store.load_events(session.id).unwrap();
    cached.events_hydrated = true;
    drop(store);
    manager.completed.write().await.insert(session.id, cached);
    (manager, dir, request)
}

fn publish(store: &Store, session: Uuid, sequence: i32) -> serde_json::Value {
    let question = PendingQuestion {
        questions: vec![QuestionItem {
            question: "Which option?".into(),
            header: "Choice".into(),
            options: vec![],
            multi_select: false,
        }],
    };
    store
        .publish_pending_question_event(
            &ConversationEvent {
                id: 0,
                session_id: session,
                sequence,
                event_type: EventType::ToolUse,
                role: None,
                created_at: chrono::Utc::now(),
                content: String::new(),
                tool_name: Some("AskUserQuestion".into()),
                tool_input: Some(Box::new(json!(question))),
                offload_id: None,
                tool_use_id: Some(format!("ask-{sequence}")),
                metadata: None,
            },
            None,
            &question,
        )
        .unwrap();
    store.manager_v2_question_target(session).unwrap().unwrap()
}

fn request(project: Uuid, session: Uuid, target: &serde_json::Value) -> AnswerPendingDecisionV1 {
    AnswerPendingDecisionV1 {
        project_id: WireUuid::new(project.to_string()).unwrap(),
        session_id: WireUuid::new(session.to_string()).unwrap(),
        decision_id: Text::new(format!(
            "pending-question:{}",
            target["publication_id"].as_str().unwrap()
        ))
        .unwrap(),
        expected_target_digest: Text::new(
            crate::store::harness_manager_v2::fingerprint(target).unwrap(),
        )
        .unwrap(),
        answer: Text::new("Use the safe option".into()).unwrap(),
        idempotency_key: WireUuid::new(Uuid::new_v4().to_string()).unwrap(),
        origin: RemoteAnswerOriginV1 {
            kind: RemoteAnswerOriginKindV1::Remote,
            client_node: Text::new("test-device".into()).unwrap(),
            gateway_epoch: WireUuid::new(Uuid::new_v4().to_string()).unwrap(),
        },
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn remote_question_pre_effect_restart_retries_once_and_replays_receipt() {
    let (manager, _dir, request) = fixture().await;
    let session = Uuid::parse_str(request.session_id.as_str()).unwrap();
    {
        let store = manager.store.lock().await;
        store.prepare_remote_answer(&request).unwrap();
        store.claim_remote_answer(Uuid::new_v4()).unwrap().unwrap();
        assert_eq!(
            store
                .recover_remote_answers(manager.program_run_boot_id)
                .unwrap(),
            1
        );
    }
    let process = launch::install_controller_candidate_test_process(session);
    tokio::time::timeout(
        Duration::from_secs(30),
        manager.reconcile_harness_managers_once(),
    )
    .await
    .unwrap()
    .unwrap();
    let store = manager.store.lock().await;
    let receipt = store.prepare_remote_answer(&request).unwrap();
    assert_eq!(receipt.state, State::Succeeded);
    assert_eq!(store.manager_v2_question_target(session).unwrap(), None);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    drop(store);
    assert_eq!(manager.reconcile_harness_managers_once().await.unwrap(), 0);
    launch::drop_controller_candidate_test_stream(session);
}

async fn race(terminal_first: bool) {
    let (manager, _dir, request) = fixture().await;
    let session = Uuid::parse_str(request.session_id.as_str()).unwrap();
    manager
        .store
        .lock()
        .await
        .prepare_remote_answer(&request)
        .unwrap();
    let process = launch::install_controller_candidate_test_process(session);
    let (reached, release) = launch::install_controller_candidate_test_pause(
        session,
        launch::ControllerCandidateTestPhase::ManagerQuestionBeforeClear,
    );
    let (winner, loser) = tokio::time::timeout(Duration::from_secs(30), async {
        if terminal_first {
            tokio::join!(
                manager.answer_question(session, "terminal answer".into()),
                async {
                    reached.await.unwrap();
                    release.send(()).unwrap();
                    manager.reconcile_harness_managers_once().await.map(|_| ())
                }
            )
        } else {
            let (winner, loser) = tokio::join!(manager.reconcile_harness_managers_once(), async {
                reached.await.unwrap();
                release.send(()).unwrap();
                manager
                    .answer_question(session, "terminal answer".into())
                    .await
            });
            (winner.map(|_| ()), loser)
        }
    })
    .await
    .unwrap();
    winner.unwrap();
    if terminal_first {
        loser.unwrap();
    } else {
        assert!(
            loser
                .unwrap_err()
                .to_string()
                .contains("question_already_answered")
        );
    }
    let receipt = manager
        .store
        .lock()
        .await
        .prepare_remote_answer(&request)
        .unwrap();
    assert_eq!(
        receipt.state,
        if terminal_first {
            State::Refused
        } else {
            State::Succeeded
        }
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    launch::drop_controller_candidate_test_stream(session);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn remote_question_terminal_first_race_delivers_one_answer() {
    race(true).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn remote_question_phone_first_race_delivers_one_answer() {
    race(false).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn remote_question_restart_after_effect_never_resends() {
    let (manager, _dir, request) = fixture().await;
    let session = Uuid::parse_str(request.session_id.as_str()).unwrap();
    let store = manager.store.lock().await;
    store.prepare_remote_answer(&request).unwrap();
    let delivery = store.claim_remote_answer(Uuid::new_v4()).unwrap().unwrap();
    store
        .set_remote_answer_delivery(&delivery, State::Running, true, None)
        .unwrap();
    drop(store);
    let process = launch::install_controller_candidate_test_process(session);
    manager.reconcile_harness_managers_once().await.unwrap();
    assert_eq!(
        manager
            .store
            .lock()
            .await
            .prepare_remote_answer(&request)
            .unwrap()
            .state,
        State::Uncertain
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    launch::drop_controller_candidate_test_stream(session);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn terminal_answer_unpublished_unresolved_and_stale_questions_have_accurate_refusal() {
    for state in ["unpublished", "unresolved", "stale"] {
        let (manager, _dir, request) = fixture().await;
        let session = Uuid::parse_str(request.session_id.as_str()).unwrap();
        let store = manager.store.lock().await;
        match state {
            "unpublished" => {
                // A legacy snapshot has never been producer-published.
                store.conn.execute("UPDATE pending_question_publications SET state='cleared' WHERE session_id=?1",
                    [session.to_string()]).unwrap();
            }
            "unresolved" => {
                store.conn.execute("UPDATE pending_question_publications SET state='unresolved' WHERE session_id=?1",
                    [session.to_string()]).unwrap();
            }
            _ => {
                let mut event = store.load_events(session).unwrap().remove(0);
                event.id = 0;
                event.sequence += 1;
                event.event_type = EventType::Message;
                event.role = Some(Role::User);
                event.tool_name = None;
                store.insert_event(&event).unwrap();
            }
        }
        drop(store);
        let error = manager
            .answer_question(session, "terminal answer".into())
            .await
            .unwrap_err();
        assert!(
            matches!(error, crate::error::DaemonError::PolicyDenied(ref code) if code == "question_not_published"),
            "{state}: {error:?}"
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn remote_question_transient_runtime_mismatch_retries_then_delivers_once() {
    let (manager, _dir, request) = fixture().await;
    let session = Uuid::parse_str(request.session_id.as_str()).unwrap();
    manager
        .store
        .lock()
        .await
        .prepare_remote_answer(&request)
        .unwrap();
    let mut completed = manager.completed.write().await;
    let events = std::mem::take(&mut completed.get_mut(&session).unwrap().events);
    drop(completed);
    manager.reconcile_remote_answers_once().await.unwrap();
    let receipt = manager
        .store
        .lock()
        .await
        .prepare_remote_answer(&request)
        .unwrap();
    assert_eq!(receipt.state, State::Queued);
    assert_eq!(receipt.outcome.unwrap()["pre_effect_retries"], 1);
    // A daemon restart while claimed must retain the retry count.
    {
        let store = manager.store.lock().await;
        store.claim_remote_answer(Uuid::new_v4()).unwrap().unwrap();
        store
            .recover_remote_answers(manager.program_run_boot_id)
            .unwrap();
        assert_eq!(
            store
                .prepare_remote_answer(&request)
                .unwrap()
                .outcome
                .unwrap()["pre_effect_retries"],
            1
        );
    }
    manager
        .completed
        .write()
        .await
        .get_mut(&session)
        .unwrap()
        .events = events;
    let process = launch::install_controller_candidate_test_process(session);
    manager.reconcile_remote_answers_once().await.unwrap();
    assert_eq!(
        manager
            .store
            .lock()
            .await
            .prepare_remote_answer(&request)
            .unwrap()
            .state,
        State::Succeeded
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    assert_eq!(manager.reconcile_remote_answers_once().await.unwrap(), 0);
    launch::drop_controller_candidate_test_stream(session);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
#[tokio::test]
async fn remote_question_transient_retry_budget_is_bounded_and_permanent_refusal_is_final() {
    for transient in [false, true] {
        let (manager, _dir, request) = fixture().await;
        let session = Uuid::parse_str(request.session_id.as_str()).unwrap();
        manager
            .store
            .lock()
            .await
            .prepare_remote_answer(&request)
            .unwrap();
        if transient {
            manager
                .completed
                .write()
                .await
                .get_mut(&session)
                .unwrap()
                .events
                .clear();
        } else {
            publish(&*manager.store.lock().await, session, 2);
        }
        let attempts = if transient {
            MAX_PRE_EFFECT_RETRIES + 1
        } else {
            1
        };
        for attempt in 0..attempts {
            manager.reconcile_remote_answers_once().await.unwrap();
            let receipt = manager
                .store
                .lock()
                .await
                .prepare_remote_answer(&request)
                .unwrap();
            assert_eq!(
                receipt.state,
                if attempt + 1 == attempts {
                    State::Refused
                } else {
                    State::Queued
                }
            );
        }
        let receipt = manager
            .store
            .lock()
            .await
            .prepare_remote_answer(&request)
            .unwrap();
        assert_eq!(
            receipt.outcome.unwrap()["code"],
            if transient {
                "remote_answer_retry_exhausted"
            } else {
                "decision_changed"
            }
        );
        assert_eq!(manager.reconcile_remote_answers_once().await.unwrap(), 0);
    }
}
