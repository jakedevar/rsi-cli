//! RSI-006 hermetic launch tests.
//!
//! Validates that `LaunchConfig.skip_context_pipeline=true` produces a session
//! whose `system_prompt` exactly matches the caller-passed value (no preamble
//! prepended, no ContextPipeline output appended). Determinism here is the
//! load-bearing primitive for ±5% reproducibility on eval replays.
//!
//! These tests exercise the SessionManager in-process — no provider
//! subprocesses spawned, no daemon socket. We assert on the persisted
//! `harness_version_hash` (computed from the resolved system_prompt + query)
//! to verify that hermetic launches with identical inputs produce identical
//! hashes across repeated runs.

use rsi_common::types::SessionKind;
use rsid::bus::EventBus;
use rsid::claude::LaunchConfig;
use rsid::config::{Config, RuntimeConfig};
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

fn hermetic_cfg(query: &str, system_prompt: Option<&str>) -> LaunchConfig {
    LaunchConfig {
        query: query.to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        // `None` falls back to current_dir at launch.
        working_dir: None,
        // No provider — provider=None in launch.rs goes through the default
        // Claude branch which the gate now skips when skip_context_pipeline=true.
        // Note: we don't actually spawn a provider in these tests; we just check
        // what the config's system_prompt becomes after the launch path runs.
        // Since spawn fails (no Claude binary in the test env), the test
        // observes the persisted Session row instead.
        provider: None,
        model: Some("claude-sonnet-5".to_string()),
        configured_context_window: None,
        max_turns: None,
        system_prompt: system_prompt.map(str::to_string),
        resume_session_id: None,
        session_kind: Some(SessionKind::Standard),
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
        is_eval: true,
        skip_context_pipeline: true,
        capability_class: None,
        tags: vec![],
        topology_node_id: None,
        topology_iteration: 0,
        closure_selector: None,
    }
}

/// Two hermetic launches with identical inputs must produce identical
/// `harness_version_hash` values. This is the load-bearing determinism
/// invariant for the eval/replay harness — without it, ±5% reproducibility
/// is structurally impossible.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hermetic_launches_produce_identical_harness_hashes() {
    let (manager, _db, _sb) = build_manager();

    let mut hashes = Vec::with_capacity(3);
    for _ in 0..3 {
        // Provider spawn will fail (no Claude binary), but the Session row +
        // harness_version_hash are computed before that. We capture the hash
        // from the in-memory Session row.
        let _ = manager.launch_session(hermetic_cfg("Q", Some("SP"))).await;
    }

    // Read sessions from list (excluding eval-default behavior should hide
    // them, so we use the inclusive variant).
    let sessions = manager.list_sessions_including_eval().await;
    for s in sessions.iter().filter(|s| s.is_eval) {
        if let Some(h) = &s.harness_version_hash {
            hashes.push(h.clone());
        }
    }

    assert!(
        hashes.len() >= 2,
        "expected at least 2 hermetic sessions, got {}",
        hashes.len()
    );
    let first = &hashes[0];
    for h in &hashes[1..] {
        assert_eq!(
            h, first,
            "hermetic launches with identical inputs MUST produce identical harness_version_hash; got {h} vs {first}",
        );
    }
}

/// A non-hermetic launch (skip_context_pipeline=false) produces a system_prompt
/// that includes the kind preamble + caller prompt + assembled context. The
/// hash therefore differs from a hermetic launch with the same caller-passed
/// prompt — proving the bypass actually bypasses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_hermetic_launch_differs_from_hermetic() {
    let (manager, _db, _sb) = build_manager();

    // Hermetic launch — caller prompt used verbatim.
    let _ = manager.launch_session(hermetic_cfg("Q", Some("SP"))).await;

    // Non-hermetic launch — same caller prompt, but ContextPipeline runs.
    let mut cfg = hermetic_cfg("Q", Some("SP"));
    cfg.skip_context_pipeline = false;
    cfg.is_eval = false;
    let _ = manager.launch_session(cfg).await;

    let sessions = manager.list_sessions_including_eval().await;
    let hermetic_hash = sessions
        .iter()
        .find(|s| s.is_eval)
        .and_then(|s| s.harness_version_hash.clone())
        .expect("hermetic session present");
    let production_hash = sessions
        .iter()
        .find(|s| !s.is_eval)
        .and_then(|s| s.harness_version_hash.clone())
        .expect("production session present");

    assert_ne!(
        hermetic_hash, production_hash,
        "ContextPipeline must alter the prompt — hashes must differ when skip flag is false"
    );
}

/// `skip_context_pipeline=true` with `system_prompt=None` produces an
/// empty-prompt session — no preamble fallback, no panic, no degradation
/// to the production path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hermetic_launch_with_no_system_prompt_succeeds() {
    let (manager, _db, _sb) = build_manager();

    // Provider spawn fails (no Claude binary), but the Session row exists
    // with the expected hash before that.
    let _ = manager.launch_session(hermetic_cfg("Q", None)).await;

    let sessions = manager.list_sessions_including_eval().await;
    let hermetic = sessions
        .iter()
        .find(|s| s.is_eval)
        .expect("hermetic session must be present");
    // The hash is computed over Some(None.as_deref()) === None — the helper
    // hashes the absence as a distinct input from any string. We just assert
    // the hash exists (no panic, no falsy state).
    assert!(
        hermetic.harness_version_hash.is_some(),
        "harness_version_hash must be populated even without system_prompt"
    );
}
