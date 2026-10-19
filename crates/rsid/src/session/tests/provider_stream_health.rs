//! #2756: the monitor turns a Claude `system/init` without a connected
//! `rsi-agent` server, and `system/api_retry` categories, into visible rows.
use super::*;
use rsi_common::types::PROVIDER_HEALTH_METADATA_KEY;

/// Run a fake Claude provider that streams `events` then reports success, with
/// or without an authority token, and return the persisted health kinds.
async fn health_kinds(with_token: bool, events: Vec<StreamEvent>) -> Vec<String> {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Running;
    session.provider = SessionProvider::Claude;
    insert_row(&manager, &session).await;
    if with_token {
        manager
            .register_agent_token(
                "a70bf50e531ce1a817561f2f5d5b6645d4e806becf58ccc5e8cf6b8045a090a8".to_string(),
                session_id,
            )
            .await;
    }
    let (mut tracked, _controls) =
        scripted_process_tracked(session_id, 7, false, 0, false, false, false);
    tracked.session = session;
    let (stop_tx, stop_rx) = mpsc::channel(1);
    tracked.stop_tx = stop_tx;
    manager.active.write().await.insert(session_id, tracked);
    let (provider_tx, provider_rx) = mpsc::channel(events.len() + 2);
    for event in events {
        provider_tx.send(event).await.expect("queue event");
    }
    provider_tx
        .send(StreamEvent {
            event_type: "result".into(),
            data: serde_json::json!({"subtype":"success","terminal_reason":"completed",
                "is_error":false,"result":"done","num_turns":1}),
        })
        .await
        .expect("queue result");
    drop(provider_tx);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
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
        ),
    )
    .await
    .expect("fake provider turn finalizes");
    manager
        .store
        .lock()
        .await
        .load_events(session_id)
        .expect("events")
        .iter()
        .filter_map(|event| {
            event
                .metadata
                .as_ref()?
                .get(PROVIDER_HEALTH_METADATA_KEY)?
                .get("kind")?
                .as_str()
                .map(str::to_string)
        })
        .collect()
}

fn init(status: &str) -> StreamEvent {
    StreamEvent {
        event_type: "system".into(),
        data: serde_json::json!({
            "subtype": "init",
            "model": "claude-opus-5",
            "mcp_servers": [{"name": "rsi-agent", "status": status}],
        }),
    }
}

fn retry(error: &str) -> StreamEvent {
    StreamEvent {
        event_type: "system".into(),
        data: serde_json::json!({"subtype": "api_retry", "attempt": 1, "error": error}),
    }
}

/// #2756 AC1: a tokened worker whose `rsi-agent` server failed gets the typed
/// session row within the first turn; a connected server adds nothing.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_agent_server_raises_a_session_event_and_connected_adds_nothing() {
    assert_eq!(
        health_kinds(true, vec![init("failed")]).await,
        vec!["agent_mcp_unavailable".to_string()]
    );
    assert!(
        health_kinds(true, vec![init("connected")]).await.is_empty(),
        "a connected server adds nothing"
    );
    assert!(
        health_kinds(false, vec![init("failed")]).await.is_empty(),
        "a session launched without the server is not judged on it"
    );
}

/// #2756 AC3: retry categories become typed rows, an unknown one is counted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_retry_categories_become_typed_rows() {
    assert_eq!(
        health_kinds(
            false,
            vec![retry("rate_limit"), retry("overloaded"), retry("brand_new")]
        )
        .await,
        vec!["api_retry".to_string(); 3]
    );
}
