use super::*;
use crate::store::provider_turn_custody::{NewProviderTurnCustody, ProviderTurnCursor};

struct ReplacedGenerationProvider {
    active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    session_id: Uuid,
    replaced: bool,
}

#[async_trait::async_trait]
impl ProviderSession for ReplacedGenerationProvider {
    async fn next_event(&mut self) -> Option<StreamEvent> {
        if self.replaced {
            return std::future::pending().await;
        }
        self.replaced = true;
        let mut active = self.active.write().await;
        let tracked = active.get_mut(&self.session_id).unwrap();
        tracked.spawn_generation += 1;
        tracked.rotation.state = RotationState::PendingInterrupt {
            deadline: tokio::time::Instant::now(),
        };
        Some(StreamEvent {
            event_type: "fixture_generation_replaced".into(),
            data: serde_json::json!({}),
        })
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn stale_rotation_deadline_settles_custody_without_finalizing_new_generation() {
    use crate::claude::{ClaudeProcess, turn_spool::DetachedTurn};
    use crate::store::provider_turn_custody::ProviderTurnCustodyState;
    let dir = crate::test_support::disk_backed_tempdir("stale-turn-custody-");
    let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
    let manager = SessionManager::new(
        Arc::new(crate::bus::EventBus::new(16)),
        Store::open(&dir.path().join("rsi.db")).unwrap(),
        false,
        dir.path().join("daemon.sock"),
        None,
        Vec::new(),
        runtime,
        dir.path().join("sandboxes"),
    )
    .unwrap();
    let mut tracked = super::live_context_state_tests::build_tracked(
        SessionProvider::Claude,
        0,
        0,
        0,
        0,
        ContextUsageConfidence::Missing,
        None,
    );
    tracked.spawn_generation = 7;
    let session_id = tracked.session.id;
    let invocation = Uuid::new_v4();
    {
        let guard = manager.store.lock().await;
        guard.insert_session(&tracked.session).unwrap();
        guard.conn.execute("INSERT INTO model_invocations
            (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at)
            VALUES(?1,'session.launch','session','foreground','paid','admitted','running','launch_session',?2,?3)",
            rusqlite::params![invocation.to_string(), session_id.to_string(), chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)],
        ).unwrap();
        guard
            .set_session_model_invocation(session_id, Some(invocation))
            .unwrap();
    }
    let spool = dir.path().join("spool");
    std::fs::create_dir(&spool).unwrap();
    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    std::fs::write(
        spool.join("shim.json"),
        serde_json::json!({"pid": pid}).to_string(),
    )
    .unwrap();
    let turn = DetachedTurn::new(invocation, spool);
    tracked.process = Some(super::super::types::ProviderProcess::Claude(
        ClaudeProcess::detached_for_test(child, turn),
    ));
    let (stop_tx, stop_rx) = mpsc::channel(1);
    tracked.stop_tx = stop_tx;
    manager.active.write().await.insert(session_id, tracked);
    let monitored = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        SessionManager::monitor_session(
            session_id,
            7,
            Box::new(ReplacedGenerationProvider {
                active: manager.active.clone(),
                session_id,
                replaced: false,
            }),
            manager.active.clone(),
            manager.completed.clone(),
            manager.event_bus.clone(),
            stop_rx,
            manager.store.clone(),
            manager.model_call_settlements.handle().unwrap(),
            manager.persistence.clone(),
            0,
            false,
            manager.socket_path.clone(),
            manager.token_counter.clone(),
            None,
            manager.retry_tx.clone(),
            manager.tool_registry.clone(),
            crate::turn_controller::TurnController::new(
                crate::turn_controller::ContinuationPolicy::Single,
            ),
            manager.runtime_config.clone(),
            manager.spawn_coordinator.clone(),
            manager.agent_tokens.clone(),
            manager.spawn_epoch.clone(),
            manager.agent_message_arbiter.clone(),
            manager.codegraph_handle.clone(),
            manager.custody_execution_runtime(),
        ),
    )
    .await;
    // Always reap the fixture, including when the monitor times out.
    if let Some(super::super::types::ProviderProcess::Claude(process)) = manager
        .active
        .write()
        .await
        .get_mut(&session_id)
        .and_then(|t| t.process.as_mut())
    {
        process.kill().await.unwrap();
    }
    monitored.expect("stale deadline exits the monitor");
    let row = manager
        .store
        .lock()
        .await
        .get_provider_turn_custody(invocation)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ProviderTurnCustodyState::Abandoned);
    let active = manager.active.read().await;
    let newer = active
        .get(&session_id)
        .expect("new generation stays active");
    assert_eq!(newer.spawn_generation, 8);
    assert_eq!(newer.session.status, SessionStatus::Running);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn spool_content_blocks_persist_once_with_exact_question_and_newline_cursor() {
    let mut tracked = super::live_context_state_tests::build_tracked(
        SessionProvider::Claude,
        0,
        0,
        0,
        0,
        ContextUsageConfidence::Missing,
        None,
    );
    tracked.spawn_generation = 7;
    let session = tracked.session.id;
    let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
    let invocation = Uuid::new_v4();
    let boot = Uuid::new_v4();
    {
        let guard = store.lock().await;
        guard.insert_session(&tracked.session).unwrap();
        guard.conn.execute("INSERT INTO model_invocations
            (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,trigger_source,session_id,created_at)
            VALUES(?1,'session.launch','session','foreground','paid','admitted','running','launch_session',?2,?3)",
            rusqlite::params![invocation.to_string(), session.to_string(), chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)],
        ).unwrap();
        guard
            .insert_provider_turn_custody(&NewProviderTurnCustody {
                invocation_id: invocation,
                session_id: session,
                spool_dir: std::env::temp_dir().join(invocation.to_string()),
                pid: 12345,
                start_time: None,
                boot_id: boot,
            })
            .unwrap();
    }
    let active = Arc::new(RwLock::new(HashMap::from([(session, tracked)])));
    let persistence = PersistenceHandle::new(store.clone());
    let stream = StreamEvent {
        event_type: "assistant".into(),
        data: serde_json::json!({"message":{"content":[
            {"type":"text","text":"first block"},
            {"type":"thinking","thinking":"reasoning block"},
            {"type":"tool_use","id":"ask-spool","name":"AskUserQuestion","input":{"questions":[
                {"question":"Choose a route", "header":"Route", "options":[], "multiSelect":false}
            ]}}
        ]}}),
    };
    let mut sequence = 0;
    let mut events =
        SessionManager::convert_recognized_stream_event(&stream, session, &mut sequence).unwrap();
    assert_eq!(events.len(), 3);
    let cursor = ProviderTurnCursor {
        invocation_id: invocation,
        boot_id: boot,
        expected_offset: 0,
        next_offset: 512,
    };
    persist_detached_batch(&active, &store, session, 7, cursor, &stream, &mut events)
        .await
        .unwrap();
    assert!(events.iter().all(|event| event.id > 0));
    for event in &events {
        let (_, id) =
            persist_tracked_provider_event(&active, &persistence, session, 7, event, None).await;
        assert_eq!(id.unwrap(), event.id);
    }
    let guard = store.lock().await;
    assert_eq!(guard.load_events(session).unwrap().len(), events.len());
    assert_eq!(
        guard
            .get_provider_turn_custody(invocation)
            .unwrap()
            .unwrap()
            .stdout_offset,
        512
    );
    assert_eq!(
        guard.manager_v2_question_target(session).unwrap().unwrap()["tool_use_id"],
        "ask-spool"
    );
    drop(guard);
    assert!(
        persist_detached_batch(&active, &store, session, 7, cursor, &stream, &mut events)
            .await
            .is_err()
    );
    assert_eq!(store.lock().await.load_events(session).unwrap().len(), 3);
    let next = ProviderTurnCursor {
        expected_offset: 512,
        next_offset: 1024,
        ..cursor
    };
    assert!(
        persist_detached_batch(&active, &store, session, 8, next, &stream, &mut [])
            .await
            .is_err()
    );
    assert_eq!(
        store
            .lock()
            .await
            .get_provider_turn_custody(invocation)
            .unwrap()
            .unwrap()
            .stdout_offset,
        512
    );
    let rejection = StreamEvent {
        event_type: "user".into(),
        data: serde_json::json!({"message":{"content":[
            {"type":"tool_result","tool_use_id":"ask-spool","content":"headless rejection","is_error":true}
        ]}}),
    };
    let mut rejected =
        SessionManager::convert_recognized_stream_event(&rejection, session, &mut sequence)
            .unwrap();
    assert_eq!(rejected.len(), 1);
    persist_detached_batch(&active, &store, session, 7, next, &rejection, &mut rejected)
        .await
        .unwrap();
    assert_eq!(
        rejected[0].metadata.as_ref().unwrap()["rsi_code"],
        super::super::question::UNDELIVERABLE_CODE
    );
    let (_, id) =
        persist_tracked_provider_event(&active, &persistence, session, 7, &rejected[0], None).await;
    assert_eq!(id.unwrap(), rejected[0].id);
    let stored = store.lock().await.load_events(session).unwrap();
    assert_eq!(stored.len(), 4);
    assert_eq!(
        stored.last().unwrap().metadata.as_ref().unwrap()["question_pending"],
        true
    );
}
