//! Hub #3046 / laptop #357: monitor-path acceptance for the #3029 fix. A real
//! `monitor_session` reads a provider stream; the provider child has exited 0
//! (scripted process) and the reconciler, which is the writer that notices the
//! dead child, stops the monitor through `signal_finalization` exactly as in
//! production. The persisted session row is the assertion; no
//! `TerminalEvidence` or `TerminalFinalizeDecision` is built by hand.
use super::*;

/// Stream `events`, wait until the monitor has persisted `last_text` (so the
/// events are consumed before the stop), then let the reconciler signal the
/// exited provider while the stream is still open. Returns the stored row.
async fn settle_after_clean_exit(events: Vec<StreamEvent>, last_text: &str) -> Session {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Running;
    session.provider = SessionProvider::Claude;
    session.stop_reason = None;
    session.terminal_reason = None;
    insert_row(&manager, &session).await;

    // The provider child already exited with code 0.
    let (mut tracked, _controls) =
        scripted_process_tracked(session_id, 7, false, 0, false, false, false);
    tracked.session = session;
    let (stop_tx, stop_rx) = mpsc::channel(1);
    tracked.stop_tx = stop_tx;
    manager.active.write().await.insert(session_id, tracked);

    let (provider_tx, provider_rx) = mpsc::channel(events.len() + 1);
    for event in events {
        provider_tx.send(event).await.expect("queue event");
    }
    let manager = std::sync::Arc::new(manager);
    let monitor = tokio::spawn({
        let manager = std::sync::Arc::clone(&manager);
        async move {
            SessionManager::monitor_session(
                session_id,
                7,
                Box::new(crate::provider::CliProviderSession::new(provider_rx)),
                std::sync::Arc::clone(&manager.active),
                std::sync::Arc::clone(&manager.completed),
                std::sync::Arc::clone(&manager.event_bus),
                stop_rx,
                std::sync::Arc::clone(&manager.store),
                manager.model_call_settlements.handle().expect("settlement"),
                manager.persistence.clone(),
                0,
                false,
                manager.socket_path.clone(),
                std::sync::Arc::clone(&manager.token_counter),
                None,
                manager.retry_tx.clone(),
                std::sync::Arc::clone(&manager.tool_registry),
                crate::turn_controller::TurnController::new(
                    crate::turn_controller::ContinuationPolicy::Single,
                ),
                std::sync::Arc::clone(&manager.runtime_config),
                std::sync::Arc::clone(&manager.spawn_coordinator),
                std::sync::Arc::clone(&manager.agent_tokens),
                std::sync::Arc::clone(&manager.spawn_epoch),
                std::sync::Arc::clone(&manager.agent_message_arbiter),
                manager.codegraph_handle.clone(),
                manager.custody_execution_runtime(),
            )
            .await
        }
    });

    // The monitor has consumed the stream once the last message is stored.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let seen = manager
                .store
                .lock()
                .await
                .load_events(session_id)
                .expect("events")
                .iter()
                .any(|event| event.content.contains(last_text));
            if seen {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("monitor persists the streamed events");
    // A result event stores no row of its own, so give the monitor time to read
    // it after the last stored row. This stays well inside the 2 s
    // `TERMINAL_DRAIN_GRACE`, the window in which the reconciler's stop lands.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    crate::reconciliation::signal_finalization(&manager.active, session_id, &manager.event_bus)
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(10), monitor)
        .await
        .expect("monitor finalizes after the reconciler signal")
        .expect("monitor task");
    drop(provider_tx);
    manager
        .store
        .lock()
        .await
        .get_session(session_id)
        .expect("read session")
        .expect("session row")
}

fn assistant(text: &str) -> StreamEvent {
    StreamEvent {
        event_type: "assistant".into(),
        data: serde_json::json!({"role": "assistant", "content": text}),
    }
}

fn result_success(text: &str) -> StreamEvent {
    StreamEvent {
        event_type: "result".into(),
        data: serde_json::json!({"subtype":"success","terminal_reason":"completed",
            "is_error":false,"result":text,"num_turns":1}),
    }
}

/// #3029 AC: a final successful result event, then the provider exits 0 and the
/// reconciler stops the monitor, settles the session Completed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_result_then_exit_zero_through_the_monitor_persists_completed() {
    let row = settle_after_clean_exit(
        vec![assistant("working 3029 turn"), result_success("all done")],
        "working 3029 turn",
    )
    .await;
    assert_eq!(row.status, SessionStatus::Completed);
    assert_eq!(row.stop_reason.as_deref(), Some("success"));
}

/// #3029 keeps: the provider exits 0 but the stream never produced a final
/// result event, so the turn was cut off and the session stays Interrupted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_zero_without_a_final_result_through_the_monitor_persists_interrupted() {
    let row =
        settle_after_clean_exit(vec![assistant("partial 3029 turn")], "partial 3029 turn").await;
    assert_eq!(row.status, SessionStatus::Interrupted);
    assert_eq!(
        row.stop_reason.as_deref(),
        Some("interrupted:provider_exit:exit_code=0")
    );
}
