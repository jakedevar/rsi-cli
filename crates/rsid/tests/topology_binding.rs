//! P1.7 — End-to-end topology binding integration test.
//!
//! Exercises the full spawn coordinator binding path:
//!   - Create a topology with two nodes (plan_v1 → impl_v1).
//!   - Create an Epic with the topology.
//!   - Create a lead session under the Epic.
//!   - Emit a directive for plan_v1 and assert it binds correctly.
//!   - Mark plan_v1 child Completed.
//!   - Emit a directive for impl_v1 and assert prereq is satisfied.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rsi_common::types::{
    ContextUsageConfidence, Session, SessionKind, SessionProvider, SessionStatus,
    TopologyDefinition, TopologyEdge, TopologyNode,
};
use rsid::bus::EventBus;
use rsid::config::{Config, RuntimeConfig};
use rsid::session::SessionManager;
use rsid::session::spawn_coordinator::{SpawnCoordinator, SpawnState};
use rsid::session::spawn_directive::SpawnDirective;
use rsid::store::Store;
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::{RwLock, mpsc};
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

fn mk_session(id: Uuid, kind: SessionKind, parent_id: Option<Uuid>, lead: Option<Uuid>) -> Session {
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
        status: SessionStatus::Running,
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
        parent_id,
        lead_session_id: lead,
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

/// End-to-end test: SpawnDirective with topology_node binds correctly,
/// auto-increments iteration, and enforces prereq ordering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_spawn_directive_binds_topology_node() {
    let (manager, _db_dir, _sandbox_base) = build_manager();
    let store = manager.store().clone();

    // 1. Create a topology: plan_v1 (Research) → impl_v1 (Task).
    let topology_id = Uuid::new_v4();
    let topology = rsi_common::types::Topology {
        id: topology_id,
        name: "ci-test-topo".to_string(),
        definition: TopologyDefinition {
            nodes: vec![
                TopologyNode {
                    id: "plan_v1".to_string(),
                    label: "Plan V1".to_string(),
                    kind: SessionKind::Research,
                    max_iterations: None,
                    prereqs: vec![],
                    on_failure: None,
                    params: std::collections::HashMap::new(),
                },
                TopologyNode {
                    id: "impl_v1".to_string(),
                    label: "Impl V1".to_string(),
                    kind: SessionKind::Task,
                    max_iterations: None,
                    prereqs: vec!["plan_v1".to_string()],
                    on_failure: None,
                    params: std::collections::HashMap::new(),
                },
            ],
            edges: vec![TopologyEdge {
                from: "plan_v1".to_string(),
                to: "impl_v1".to_string(),
                loop_edge: false,
            }],
            until: None,
        },
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    {
        let g = store.lock().await;
        g.insert_topology(&topology).expect("insert topology");
    }

    // 2. Create an Epic with workflow_id pointing at the topology.
    let epic_id = Uuid::new_v4();
    let lead_id = Uuid::new_v4();
    let mut epic = mk_session(epic_id, SessionKind::Epic, None, Some(lead_id));
    epic.workflow_id = Some(topology_id);

    // 3. Create a lead session (Research kind) under the Epic.
    let lead = mk_session(lead_id, SessionKind::Research, Some(epic_id), None);

    {
        let g = store.lock().await;
        g.insert_session(&epic).expect("insert epic");
        g.insert_session(&lead).expect("insert lead");
    }

    // 4. Promote lead via direct session mutation (simulate SetEpicLead).
    // The epic already has lead_session_id = Some(lead_id) from construction.

    // 5. Set up SpawnCoordinator.
    let (tx, mut rx) = mpsc::channel::<rsid::session::spawn_coordinator::SpawnRequest>(8);
    let coord = SpawnCoordinator::new(tx);
    let active: Arc<RwLock<HashMap<Uuid, rsid::session::types::TrackedSession>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let completed: Arc<RwLock<HashMap<Uuid, rsid::session::types::CompletedSession>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // 6. Simulate lead emitting /spawn_child directive for plan_v1.
    let directive_plan = SpawnDirective {
        kind: SessionKind::Research,
        provider: None,
        model: None,
        effort: None,
        agent_role: None,
        query: "research phase".to_string(),
        topology_node: Some("plan_v1".to_string()),
        iteration: None, // auto-increment
        tags: None,
    };

    let state = coord
        .handle(lead_id, 100, directive_plan, &active, &completed, &store)
        .await;

    // 7. Assert spawn bound correctly.
    assert!(
        matches!(state, SpawnState::Spawning { .. }),
        "expected Spawning, got {state:?}"
    );
    let req = rx.try_recv().expect("spawn request enqueued");
    assert_eq!(
        req.config.topology_node_id,
        Some("plan_v1".to_string()),
        "topology_node_id must be plan_v1"
    );
    assert_eq!(
        req.config.topology_iteration, 1,
        "first iteration must auto-increment to 1"
    );
    assert_eq!(req.config.parent_id, Some(epic_id));

    // 8. Simulate plan_v1 child completing: insert into both store and completed map
    // so the coordinator's sessions_view can see it.
    let mut plan_child = mk_session(Uuid::new_v4(), SessionKind::Research, Some(epic_id), None);
    plan_child.topology_node_id = Some("plan_v1".to_string());
    plan_child.topology_iteration = 1;
    plan_child.status = SessionStatus::Completed;
    {
        let g = store.lock().await;
        g.insert_session(&plan_child).expect("insert plan child");
    }
    // Add to completed map so the binding block's sessions_view includes it.
    {
        let mut cw = completed.write().await;
        cw.insert(
            plan_child.id,
            rsid::session::types::CompletedSession::for_test(plan_child.clone()),
        );
    }

    // 9. Simulate directive for impl_v1 — prereq plan_v1 now Completed.
    let directive_impl = SpawnDirective {
        kind: SessionKind::Task,
        provider: None,
        model: None,
        effort: None,
        agent_role: None,
        query: "implementation phase".to_string(),
        topology_node: Some("impl_v1".to_string()),
        iteration: None,
        tags: None,
    };

    let state2 = coord
        .handle(lead_id, 101, directive_impl, &active, &completed, &store)
        .await;

    // 10. Assert impl_v1 spawns (prereq satisfied by plan_child in store).
    assert!(
        matches!(state2, SpawnState::Spawning { .. }),
        "impl_v1 spawn must succeed after plan_v1 Completed; got {state2:?}"
    );
    let req2 = rx.try_recv().expect("impl spawn request enqueued");
    assert_eq!(
        req2.config.topology_node_id,
        Some("impl_v1".to_string()),
        "topology_node_id must be impl_v1"
    );
    assert_eq!(
        req2.config.topology_iteration, 1,
        "first impl iteration must auto-increment to 1"
    );
}
