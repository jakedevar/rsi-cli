//! P1.12 — `validate_parent_id` rejection paths.
//!
//! Integration test that exercises `rpc::validate_parent_id_against` against a
//! real `SessionManager` populated with known session rows. Covers the three
//! rejection branches (not-found, leaf-kind, hidden container state) plus
//! completed-container acceptance.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use rsi_common::types::{
    ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
};
use rsid::bus::EventBus;
use rsid::config::{Config, RuntimeConfig};
use rsid::error::DaemonError;
use rsid::rpc::validate_parent_id_against;
use rsid::session::SessionManager;
use rsid::store::Store;
use tempfile::TempDir;
use uuid::Uuid;

fn build_manager() -> (Arc<SessionManager>, TempDir, TempDir) {
    let db_dir = TempDir::new().unwrap();
    let sandbox_base = TempDir::new().unwrap();
    let db_path = db_dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();
    let event_bus = Arc::new(EventBus::new(64));
    let runtime_config = RuntimeConfig::from_config(&Config::from_env());
    let socket_path = db_dir.path().join("daemon.sock");
    let manager = SessionManager::new(
        event_bus,
        store,
        false,
        socket_path,
        None,
        Vec::new(),
        runtime_config,
        sandbox_base.path().to_path_buf(),
    )
    .expect("SessionManager::new");
    (Arc::new(manager), db_dir, sandbox_base)
}

fn mk_session(id: Uuid, kind: SessionKind, status: SessionStatus) -> Session {
    let now = chrono::Utc::now();
    Session {
        context_fill_pct: None,
        id,
        provider: SessionProvider::Claude,
        claude_session_id: None,
        query: "test".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: std::path::PathBuf::from("/tmp"),
        git_branch: None,
        status,
        project_id: None,
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        session_kind: kind,
        created_at: now,
        updated_at: now,
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        model: Some("claude-sonnet-5".to_string()),
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_output_tokens: None,
        total_cache_creation_tokens: None,
        total_cache_read_tokens: None,
        stop_reason: None,
        continued_from: None,
        context_usage_confidence: ContextUsageConfidence::Missing,
        daemon_input_tokens: None,
        daemon_output_tokens: None,
        handoff_filepath: None,
        active_task: None,
        group_id: None,
        pipeline_artifact: None,
        workflow_id: None,
        workflow_id_override: None,
        rotation_depth: 0,
        retry_attempt: None,
        max_retries: None,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        rating: None,
        harness_version_hash: None,
        test_passed: None,
        clippy_passed: None,
        turn_count: None,
        retry_count: None,
        approval_wait_ms: None,
        work_time_ms: None,
        approval_started_at: None,
        sandbox_kind: None,
        sandbox_root: None,
        sandbox_branch: None,
        sandbox_cleanup_state: None,
        tag: String::new(),
        tags: Vec::new(),
        parent_id: None,
        lead_session_id: None,
        is_eval: false,
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

/// Insert a session row into the store AND hydrate it into the manager's
/// in-memory `completed` map via `restore_sessions`. This is the canonical
/// daemon-startup-style path; tests rely on the same lifecycle the daemon uses.
///
/// Note: `restore_sessions` rewrites Running/Starting/WaitingApproval rows to
/// Failed (they "didn't survive the daemon restart"). Tests that need a live
/// Running Epic should keep that in mind — the rewrite happens once per call.
async fn insert_and_restore(manager: &Arc<SessionManager>, session: Session) {
    let store = manager.store().clone();
    {
        let g = store.lock().await;
        g.insert_session(&session).expect("insert");
    }
    manager.restore_sessions().await.expect("restore_sessions");
}

/// None → Ok (legacy backward-compat path).
#[tokio::test]
async fn validate_parent_id_none_is_ok() {
    let (manager, _db_dir, _sandbox_base) = build_manager();
    assert!(validate_parent_id_against(&manager, None).await.is_ok());
}

/// Random Uuid → `InvalidParam` "not found".
#[tokio::test]
async fn validate_parent_id_missing_session_rejected() {
    let (manager, _db_dir, _sandbox_base) = build_manager();
    let bogus = Uuid::new_v4();
    let result = validate_parent_id_against(&manager, Some(bogus)).await;
    match result {
        Err(DaemonError::InvalidParam(msg)) => {
            assert!(
                msg.contains("not found"),
                "expected 'not found' message, got: {msg}"
            );
        }
        other => panic!("expected InvalidParam('not found'), got {other:?}"),
    }
}

/// Existing session of leaf kind (Task) → `InvalidParam` "leaf kind".
///
/// Note: `restore_sessions` rewrites the inserted Running row to Failed, but
/// the leaf-kind check fires BEFORE the hidden-container-state check, so the
/// assertion stays robust. We deliberately exercise the kind branch ahead of
/// state.
#[tokio::test]
async fn validate_parent_id_leaf_kind_rejected() {
    let (manager, _db_dir, _sandbox_base) = build_manager();
    let leaf_id = Uuid::new_v4();
    let session = mk_session(leaf_id, SessionKind::Task, SessionStatus::Running);
    insert_and_restore(&manager, session).await;

    let result = validate_parent_id_against(&manager, Some(leaf_id)).await;
    match result {
        Err(DaemonError::InvalidParam(msg)) => {
            assert!(
                msg.contains("leaf kind"),
                "expected 'leaf kind' message, got: {msg}"
            );
        }
        other => panic!("expected InvalidParam('leaf kind'), got {other:?}"),
    }
}

/// Existing Epic with Completed status → Ok.
///
/// Containers are non-spawnable organizational nodes. `CreateContainer` stores
/// them as Completed, so ExecuteTopology must still be able to attach spawned
/// sessions under them via `parent_id`.
#[tokio::test]
async fn validate_parent_id_completed_epic_accepted() {
    let (manager, _db_dir, _sandbox_base) = build_manager();
    let epic_id = Uuid::new_v4();
    let session = mk_session(epic_id, SessionKind::Epic, SessionStatus::Completed);
    insert_and_restore(&manager, session).await;

    let result = validate_parent_id_against(&manager, Some(epic_id)).await;
    assert!(
        result.is_ok(),
        "Completed Epic containers must validate, got: {result:?}"
    );
}

/// Existing Archived/Deleted containers → `InvalidParam` "not attachable".
#[tokio::test]
async fn validate_parent_id_hidden_container_states_rejected() {
    for status in [SessionStatus::Archived, SessionStatus::Deleted] {
        let (manager, _db_dir, _sandbox_base) = build_manager();
        let group_id = Uuid::new_v4();
        let session = mk_session(group_id, SessionKind::Group, status);
        insert_and_restore(&manager, session).await;

        let result = validate_parent_id_against(&manager, Some(group_id)).await;
        match result {
            Err(DaemonError::InvalidParam(msg)) => {
                assert!(
                    msg.contains("not attachable"),
                    "expected 'not attachable' message, got: {msg}"
                );
            }
            other => panic!("expected InvalidParam('not attachable'), got {other:?}"),
        }
    }
}

/// Happy path: existing Group container with non-hidden status → Ok.
///
/// `restore_sessions` only rewrites Running/Starting/WaitingApproval → Failed;
/// Interrupted is preserved as-is, so we get a guaranteed visible container in
/// the `completed` map for the validator to accept.
#[tokio::test]
async fn validate_parent_id_live_group_accepted() {
    let (manager, _db_dir, _sandbox_base) = build_manager();
    let group_id = Uuid::new_v4();
    let session = mk_session(group_id, SessionKind::Group, SessionStatus::Interrupted);
    insert_and_restore(&manager, session).await;

    let result = validate_parent_id_against(&manager, Some(group_id)).await;
    assert!(result.is_ok(), "live Group must validate, got: {result:?}");
}

/// Canonical ticket §8 test name. Combines the two rejection paths the plan
/// calls out — random Uuid and leaf-kind — into a single named test so the
/// verification manifest's "test_execute_topology_with_invalid_parent_id_rejected"
/// item can be ticked off by a direct test discovery filter.
#[tokio::test]
async fn test_execute_topology_with_invalid_parent_id_rejected() {
    let (manager, _db_dir, _sandbox_base) = build_manager();

    // Part A: random Uuid → "not found".
    let bogus = Uuid::new_v4();
    match validate_parent_id_against(&manager, Some(bogus)).await {
        Err(DaemonError::InvalidParam(msg)) => {
            assert!(
                msg.contains("not found"),
                "Part A: expected 'not found', got: {msg}"
            );
        }
        other => panic!("Part A: expected `InvalidParam`('not found'), got {other:?}"),
    }

    // Part B: leaf-kind (Task) session → "leaf kind".
    let leaf_id = Uuid::new_v4();
    let leaf = mk_session(leaf_id, SessionKind::Task, SessionStatus::Running);
    insert_and_restore(&manager, leaf).await;
    match validate_parent_id_against(&manager, Some(leaf_id)).await {
        Err(DaemonError::InvalidParam(msg)) => {
            assert!(
                msg.contains("leaf kind"),
                "Part B: expected 'leaf kind', got: {msg}"
            );
        }
        other => panic!("Part B: expected `InvalidParam`('leaf kind'), got {other:?}"),
    }
}
