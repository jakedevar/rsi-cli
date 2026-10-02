use rsi_common::types::{
    ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
};
use rsid::bus::EventBus;
use rsid::config::{Config, RuntimeConfig};
use rsid::session::SessionManager;
use rsid::store::Store;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

fn manager() -> (SessionManager, TempDir) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("rsi.db");
    let store = Store::open(&db_path).expect("open store");
    let config = Config::from_env();
    let runtime_config = RuntimeConfig::from_config(&config);
    let manager = SessionManager::new(
        Arc::new(EventBus::new(16)),
        store,
        false,
        dir.path().join("daemon.sock"),
        None,
        Vec::new(),
        runtime_config,
        dir.path().join("sandboxes"),
    )
    .expect("manager");
    (manager, dir)
}

fn bare_session(id: Uuid) -> Session {
    let now = chrono::Utc::now();
    Session {
        context_fill_pct: None,
        id,
        status: SessionStatus::Completed,
        session_kind: SessionKind::Standard,
        provider: SessionProvider::default(),
        context_usage_confidence: ContextUsageConfidence::default(),
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        created_at: now,
        updated_at: now,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        query: "test".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        working_dir: std::path::PathBuf::from("/tmp"),
        git_branch: None,
        model: None,
        claude_session_id: None,
        project_id: None,
        continued_from: None,
        tag: String::new(),
        tags: Vec::new(),
        parent_id: None,
        lead_session_id: None,
        is_eval: false,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        scheduled_job_id: None,
        stop_reason: None,
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_prompt_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
        workflow_id: None,
        workflow_id_override: None,
        pipeline_artifact: None,
        pending_question: None,
        pending_archive: false,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        rating: None,
        harness_version_hash: None,
        test_passed: None,
        clippy_passed: None,
        turn_count: None,
        retry_count: None,
        approval_wait_ms: Some(0),
        work_time_ms: None,
        approval_started_at: None,
        sandbox_kind: None,
        sandbox_root: None,
        sandbox_branch: None,
        sandbox_cleanup_state: None,
        capability_class: None,
        topology_node_id: None,
        topology_iteration: 0,
        provider_cli_version: None,
        provider_capabilities: Vec::new(),
        thinking_tokens: None,
        service_tier: None,
        cache_creation_1h_tokens: None,
        cache_creation_5m_tokens: None,
        permission_denial_count: None,
        subagent_stats_json: None,
        queued_turn_count: None,
        terminal_reason: None,
    }
}

#[tokio::test]
async fn health_keeps_restart_evidence_visible_while_store_is_busy() {
    let directory = TempDir::new().unwrap();
    let store = Store::open(&directory.path().join("rsi.db")).unwrap();
    let observed_at = chrono::Utc::now();
    let restart = rsid::watchdog::RestartRecord {
        version: 1,
        id: Uuid::new_v4(),
        observed_at,
        last_healthy_at: observed_at - chrono::Duration::seconds(30),
        failed_probes: vec!["store_probe_timeout".to_owned()],
    };
    store.persist_daemon_restart_record(&restart).unwrap();
    let runtime_config = RuntimeConfig::from_config(&Config::from_env());
    let manager = SessionManager::new(
        Arc::new(EventBus::new(16)),
        store,
        false,
        directory.path().join("daemon.sock"),
        None,
        Vec::new(),
        runtime_config,
        directory.path().join("sandboxes"),
    )
    .unwrap();

    let _busy_store = manager.store().lock().await;
    let health = manager.get_health_status().await;
    let visible = health.latest_daemon_restart.unwrap();
    assert_eq!(visible.id, restart.id);
    assert_eq!(visible.failed_probes, restart.failed_probes);
}

#[tokio::test]
async fn get_session_falls_back_to_store() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let session = bare_session(session_id);
    let store = manager.store().clone();

    tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store.insert_session(&session)
    })
    .await
    .unwrap()
    .expect("insert session");

    let loaded = manager
        .get_session(session_id)
        .await
        .expect("session should load from store fallback");
    assert_eq!(loaded.id, session_id);
}

#[tokio::test]
async fn get_session_stamps_context_fill_pct_for_idle_completed() {
    // An idle/Completed session that only exists in the store (no live
    // TrackedSession) must still come back with a daemon-computed
    // context_fill_pct so the TUI renders a bar without recomputing.
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.provider = SessionProvider::Claude;
    session.model = Some("claude-opus-4-7".to_string()); // 1M window
    session.total_input_tokens = Some(500_000);
    let store = manager.store().clone();
    let to_insert = session.clone();
    tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store.insert_session(&to_insert)
    })
    .await
    .unwrap()
    .expect("insert session");

    let loaded = manager.get_session(session_id).await.expect("load");
    assert_eq!(
        loaded.context_fill_pct,
        Some(50.0),
        "idle Completed session must be stamped with the daemon-computed pct"
    );

    // And it appears on the list path too.
    let listed = manager.list_sessions().await;
    let row = listed
        .iter()
        .find(|s| s.id == session_id)
        .expect("session in list");
    assert_eq!(row.context_fill_pct, Some(50.0));
}

#[tokio::test]
async fn archived_and_deleted_lists_stamp_context_fill_pct() {
    let (manager, _dir) = manager();
    let archived_id = Uuid::new_v4();
    let deleted_id = Uuid::new_v4();
    let mut archived = bare_session(archived_id);
    archived.status = SessionStatus::Archived;
    archived.provider = SessionProvider::Claude;
    archived.model = Some("claude-opus-4-7".to_string());
    archived.total_input_tokens = Some(500_000);

    let mut deleted = bare_session(deleted_id);
    deleted.status = SessionStatus::Deleted;
    deleted.provider = SessionProvider::Claude;
    deleted.model = Some("claude-opus-4-7".to_string());
    deleted.total_input_tokens = Some(250_000);

    let store = manager.store().clone();
    tokio::task::spawn_blocking(move || {
        let store = store.blocking_lock();
        store.insert_session(&archived)?;
        store.insert_session(&deleted)?;
        Ok::<_, rsid::error::DaemonError>(())
    })
    .await
    .unwrap()
    .expect("insert sessions");

    let archived = manager
        .list_archived_sessions(None)
        .await
        .expect("archived list");
    let archived_row = archived
        .iter()
        .find(|s| s.id == archived_id)
        .expect("archived session");
    assert_eq!(archived_row.context_fill_pct, Some(50.0));

    let deleted = manager
        .list_deleted_sessions(None)
        .await
        .expect("deleted list");
    let deleted_row = deleted
        .iter()
        .find(|s| s.id == deleted_id)
        .expect("deleted session");
    assert_eq!(deleted_row.context_fill_pct, Some(25.0));
}
