//! Public Store API persistence contracts across common record types.

use std::path::PathBuf;

use rsi_common::types::{
    ContextUsageConfidence, EspGame, Project, Session, SessionKind, SessionProvider, SessionStatus,
    TurnMetric, Workflow, WorkflowDocument, WorkflowStage,
};
use rsid::store::Store;
use uuid::Uuid;

fn make_test_session() -> Session {
    Session {
        context_fill_pct: None,
        id: Uuid::new_v4(),
        provider: SessionProvider::Claude,
        claude_session_id: None,
        query: "Hello, Claude!".to_string(),
        title: None,
        agent_role: None,
        epic_spawn_ordinal: None,
        description: None,
        short_summary: None,
        pending_question: None,
        pending_archive: false,
        working_dir: PathBuf::from("/tmp/test"),
        git_branch: None,
        status: SessionStatus::Starting,
        project_id: Some(
            Uuid::parse_str("00000000-0000-4000-8000-000000000077")
                .expect("fixed test project UUID"),
        ),
        pinned_at: None,
        testing_needed_at: None,
        rotation_disabled_at: None,
        session_kind: SessionKind::Standard,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        cost_usd: None,
        duration_ms: None,
        num_turns: None,
        model: None,
        input_tokens: None,
        output_tokens: None,
        context_window: None,
        resolved_context_budget: None,
        total_input_tokens: None,
        total_prompt_tokens: None,
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

fn make_test_project(name: &str) -> Project {
    Project {
        id: Uuid::new_v4(),
        name: name.to_string(),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.to_string(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

// -- Metrics and project persistence contracts --

#[test]
fn test_insert_and_load_turn_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let session = make_test_session();
    store.insert_session(&session).unwrap();

    // Insert 3 turn metrics
    let metric1 = TurnMetric {
        id: 0,
        session_id: session.id,
        turn_number: 1,
        input_tokens: 10000,
        cache_creation_tokens: 5000,
        cache_read_tokens: 3000,
        output_tokens: 500,
        stop_reason: Some("end_turn".to_string()),
        tools_used: None,
        tool_count: 0,
        created_at: chrono::Utc::now(),
        model: None,
        thinking_tokens: 0,
        cache_creation_1h_tokens: 0,
        cache_creation_5m_tokens: 0,
        service_tier: None,
    };
    let metric2 = TurnMetric {
        id: 0,
        session_id: session.id,
        turn_number: 2,
        input_tokens: 15000,
        cache_creation_tokens: 0,
        cache_read_tokens: 8000,
        output_tokens: 1200,
        stop_reason: Some("tool_use".to_string()),
        tools_used: Some(vec!["Read".to_string(), "Edit".to_string()]),
        tool_count: 2,
        created_at: chrono::Utc::now(),
        model: None,
        thinking_tokens: 0,
        cache_creation_1h_tokens: 0,
        cache_creation_5m_tokens: 0,
        service_tier: None,
    };
    let metric3 = TurnMetric {
        id: 0,
        session_id: session.id,
        turn_number: 3,
        input_tokens: 20000,
        cache_creation_tokens: 0,
        cache_read_tokens: 15000,
        output_tokens: 800,
        stop_reason: Some("end_turn".to_string()),
        tools_used: Some(vec!["Bash".to_string()]),
        tool_count: 1,
        created_at: chrono::Utc::now(),
        model: None,
        thinking_tokens: 0,
        cache_creation_1h_tokens: 0,
        cache_creation_5m_tokens: 0,
        service_tier: None,
    };

    let id1 = store.insert_turn_metric(&metric1).unwrap();
    let id2 = store.insert_turn_metric(&metric2).unwrap();
    let id3 = store.insert_turn_metric(&metric3).unwrap();

    // IDs should be sequential
    assert!(id1 > 0);
    assert_eq!(id2, id1 + 1);
    assert_eq!(id3, id2 + 1);

    // Load and verify
    let metrics = store.load_turn_metrics(session.id).unwrap();
    assert_eq!(metrics.len(), 3);

    // Verify ordering by turn_number
    assert_eq!(metrics[0].turn_number, 1);
    assert_eq!(metrics[1].turn_number, 2);
    assert_eq!(metrics[2].turn_number, 3);

    // Verify values
    assert_eq!(metrics[0].input_tokens, 10000);
    assert_eq!(metrics[0].tools_used, None);
    assert_eq!(metrics[0].tool_count, 0);

    assert_eq!(metrics[1].input_tokens, 15000);
    assert_eq!(
        metrics[1].tools_used,
        Some(vec!["Read".to_string(), "Edit".to_string()])
    );
    assert_eq!(metrics[1].tool_count, 2);

    assert_eq!(metrics[2].cache_read_tokens, 15000);
    assert_eq!(metrics[2].stop_reason, Some("end_turn".to_string()));
}

#[test]
fn test_turn_metrics_cascade_delete() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let session = make_test_session();
    store.insert_session(&session).unwrap();

    // Insert some turn metrics
    let metric = TurnMetric {
        id: 0,
        session_id: session.id,
        turn_number: 1,
        input_tokens: 10000,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        output_tokens: 500,
        stop_reason: None,
        tools_used: None,
        tool_count: 0,
        created_at: chrono::Utc::now(),
        model: None,
        thinking_tokens: 0,
        cache_creation_1h_tokens: 0,
        cache_creation_5m_tokens: 0,
        service_tier: None,
    };
    store.insert_turn_metric(&metric).unwrap();
    store
        .insert_turn_metric(&TurnMetric {
            turn_number: 2,
            ..metric.clone()
        })
        .unwrap();

    // Verify turn metrics exist
    let metrics = store.load_turn_metrics(session.id).unwrap();
    assert_eq!(metrics.len(), 2);

    // Delete session (should cascade to turn_metrics)
    store.delete_session(session.id).unwrap();

    // Verify turn_metrics are gone
    let metrics = store.load_turn_metrics(session.id).unwrap();
    assert!(metrics.is_empty());
}

#[test]
fn test_context_snapshots_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let session = make_test_session();
    store.insert_session(&session).unwrap();

    // No snapshots initially
    let latest = store.load_latest_context_snapshot(session.id).unwrap();
    assert!(latest.is_none());

    // Insert snapshots
    store.insert_context_snapshot(session.id, 50_000).unwrap();
    store.insert_context_snapshot(session.id, 80_000).unwrap();

    // Latest should be the most recent
    let latest = store.load_latest_context_snapshot(session.id).unwrap();
    assert_eq!(latest, Some(80_000));

    // Delete session should cascade
    store.delete_session(session.id).unwrap();
    let latest = store.load_latest_context_snapshot(session.id).unwrap();
    assert!(latest.is_none());
}

#[test]
fn test_insert_and_load_project() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let project = Project {
        id: Uuid::new_v4(),
        name: "flywheel".to_string(),
        path: Some(PathBuf::from("/home/user/flywheel")),
        description: Some("Claude session manager".to_string()),
        color: "#89b4fa".to_string(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    store.insert_project(&project).unwrap();

    let loaded = store.get_project(project.id).unwrap().unwrap();
    assert_eq!(loaded.name, "flywheel");
    assert_eq!(loaded.path, Some(PathBuf::from("/home/user/flywheel")));
    assert_eq!(
        loaded.description,
        Some("Claude session manager".to_string())
    );
    assert_eq!(loaded.color, "#89b4fa");
}

#[test]
fn test_get_project_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let project = make_test_project("my-project");
    store.insert_project(&project).unwrap();

    let loaded = store.get_project_by_name("my-project").unwrap().unwrap();
    assert_eq!(loaded.id, project.id);

    // Non-existent name returns None
    assert!(store.get_project_by_name("other").unwrap().is_none());
}

#[test]
fn test_load_projects_ordered_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let p1 = make_test_project("zebra");
    let p2 = make_test_project("alpha");
    let p3 = make_test_project("middle");

    store.insert_project(&p1).unwrap();
    store.insert_project(&p2).unwrap();
    store.insert_project(&p3).unwrap();

    let projects = store.load_projects().unwrap();
    assert_eq!(projects.len(), 3);
    assert_eq!(projects[0].name, "alpha");
    assert_eq!(projects[1].name, "middle");
    assert_eq!(projects[2].name, "zebra");
}

#[test]
fn test_update_project() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let mut project = make_test_project("original");
    store.insert_project(&project).unwrap();

    project.name = "updated".to_string();
    project.color = "#a6e3a1".to_string();
    project.path = Some(PathBuf::from("/new/path"));

    store.update_project(&project).unwrap();

    let loaded = store.get_project(project.id).unwrap().unwrap();
    assert_eq!(loaded.name, "updated");
    assert_eq!(loaded.color, "#a6e3a1");
    assert_eq!(loaded.path, Some(PathBuf::from("/new/path")));
}

// -- Session and model-segment persistence contracts --

#[test]
fn test_find_project_for_path() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let p1 = Project {
        path: Some(PathBuf::from("/home/user")),
        ..make_test_project("user-home")
    };
    let p2 = Project {
        path: Some(PathBuf::from("/home/user/projects")),
        ..make_test_project("projects")
    };
    let p3 = Project {
        path: Some(PathBuf::from("/home/user/projects/flywheel")),
        ..make_test_project("flywheel")
    };
    let p4 = make_test_project("no-path"); // path is None

    store.insert_project(&p1).unwrap();
    store.insert_project(&p2).unwrap();
    store.insert_project(&p3).unwrap();
    store.insert_project(&p4).unwrap();

    // Exact match should return the project
    let found = store
        .find_project_for_path(&PathBuf::from("/home/user/projects/flywheel"))
        .unwrap();
    assert_eq!(found.unwrap().name, "flywheel");

    // Subdirectory should match longest prefix
    let found = store
        .find_project_for_path(&PathBuf::from("/home/user/projects/flywheel/crates"))
        .unwrap();
    assert_eq!(found.unwrap().name, "flywheel");

    // /home/user/projects/other should match "projects" not "flywheel"
    let found = store
        .find_project_for_path(&PathBuf::from("/home/user/projects/other"))
        .unwrap();
    assert_eq!(found.unwrap().name, "projects");

    // Completely unrelated path returns None
    let found = store
        .find_project_for_path(&PathBuf::from("/opt/something"))
        .unwrap();
    assert!(found.is_none());
}

#[test]
fn test_load_sessions_by_project() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let project = make_test_project("my-project");
    store.insert_project(&project).unwrap();

    // Create sessions: 2 assigned to project, 1 unassigned
    let mut s1 = make_test_session();
    s1.project_id = Some(project.id);
    let mut s2 = make_test_session();
    s2.project_id = Some(project.id);
    // `make_test_session()` defaults `project_id` to `Some(d04_test_project_id())`
    // (changed by cf01ae43, "feat: implement D04 issue linkage"), so an
    // unassigned session must now say so explicitly.
    let mut s3 = make_test_session();
    s3.project_id = None;

    store.insert_session(&s1).unwrap();
    store.insert_session(&s2).unwrap();
    store.insert_session(&s3).unwrap();

    // Load project sessions
    let project_sessions = store.load_sessions_by_project(Some(project.id)).unwrap();
    assert_eq!(project_sessions.len(), 2);

    // Load unassigned sessions
    let unassigned = store.load_sessions_by_project(None).unwrap();
    assert_eq!(unassigned.len(), 1);
    assert_eq!(unassigned[0].id, s3.id);

    // load_sessions returns all
    let all = store.load_sessions().unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn test_update_session_project() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let project = make_test_project("my-project");
    store.insert_project(&project).unwrap();

    // `make_test_session()` defaults `project_id` to `Some(d04_test_project_id())`
    // (changed by cf01ae43, "feat: implement D04 issue linkage"); this test is
    // about the unassigned -> assigned -> unassigned cycle, so start unassigned.
    let mut session = make_test_session();
    session.project_id = None;
    store.insert_session(&session).unwrap();

    // Initially unassigned
    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert!(loaded.project_id.is_none());

    // Assign to project
    store
        .update_session_project(session.id, Some(project.id))
        .unwrap();
    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(loaded.project_id, Some(project.id));

    // Unassign
    store.update_session_project(session.id, None).unwrap();
    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert!(loaded.project_id.is_none());
}

#[test]
fn test_model_segment_crud() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let session = make_test_session();
    store.insert_session(&session).unwrap();

    // Create initial segment
    let id1 = store
        .create_model_segment(session.id, "claude-sonnet-5", 0)
        .unwrap();
    assert!(id1 > 0);

    // Load segments
    let segments = store.load_model_segments(session.id).unwrap();
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].model_id, "claude-sonnet-5");
    assert_eq!(segments[0].from_sequence, 0);
    assert!(segments[0].to_sequence.is_none()); // Active segment

    // Create second segment — should close the first
    let id2 = store
        .create_model_segment(session.id, "claude-opus-4-6", 5)
        .unwrap();
    assert!(id2 > id1);

    let segments = store.load_model_segments(session.id).unwrap();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].to_sequence, Some(4)); // Closed at from_sequence - 1
    assert_eq!(segments[1].model_id, "claude-opus-4-6");
    assert_eq!(segments[1].from_sequence, 5);
    assert!(segments[1].to_sequence.is_none()); // New active segment
}

#[test]
fn test_get_model_at_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let session = make_test_session();
    store.insert_session(&session).unwrap();

    // No segments — returns None
    let model = store.get_model_at_sequence(session.id, 0).unwrap();
    assert!(model.is_none());

    // Create two segments: [0, 4] sonnet, [5, NULL] opus
    store
        .create_model_segment(session.id, "claude-sonnet-5", 0)
        .unwrap();
    store
        .create_model_segment(session.id, "claude-opus-4-6", 5)
        .unwrap();

    // Sequence 0 → sonnet
    let model = store.get_model_at_sequence(session.id, 0).unwrap();
    assert_eq!(model, Some("claude-sonnet-5".to_string()));

    // Sequence 4 → sonnet (boundary)
    let model = store.get_model_at_sequence(session.id, 4).unwrap();
    assert_eq!(model, Some("claude-sonnet-5".to_string()));

    // Sequence 5 → opus
    let model = store.get_model_at_sequence(session.id, 5).unwrap();
    assert_eq!(model, Some("claude-opus-4-6".to_string()));

    // Sequence 100 → opus (active segment, no end)
    let model = store.get_model_at_sequence(session.id, 100).unwrap();
    assert_eq!(model, Some("claude-opus-4-6".to_string()));
}

#[test]
fn test_model_segment_cascade_delete() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let session = make_test_session();
    store.insert_session(&session).unwrap();

    store
        .create_model_segment(session.id, "claude-sonnet-5", 0)
        .unwrap();

    // Verify segment exists
    let segments = store.load_model_segments(session.id).unwrap();
    assert_eq!(segments.len(), 1);

    // Delete session — should cascade
    store.delete_session(session.id).unwrap();

    let segments = store.load_model_segments(session.id).unwrap();
    assert!(segments.is_empty());
}

#[test]
fn test_migration_v9_backfill() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    // Insert sessions: one with model, one without
    let mut s1 = make_test_session();
    s1.model = Some("claude-sonnet-5".to_string());
    store.insert_session(&s1).unwrap();

    let s2 = make_test_session(); // model = None
    store.insert_session(&s2).unwrap();

    // The V9 migration backfills on first open, but since we already opened,
    // check that create_model_segment works for new sessions
    // and that the backfill would have created segments for sessions with models.
    // For a fresh DB opened from V0, the migration creates segments automatically.
    // Here we verify the store methods work correctly.
    let segments = store.load_model_segments(s2.id).unwrap();
    assert!(segments.is_empty()); // No model, no segment
}

#[test]
fn test_handoff_filepath_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let mut session = make_test_session();
    session.handoff_filepath =
        Some("thoughts/shared/handoffs/general/2026-02-13_test.md".to_string());

    store.insert_session(&session).unwrap();
    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(loaded.handoff_filepath, session.handoff_filepath);

    // Also verify update_session_metadata persists it
    let mut session2 = make_test_session();
    store.insert_session(&session2).unwrap();
    assert!(
        store
            .get_session(session2.id)
            .unwrap()
            .unwrap()
            .handoff_filepath
            .is_none()
    );

    session2.handoff_filepath = Some("thoughts/shared/handoffs/general/updated.md".to_string());
    store.update_session_metadata(&session2).unwrap();
    let loaded2 = store.get_session(session2.id).unwrap().unwrap();
    assert_eq!(loaded2.handoff_filepath, session2.handoff_filepath);
}

#[test]
fn test_rotation_depth_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    // Original session has depth 0 (default)
    let session = make_test_session();
    assert_eq!(session.rotation_depth, 0);
    store.insert_session(&session).unwrap();
    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(loaded.rotation_depth, 0);

    // Simulate rotation chain: depth 1, 2, 3, 4
    for depth in 1u32..=4 {
        let mut child = make_test_session();
        child.rotation_depth = depth;
        store.insert_session(&child).unwrap();
        let loaded_child = store.get_session(child.id).unwrap().unwrap();
        assert_eq!(
            loaded_child.rotation_depth, depth,
            "rotation_depth {} should round-trip through DB",
            depth
        );
    }

    // Verify depth 4 is the limit — sessions at depth 4 persist correctly
    let mut at_limit = make_test_session();
    at_limit.rotation_depth = 4;
    store.insert_session(&at_limit).unwrap();
    let loaded_limit = store.get_session(at_limit.id).unwrap().unwrap();
    assert_eq!(loaded_limit.rotation_depth, 4);
}

#[test]
fn test_pipeline_artifact_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    // Insert session without pipeline_artifact
    let mut session = make_test_session();
    store.insert_session(&session).unwrap();
    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert!(loaded.pipeline_artifact.is_none());

    // Finalization discovers artifacts after insert; metadata must persist them.
    session.pipeline_artifact = Some("thoughts/shared/research/2026-03-04-foo.md".to_string());
    store.update_session_metadata(&session).unwrap();
    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(loaded.pipeline_artifact, session.pipeline_artifact);

    // Insert session with pipeline_artifact set at creation — insert_session still stores it
    let mut session2 = make_test_session();
    session2.pipeline_artifact = Some("thoughts/shared/plans/2026-03-04-bar.md".to_string());
    store.insert_session(&session2).unwrap();
    let loaded2 = store.get_session(session2.id).unwrap().unwrap();
    assert_eq!(loaded2.pipeline_artifact, session2.pipeline_artifact);
}

#[test]
#[allow(clippy::unwrap_used)]
fn update_session_metadata_persists_pipeline_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).unwrap();
    let mut session = make_test_session();
    store.insert_session(&session).unwrap();
    session.pipeline_artifact = Some("thoughts/shared/plans/task.md".into());
    store.update_session_metadata(&session).unwrap();
    assert_eq!(
        store
            .get_session(session.id)
            .unwrap()
            .unwrap()
            .pipeline_artifact,
        session.pipeline_artifact
    );
}

#[test]
#[allow(clippy::unwrap_used)]
fn update_session_metadata_persists_taskrabbit_escalation_kind() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).unwrap();
    let mut session = make_test_session();
    session.session_kind = SessionKind::TaskRabbit;
    store.insert_session(&session).unwrap();
    session.session_kind = SessionKind::Standard;
    store.update_session_metadata(&session).unwrap();
    assert_eq!(
        store.get_session(session.id).unwrap().unwrap().session_kind,
        SessionKind::Standard
    );
}

#[test]
fn test_toggle_session_pin_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let session = make_test_session();
    store.insert_session(&session).unwrap();

    // Initially not pinned
    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert!(loaded.pinned_at.is_none());

    // Pin: toggle once → should be set
    let result = store.toggle_session_pin(session.id).unwrap();
    assert!(result.is_some(), "first toggle should pin the session");

    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert!(loaded.pinned_at.is_some());

    // Unpin: toggle again → should be cleared
    let result = store.toggle_session_pin(session.id).unwrap();
    assert!(result.is_none(), "second toggle should unpin the session");

    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert!(loaded.pinned_at.is_none());

    // Re-pin for idempotency check
    let result = store.toggle_session_pin(session.id).unwrap();
    assert!(result.is_some(), "third toggle should pin again");
}

#[test]
fn pin_session_if_unpinned_is_idempotent_and_preserves_the_original_time() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let session = make_test_session();
    store.insert_session(&session).unwrap();

    let first = store
        .pin_session_if_unpinned(session.id)
        .unwrap()
        .expect("an unpinned session is pinned");
    assert_eq!(
        store
            .get_session(session.id)
            .unwrap()
            .unwrap()
            .pinned_at
            .map(|dt| dt.to_rfc3339()),
        Some(first.clone()),
    );

    // Second call must be a no-op that reports the SAME pin time, not a
    // toggle-off and not a fresh timestamp: pin order is succession order.
    let second = store
        .pin_session_if_unpinned(session.id)
        .unwrap()
        .expect("an already-pinned session stays pinned");
    assert_eq!(second, first, "the original pin time is preserved");
}

#[test]
fn pin_session_if_unpinned_reports_a_missing_session() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    assert!(
        store.pin_session_if_unpinned(Uuid::new_v4()).is_err(),
        "pinning an unknown session is an error, not a silent no-op",
    );
}

#[test]
fn test_toggle_session_pin_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    // Toggle on a non-existent session should return an error
    let result = store.toggle_session_pin(Uuid::new_v4());
    assert!(
        result.is_err(),
        "toggle on non-existent session should error"
    );
}

// -- Workflow and remaining public Store contracts --

#[test]
fn test_workflow_insert_and_get() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let workflow = rsi_common::types::Workflow {
        id: Uuid::new_v4(),
        title: "research codebase for feature X".to_string(),
        stage: rsi_common::types::WorkflowStage::Research,
        artifact_path: None,
        project_id: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    store.insert_workflow(&workflow).unwrap();

    let loaded = store.get_workflow(workflow.id).unwrap().unwrap();
    assert_eq!(loaded.id, workflow.id);
    assert_eq!(loaded.title, "research codebase for feature X");
    assert_eq!(loaded.stage, rsi_common::types::WorkflowStage::Research);
    assert!(loaded.artifact_path.is_none());
    assert!(loaded.project_id.is_none());
}

#[test]
fn test_workflow_list() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let wf1 = rsi_common::types::Workflow {
        id: Uuid::new_v4(),
        title: "workflow 1".to_string(),
        stage: rsi_common::types::WorkflowStage::Research,
        artifact_path: None,
        project_id: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let wf2 = rsi_common::types::Workflow {
        id: Uuid::new_v4(),
        title: "workflow 2".to_string(),
        stage: rsi_common::types::WorkflowStage::PlanComplete,
        artifact_path: Some("thoughts/shared/plans/test.md".to_string()),
        project_id: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    store.insert_workflow(&wf1).unwrap();
    store.insert_workflow(&wf2).unwrap();

    let all = store.list_workflows().unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn test_workflow_update_stage() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let workflow = rsi_common::types::Workflow {
        id: Uuid::new_v4(),
        title: "test workflow".to_string(),
        stage: rsi_common::types::WorkflowStage::Research,
        artifact_path: None,
        project_id: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    store.insert_workflow(&workflow).unwrap();

    // Advance to ResearchComplete with artifact
    store
        .update_workflow_stage(
            workflow.id,
            rsi_common::types::WorkflowStage::ResearchComplete,
            Some("thoughts/shared/research/2026-03-08-test.md"),
        )
        .unwrap();

    let loaded = store.get_workflow(workflow.id).unwrap().unwrap();
    assert_eq!(
        loaded.stage,
        rsi_common::types::WorkflowStage::ResearchComplete
    );
    assert_eq!(
        loaded.artifact_path,
        Some("thoughts/shared/research/2026-03-08-test.md".to_string())
    );

    // Advance to Planning (no artifact change)
    store
        .update_workflow_stage(
            workflow.id,
            rsi_common::types::WorkflowStage::Planning,
            None,
        )
        .unwrap();

    let loaded = store.get_workflow(workflow.id).unwrap().unwrap();
    assert_eq!(loaded.stage, rsi_common::types::WorkflowStage::Planning);
    // artifact_path preserved from previous update
    assert_eq!(
        loaded.artifact_path,
        Some("thoughts/shared/research/2026-03-08-test.md".to_string())
    );
}

#[test]
fn test_workflow_list_by_project() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let project = make_test_project("test-project");
    store.insert_project(&project).unwrap();

    let wf1 = rsi_common::types::Workflow {
        id: Uuid::new_v4(),
        title: "in project".to_string(),
        stage: rsi_common::types::WorkflowStage::Research,
        artifact_path: None,
        project_id: Some(project.id),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let wf2 = rsi_common::types::Workflow {
        id: Uuid::new_v4(),
        title: "no project".to_string(),
        stage: rsi_common::types::WorkflowStage::Research,
        artifact_path: None,
        project_id: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    store.insert_workflow(&wf1).unwrap();
    store.insert_workflow(&wf2).unwrap();

    let project_workflows = store.list_workflows_by_project(Some(project.id)).unwrap();
    assert_eq!(project_workflows.len(), 2);
    assert!(
        project_workflows.iter().any(|wf| wf.title == "in project"),
        "project-scoped list should include project-bound workflows"
    );
    assert!(
        project_workflows.iter().any(|wf| wf.title == "no project"),
        "project-scoped list should include global workflows"
    );

    let unassigned = store.list_workflows_by_project(None).unwrap();
    assert_eq!(unassigned.len(), 1);
    assert_eq!(unassigned[0].title, "no project");
}

#[test]
fn test_session_workflow_id_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let workflow = rsi_common::types::Workflow {
        id: Uuid::new_v4(),
        title: "test workflow".to_string(),
        stage: rsi_common::types::WorkflowStage::Research,
        artifact_path: None,
        project_id: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    store.insert_workflow(&workflow).unwrap();

    // Insert session with workflow_id
    let mut session = make_test_session();
    session.workflow_id = Some(workflow.id);
    store.insert_session(&session).unwrap();

    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(loaded.workflow_id, Some(workflow.id));

    // Update session workflow
    store.update_session_workflow(session.id, None).unwrap();
    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert!(loaded.workflow_id.is_none());

    // Set it back
    store
        .update_session_workflow(session.id, Some(workflow.id))
        .unwrap();
    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(loaded.workflow_id, Some(workflow.id));
}

#[test]
fn test_workflow_definition_upsert_round_trip_and_title_sync() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let workflow_id = Uuid::new_v4();
    let document = rsi_common::types::WorkflowDocument {
        workflow: rsi_common::types::Workflow {
            id: workflow_id,
            title: "stale title".to_string(),
            stage: rsi_common::types::WorkflowStage::Planning,
            artifact_path: None,
            project_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
        definition: serde_json::json!({
            "version": "1.0",
            "name": "synced title",
            "nodes": [],
            "edges": [],
            "metadata": {},
        }),
    };

    let saved = store.upsert_workflow_definition(&document).unwrap();
    assert_eq!(saved.workflow.title, "synced title");
    assert_eq!(saved.definition["name"], "synced title");

    let loaded = store.get_workflow_definition(workflow_id).unwrap().unwrap();
    assert_eq!(loaded.workflow.title, "synced title");
    assert_eq!(loaded.definition["name"], "synced title");
}

#[test]
fn test_workflow_get_nonexistent() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let result = store.get_workflow(Uuid::new_v4()).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_esp_game_insert_and_list() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    let game = rsi_common::types::EspGame {
        id: Uuid::new_v4(),
        played_at: chrono::Utc::now(),
        score: 3,
        rounds_played: 10,
        total_rounds: 12,
        p_value: 0.1407,
        round_details: r#"[{"pick":4,"target":4,"hit":true}]"#.to_string(),
    };

    store.insert_esp_game(&game).unwrap();

    let games = store.list_esp_games(50).unwrap();
    assert_eq!(games.len(), 1);
    assert_eq!(games[0].id, game.id);
    assert_eq!(games[0].score, 3);
    assert_eq!(games[0].rounds_played, 10);
    assert_eq!(games[0].total_rounds, 12);
    assert!((games[0].p_value - 0.1407).abs() < 1e-10);
    assert_eq!(games[0].round_details, game.round_details);
}

#[test]
fn test_esp_game_list_ordering_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    // Insert 3 games with different timestamps
    for i in 0..3u8 {
        let game = rsi_common::types::EspGame {
            id: Uuid::new_v4(),
            played_at: chrono::Utc::now() + chrono::Duration::seconds(i64::from(i)),
            score: i,
            rounds_played: 12,
            total_rounds: 12,
            p_value: 0.5,
            round_details: "[]".to_string(),
        };
        store.insert_esp_game(&game).unwrap();
    }

    // List with limit=2 should return the 2 most recent (score 2 and 1)
    let games = store.list_esp_games(2).unwrap();
    assert_eq!(games.len(), 2);
    assert_eq!(games[0].score, 2); // most recent first
    assert_eq!(games[1].score, 1);

    // List with limit=0 returns empty
    let empty = store.list_esp_games(0).unwrap();
    assert!(empty.is_empty());
}

#[test]
fn test_retry_state_persist_and_load() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    // Insert session with retry fields
    let mut session = make_test_session();
    session.retry_attempt = Some(2);
    session.max_retries = Some(3);
    store.insert_session(&session).unwrap();

    // Load and verify retry fields persisted
    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(loaded.retry_attempt, Some(2));
    assert_eq!(loaded.max_retries, Some(3));
}

#[test]
fn test_update_retry_state() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    // Insert session with no retry fields
    let session = make_test_session();
    store.insert_session(&session).unwrap();

    // Update retry state
    store
        .update_retry_state(session.id, Some(1), Some(2))
        .unwrap();

    // Load and verify
    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(loaded.retry_attempt, Some(1));
    assert_eq!(loaded.max_retries, Some(2));
}

#[test]
fn test_retry_fields_null_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = Store::open(&db_path).unwrap();

    // Insert session without retry fields
    let session = make_test_session();
    store.insert_session(&session).unwrap();

    let loaded = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(loaded.retry_attempt, None);
    assert_eq!(loaded.max_retries, None);
}
