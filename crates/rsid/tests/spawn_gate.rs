//! Phase 1b spawn-gate test: launching a container-kind session must be
//! rejected with `InvalidParam` before any process is spawned or row is
//! inserted. Uses the same lightweight SessionManager fixture pattern as
//! the sandbox_cleanup tests.

use rsi_common::types::SessionKind;
use rsid::bus::EventBus;
use rsid::claude::LaunchConfig;
use rsid::config::{Config, RuntimeConfig};
use rsid::error::DaemonError;
use rsid::session::SessionManager;
use rsid::store::Store;
use std::sync::Arc;
use tempfile::TempDir;

fn build_manager() -> (SessionManager, TempDir, TempDir) {
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
    (manager, db_dir, sandbox_base)
}

fn cfg(kind: SessionKind) -> LaunchConfig {
    LaunchConfig {
        query: "hello".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        working_dir: None,
        provider: None,
        model: None,
        configured_context_window: None,
        max_turns: None,
        system_prompt: None,
        resume_session_id: None,
        session_kind: Some(kind),
        project_id: None,
        rsi_session_id: None,
        rsi_socket: None,
        rsi_session_token: None,
        continued_from: None,
        openai_base_url: None,
        openai_api_key: None,
        conversation_history: None,
        workflow_id: None,
        workflow_id_override: None,
        max_retries: None,
        group_id: None,
        parent_id: None,
        effort: None,
        issue_identifier: None,
        issue_url: None,
        issue_tracker_id: None,
        scheduled_job_id: None,
        model_invocation_owner: None,
        model_invocation_dedup_key: None,
        model_invocation_request_fingerprint: None,
        skip_project_model_default: false,
        model_invocation_purpose:
            rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
        sandbox: None,
        cargo_target_dir: None,
        execution_scratch: None,
        is_eval: false,
        skip_context_pipeline: false,
        capability_class: None,
        tags: vec![],
        topology_node_id: None,
        topology_iteration: 0,
        closure_selector: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_group_kind_is_rejected() {
    let (manager, _db, _sb) = build_manager();
    let err = manager
        .launch_session(cfg(SessionKind::Group))
        .await
        .expect_err("container kind must be rejected");
    match err {
        DaemonError::InvalidParam(msg) => {
            assert!(
                msg.contains("container-kind"),
                "message should mention container-kind, got: {msg}"
            );
        }
        other => panic!("expected InvalidParam, got {:?}", other),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_epic_kind_is_rejected() {
    let (manager, _db, _sb) = build_manager();
    let err = manager
        .launch_session(cfg(SessionKind::Epic))
        .await
        .expect_err("container kind must be rejected");
    assert!(matches!(err, DaemonError::InvalidParam(_)));
}
